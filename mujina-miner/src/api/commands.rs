//! Command types sent from API handlers to backend components.
//!
//! Each command carries a oneshot reply channel so the handler can
//! await the result and translate it into an HTTP response.

use anyhow::Result;
use tokio::sync::oneshot;

/// Commands from the API to the scheduler.
#[derive(Debug)]
pub enum SchedulerCommand {
    /// Pause job distribution to all threads.
    PauseMining { reply: oneshot::Sender<Result<()>> },

    /// Resume job distribution after a pause.
    ResumeMining { reply: oneshot::Sender<Result<()>> },

    /// Change one live source's weight without reconnecting it.
    /// `name` is the registration name (`pool-0`, ...).
    SetWeight { name: String, weight: u16 },
}

/// Commands from the API to board management.
pub enum BoardCommand {
    /// Set a fan's target duty cycle on a specific board.
    SetFanTarget {
        board: String,
        fan: String,
        /// Target duty cycle (0--100), or None for automatic control.
        percent: Option<u8>,
        reply: oneshot::Sender<Result<()>>,
    },
}
