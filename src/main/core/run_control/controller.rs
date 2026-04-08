use shadow_shim_helper_rs::emulated_time::EmulatedTime;

use super::commands::ControlDecision;

/// Contextual information provided to the controller at each window boundary.
pub struct WindowBoundaryContext {
    pub current_sim_time_ns: u64,
    pub min_next_event_time: EmulatedTime,
    pub window_start: EmulatedTime,
    pub window_end: EmulatedTime,
}

/// Callback for printing host/PID info about the upcoming window.
/// The controller receives this so interactive implementations can show
/// diagnostic information on demand while paused.
pub type PrintNextWindowInfoFn<'a> = &'a mut dyn FnMut();

/// The trait that any time-control implementation must satisfy.
///
/// `TimeController` is invoked at well-defined points in the simulation
/// lifecycle. Implementations can block (e.g. wait for user input), inspect
/// state, or request actions such as restart/checkpoint.
pub trait TimeController: Send + Sync {
    /// Called once before the first simulation window is executed.
    fn on_simulation_start(&self);

    /// Called after every window completes, *before* computing the next window.
    ///
    /// `print_info` can be called by interactive controllers to display
    /// information about scheduled hosts in the upcoming window.
    fn on_window_boundary(
        &self,
        ctx: &WindowBoundaryContext,
        print_info: PrintNextWindowInfoFn<'_>,
    ) -> ControlDecision;

    /// Called once after the simulation loop exits (either normally or before a
    /// restart).
    fn on_simulation_end(&self);
}

/// A no-op controller that always continues. Used when the `enable_run_control`
/// feature is disabled or when no interactive terminal is available.
pub struct NoopController;

impl TimeController for NoopController {
    fn on_simulation_start(&self) {}

    fn on_window_boundary(
        &self,
        _ctx: &WindowBoundaryContext,
        _print_info: PrintNextWindowInfoFn<'_>,
    ) -> ControlDecision {
        ControlDecision::Continue
    }

    fn on_simulation_end(&self) {}
}
