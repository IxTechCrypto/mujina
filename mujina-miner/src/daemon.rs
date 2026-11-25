//! Daemon lifecycle management for mujina-miner.
//!
//! This module handles the core daemon functionality including initialization,
//! task management, signal handling, and graceful shutdown.

use std::env;

use tokio::signal::unix::{self, SignalKind};
use tokio::sync::{mpsc, watch};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::api_client::types::MinerTelemetry;
use crate::tracing::prelude::*;
use crate::{
    api::{self, ApiConfig, commands::SchedulerCommand},
    backplane::Backplane,
    cpu_miner::CpuMinerConfig,
    scheduler::{self, SourceRegistration, ThreadRegistration},
    transport::{CpuDeviceInfo, TransportEvent, UsbTransport, cpu as cpu_transport},
};

/// The main daemon.
pub struct Daemon {
    shutdown: CancellationToken,
    tracker: TaskTracker,
}

impl Daemon {
    /// Create a new daemon instance.
    pub fn new() -> Self {
        Self {
            shutdown: CancellationToken::new(),
            tracker: TaskTracker::new(),
        }
    }

    /// Run the daemon until shutdown is requested.
    pub async fn run(self) -> anyhow::Result<()> {
        crate::api::cleanup_stale_staged_firmware();

        // Create channels for component communication. Each transport gets its
        // own event channel; the backplane waits for one enumeration completion
        // per channel.
        let (thread_tx, thread_rx) = mpsc::channel::<ThreadRegistration>(10);
        let (source_reg_tx, source_reg_rx) = mpsc::channel::<SourceRegistration>(10);
        let mut transport_rxs: Vec<mpsc::Receiver<TransportEvent>> = Vec::new();

        // Create and start USB transport discovery
        if std::env::var("MUJINA_USB_DISABLE").is_err() {
            let (usb_tx, usb_rx) = mpsc::channel::<TransportEvent>(100);
            let usb_transport = UsbTransport::new(usb_tx);
            if let Err(e) = usb_transport.start_discovery(self.shutdown.clone()).await {
                error!("Failed to start USB discovery: {}", e);
            }
            transport_rxs.push(usb_rx);
        } else {
            info!("USB discovery disabled (MUJINA_USB_DISABLE set)");
        }

        // Inject CPU miner virtual device if configured
        if let Some(config) = CpuMinerConfig::from_env() {
            info!(
                threads = config.thread_count,
                duty = config.duty_percent,
                "CPU miner enabled"
            );
            let (cpu_tx, cpu_rx) = mpsc::channel::<TransportEvent>(100);
            let device = TransportEvent::Cpu(cpu_transport::TransportEvent::CpuDeviceConnected(
                CpuDeviceInfo {
                    device_id: format!("cpu-{}x{}%", config.thread_count, config.duty_percent),
                    thread_count: config.thread_count,
                    duty_percent: config.duty_percent,
                },
            ));
            // Send the device and its enumeration completion, then drop the
            // sender; the CPU transport has no further events.
            if let Err(e) = cpu_tx.send(device).await {
                error!("Failed to send CPU miner event: {}", e);
            }
            let _ = cpu_tx
                .send(TransportEvent::InitialEnumerationComplete)
                .await;
            transport_rxs.push(cpu_rx);
        }

        // Board registration channel: backplane forwards board
        // registrations here, the API server collects and serves them.
        let (board_reg_tx, board_reg_rx) = mpsc::channel(10);

        // Create and start backplane
        let mut backplane = Backplane::new(transport_rxs, thread_tx, board_reg_tx);
        self.tracker.spawn({
            let shutdown = self.shutdown.clone();
            async move {
                tokio::select! {
                    result = backplane.run() => {
                        if let Err(e) = result {
                            error!("Backplane error: {}", e);
                        }
                    }
                    _ = shutdown.cancelled() => {}
                }

                backplane.shutdown_all_boards().await;
            }
        });

        // Miner state channel: scheduler publishes snapshots, API serves them.
        let (miner_telemetry_tx, miner_telemetry_rx) = watch::channel(MinerTelemetry::default());

        // Command channel: API sends commands, scheduler processes them.
        let (scheduler_cmd_tx, scheduler_cmd_rx) = mpsc::channel::<SchedulerCommand>(16);

        // Pool reload: PUT/PATCH write the file, then hand the new set to
        // the director. The director adds and removes stratum sessions
        // in this process. A process restart would leave the Core 1 IPC
        // handle stale, so there is no restart path here.
        let (pool_reload_tx, pool_reload_rx) = mpsc::channel(4);
        let pools = crate::pool_config::effective_pools();
        info!(pools = pools.len(), "Pool director starting");
        self.tracker.spawn(crate::pool_director::run(
            self.shutdown.clone(),
            pool_reload_rx,
            source_reg_tx.clone(),
            scheduler_cmd_tx.clone(),
            pools,
        ));

        // Start the scheduler
        self.tracker.spawn(scheduler::task(
            self.shutdown.clone(),
            thread_rx,
            source_reg_rx,
            miner_telemetry_tx,
            scheduler_cmd_rx,
        ));

        // Start the API server
        self.tracker.spawn({
            let shutdown = self.shutdown.clone();
            async move {
                // ASCII 'M' (77) + 'U' (85) = 7785
                const API_PORT: u16 = 7785;

                let bind_addr = match env::var("MUJINA_API_LISTEN") {
                    Ok(addr) if addr.contains(':') => addr,
                    Ok(addr) => format!("{addr}:{API_PORT}"),
                    Err(_) => format!("127.0.0.1:{API_PORT}"),
                };
                let config = ApiConfig { bind_addr };
                if let Err(e) = api::serve(
                    config,
                    shutdown,
                    miner_telemetry_rx,
                    board_reg_rx,
                    scheduler_cmd_tx,
                    pool_reload_tx,
                )
                .await
                {
                    error!("API server error: {}", e);
                }
            }
        });

        self.tracker.close();

        info!("Started.");
        info!("For debugging, set MUJINA_LOG=debug or trace.");

        // Install signal handlers
        let mut sigint = unix::signal(SignalKind::interrupt())?;
        let mut sigterm = unix::signal(SignalKind::terminate())?;

        // Wait for shutdown signal
        tokio::select! {
            _ = sigint.recv() => {
                info!("Received SIGINT.");
            },
            _ = sigterm.recv() => {
                info!("Received SIGTERM.");
            },
        }

        // Initiate shutdown
        self.shutdown.cancel();

        // Wait for all tasks to complete
        self.tracker.wait().await;
        info!("Exiting.");

        Ok(())
    }
}

impl Default for Daemon {
    fn default() -> Self {
        Self::new()
    }
}
