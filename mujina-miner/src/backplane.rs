//! Backplane for board communication and lifecycle management.
//!
//! The Backplane acts as the communication substrate between mining boards and
//! the scheduler. Like a hardware backplane, it provides connection points for
//! boards to plug into, routes events between components, and manages board
//! lifecycle (hotplug, emergency shutdown, etc.).

use anyhow::Result;
use std::collections::{HashMap, HashSet};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::{StreamExt, StreamMap};

use futures::future::BoxFuture;

use crate::{
    api::BoardRegistration,
    board::{BackplaneConnector, BoardDescriptor, BoardInfo, VirtualBoardRegistry},
    scheduler::ThreadRegistration,
    tracing::prelude::*,
    transport::{
        TransportEvent, UsbDeviceInfo, cpu::TransportEvent as CpuTransportEvent,
        usb::TransportEvent as UsbTransportEvent,
    },
};

/// Board registry that uses inventory to find registered boards.
pub struct BoardRegistry;

impl BoardRegistry {
    /// Find the best matching board descriptor for this USB device.
    ///
    /// Uses pattern matching with specificity scoring to select the most
    /// appropriate board handler. When multiple patterns match, the one
    /// with the highest specificity score wins.
    ///
    /// Returns None if no registered boards match the device.
    pub fn find_descriptor(&self, device: &UsbDeviceInfo) -> Option<&'static BoardDescriptor> {
        inventory::iter::<BoardDescriptor>()
            .filter(|desc| desc.pattern.matches(device))
            .max_by_key(|desc| desc.pattern.specificity())
    }
}

/// Backplane that connects boards to the scheduler.
///
/// Acts as the communication substrate between mining boards and the work
/// scheduler. Boards plug into the backplane, which routes their events and
/// manages their lifecycle.
pub struct Backplane {
    registry: BoardRegistry,
    virtual_registry: VirtualBoardRegistry,
    /// Active boards managed by the backplane
    boards: HashMap<String, ActiveBoard>,
    /// One event receiver per transport; the count sets how many initial
    /// enumeration completions to wait for.
    event_rxs: Vec<mpsc::Receiver<TransportEvent>>,
    /// Channel to register hash threads with the scheduler
    scheduler_tx: mpsc::Sender<ThreadRegistration>,
    /// Channel to forward board registrations to the API server
    board_reg_tx: mpsc::Sender<BoardRegistration>,
}

impl Backplane {
    /// Create a new backplane.
    pub fn new(
        event_rxs: Vec<mpsc::Receiver<TransportEvent>>,
        scheduler_tx: mpsc::Sender<ThreadRegistration>,
        board_reg_tx: mpsc::Sender<BoardRegistration>,
    ) -> Self {
        Self {
            registry: BoardRegistry,
            virtual_registry: VirtualBoardRegistry,
            boards: HashMap::new(),
            event_rxs,
            scheduler_tx,
            board_reg_tx,
        }
    }

    /// Run the backplane event loop.
    pub async fn run(&mut self) -> Result<()> {
        // Multiplex the per-transport receivers. The number of transports is
        // how many initial-enumeration completions to wait for.
        let mut streams: StreamMap<usize, ReceiverStream<TransportEvent>> = StreamMap::new();
        for (i, rx) in std::mem::take(&mut self.event_rxs).into_iter().enumerate() {
            streams.insert(i, ReceiverStream::new(rx));
        }
        let transport_count = streams.len();
        let mut completed: HashSet<usize> = HashSet::new();
        let mut completion_sent = false;

        // No transports means nothing to enumerate; let the scheduler proceed.
        if transport_count == 0 {
            self.send_enumeration_complete().await;
            completion_sent = true;
        }

        while let Some((transport, event)) = streams.next().await {
            match event {
                TransportEvent::Usb(usb_event) => {
                    self.handle_usb_event(usb_event).await?;
                }
                TransportEvent::Cpu(cpu_event) => {
                    self.handle_cpu_event(cpu_event).await?;
                }
                TransportEvent::InitialEnumerationComplete => {
                    completed.insert(transport);
                    if !completion_sent && completed.len() == transport_count {
                        self.send_enumeration_complete().await;
                        completion_sent = true;
                    }
                }
            }
        }

        Ok(())
    }

    /// Shutdown all boards managed by this backplane.
    pub async fn shutdown_all_boards(&mut self) {
        let board_ids: Vec<String> = self.boards.keys().cloned().collect();

        for board_id in board_ids {
            if let Some(mut board) = self.boards.remove(&board_id) {
                board.shutdown().await;
                info!(
                    board = %board.info.model,
                    serial = %board_id,
                    "Board stopped"
                );
            }
        }
    }

    /// Route a board connection's parts to where they belong.
    async fn start_board(
        &mut self,
        board_id: String,
        device_path: Option<String>,
        conn: BackplaneConnector,
    ) {
        let BackplaneConnector {
            info,
            threads,
            telemetry_rx,
            command_tx,
            shutdown,
        } = conn;

        // Record which threads belong to this board before handing them to
        // the scheduler. This is the only point where the pairing is known:
        // afterwards the threads live in the scheduler and the telemetry in
        // the API, with nothing linking them. Recovering it later would mean
        // inferring the relationship from a naming convention.
        let thread_names: Vec<String> = threads.iter().map(|t| t.name().to_string()).collect();

        let registration = BoardRegistration {
            telemetry_rx,
            command_tx,
            thread_names,
        };
        if let Err(e) = self.board_reg_tx.send(registration).await {
            error!(
                board = %info.model,
                error = %e,
                "Failed to register board with API server"
            );
        }

        info!(
            board = %info.model,
            serial = %board_id,
            threads = threads.len(),
            "Board started."
        );

        for thread in threads {
            if let Err(e) = self
                .scheduler_tx
                .send(ThreadRegistration::Thread(thread))
                .await
            {
                error!(
                    board = %info.model,
                    error = %e,
                    "Failed to send thread to scheduler"
                );
                break;
            }
        }

        self.boards.insert(
            board_id,
            ActiveBoard {
                info,
                device_path,
                shutdown,
            },
        );
    }

    /// Tell the scheduler that startup enumeration across all transports is
    /// done. Sent on the thread channel, after every starting thread, so FIFO
    /// ordering guarantees the scheduler has registered them all first.
    async fn send_enumeration_complete(&mut self) {
        if self
            .scheduler_tx
            .send(ThreadRegistration::InitialEnumerationComplete)
            .await
            .is_err()
        {
            debug!("Scheduler dropped before initial enumeration completed");
        }
    }

    /// Handle USB transport events.
    async fn handle_usb_event(&mut self, event: UsbTransportEvent) -> Result<()> {
        match event {
            UsbTransportEvent::UsbDeviceConnected(device_info) => {
                let Some(descriptor) = self.registry.find_descriptor(&device_info) else {
                    return Ok(());
                };

                info!(
                    board = descriptor.name,
                    vid = %format!("{:04x}", device_info.vid),
                    pid = %format!("{:04x}", device_info.pid),
                    manufacturer = ?device_info.manufacturer,
                    product = ?device_info.product,
                    serial = ?device_info.serial_number,
                    "Hash board connected via USB."
                );

                let device_path_clone = device_info.device_path.clone();
                let conn = match (descriptor.create_fn)(device_info).await {
                    Ok(conn) => conn,
                    Err(e) => {
                        error!(
                            board = descriptor.name,
                            error = %e,
                            "Failed to create board"
                        );
                        return Ok(());
                    }
                };

                let board_id = if let Some(ref serial) = conn.info.serial_number {
                    serial.clone()
                } else {
                    format!(
                        "unknown-{}",
                        device_path_clone.replace(['/', '\\', ':', '.'], "-")
                    )
                };

                self.start_board(board_id, Some(device_path_clone), conn).await;
            }
            UsbTransportEvent::UsbDeviceDisconnected { device_path } => {
                // Find and shutdown the board matching this device_path
                let board_id = self.boards.iter().find_map(|(id, board)| {
                    if board.device_path.as_deref() == Some(device_path.as_str()) {
                        Some(id.clone())
                    } else {
                        None
                    }
                });

                if let Some(board_id) = board_id {
                    if let Some(mut board) = self.boards.remove(&board_id) {
                        board.shutdown().await;
                        info!(
                            board = %board.info.model,
                            serial = %board_id,
                            "Board disconnected"
                        );
                    }
                } else {
                    warn!(%device_path, "Disconnected device not found in active boards");
                }
            }
        }

        Ok(())
    }

    /// Handle CPU miner transport events.
    async fn handle_cpu_event(&mut self, event: CpuTransportEvent) -> Result<()> {
        match event {
            CpuTransportEvent::CpuDeviceConnected(device_info) => {
                let Some(descriptor) = self.virtual_registry.find("cpu_miner") else {
                    error!("No virtual board descriptor found for cpu_miner");
                    return Ok(());
                };

                info!(
                    board = descriptor.name,
                    threads = device_info.thread_count,
                    duty = device_info.duty_percent,
                    "CPU miner board connected."
                );

                let conn = match (descriptor.create_fn)().await {
                    Ok(conn) => conn,
                    Err(e) => {
                        error!(
                            board = descriptor.name,
                            error = %e,
                            "Failed to create CPU miner board"
                        );
                        return Ok(());
                    }
                };

                let board_id = device_info.device_id.clone();
                self.start_board(board_id, None, conn).await;
            }
            CpuTransportEvent::CpuDeviceDisconnected { device_id } => {
                if let Some(mut board) = self.boards.remove(&device_id) {
                    board.shutdown().await;
                    info!(board = %board.info.model, serial = %device_id, "Board disconnected");
                }
            }
        }

        Ok(())
    }
}

/// Per-board state the backplane keeps for lifecycle management.
struct ActiveBoard {
    info: BoardInfo,
    device_path: Option<String>,
    shutdown: Option<BoxFuture<'static, ()>>,
}

impl ActiveBoard {
    async fn shutdown(&mut self) {
        if let Some(fut) = self.shutdown.take() {
            fut.await;
        }
    }
}
