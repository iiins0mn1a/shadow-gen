//! Unix domain socket driver for external time-control.
//!
//! An external tool (Python, Go, etc.) connects to a Unix socket and sends
//! JSON-encoded commands. Unlike the original script-oriented driver, this
//! controller is designed to back a human-facing panel. It therefore supports:
//! - long-running `continue`
//! - asynchronous `pause` requests while the simulation is running
//! - `continue_for`
//! - `step_one_window`
//! - `show_info` while paused
//! - instant commands such as `checkpoint`, `restore`, and `restart` while paused
//!
//! Protocol (newline-delimited JSON over Unix stream socket):
//! ```text
//! -> {"cmd":"continue"}
//! <- {"status":"ok","sim_time_ns":0,"message":"continuing until paused"}
//! -> {"cmd":"pause"}
//! <- {"status":"ok","sim_time_ns":123456,"message":"pause requested at next window boundary"}
//! -> {"cmd":"info"}
//! <- {"status":"ok","sim_time_ns":123456,"message":"** Next window ..."}
//! -> {"cmd":"wait_until_paused"}
//! <- {"status":"ok","sim_time_ns":123456,"message":"simulation paused"}
//! -> {"cmd":"checkpoint","label":"cp1"}
//! <- {"status":"ok","sim_time_ns":5000000000}
//! ```

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};

use serde::{Deserialize, Serialize};

use super::commands::{ControlDecision, RestartSource};
use super::controller::{PrintNextWindowInfoFn, TimeController, WindowBoundaryContext};

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
    run_continuously: bool,
    pause_requested: bool,
    step_windows_remaining: u64,
    info_requested: bool,
    last_info: Option<String>,
}

struct SharedState {
    inner: Mutex<StateInner>,
    cv: Condvar,
}

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
                run_continuously: false,
                pause_requested: false,
                step_windows_remaining: 0,
                info_requested: false,
                last_info: None,
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

fn write_json(writer: &mut UnixStream, resp: Response) -> anyhow::Result<()> {
    writeln!(writer, "{}", serde_json::to_string(&resp)?)?;
    Ok(())
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
                write_json(
                    &mut writer,
                    Response {
                        status: "error".into(),
                        sim_time_ns: None,
                        message: Some(format!("Invalid JSON: {e}")),
                    },
                )?;
                continue;
            }
        };

        if req.cmd == "status" {
            log::trace!("Control socket received: {:?}", req);
        } else {
            log::info!("Control socket received: {:?}", req);
        }

        match req.cmd.as_str() {
            "wait_until_paused" => {
                let guard = state.inner.lock().unwrap();
                let guard = state.cv.wait_while(guard, |g| !g.sim_waiting).unwrap();
                write_json(
                    &mut writer,
                    Response {
                        status: "ok".into(),
                        sim_time_ns: Some(guard.sim_time_ns),
                        message: Some("simulation paused".into()),
                    },
                )?;
            }
            "status" => {
                let guard = state.inner.lock().unwrap();
                write_json(
                    &mut writer,
                    Response {
                        status: "ok".into(),
                        sim_time_ns: Some(guard.sim_time_ns),
                        message: Some(format!(
                            "sim_waiting={}, run_continuously={}, auto_run_until={:?}, pause_requested={}, step_windows_remaining={}",
                            guard.sim_waiting,
                            guard.run_continuously,
                            guard.auto_run_until_ns,
                            guard.pause_requested,
                            guard.step_windows_remaining
                        )),
                    },
                )?;
            }
            "continue" => {
                let mut guard = state.inner.lock().unwrap();
                guard.run_continuously = true;
                guard.pause_requested = false;
                guard.auto_run_until_ns = None;
                guard.step_windows_remaining = 0;
                state.cv.notify_all();
                write_json(
                    &mut writer,
                    Response {
                        status: "ok".into(),
                        sim_time_ns: Some(guard.sim_time_ns),
                        message: Some("continuing until paused".into()),
                    },
                )?;
            }
            "pause" => {
                let mut guard = state.inner.lock().unwrap();
                guard.pause_requested = true;
                state.cv.notify_all();
                write_json(
                    &mut writer,
                    Response {
                        status: "ok".into(),
                        sim_time_ns: Some(guard.sim_time_ns),
                        message: Some("pause requested at next window boundary".into()),
                    },
                )?;
            }
            "continue_for" => {
                let Some(dur) = req.duration_ns else {
                    write_json(
                        &mut writer,
                        Response {
                            status: "error".into(),
                            sim_time_ns: None,
                            message: Some("Missing 'duration_ns' for continue_for".into()),
                        },
                    )?;
                    continue;
                };
                let mut guard = state.inner.lock().unwrap();
                guard.run_continuously = false;
                guard.pause_requested = false;
                guard.step_windows_remaining = 0;
                guard.auto_run_until_ns = Some(guard.sim_time_ns.saturating_add(dur));
                state.cv.notify_all();
                write_json(
                    &mut writer,
                    Response {
                        status: "ok".into(),
                        sim_time_ns: Some(guard.sim_time_ns),
                        message: Some(format!("continuing for {} ns", dur)),
                    },
                )?;
            }
            "step_one_window" | "step" => {
                let mut guard = state.inner.lock().unwrap();
                if !guard.sim_waiting {
                    write_json(
                        &mut writer,
                        Response {
                            status: "error".into(),
                            sim_time_ns: Some(guard.sim_time_ns),
                            message: Some("simulation is not paused; cannot step".into()),
                        },
                    )?;
                    continue;
                }
                guard.run_continuously = false;
                guard.pause_requested = false;
                guard.auto_run_until_ns = None;
                guard.step_windows_remaining = 1;
                state.cv.notify_all();
                write_json(
                    &mut writer,
                    Response {
                        status: "ok".into(),
                        sim_time_ns: Some(guard.sim_time_ns),
                        message: Some("will run exactly one window".into()),
                    },
                )?;
            }
            "show_info" | "info" => {
                let mut guard = state.inner.lock().unwrap();
                if !guard.sim_waiting {
                    write_json(
                        &mut writer,
                        Response {
                            status: "error".into(),
                            sim_time_ns: Some(guard.sim_time_ns),
                            message: Some(
                                "simulation is not paused; info is only available while paused"
                                    .into(),
                            ),
                        },
                    )?;
                    continue;
                }
                guard.info_requested = true;
                guard.last_info = None;
                state.cv.notify_all();
                while guard.last_info.is_none() {
                    guard = state.cv.wait(guard).unwrap();
                }
                write_json(
                    &mut writer,
                    Response {
                        status: "ok".into(),
                        sim_time_ns: Some(guard.sim_time_ns),
                        message: guard.last_info.take(),
                    },
                )?;
            }
            "restart" | "replay" | "checkpoint" | "restore" => {
                let decision = match req.cmd.as_str() {
                    "restart" | "replay" => ControlDecision::Restart {
                        run_until_ns: req.run_until_ns,
                        source: RestartSource::External,
                    },
                    "checkpoint" => ControlDecision::CheckpointNow {
                        label: req.label.unwrap_or_else(|| "default".into()),
                    },
                    "restore" => ControlDecision::RestoreCheckpoint {
                        label: req.label.unwrap_or_else(|| "default".into()),
                    },
                    _ => unreachable!(),
                };

                {
                    let mut guard = state.inner.lock().unwrap();
                    if !guard.sim_waiting {
                        write_json(
                            &mut writer,
                            Response {
                                status: "error".into(),
                                sim_time_ns: Some(guard.sim_time_ns),
                                message: Some(format!(
                                    "simulation is not paused; '{}' must be issued while paused",
                                    req.cmd
                                )),
                            },
                        )?;
                        continue;
                    }
                    guard.pending = Some(decision);
                    guard.sim_waiting = false;
                    state.cv.notify_all();
                }

                let guard = state.inner.lock().unwrap();
                let guard = state.cv.wait_while(guard, |g| !g.sim_waiting).unwrap();
                write_json(
                    &mut writer,
                    Response {
                        status: "ok".into(),
                        sim_time_ns: Some(guard.sim_time_ns),
                        message: None,
                    },
                )?;
            }
            other => {
                write_json(
                    &mut writer,
                    Response {
                        status: "error".into(),
                        sim_time_ns: None,
                        message: Some(format!("Unknown command: {other}")),
                    },
                )?;
            }
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
        print_info: PrintNextWindowInfoFn<'_>,
    ) -> ControlDecision {
        let trace = run_control_trace_enabled();
        let mut guard = self.state.inner.lock().unwrap();
        guard.sim_time_ns = ctx.current_sim_time_ns;
        if trace {
            log::info!(
                "run-control boundary enter: current_sim_time_ns={} window_start_ns={} window_end_ns={} min_next_event_ns={} sim_waiting={} run_continuously={} auto_run_until={:?} pause_requested={} step_windows_remaining={}",
                ctx.current_sim_time_ns,
                (ctx.window_start - shadow_shim_helper_rs::emulated_time::EmulatedTime::SIMULATION_START).as_nanos(),
                (ctx.window_end - shadow_shim_helper_rs::emulated_time::EmulatedTime::SIMULATION_START).as_nanos(),
                (ctx.min_next_event_time - shadow_shim_helper_rs::emulated_time::EmulatedTime::SIMULATION_START).as_nanos(),
                guard.sim_waiting,
                guard.run_continuously,
                guard.auto_run_until_ns,
                guard.pause_requested,
                guard.step_windows_remaining,
            );
        }

        if guard.step_windows_remaining > 0 {
            guard.step_windows_remaining -= 1;
            if guard.step_windows_remaining == 0 {
                guard.run_continuously = false;
            } else {
                if trace {
                    log::info!("run-control boundary decision: continue step_windows_remaining={}", guard.step_windows_remaining);
                }
                return ControlDecision::Continue;
            }
        }

        if let Some(deadline) = guard.auto_run_until_ns {
            if ctx.current_sim_time_ns < deadline {
                if trace {
                    log::info!(
                        "run-control boundary decision: continue until deadline={} current={}",
                        deadline,
                        ctx.current_sim_time_ns
                    );
                }
                return ControlDecision::Continue;
            }
            guard.auto_run_until_ns = None;
            guard.run_continuously = false;
        }

        if guard.pause_requested {
            guard.pause_requested = false;
            guard.run_continuously = false;
        } else if guard.run_continuously {
            return ControlDecision::Continue;
        }

        guard.sim_waiting = true;
        self.state.cv.notify_all();
        if trace {
            log::info!("run-control boundary paused: current_sim_time_ns={}", guard.sim_time_ns);
        }

        loop {
            if guard.info_requested {
                guard.info_requested = false;
                guard.last_info = Some(print_info());
                self.state.cv.notify_all();
            }

            if let Some(cmd) = guard.pending.take() {
                guard.sim_waiting = false;
                self.state.cv.notify_all();
                if trace {
                    log::info!("run-control boundary decision: pending {:?}", cmd);
                }
                return cmd;
            }

            if guard.run_continuously
                || guard.auto_run_until_ns.is_some()
                || guard.step_windows_remaining > 0
            {
                guard.sim_waiting = false;
                self.state.cv.notify_all();
                if trace {
                    log::info!(
                        "run-control boundary decision: resume from paused run_continuously={} auto_run_until={:?} step_windows_remaining={}",
                        guard.run_continuously,
                        guard.auto_run_until_ns,
                        guard.step_windows_remaining
                    );
                }
                return ControlDecision::Continue;
            }

            guard = self.state.cv.wait(guard).unwrap();
        }
    }

    fn on_simulation_end(&self) {
        let mut guard = self.state.inner.lock().unwrap();
        guard.sim_waiting = true;
        guard.auto_run_until_ns = None;
        guard.run_continuously = false;
        guard.pause_requested = false;
        guard.step_windows_remaining = 0;
        self.state.cv.notify_all();
    }
}

fn run_control_trace_enabled() -> bool {
    std::env::var("SHADOW_RUN_CONTROL_TRACE")
        .map(|raw| !(raw.trim().is_empty() || raw.trim() == "0"))
        .unwrap_or(false)
}

impl Drop for SocketController {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
    }
}
