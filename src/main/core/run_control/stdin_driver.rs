use std::io::{BufRead, IsTerminal};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use super::commands::{ControlDecision, RestartSource, INTERNAL_RESTART_WARNING};
use super::controller::{PrintNextWindowInfoFn, TimeController, WindowBoundaryContext};

/// Interactive stdin-driven time controller.
///
/// Implements a *soft pause*: at a window boundary the simulation blocks on a
/// `Condvar` until resumed. This avoids stopping in the middle of host
/// execution or shim IPC.
///
/// Supported commands (when stdin is a TTY):
/// - `p` : pause at next window boundary
/// - `c` : continue
/// - `cN` : continue for N simulated seconds, then pause
/// - `n` : run one more window, then pause
/// - `s` / `info` : show next-window hosts/PIDs (while paused)
/// - `s:<pid>` : print gdb attach command
/// - `r` : restart from t=0
/// - `rN` : restart and run to N seconds
pub struct InteractiveController {
    inner: Arc<InteractiveState>,
}

struct InteractiveState {
    pause_requested: AtomicBool,
    restart_requested: AtomicBool,
    restart_run_until_ns: AtomicU64,
    info_requested: AtomicBool,
    skip_start_pause: AtomicBool,
    run_for_ns: AtomicU64,
    run_until_abs_ns: AtomicU64,
    step_windows_remaining: AtomicU64,
    paused: Mutex<bool>,
    cv: Condvar,
}

static STDIN_THREAD_STARTED: OnceLock<()> = OnceLock::new();

/// Global slot for passing `run_until_ns` across in-process restarts.
static RESTART_RUN_UNTIL_NS: AtomicU64 = AtomicU64::new(u64::MAX);

/// Called from `shadow.rs` after catching a restart to seed the next run's
/// `run_until_abs_ns`.
pub fn set_restart_run_until(run_until_ns: Option<u64>) {
    RESTART_RUN_UNTIL_NS.store(run_until_ns.unwrap_or(u64::MAX), Ordering::Relaxed);
}

impl InteractiveController {
    pub fn new() -> Self {
        let inner = Arc::new(InteractiveState {
            pause_requested: AtomicBool::new(false),
            restart_requested: AtomicBool::new(false),
            restart_run_until_ns: AtomicU64::new(u64::MAX),
            info_requested: AtomicBool::new(false),
            skip_start_pause: AtomicBool::new(false),
            run_for_ns: AtomicU64::new(0),
            run_until_abs_ns: AtomicU64::new(u64::MAX),
            step_windows_remaining: AtomicU64::new(0),
            paused: Mutex::new(false),
            cv: Condvar::new(),
        });

        Self { inner }
    }

    /// Reset mutable state for a fresh simulation run (important for
    /// in-process restarts).
    fn reset(&self) {
        let s = &self.inner;
        s.pause_requested.store(false, Ordering::Relaxed);
        s.restart_requested.store(false, Ordering::Relaxed);
        s.info_requested.store(false, Ordering::Relaxed);
        s.run_for_ns.store(0, Ordering::Relaxed);
        s.run_until_abs_ns.store(u64::MAX, Ordering::Relaxed);
        s.step_windows_remaining.store(0, Ordering::Relaxed);
        s.skip_start_pause.store(false, Ordering::Relaxed);
        s.restart_run_until_ns.store(u64::MAX, Ordering::Relaxed);

        let pending = RESTART_RUN_UNTIL_NS.swap(u64::MAX, Ordering::Relaxed);
        if pending != u64::MAX {
            s.run_until_abs_ns.store(pending, Ordering::Relaxed);
            s.skip_start_pause.store(true, Ordering::Relaxed);
        }
    }

    /// Spawn the stdin reader thread (at most once per process).
    fn spawn_stdin_thread(&self) {
        let state = Arc::clone(&self.inner);
        STDIN_THREAD_STARTED.get_or_init(move || {
            if !std::io::stdin().is_terminal() {
                return;
            }

            std::thread::spawn(move || {
                eprintln!(
                    "\
** Shadow run-control (stdin; simulated time)\n\
**   p<Enter>: pause at next window boundary\n\
**   c<Enter>: continue\n\
**   cN<Enter>: continue for N simulated seconds, then pause at next window boundary (e.g. c10)\n\
**   n<Enter>: run exactly one window, then pause\n\
**   s<Enter>: show next-window hosts/PIDs (when paused)\n\
**   s:<pid><Enter>: print gdb attach command (e.g. s:12345)\n\
**   info<Enter>: show next-window hosts/PIDs (when paused)\n\
**   r<Enter>: restart from t=0s (in-process)\n\
**   rN<Enter>: restart and run to N seconds (e.g. r10)\n"
                );

                let stdin = std::io::stdin();
                for line in stdin.lock().lines().flatten() {
                    Self::handle_stdin_line(&state, line.trim());
                }
            });
        });
    }

    fn handle_stdin_line(s: &InteractiveState, cmd: &str) {
        if cmd.is_empty() {
            return;
        }

        if cmd == "p" {
            s.pause_requested.store(true, Ordering::Relaxed);
            eprintln!("** run-control: pause requested (will pause at next window boundary)");
            return;
        }

        if cmd == "n" {
            s.step_windows_remaining.store(1, Ordering::Relaxed);
            s.run_until_abs_ns.store(u64::MAX, Ordering::Relaxed);
            *s.paused.lock().unwrap() = false;
            s.cv.notify_all();
            eprintln!("** run-control: will run 1 window and then pause");
            return;
        }

        if cmd == "r" {
            s.restart_run_until_ns.store(u64::MAX, Ordering::Relaxed);
            s.skip_start_pause.store(false, Ordering::Relaxed);
            s.restart_requested.store(true, Ordering::Relaxed);
            s.cv.notify_all();
            eprintln!("** run-control: restart requested (in-process, internal only)");
            eprintln!("{INTERNAL_RESTART_WARNING}");
            return;
        }

        if let Some(rest) = cmd.strip_prefix('r') {
            if !rest.is_empty() {
                if let Ok(secs) = rest.parse::<u64>() {
                    s.restart_run_until_ns
                        .store(secs.saturating_mul(1_000_000_000), Ordering::Relaxed);
                    s.skip_start_pause.store(true, Ordering::Relaxed);
                    s.restart_requested.store(true, Ordering::Relaxed);
                    s.cv.notify_all();
                    eprintln!("** run-control: restart requested (run to t={secs}s, internal only)");
                    eprintln!("{INTERNAL_RESTART_WARNING}");
                    return;
                }
            }
        }

        if cmd == "s" || cmd == "info" {
            s.info_requested.store(true, Ordering::Relaxed);
            s.cv.notify_all();
            eprintln!("** run-control: info requested (will print while paused)");
            return;
        }

        if let Some(rest) = cmd.strip_prefix("s:") {
            if let Ok(pid) = rest.parse::<i32>() {
                eprintln!(
                    "** run-control: attach gdb manually with: gdb/dlv -p/attach {}",
                    pid
                );
                return;
            } else {
                eprintln!("** run-control: invalid PID: '{}'", rest);
                return;
            }
        }

        if cmd == "c" {
            s.step_windows_remaining.store(0, Ordering::Relaxed);
            s.run_until_abs_ns.store(u64::MAX, Ordering::Relaxed);
            s.run_for_ns.store(0, Ordering::Relaxed);
            *s.paused.lock().unwrap() = false;
            s.cv.notify_all();
            eprintln!("** run-control: continue");
            return;
        }

        if let Some(rest) = cmd.strip_prefix('c') {
            if let Ok(secs) = rest.parse::<u64>() {
                s.step_windows_remaining.store(0, Ordering::Relaxed);
                s.run_until_abs_ns.store(u64::MAX, Ordering::Relaxed);
                s.run_for_ns
                    .store(secs.saturating_mul(1_000_000_000), Ordering::Relaxed);
                *s.paused.lock().unwrap() = false;
                s.cv.notify_all();
                eprintln!(
                    "** run-control: continue for {secs}s simulated time (will pause at a window boundary)"
                );
                return;
            }
        }

        eprintln!(
            "** Unknown command: '{cmd}'. Use: p | c | cN (e.g. c10) | n | s | s:<pid> | info | r | rN (e.g. r10)"
        );
    }
}

fn fmt_s(ns: u64) -> String {
    if ns % 1_000_000_000 == 0 {
        format!("{}s", ns / 1_000_000_000)
    } else {
        format!("{:.6}s", (ns as f64) / 1_000_000_000.0)
    }
}

impl TimeController for InteractiveController {
    fn on_simulation_start(&self) {
        self.reset();
        self.spawn_stdin_thread();

        // Auto-pause at t=0 when stdin is a TTY.
        if std::io::stdin().is_terminal() {
            let s = &self.inner;
            let mut paused = s.paused.lock().unwrap();
            let skip = s.skip_start_pause.load(Ordering::Relaxed);
            s.skip_start_pause.store(false, Ordering::Relaxed);
            if !*paused && !skip {
                *paused = true;
                eprintln!(
                    "\
** Shadow paused at start (t=0s)\n\
** Commands: c | cN (e.g. c10) | n | info | s:<pid> | r | rN"
                );
            }
        }

        // Block if paused at start.
        let s = &self.inner;
        let mut paused = s.paused.lock().unwrap();
        while *paused {
            paused = s.cv.wait(paused).unwrap();
        }
    }

    fn on_window_boundary(
        &self,
        ctx: &WindowBoundaryContext,
        print_info: PrintNextWindowInfoFn<'_>,
    ) -> ControlDecision {
        let s = &self.inner;

        // Apply cN: convert relative duration -> absolute pause target.
        let run_for = s.run_for_ns.swap(0, Ordering::Relaxed);
        if run_for != 0 {
            let target = ctx.current_sim_time_ns.saturating_add(run_for);
            s.run_until_abs_ns.store(target, Ordering::Relaxed);
            eprintln!(
                "** run-control: will pause at ~t={} (after +{} simulated seconds; next window boundary >= target)",
                fmt_s(target),
                run_for / 1_000_000_000
            );
        }

        // Apply n: count down windows.
        let steps_left = s.step_windows_remaining.load(Ordering::Relaxed);
        if steps_left > 0 {
            let new = s.step_windows_remaining.fetch_sub(1, Ordering::Relaxed) - 1;
            if new == 0 {
                s.pause_requested.store(true, Ordering::Relaxed);
            }
        }

        // Auto-pause when run-until time is reached.
        let run_until = s.run_until_abs_ns.load(Ordering::Relaxed);
        if run_until != u64::MAX && ctx.current_sim_time_ns >= run_until {
            s.run_until_abs_ns.store(u64::MAX, Ordering::Relaxed);
            s.pause_requested.store(true, Ordering::Relaxed);
        }

        // Check restart request.
        if s.restart_requested.swap(false, Ordering::Relaxed) {
            let run_until = s.restart_run_until_ns.load(Ordering::Relaxed);
            let run_until_ns = if run_until == u64::MAX {
                None
            } else {
                Some(run_until)
            };
            return ControlDecision::Restart {
                run_until_ns,
                source: RestartSource::Internal,
            };
        }

        // Handle pause request and block.
        if s.pause_requested.swap(false, Ordering::Relaxed) {
            let mut paused = s.paused.lock().unwrap();
            *paused = true;

            eprintln!(
                "\
** Shadow paused at window boundary\n\
**   next window start: t={}",
                fmt_s(ctx.current_sim_time_ns),
            );

            print_info();

            eprintln!("**");
            eprintln!("** To attach gdb: s:<pid> (e.g. s:12345)");
            eprintln!("** Commands: c | cN (e.g. c10) | n | p | s | s:<pid> | info | r | rN");
        }

        // If paused, block until resumed.
        let mut paused = s.paused.lock().unwrap();
        while *paused {
            if s.restart_requested.swap(false, Ordering::Relaxed) {
                let run_until = s.restart_run_until_ns.load(Ordering::Relaxed);
                *paused = false;
                let run_until_ns = if run_until == u64::MAX {
                    None
                } else {
                    Some(run_until)
                };
                return ControlDecision::Restart {
                    run_until_ns,
                    source: RestartSource::Internal,
                };
            }

            if s.info_requested.swap(false, Ordering::Relaxed) {
                print_info();
            }

            paused = s.cv.wait(paused).unwrap();
        }

        ControlDecision::Continue
    }

    fn on_simulation_end(&self) {}
}
