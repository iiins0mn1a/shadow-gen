/// The origin of a restart/replay request. This distinction is important
/// because Shadow can only reset its own internal simulation state; external
/// stateful dependencies (databases, ledgers, file stores, etc.) must be
/// reset by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartSource {
    /// Initiated from the interactive stdin console (`r` / `rN`).
    /// Shadow will warn that only internal state is reset.
    Internal,
    /// Initiated from the external control socket (JSON-over-UDS).
    /// The external orchestrator is responsible for resetting any
    /// stateful dependencies before issuing this command.
    External,
}

/// Commands that can be sent to the time controller from an external source
/// (stdin, socket, API, etc.).
#[derive(Debug, Clone)]
pub enum ControlCommand {
    Pause,
    Continue,
    ContinueFor {
        sim_duration_ns: u64,
    },
    StepOneWindow,
    Restart {
        run_until_ns: Option<u64>,
        source: RestartSource,
    },
    ShowInfo,
    AttachInfo {
        pid: i32,
    },
    Checkpoint {
        label: String,
    },
    Restore {
        label: String,
    },
}

/// Decision returned by the [`TimeController`](super::TimeController) at each window boundary.
/// The simulation main loop acts on this decision before proceeding to the next window.
#[derive(Debug, Clone)]
pub enum ControlDecision {
    /// Continue running normally.
    Continue,
    /// Pause at the current window boundary (block until resumed).
    PauseAtBoundary,
    /// Restart the simulation from t=0. Optionally run until a given simulated
    /// nanosecond before pausing again. `source` indicates who initiated the
    /// restart so appropriate warnings can be emitted.
    Restart {
        run_until_ns: Option<u64>,
        source: RestartSource,
    },
    /// Take a checkpoint at the current window boundary.
    CheckpointNow { label: String },
    /// Restore from a previously saved checkpoint.
    RestoreCheckpoint { label: String },
}

/// The outcome of [`Manager::run`] when it finishes (or is interrupted by the
/// controller). This replaces the previous `RestartRequest` error-based mechanism.
#[derive(Debug)]
pub enum SimulationRunResult {
    /// Simulation ran to completion.
    Completed { num_plugin_errors: u32 },
    /// The controller requested an in-process restart.
    RestartRequested {
        run_until_ns: Option<u64>,
        source: RestartSource,
    },
    /// The controller requested restoring from a previously saved checkpoint.
    RestoreRequested { label: String },
}

/// Warning message emitted when an internal-only restart is triggered.
pub const INTERNAL_RESTART_WARNING: &str = "\
** WARNING: This restart only resets Shadow's internal simulation state \
(event queues, simulated time, host models). It does NOT reset external \
stateful dependencies such as databases, ledgers, or file stores that \
your simulated applications may depend on. If your simulation involves \
persistent external state, use the external control socket API so that \
your orchestrator can reset those dependencies before restarting Shadow.";
