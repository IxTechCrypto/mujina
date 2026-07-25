//! Dynamic board registration tracking.

use crate::api::commands::BoardCommand;
use crate::api_client::types::{BoardTelemetry, ThreadTelemetry};
use tokio::sync::{mpsc, watch};

/// Dynamic collection of board registrations.
///
/// Boards are added via `push()` from a background drain task that
/// receives registrations as boards connect. The registry cleans up
/// disconnected boards lazily when `boards()` is called.
pub struct BoardRegistry {
    boards: Vec<BoardRegistration>,
}

impl BoardRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self { boards: Vec::new() }
    }

    /// Add a board registration.
    pub fn push(&mut self, reg: BoardRegistration) {
        self.boards.push(reg);
    }

    /// Snapshot all connected boards, attaching each board's threads.
    ///
    /// Removes boards whose sender has been dropped (board disconnected)
    /// and returns the current state of each.
    ///
    /// `threads` comes from the scheduler, the only component that measures
    /// hashrate per thread. A board reports everything else about itself but
    /// cannot know its own hashrate, so it is filled in here using the
    /// thread names recorded when the board registered.
    pub fn boards(&mut self, threads: &[ThreadTelemetry]) -> Vec<BoardTelemetry> {
        self.boards
            .retain(|reg| reg.telemetry_rx.has_changed().is_ok());
        self.boards
            .iter()
            .map(|reg| {
                let mut board = reg.telemetry_rx.borrow().clone();
                board.threads = threads
                    .iter()
                    .filter(|t| reg.thread_names.iter().any(|n| n == &t.name))
                    .cloned()
                    .collect();
                board
            })
            .collect()
    }

    /// Command sender for the board with the given telemetry name, if it
    /// is connected and accepts commands.
    pub fn command_sender(&self, name: &str) -> Option<mpsc::Sender<BoardCommand>> {
        self.boards
            .iter()
            .find(|reg| reg.telemetry_rx.borrow().name == name)
            .and_then(|reg| reg.command_tx.clone())
    }
}

/// A board's registration with the API server.
pub struct BoardRegistration {
    pub telemetry_rx: watch::Receiver<BoardTelemetry>,
    /// Runtime command sender for this board, or `None` if it accepts
    /// no commands.
    pub command_tx: Option<mpsc::Sender<BoardCommand>>,
    /// Names of the hash threads this board handed to the scheduler.
    ///
    /// Captured at registration because that is the only moment the
    /// pairing exists in one place; afterwards the threads belong to the
    /// scheduler and the telemetry to the API.
    pub thread_names: Vec<String>,
}

#[cfg(test)]
mod tests {
    use tokio::sync::watch;

    use super::*;

    /// Create a board registration with the given name, returning the
    /// state sender so the test can update or drop it.
    fn make_board(name: &str) -> (watch::Sender<BoardTelemetry>, BoardRegistration) {
        let telemetry = BoardTelemetry {
            name: name.into(),
            model: "Test".into(),
            ..Default::default()
        };
        let (tx, rx) = watch::channel(telemetry);
        (
            tx,
            BoardRegistration {
                telemetry_rx: rx,
                command_tx: None,
                thread_names: Vec::new(),
            },
        )
    }

    #[test]
    fn tracks_pushed_registrations() {
        let mut registry = BoardRegistry::new();

        let (_keep_a, reg_a) = make_board("board-a");
        let (_keep_b, reg_b) = make_board("board-b");
        registry.push(reg_a);
        registry.push(reg_b);

        let boards = registry.boards(&[]);
        assert_eq!(boards.len(), 2);
        assert_eq!(boards[0].name, "board-a");
        assert_eq!(boards[1].name, "board-b");
    }

    #[test]
    fn removes_disconnected_boards() {
        let mut registry = BoardRegistry::new();

        let (keep, reg_a) = make_board("stays");
        let (drop_me, reg_b) = make_board("goes-away");
        registry.push(reg_a);
        registry.push(reg_b);

        // Both present initially
        assert_eq!(registry.boards(&[]).len(), 2);

        // Drop the sender for board B -- simulates board disconnect
        drop(drop_me);
        let boards = registry.boards(&[]);
        assert_eq!(boards.len(), 1);
        assert_eq!(boards[0].name, "stays");

        // Sender A still alive
        drop(keep);
    }

    #[test]
    fn attaches_only_a_boards_own_threads() {
        let mut registry = BoardRegistry::new();

        let (_a, mut reg_a) = make_board("board-a");
        reg_a.thread_names = vec!["thread-a1".into(), "thread-a2".into()];
        let (_b, mut reg_b) = make_board("board-b");
        reg_b.thread_names = vec!["thread-b1".into()];
        registry.push(reg_a);
        registry.push(reg_b);

        let threads = vec![
            ThreadTelemetry {
                name: "thread-a1".into(),
                hashrate: 100,
                is_active: true,
                chips: Vec::new(),
            },
            ThreadTelemetry {
                name: "thread-b1".into(),
                hashrate: 200,
                is_active: true,
                chips: Vec::new(),
            },
            ThreadTelemetry {
                name: "thread-a2".into(),
                hashrate: 300,
                is_active: false,
                chips: Vec::new(),
            },
            // Belongs to no registered board -- e.g. a board that just
            // disconnected but whose thread the scheduler has not dropped
            // yet. It must not be attributed to anybody.
            ThreadTelemetry {
                name: "orphan".into(),
                hashrate: 999,
                is_active: true,
                chips: Vec::new(),
            },
        ];

        let boards = registry.boards(&threads);
        let a = boards.iter().find(|b| b.name == "board-a").unwrap();
        let b = boards.iter().find(|b| b.name == "board-b").unwrap();

        assert_eq!(a.threads.iter().map(|t| t.hashrate).sum::<u64>(), 400);
        assert_eq!(b.threads.iter().map(|t| t.hashrate).sum::<u64>(), 200);
        assert!(
            boards
                .iter()
                .all(|bd| bd.threads.iter().all(|t| t.name != "orphan")),
            "a thread with no matching board must not be attributed"
        );
    }

    #[test]
    fn board_with_no_threads_reports_none() {
        // A board that handed over no hash threads must come back with an
        // empty list rather than inheriting somebody else's.
        let mut registry = BoardRegistry::new();
        let (_keep, reg) = make_board("idle-board");
        registry.push(reg);

        let threads = vec![ThreadTelemetry {
            name: "someone-elses".into(),
            hashrate: 500,
            is_active: true,
            chips: Vec::new(),
        }];
        assert!(registry.boards(&threads)[0].threads.is_empty());
    }

    #[test]
    fn reflects_updated_state() {
        let mut registry = BoardRegistry::new();

        let (tx, reg) = make_board("board-a");
        registry.push(reg);

        assert_eq!(registry.boards(&[])[0].model, "Test");

        tx.send_modify(|s| s.model = "Updated".into());
        assert_eq!(registry.boards(&[])[0].model, "Updated");
    }
}
