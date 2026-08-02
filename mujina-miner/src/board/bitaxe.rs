use anyhow::{Context as _, Result, anyhow, bail};
use async_trait::async_trait;
use futures::sink::SinkExt;
use std::{
    pin::Pin,
    sync::{Arc, Mutex as StdMutex},
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, ReadBuf},
    sync::{Mutex, mpsc, watch},
    time::{self, Instant, MissedTickBehavior},
};
use tokio_stream::StreamExt;
use tokio_util::{
    codec::{FramedRead, FramedWrite},
    sync::CancellationToken,
};

use crate::{
    api::BoardCommand,
    api_client::types::{BoardTelemetry, Fan, PowerMeasurement, TemperatureSensor},
    asic::{
        ChipInfo,
        bm13xx::{
            self, BM13xxProtocol,
            protocol::Command,
            thread::{BM13xxThread, FrequencyControl},
        },
        hash_thread::{AsicEnable, BoardPeripherals, HashThread, ThreadRemovalSignal},
    },
    board::fan_control::FanController,
    hw_trait::{
        gpio::{Gpio, GpioPin, PinValue},
        i2c::I2c,
    },
    mgmt_protocol::{
        ControlChannel,
        bitaxe_raw::{
            ResponseFormat,
            gpio::{BitaxeRawGpioController, BitaxeRawGpioPin},
            i2c::BitaxeRawI2c,
        },
    },
    peripheral::{
        emc2101::{Emc2101, Percent},
        tps546::{Tps546, Tps546Config},
    },
    tracing::prelude::*,
    transport::{
        UsbDeviceInfo,
        serial::{SerialReader, SerialStream, SerialWriter},
    },
    types::Temperature,
};

use super::{BackplaneConnector, BoardInfo, pattern::Match};

// Register this board type with the inventory system
inventory::submit! {
    crate::board::BoardDescriptor {
        pattern: crate::board::pattern::BoardPattern {
            // Match by VID:PID (c0de:cafe for bitaxe-raw firmware). Windows
            // reports a generic "Microsoft" manufacturer for CDC ACM devices
            // instead of the real "OSMU"/"Bitaxe" strings, so string matching
            // fails there; VID:PID is stable across platforms.
            vid: Match::Specific(0xc0de),
            pid: Match::Specific(0xcafe),
            bcd_device: Match::Any,
            manufacturer: Match::Any,
            product: Match::Any,
            serial_pattern: Match::Any,
        },
        name: "Bitaxe Gamma",
        create_fn: |device| Box::pin(create_from_usb(device)),
    }
}


/// Create a Bitaxe board from USB device info.
async fn create_from_usb(device: UsbDeviceInfo) -> Result<BackplaneConnector> {
    let (model, prefix) = ("Bitaxe Gamma", "bitaxe");

    let serial_ports = device.get_serial_ports(2).await?;

    debug!(
        serial = ?device.serial_number,
        control = %serial_ports[0],
        data = %serial_ports[1],
        "Opening {} serial ports",
        model
    );

    let control_port = SerialStream::new(&serial_ports[0], 115200)
        .context("failed to open control port")?;
    let data_stream = SerialStream::new(&serial_ports[1], 115200)
        .context("failed to open data port")?;
    let (data_reader, data_writer, _data_control) = data_stream.split();
    let tracing_reader = TracingReader::new(data_reader, "Data");
    let mut data_reader = FramedRead::new(tracing_reader, bm13xx::FrameCodec);
    let mut data_writer = FramedWrite::new(data_writer, bm13xx::FrameCodec);

    // Brief pause to let Windows composite USB driver settle both COM port handles
    time::sleep(Duration::from_millis(50)).await;

    #[cfg(not(windows))]
    {
        debug!("Issuing ESP32 auto-reset pulse via DTR/RTS modem control lines");
        let _ = control_port.write_data_terminal_ready(false);
        let _ = control_port.write_request_to_send(true);
        time::sleep(Duration::from_millis(50)).await;
        let _ = control_port.write_request_to_send(false);
        let _ = control_port.write_data_terminal_ready(false);
        time::sleep(Duration::from_millis(150)).await;
    }

    #[cfg(not(windows))]
    let _ = control_port.clear(tokio_serial::ClearBuffer::Input);

    let control_channel = ControlChannel::new(control_port, ResponseFormat::V0);
    let mut i2c = BitaxeRawI2c::new(control_channel.clone());


    const ASIC_RESET_PIN: u8 = 0;
    let mut gpio_controller = BitaxeRawGpioController::new(control_channel);
    let mut reset_pin = gpio_controller.pin(ASIC_RESET_PIN).await?;

    // Hold ASIC in reset during power configuration
    reset_pin.write(PinValue::Low).await?;

    i2c.set_frequency(100_000).await?;

    let emc2101 = init_fan_controller(i2c.clone()).await?;
    let regulator = Arc::new(Mutex::new(init_power_controller(i2c.clone()).await?));

    time::sleep(Duration::from_millis(500)).await;

    // Release ASIC from reset for discovery. The BM1370 needs time to boot
    // its UART before it will answer; give it the same ~500ms the reference
    // Bitaxe firmware allows rather than a marginal 200ms.
    debug!("De-asserting ASIC nRST");
    reset_pin.write(PinValue::High).await?;

    let chip_infos = discover_chain(&mut data_reader, &mut data_writer).await?;

    debug!(count = chip_infos.len(), "Discovered chips");

    // Verify expected BM1370 chip
    const EXPECTED_CHIP_ID: [u8; 2] = [0x13, 0x70];
    if let Some(first_chip) = chip_infos.first()
        && first_chip.chip_id != EXPECTED_CHIP_ID
    {
        bail!(
            "wrong chip type for {model}: expected BM1370 ({:02x}{:02x}), found {:02x}{:02x}",
            EXPECTED_CHIP_ID[0],
            EXPECTED_CHIP_ID[1],
            first_chip.chip_id[0],
            first_chip.chip_id[1]
        );
    }

    // Put chip back in reset before handing off to hash thread
    reset_pin.write(PinValue::Low).await?;

    // Create hash thread
    let (thread_shutdown_tx, thread_shutdown_rx) = watch::channel(ThreadRemovalSignal::Running);

    let thread_name = match &device.serial_number {
        Some(serial) => format!("{}-{}", model.replace(' ', "-"), &serial[..8.min(serial.len())]),
        None => model.replace(' ', "-"),
    };

    let asic_enable = BitaxeAsicEnable {
        nrst_pin: reset_pin,
        enabled_since: Arc::new(StdMutex::new(None)),
    };
    let asic_enable_monitor = asic_enable.clone();
    let peripherals = BoardPeripherals {
        asic_enable: Some(Box::new(asic_enable)),
        voltage_regulator: None,
    };

    let thread = BM13xxThread::new(
        thread_name,
        data_reader,
        data_writer,
        peripherals,
        thread_shutdown_rx,
        chip_infos.len(),
    );
    let freq_control = thread.frequency_control();
    let threads: Vec<Box<dyn HashThread>> = vec![Box::new(thread)];

    debug!("{model} board initialized with {} chips", chip_infos.len());

    // Telemetry channel seeded with board identity
    let serial = device.serial_number.clone();
    let board_name = format!("{prefix}-{}", serial.as_deref().unwrap_or("unknown"));
    let initial_state = BoardTelemetry {
        name: board_name.clone(),
        model: model.to_string(),
        serial: serial.clone(),
        chip_model: Some("BM1370".into()),
        chip_count: Some(chip_infos.len() as u32),
        frequency_mhz: Some(bm13xx::thread::TARGET_FREQUENCY_MHZ),
        thread_count: threads.len() as u32,
        ..Default::default()
    };
    let (telemetry_tx, telemetry_rx) = watch::channel(initial_state);

    let info = BoardInfo {
        model: model.to_string(),
        firmware_version: Some("bitaxe-raw".to_string()),
        serial_number: device.serial_number.clone(),
    };

    // Assemble internal state and spawn the board monitor
    let bitaxe = Bitaxe {
        emc2101,
        regulator,
        thread_shutdown: thread_shutdown_tx,
        board_name,
        board_model: model,
        board_serial: serial,
        chip_model: "BM1370",
        chip_count: chip_infos.len() as u32,
        thread_count: threads.len() as u32,
        fan: FanController::default(),
        freq_control,
        current_freq_mhz: bm13xx::thread::TARGET_FREQUENCY_MHZ,
        over_temp_count: 0,
        sensor_fault_count: 0,
        asic_enable: asic_enable_monitor,
    };

    // Runtime command channel (fan control, etc.). Small buffer: commands
    // are rare, human-driven API calls.
    let (command_tx, command_rx) = mpsc::channel::<BoardCommand>(8);

    let cancel = CancellationToken::new();
    let monitor_handle = tokio::spawn(bitaxe.run_monitor(telemetry_tx, command_rx, cancel.clone()));

    let shutdown = Box::pin(async move {
        cancel.cancel();
        let _ = monitor_handle.await;
    });

    Ok(BackplaneConnector {
        info,
        threads,
        telemetry_rx,
        command_tx: Some(command_tx),
        shutdown: Some(shutdown),
    })
}

/// Lowest core voltage accepted from a runtime tuning request, in mV.
/// Aliases the BM1370 entry in [`bm13xx::chip_profile`], the single
/// source of truth for chip envelopes.
const MIN_CORE_VOLTAGE_MV: u16 = bm13xx::chip_profile::BM1370.min_voltage_mv;
/// Highest core voltage accepted from a runtime tuning request, in mV.
/// The BM1370 should not run above ~1300 mV sustained.
const MAX_CORE_VOLTAGE_MV: u16 = bm13xx::chip_profile::BM1370.max_voltage_mv;

/// Board monitor tick period. Also the fan controller's integral dt.
const MONITOR_TICK: Duration = Duration::from_secs(2);

/// Internal state owned by the board monitor task.
///
/// The factory assembles this and moves it into `run_monitor()`.
struct Bitaxe {
    emc2101: Emc2101<BitaxeRawI2c>,
    regulator: Arc<Mutex<Tps546<BitaxeRawI2c>>>,
    thread_shutdown: watch::Sender<ThreadRemovalSignal>,
    board_name: String,
    board_model: &'static str,
    board_serial: Option<String>,
    /// ASIC chip model, for `chip_profile` lookups (tuning bounds, target
    /// mode UI ranges). Bitaxe boards are single-chain BM1370.
    chip_model: &'static str,
    /// Number of ASIC chips discovered on this board's chain.
    chip_count: u32,
    /// Number of hash threads handed to the backplane. Fixed for the
    /// board's lifetime; re-sent on every telemetry update so the field
    /// does not decay to the `Default` zero after the first snapshot.
    thread_count: u32,
    /// Fan control policy and its PI state, evaluated each monitor cycle.
    fan: FanController,
    /// Handle for retuning the ASIC hash clock at runtime.
    freq_control: FrequencyControl,
    /// Last hash clock commanded to the ASIC, in MHz. Reported in telemetry.
    current_freq_mhz: f32,
    /// Consecutive readings at or above the emergency temperature.
    /// Triggers a fast thermal shutdown.
    over_temp_count: u32,
    /// Consecutive cycles with no usable temperature reading (I2C
    /// error or out-of-range value). Tolerated longer than a genuine
    /// over-temp because it is usually a transient control-bus desync,
    /// but still shuts the board down if the sensor stays unreadable.
    sensor_fault_count: u32,
    asic_enable: BitaxeAsicEnable,
}

impl Bitaxe {
    async fn run_monitor(
        mut self,
        telemetry_tx: watch::Sender<BoardTelemetry>,
        mut command_rx: mpsc::Receiver<BoardCommand>,
        cancel: CancellationToken,
    ) {
        let mut tick = time::interval(MONITOR_TICK);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut last_log = Instant::now();

        loop {
            tokio::select! {
                _ = tick.tick() => {
                    if let Err(e) = self.monitor_tick(&telemetry_tx, &mut last_log).await {
                        error!(error = %e, "Board monitor failed");
                        self.shutdown().await;
                        return;
                    }
                }
                Some(cmd) = command_rx.recv() => {
                    self.handle_command(cmd).await;
                }
                _ = cancel.cancelled() => {
                    self.shutdown().await;
                    if let Err(e) = self.emc2101.set_fan_speed(Percent::new_clamped(25)).await {
                        warn!("Failed to reduce fan speed: {}", e);
                    }
                    return;
                }
            }
        }
    }

    /// Apply a runtime command.
    ///
    /// Fan changes take effect on the next monitor tick; voltage and
    /// frequency are applied immediately to the hardware here.
    async fn handle_command(&mut self, cmd: BoardCommand) {
        match cmd {
            BoardCommand::SetFanControl { update, reply } => {
                self.fan.update(update);
                info!(policy = ?self.fan.control(), "Fan control updated");
                let _ = reply.send(Ok(()));
            }
            BoardCommand::SetCoreVoltage { millivolts, reply } => {
                let clamped = millivolts.clamp(MIN_CORE_VOLTAGE_MV, MAX_CORE_VOLTAGE_MV);
                let volts = clamped as f32 / 1000.0;
                let result = self.regulator.lock().await.set_vout(volts).await;
                match &result {
                    Ok(()) => info!(millivolts = clamped, "Core voltage set"),
                    Err(e) => warn!(error = %e, "Failed to set core voltage"),
                }
                let _ = reply.send(result);
            }
            BoardCommand::SetFrequency { mhz, reply } => {
                // A live PLL ramp takes seconds. Run it in a detached task so
                // the monitor loop keeps calling monitor_tick — the thermal
                // watchdog must not be starved while the clock is changing.
                // Reflect the requested (clamped) clock in telemetry now; the
                // thread owns the actual ramp.
                self.current_freq_mhz = mhz.clamp(
                    bm13xx::thread::MIN_FREQUENCY_MHZ,
                    bm13xx::thread::MAX_FREQUENCY_MHZ,
                );
                let freq_control = self.freq_control.clone();
                tokio::spawn(async move {
                    let result = freq_control.set(mhz).await;
                    match &result {
                        Ok(()) => info!(mhz, "Hash clock retune complete"),
                        Err(e) => warn!(error = %e, "Failed to set hash clock"),
                    }
                    let _ = reply.send(result);
                });
            }
        }
    }

    /// Run one monitoring cycle. Returns `Err` on thermal emergency.
    ///
    /// Reads all sensors, classifies the temperature reading, publishes
    /// telemetry, and logs a periodic summary.
    ///
    /// Temperature readings fall into four categories:
    /// - I2C errors: no temperature available (usually a transient
    ///   control-bus desync), increments the sensor-fault counter.
    /// - Out of plausible range (outside 0..120 C): implausible,
    ///   also counted as a sensor fault.
    /// - At or above the emergency threshold: valid but dangerous,
    ///   increments the over-temperature counter.
    /// - Normal: valid and safe, resets both counters.
    ///
    /// A genuine over-temperature trips a fast shutdown after
    /// OVER_TEMP_LIMIT consecutive readings. An unreadable sensor is
    /// tolerated for longer (SENSOR_FAULT_LIMIT) so a transient bus
    /// glitch can re-sync, but still shuts down if it persists — we
    /// cannot run the ASIC blind to its temperature.
    async fn monitor_tick(
        &mut self,
        tx: &watch::Sender<BoardTelemetry>,
        last_log: &mut Instant,
    ) -> Result<()> {
        // Read all sensors in one pass
        let raw_temp = self.emc2101.get_external_temperature().await;
        let fan_percent = self.emc2101.get_fan_speed().await.ok().map(u8::from);
        let fan_rpm = self.emc2101.get_rpm().await.ok();

        let (vin_mv, vout_mv, iout_ma, power_mw, vr_temp) = {
            let mut reg = self.regulator.lock().await;

            if let Err(e) = reg.check_status().await {
                error!("Power controller fault: {}", e);
                if let Err(e) = reg.clear_faults().await {
                    error!("Failed to clear faults: {}", e);
                }
            }

            (
                reg.get_vin().await.ok(),
                reg.get_vout().await.ok(),
                reg.get_iout().await.ok(),
                reg.get_power().await.ok(),
                reg.get_temperature().await.ok(),
            )
        };

        const EXPECTED_MIN_C: f32 = 0.0;
        const EXPECTED_MAX_C: f32 = 120.0;
        const EMERGENCY_TEMP_C: f32 = 80.0;
        // Consecutive genuinely-dangerous readings before shutdown.
        const OVER_TEMP_LIMIT: u32 = 3;
        // Consecutive unreadable-sensor cycles before shutdown. At the
        // ~2s monitor cadence this is ~20s, long enough for a transient
        // I2C/control-bus desync to re-sync without nuking the board.
        const SENSOR_FAULT_LIMIT: u32 = 10;
        // The EMC2101 measures temperature via a diode on the ASIC
        // die. When the ASIC comes out of reset, the resulting
        // electrical transient corrupts the first few ADC conversions.
        // Wait for the measurement to settle before trusting readings.
        const DIODE_SETTLE: Duration = Duration::from_millis(500);
        let diode_ready = self
            .asic_enable
            .enabled_since()
            .context("failed to read ASIC enable state")?
            .is_some_and(|since| since.elapsed() >= DIODE_SETTLE);
        let asic_temp = if diode_ready {
            match raw_temp {
                Ok(t) if !(EXPECTED_MIN_C..=EXPECTED_MAX_C).contains(&t) => {
                    self.sensor_fault_count += 1;
                    trace!(temp_c = t, "Discarding out-of-range temperature reading");
                    None
                }
                Ok(t) if t >= EMERGENCY_TEMP_C => {
                    self.over_temp_count += 1;
                    self.sensor_fault_count = 0;
                    warn!(
                        temp_c = t,
                        consecutive = self.over_temp_count,
                        "Temperature above emergency threshold"
                    );
                    Some(t)
                }
                Ok(t) => {
                    self.over_temp_count = 0;
                    self.sensor_fault_count = 0;
                    Some(t)
                }
                Err(e) => {
                    self.sensor_fault_count += 1;
                    warn!(
                        consecutive = self.sensor_fault_count,
                        "Temperature read failed: {}", e
                    );
                    None
                }
            }
        } else {
            self.over_temp_count = 0;
            self.sensor_fault_count = 0;
            None
        };

        // A sustained run of genuinely dangerous temperatures is a real
        // thermal emergency: shut down fast.
        if self.over_temp_count >= OVER_TEMP_LIMIT {
            error!(
                consecutive = self.over_temp_count,
                "THERMAL EMERGENCY: shutting down board"
            );
            if let Err(e) = self.emc2101.set_fan_speed(Percent::FULL).await {
                error!("Failed to set fan speed: {}", e);
            }
            bail!(
                "thermal emergency after {} consecutive over-temperature readings",
                self.over_temp_count
            );
        }

        // A missing temperature reading is usually a transient control-bus
        // desync that re-syncs on its own. Tolerate a longer window, but
        // still shut down if the sensor stays unreadable — we cannot run
        // the ASIC blind to its temperature.
        if self.sensor_fault_count >= SENSOR_FAULT_LIMIT {
            error!(
                consecutive = self.sensor_fault_count,
                "Temperature sensor unreadable: shutting down board"
            );
            if let Err(e) = self.emc2101.set_fan_speed(Percent::FULL).await {
                error!("Failed to set fan speed: {}", e);
            }
            bail!(
                "temperature sensor unreadable after {} consecutive failed readings",
                self.sensor_fault_count
            );
        }

        // Apply the fan control policy. In manual mode the operator's duty
        // cycle is held and the PI state is reset so a later switch back to
        // automatic starts clean rather than resuming a stale integral. In
        // automatic mode the raw reading is EMA-filtered and fed to the PI
        // curve; when the temperature is unreadable we leave the fan where
        // it is rather than guess (the emergency and sensor-fault paths
        // above own the sustained-failure cases), and the filter/integral
        // simply hold at their last value until a reading returns.
        let commanded_percent = self.fan.tick(asic_temp, MONITOR_TICK.as_secs_f32());
        if let Some(percent) = commanded_percent
            && let Err(e) = self
                .emc2101
                .set_fan_speed(Percent::new_clamped(percent))
                .await
        {
            warn!("Failed to set fan speed: {}", e);
        }

        // Publish telemetry
        let _ = tx.send(BoardTelemetry {
            name: self.board_name.clone(),
            model: self.board_model.into(),
            serial: self.board_serial.clone(),
            chip_model: Some(self.chip_model.into()),
            chip_count: Some(self.chip_count),
            frequency_mhz: Some(self.current_freq_mhz),
            thread_count: self.thread_count,
            fans: vec![Fan {
                name: "fan".into(),
                rpm: fan_rpm,
                percent: fan_percent,
                target_percent: commanded_percent,
                auto: Some(self.fan.control().is_auto()),
                target_c: self.fan.control().target_c(),
                min_percent: self.fan.control().min_percent(),
            }],
            temperatures: vec![
                TemperatureSensor {
                    name: "asic".into(),
                    temperature: asic_temp.map(Temperature::from_celsius),
                },
                TemperatureSensor {
                    name: "vr".into(),
                    temperature: vr_temp.map(|t| Temperature::from_celsius(t as f32)),
                },
            ],
            powers: vec![
                PowerMeasurement {
                    name: "input".into(),
                    voltage_v: vin_mv.map(|mv| mv as f32 / 1000.0),
                    current_a: None,
                    power_w: None,
                },
                PowerMeasurement {
                    name: "core".into(),
                    voltage_v: vout_mv.map(|mv| mv as f32 / 1000.0),
                    current_a: iout_ma.map(|ma| ma as f32 / 1000.0),
                    power_w: power_mw.map(|mw| mw as f32 / 1000.0),
                },
            ],
            threads: Vec::new(), // TODO: populate from hash thread telemetry
        });

        // Periodic log
        const LOG_INTERVAL: Duration = Duration::from_secs(30);
        if last_log.elapsed() >= LOG_INTERVAL {
            *last_log = Instant::now();
            info!(
                board = %self.board_model,
                serial = ?self.board_serial,
                asic_temp_c = ?asic_temp,
                fan_percent = ?fan_percent,
                fan_target_percent = ?commanded_percent,
                fan_rpm = ?fan_rpm,
                vr_temp_c = ?vr_temp,
                power_w = ?power_mw.map(|mw| mw as f32 / 1000.0),
                current_a = ?iout_ma.map(|ma| ma as f32 / 1000.0),
                vin_v = ?vin_mv.map(|mv| mv as f32 / 1000.0),
                vout_v = ?vout_mv.map(|mv| mv as f32 / 1000.0),
                "Board status"
            );
        }

        Ok(())
    }

    async fn shutdown(&mut self) {
        if let Err(e) = self.thread_shutdown.send(ThreadRemovalSignal::Shutdown) {
            warn!("Failed to send shutdown signal to threads: {}", e);
        } else {
            time::sleep(Duration::from_millis(200)).await;
        }

        if let Err(e) = self.asic_enable.disable().await {
            warn!("Failed to hold chips in reset: {}", e);
        }

        match self.regulator.lock().await.set_vout(0.0).await {
            Ok(()) => debug!("Core voltage turned off"),
            Err(e) => warn!("Failed to turn off core voltage: {}", e),
        }
    }
}

async fn init_fan_controller(i2c: BitaxeRawI2c) -> Result<Emc2101<BitaxeRawI2c>> {
    let mut fan = Emc2101::new(i2c);
    fan.init().await.context("EMC2101 init failed")?;

    // Calibrate remote diode junction parameters for BM1370 ASIC
    fan.set_ideality_factor(0x24)
        .await
        .context("failed to set EMC2101 ideality factor")?;
    fan.set_beta_compensation(0x00)
        .await
        .context("failed to set EMC2101 beta compensation")?;

    fan.set_fan_speed(Percent::FULL)
        .await
        .context("failed to set initial fan speed")?;
    debug!("Fan speed set to 100%");
    Ok(fan)
}

async fn init_power_controller(i2c: BitaxeRawI2c) -> Result<Tps546<BitaxeRawI2c>> {
    let config = Tps546Config {
        phase: 0x00,
        frequency_switch_khz: 650,

        vin_on: 4.8,
        vin_off: 4.5,
        vin_uv_warn_limit: 0.0, // Disabled due to TI bug
        vin_ov_fault_limit: 6.5,
        vin_ov_fault_response: 0xB7,

        vout_scale_loop: 0.25,
        vout_min: 1.0,
        vout_max: 2.0,
        vout_command: 1.15,

        vout_ov_fault_limit: 1.25,
        vout_ov_warn_limit: 1.16,
        vout_margin_high: 1.10,
        vout_margin_low: 0.90,
        vout_uv_warn_limit: 0.90,
        vout_uv_fault_limit: 0.75,

        iout_oc_warn_limit: 25.0,
        iout_oc_fault_limit: 30.0,
        iout_oc_fault_response: 0xC0,

        ot_warn_limit: 105,
        ot_fault_limit: 145,
        ot_fault_response: 0xFF,

        ton_delay: 0,
        ton_rise: 3,
        ton_max_fault_limit: 0,
        ton_max_fault_response: 0x3B,
        toff_delay: 0,
        toff_fall: 0,

        pin_detect_override: 0xFFFF,
    };

    let mut tps546 = Tps546::new(i2c, config);

    tps546
        .init()
        .await
        .context("power controller init failed")?;

    time::sleep(Duration::from_millis(100)).await;

    const DEFAULT_VOUT: f32 = bm13xx::chip_profile::BM1370.default_voltage_mv as f32 / 1000.0;
    tps546
        .set_vout(DEFAULT_VOUT)
        .await
        .context("failed to set core voltage")?;
    debug!("Core voltage set to {DEFAULT_VOUT}V");

    time::sleep(Duration::from_millis(500)).await;

    match tps546.get_vout().await {
        Ok(mv) => debug!("Core voltage readback: {:.3}V", mv as f32 / 1000.0),
        Err(e) => warn!("Failed to read core voltage: {}", e),
    }

    if let Err(e) = tps546.dump_configuration().await {
        warn!("Failed to dump TPS546 configuration: {}", e);
    }

    Ok(tps546)
}

/// Discard any frames already sitting in the reader.
///
/// Used to make each discovery attempt independent of the last. Stops at
/// the first quiet moment rather than reading for a fixed period, so it
/// costs nothing on the common path where nothing is pending.
async fn drain_pending(reader: &mut FramedRead<TracingReader<SerialReader>, bm13xx::FrameCodec>) {
    /// How long to wait for a straggler before calling the line quiet.
    const QUIET: Duration = Duration::from_millis(20);

    // Ends on a quiet line or a closed stream; either way there is nothing
    // stale left to confuse the caller.
    let mut dropped = 0usize;
    while let Ok(Some(_)) = time::timeout(QUIET, reader.next()).await {
        dropped += 1;
    }
    if dropped > 0 {
        debug!(dropped, "Discarded stale frames before chip discovery");
    }
}

pub(crate) async fn discover_chips(
    reader: &mut FramedRead<TracingReader<SerialReader>, bm13xx::FrameCodec>,
    writer: &mut FramedWrite<SerialWriter, bm13xx::FrameCodec>,
) -> Result<Vec<ChipInfo>> {
    // Drop anything already buffered before asking.
    //
    // A discovery attempt that times out does not cancel the chips'
    // replies -- they simply arrive after the window closed and sit in the
    // reader. The next attempt would then count them *plus* its own,
    // reporting twice the real chip count. Observed on a 4-chip chain as
    // "discovered 8": the first attempt was too early, and the second saw
    // both sets. A single chip answers fast enough that this rarely shows
    // up on the Bitaxe, which is why it survived this long.
    drain_pending(reader).await;

    let discover_cmd = BM13xxProtocol::discover_chips();

    writer
        .send(discover_cmd)
        .await
        .context("failed to send chip discovery command")?;

    let mut chip_infos = Vec::new();
    let timeout = Duration::from_millis(500);
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        tokio::select! {
            response = reader.next() => {
                match response {
                    Some(Ok(bm13xx::Response::ReadRegister {
                        chip_address: _,
                        register: bm13xx::Register::ChipId { chip_type, core_count, address }
                    })) => {
                        let chip_id = chip_type.id_bytes();
                        debug!("Discovered chip {:?} ({:02x}{:02x}) at address {address}",
                                     chip_type, chip_id[0], chip_id[1]);

                        chip_infos.push(ChipInfo {
                            chip_id,
                            core_count: core_count.into(),
                            address,
                            supports_version_rolling: true,
                        });
                    }
                    Some(Ok(_)) => {
                        warn!("Unexpected response during chip discovery");
                    }
                    Some(Err(e)) => {
                        error!("Error during chip discovery: {e}");
                    }
                    None => break,
                }
            }
            _ = time::sleep_until(deadline) => {
                break;
            }
        }
    }

    if chip_infos.is_empty() {
        bail!("no chips discovered");
    }
    Ok(chip_infos)
}

/// Bring a freshly-reset BM13xx chain up to the point of enumeration.
///
/// The caller must have already released the ASIC(s) from reset. This waits
/// for the chip UARTs to boot, broadcasts the version-mask configuration the
/// chips need before they will answer, then discovers them — retrying the
/// whole preamble because a cold ASIC can miss the first round while its
/// UART is still coming up. Shared by every BM13xx board (Bitaxe, NerdQAxe++)
/// so the proven timing lives in one place.
pub(crate) async fn discover_chain(
    reader: &mut FramedRead<TracingReader<SerialReader>, bm13xx::FrameCodec>,
    writer: &mut FramedWrite<SerialWriter, bm13xx::FrameCodec>,
) -> Result<Vec<ChipInfo>> {
    time::sleep(Duration::from_millis(500)).await;

    const DISCOVERY_ATTEMPTS: usize = 5;
    for attempt in 1..=DISCOVERY_ATTEMPTS {
        debug!("Sending version mask configuration (3 times)");
        for i in 1..=3 {
            trace!("Version mask send {}/3", i);
            let version_cmd = Command::WriteRegister {
                broadcast: true,
                chip_address: 0x00,
                register: bm13xx::protocol::Register::VersionMask(
                    bm13xx::protocol::VersionMask::full_rolling(),
                ),
            };
            writer
                .send(version_cmd)
                .await
                .context("failed to send config command")?;
            time::sleep(Duration::from_millis(5)).await;
        }

        time::sleep(Duration::from_millis(10)).await;

        match discover_chips(reader, writer).await {
            Ok(chips) => return Ok(chips),
            Err(e) if attempt < DISCOVERY_ATTEMPTS => {
                warn!(
                    "Chip discovery attempt {attempt}/{DISCOVERY_ATTEMPTS} failed: {e}; retrying"
                );
                time::sleep(Duration::from_millis(200)).await;
            }
            Err(e) => return Err(e),
        }
    }

    unreachable!("loop returns on the final attempt")
}

/// GPIO-based ASIC reset control that records when the ASIC was
/// last enabled.
#[derive(Clone)]
struct BitaxeAsicEnable {
    nrst_pin: BitaxeRawGpioPin,
    enabled_since: Arc<StdMutex<Option<Instant>>>,
}

impl BitaxeAsicEnable {
    /// When the ASIC was last taken out of reset, or `None` if it
    /// is currently in reset. Safe to call from another task.
    fn enabled_since(&self) -> Result<Option<Instant>> {
        self.enabled_since
            .lock()
            .map(|guard| *guard)
            .map_err(|_| anyhow!("ASIC enable state lock poisoned"))
    }
}

#[async_trait]
impl AsicEnable for BitaxeAsicEnable {
    async fn enable(&mut self) -> Result<()> {
        self.nrst_pin
            .write(PinValue::High)
            .await
            .map_err(|e| anyhow!("failed to release reset: {}", e))?;
        *self
            .enabled_since
            .lock()
            .map_err(|_| anyhow!("ASIC enable state lock poisoned"))? = Some(Instant::now());
        Ok(())
    }

    async fn disable(&mut self) -> Result<()> {
        self.nrst_pin
            .write(PinValue::Low)
            .await
            .map_err(|e| anyhow!("failed to assert reset: {}", e))?;
        *self
            .enabled_since
            .lock()
            .map_err(|_| anyhow!("ASIC enable state lock poisoned"))? = None;
        Ok(())
    }
}

/// A wrapper around AsyncRead that traces raw bytes as they're read.
pub(crate) struct TracingReader<R> {
    inner: R,
    name: &'static str,
}

impl<R: AsyncRead + Unpin> TracingReader<R> {
    pub(crate) fn new(inner: R, name: &'static str) -> Self {
        Self { inner, name }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for TracingReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before_len = buf.filled().len();

        let result = Pin::new(&mut self.inner).poll_read(cx, buf);

        if let Poll::Ready(Ok(())) = &result {
            let after_len = buf.filled().len();
            if after_len > before_len {
                let new_bytes = &buf.filled()[before_len..after_len];
                trace!(
                    "{} RX: {} bytes => {:02x?}",
                    self.name,
                    new_bytes.len(),
                    new_bytes
                );
            }
        }

        result
    }
}
