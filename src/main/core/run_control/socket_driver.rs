//! Unix domain socket driver for external time-control.
//!
//! An external tool (Python, Go, etc.) connects to a Unix socket and sends
//! JSON-encoded commands. The simulation blocks at each window boundary
//! waiting for a command from the connected client when in "paused" mode.
//!
//! Protocol (newline-delimited JSON over Unix stream socket):
//! ```text
//! -> {"cmd":"continue"}
//! <- {"status":"ok","sim_time_ns":123456}
//! -> {"cmd":"continue_for","duration_ns":5000000000}
//! <- {"status":"ok","sim_time_ns":5000000000}
//! -> {"cmd":"checkpoint","label":"cp1"}
//! <- {"status":"ok","sim_time_ns":5000000000}
//! ```
//!
//! The simulation starts **paused** and waits for the first command.
//! `continue_for` sets a deadline; once the deadline is reached the sim
//! automatically pauses again. The response is only sent back to the client
//! when the simulation has finished processing the command (i.e. reached the
//! deadline or finished the instant command).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};

use serde::{Deserialize, Serialize};

use super::commands::{ControlDecision, RestartSource};
use super::controller::{PrintNextWindowInfoFn, TimeController, WindowBoundaryContext};

/// JSON request from the external client.
#[derive(Debug, Deserialize)]
struct Request {
    cmd: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    duration_ns: Option<u64>,
    #[serde(default)]
    run_until_ns: Option<u64>,
}

/// JSON response sent back to the client.
#[derive(Debug, Serialize)]
struct Response {
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    sim_time_ns: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

struct StateInner {
    sim_time_ns: u64,
    pending: Option<ControlDecision>,
    auto_run_until_ns: Option<u64>,
    sim_waiting: bool,
}

struct SharedState {
    inner: Mutex<StateInner>,
    cv: Condvar,
}

/// A time controller driven by an external tool over a Unix domain socket.
pub struct SocketController {
    socket_path: PathBuf,
    state: &'static SharedState,
    listener_spawned: AtomicBool,
}

impl SocketController {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        let state = Box::leak(Box::new(SharedState {
            inner: Mutex::new(StateInner {
                sim_time_ns: 0,
                pending: None,
                auto_run_until_ns: None,
                sim_waiting: false,
            }),
            cv: Condvar::new(),
        }));
        Self {
            socket_path: socket_path.into(),
            state,
            listener_spawned: AtomicBool::new(false),
        }
    }

    fn spawn_listener(&self) {
        if self.listener_spawned.swap(true, Ordering::SeqCst) {
            return;
        }

        let path = self.socket_path.clone();
        let state: &'static SharedState = self.state;

        let _ = std::fs::remove_file(&path);

        std::thread::spawn(move || {
            let listener = match UnixListener::bind(&path) {
                Ok(l) => {
                    eprintln!("** Shadow control socket listening on: {}", path.display());
                    l
                }
                Err(e) => {
                    log::error!("Failed to bind control socket at {}: {e}", path.display());
                    return;
                }
            };

            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        std::thread::spawn(move || {
                            if let Err(e) = handle_client(stream, state) {
                                log::warn!("Control socket client error: {e}");
                            }
                        });
                    }
                    Err(e) => {
                        log::warn!("Control socket accept error: {e}");
                    }
                }
            }
        });
    }
}

fn handle_client(stream: UnixStream, state: &SharedState) -> anyhow::Result<()> {
    let reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let resp = Response {
                    status: "error".into(),
                    sim_time_ns: None,
                    message: Some(format!("Invalid JSON: {e}")),
                };
                writeln!(writer, "{}", serde_json::to_string(&resp)?)?;
                continue;
            }
        };

        log::info!("Control socket received: {:?}", req);

        let (decision, is_duration_cmd) = match req.cmd.as_str() {
            "pause" => (ControlDecision::PauseAtBoundary, false),
            "continue" => (ControlDecision::Continue, true),
            "continue_for" => {
                if req.duration_ns.is_none() {
                    let resp = Response {
                        status: "error".into(),
                        sim_time_ns: None,
                        message: Some("Missing 'duration_ns' for continue_for".into()),
                    };
                    writeln!(writer, "{}", serde_json::to_string(&resp)?)?;
                    continue;
                }
                (ControlDecision::Continue, true)
            }
            "restart" | "replay" => (
                ControlDecision::Restart {
                    run_until_ns: req.run_until_ns,
                    source: RestartSource::External,
                },
                false,
            ),
            "checkpoint" => {
                let label = req.label.unwrap_or_else(|| "default".into());
                (ControlDecision::CheckpointNow { label }, false)
            }
            "restore" => {
                let label = req.label.unwrap_or_else(|| "default".into());
                (ControlDecision::RestoreCheckpoint { label }, false)
            }
            "status" => {
                let guard = state.inner.lock().unwrap();
                let resp = Response {
                    status: "ok".into(),
                    sim_time_ns: Some(guard.sim_time_ns),
                    message: Some(format!(
                        "sim_waiting={}, auto_run_until={:?}",
                        guard.sim_waiting, guard.auto_run_until_ns
                    )),
                };
                drop(guard);
                writeln!(writer, "{}", serde_json::to_string(&resp)?)?;
                continue;
            }
            other => {
                let resp = Response {
                    status: "error".into(),
                    sim_time_ns: None,
                    message: Some(format!("Unknown command: {other}")),
                };
                writeln!(writer, "{}", serde_json::to_string(&resp)?)?;
                continue;
            }
        };

        // 1. Wait until the simulation thread is parked (waiting for a command).
        {
            let mut guard = state.inner.lock().unwrap();
            while !guard.sim_waiting {
                guard = state.cv.wait(guard).unwrap();
            }

            // 2. Deliver the command.
            if req.cmd == "continue_for" {
                let dur = req.duration_ns.unwrap();
                guard.auto_run_until_ns = Some(guard.sim_time_ns.saturating_add(dur));
            }
            guard.pending = Some(decision);
            guard.sim_waiting = false;
            state.cv.notify_all();
        }

        if is_duration_cmd {
            // 3a. For duration commands, wait until the sim parks again
            //     (meaning it reached the deadline or end of simulation).
            let guard = state.inner.lock().unwrap();
            let guard = state.cv.wait_while(guard, |g| !g.sim_waiting).unwrap();
            let time = guard.sim_time_ns;
            drop(guard);

            let resp = Response {
                status: "ok".into(),
                sim_time_ns: Some(time),
                message: None,
            };
            writeln!(writer, "{}", serde_json::to_string(&resp)?)?;
        } else {
            // 3b. For instant commands (checkpoint, restore, pause),
            //     wait until the sim parks again after processing.
            let guard = state.inner.lock().unwrap();
            let guard = state.cv.wait_while(guard, |g| !g.sim_waiting).unwrap();
            let time = guard.sim_time_ns;
            drop(guard);

            let resp = Response {
                status: "ok".into(),
                sim_time_ns: Some(time),
                message: None,
            };
            writeln!(writer, "{}", serde_json::to_string(&resp)?)?;
        }
    }

    Ok(())
}

impl TimeController for SocketController {
    fn on_simulation_start(&self) {
        self.spawn_listener();
    }

    fn on_window_boundary(
        &self,
        ctx: &WindowBoundaryContext,
        _print_info: PrintNextWindowInfoFn<'_>,
    ) -> ControlDecision {
        let mut guard = self.state.inner.lock().unwrap();
        guard.sim_time_ns = ctx.current_sim_time_ns;

        // If auto-running towards a deadline, check if we've reached it.
        if let Some(deadline) = guard.auto_run_until_ns {
            if ctx.current_sim_time_ns < deadline {
                return ControlDecision::Continue;
            }
            guard.auto_run_until_ns = None;
        }

        // Park: signal that we are waiting and block until a command arrives.
        guard.sim_waiting = true;
        self.state.cv.notify_all();

        while guard.pending.is_none() {
            guard = self.state.cv.wait(guard).unwrap();
        }

        let cmd = guard.pending.take().unwrap();
        guard.sim_waiting = false;
        self.state.cv.notify_all();

        cmd
    }

    fn on_simulation_end(&self) {
        // Signal any waiting clients that the sim is done / paused.
        // Do NOT delete the socket file here — the controller survives
        // across restart / restore cycles in shadow.rs's main loop.
        let mut guard = self.state.inner.lock().unwrap();
        guard.sim_waiting = true;
        guard.auto_run_until_ns = None;
        self.state.cv.notify_all();
    }
}

impl Drop for SocketController {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
    }
}
