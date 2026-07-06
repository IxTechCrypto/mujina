//! Command types sent from API handlers to backend components.
//!
//! Each command carries a oneshot reply channel so the handler can
//! await the result and translate it into an HTTP response.

use anyhow::Result;
use tokio::sync::oneshot;

/// Commands from the API to the scheduler.
pub enum SchedulerCommand {
    /// Pause job distribution to all threads.
    PauseMining { reply: oneshot::Sender<Result<()>> },

    /// Resume job distribution after a pause.
    ResumeMining { reply: oneshot::Sender<Result<()>> },
}

/// A requested change to a board's fan control policy.
///
/// Fields left `None` keep their current value, so a caller can flip
/// only the target temperature without disturbing the minimum speed.
#[derive(Clone, Copy, Debug, Default)]
pub struct FanControlUpdate {
    /// `true` = automatic temperature-tracking curve, `false` = fixed
    /// manual duty cycle. Required; the other fields refine the chosen mode.
    pub auto: bool,
    /// Automatic-mode target ASIC temperature, in Celsius.
    pub target_c: Option<f32>,
    /// Automatic-mode minimum duty cycle (0--100).
    pub min_percent: Option<u8>,
    /// Manual-mode fixed duty cycle (0--100).
    pub percent: Option<u8>,
}

/// Commands from the API to board management.
pub enum BoardCommand {
    /// Update the fan control policy on the board this channel serves.
    SetFanControl {
        update: FanControlUpdate,
        reply: oneshot::Sender<Result<()>>,
    },
}
