//! Time-control subsystem for the Shadow simulation.
//!
//! This module defines the [`TimeController`] trait and several concrete
//! implementations that allow pausing, stepping, restarting, and (in the
//! future) checkpointing the simulation at window boundaries.

pub mod commands;
pub mod controller;
pub mod socket_driver;
pub mod stdin_driver;

pub use commands::{ControlCommand, ControlDecision, RestartSource, SimulationRunResult};
pub use controller::{NoopController, TimeController, WindowBoundaryContext};
pub use socket_driver::SocketController;
pub use stdin_driver::InteractiveController;
