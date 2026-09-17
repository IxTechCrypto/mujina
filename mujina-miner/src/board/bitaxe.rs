use anyhow::{Context as _, Result, anyhow, bail};
use async_trait::async_trait;
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
use tokio_util::{
    codec::{FramedRead, FramedWrite},
    sync::CancellationToken,
};

use crate::{
    api::BoardCommand,
    api_client::types::{BoardTelemetry, Fan, PowerMeasurement, TemperatureSensor},
    asic::{
        bm13xx::{
            self, chip_config,
            peripherals::{BoardPeripherals, ResetLine},
            register::ChipModel,
            thread::{BM13xxThread, FrequencyControl},
            topology::TopologySpec,
        },
        hash_thread::HashThread,
    },
    board::fan_control::{FanControl, FanController},
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
        tps546::{Tps546, Tps546Config, Tps546Regulator},
    },
    tracing::prelude::*,
    transport::{
        UsbDeviceInfo,
        serial::SerialStream,
    },
    types::{Ratio, Temperature, Voltage},
};

use super::{BackplaneConnector, BoardInfo, pattern::Match};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Firmware {
    /// bitaxe-raw, the original pass-through firmware.
    BitaxeRaw,
    /// RHAP-D, the successor to bitaxe-raw.
    RhapD,
}

impl std::fmt::Display for Firmware {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Firmware::BitaxeRaw => f.write_str("bitaxe-raw"),
            Firmware::RhapD => f.write_str("RHAP-D"),
        }
    }
}

impl Firmware {
    /// bitaxe-raw sends the v0 frame with no status byte. RHAP-D
    /// has only ever sent the v1 frame, the one the EmberOne
    /// firmware also adopted.
    fn response_format(self) -> ResponseFormat {
        match self {
            Firmware::BitaxeRaw => ResponseFormat::V0,
            Firmware::RhapD => ResponseFormat::V1,
        }
    }
}

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
        create_fn: |device| Box::pin(create_from_usb(device, Firmware::BitaxeRaw)),
    }
}

inventory::submit! {
    crate::board::BoardDescriptor {
        pattern: crate::board::pattern::BoardPattern {
            // pid.codes allocation to the Bitaxe project for RHAP-D firmware
            vid: Match::Specific(0x1209),
            pid: Match::Specific(0x6102),
            bcd_device: Match::Any,
            manufacturer: Match::Any,
            product: Match::Any,
            serial_pattern: Match::Any,
        },
        name: "Bitaxe Gamma",
        create_fn: |device| Box::pin(create_from_usb(device, Firmware::RhapD)),
    }
}

/// Create a Bitaxe board from USB device info.
async fn create_from_usb(device: UsbDeviceInfo, firmware: Firmware) -> Result<BackplaneConnector> {
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
    let data_reader = FramedRead::new(tracing_reader, bm13xx::FrameCodec::new(ChipModel::BM1370));
    let data_writer = FramedWrite::new(data_writer, bm13xx::FrameCodec::new(ChipModel::BM1370));

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

    let control_channel = ControlChannel::new(control_port, firmware.response_format());
    let mut i2c = BitaxeRawI2c::new(control_channel.clone());


    const ASIC_RESET_PIN: u8 = 0;
    let mut gpio_controller = BitaxeRawGpioController::new(control_channel);
    let mut reset_pin = gpio_controller.pin(ASIC_RESET_PIN).await?;

    // Hold ASIC in reset during power configuration
    reset_pin.write(PinValue::Low).await?;

    i2c.set_frequency(100_000).await?;

    let emc2101 = init_fan_controller(i2c.clone()).await?;
    let regulator = Arc::new(Mutex::new(init_power_controller(i2c.clone()).await?));

    let reset_line = BitaxeResetLine {
        nrst_pin: reset_pin,
        released_since: Arc::new(StdMutex::new(None)),
    };

    let voltage_regulator = Tps546Regulator::new(regulator.clone());
    let peripherals = BoardPeripherals {
        reset_line: Box::new(reset_line.clone()),
        voltage_regulator: Box::new(voltage_regulator),
    };

    let (thread_shutdown_tx, thread_shutdown_rx) = watch::channel(());

    let thread_name = match &device.serial_number {
        Some(serial) => format!("{}-{}", model.replace(' ', "-"), &serial[..8.min(serial.len())]),
        None => model.replace(' ', "-"),
    };

    let thread = BM13xxThread::new(
        thread_name,
        chip_config::bm1370(),
        TopologySpec::single_domain(1),
        data_reader,
        data_writer,
        peripherals,
        thread_shutdown_rx,
    );
    let freq_control = thread.frequency_control();
    let threads: Vec<Box<dyn HashThread>> = vec![Box::new(thread)];

    debug!("{model} board initialized");

    // Telemetry channel seeded with board identity
    let serial = device.serial_number.clone();
    let board_name = format!("{prefix}-{}", serial.as_deref().unwrap_or("unknown"));
    let initial_state = BoardTelemetry {
        name: board_name.clone(),
        model: model.to_string(),
        serial: serial.clone(),
        chip_model: Some("BM1370".into()),
        chip_count: Some(1),
        frequency_mhz: Some(bm13xx::thread::TARGET_FREQUENCY_MHZ),
        thread_count: threads.len() as u32,
        ..Default::default()
    };
    let (telemetry_tx, telemetry_rx) = watch::channel(initial_state);

    let info = BoardInfo {
        model: model.to_string(),
        firmware_version: Some(firmware.to_string()),
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
        chip_count: 1,
        thread_count: threads.len() as u32,
        fan: FanController::new(FanControl::Manual { percent: 100 }),
        freq_control,
        current_freq_mhz: bm13xx::thread::TARGET_FREQUENCY_MHZ,
        over_temp_count: 0,
        sensor_fault_count: 0,
        reset_line,
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
    thread_shutdown: watch::Sender<()>,
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
    reset_line: BitaxeResetLine,
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
                let voltage = Voltage::from_mv(clamped as i32);
                let result = self.regulator.lock().await.set_vout_target(voltage).await;
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

        let (vin, vout, iout_ma, power_mw, vr_temp) = {
            let mut reg = self.regulator.lock().await;

            if let Err(e) = reg.check_status().await {
                let err_str = e.to_string();
                if err_str.contains("timeout") || err_str.contains("TimedOut") {
                    warn!("Power controller status read timed out: {}", e);
                } else {
                    error!("Power controller fault: {}", e);
                    if let Err(e) = reg.clear_faults().await {
                        error!("Failed to clear faults: {}", e);
                    }
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
            .reset_line
            .released_since()
            .context("failed to read ASIC reset line state")?
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
                    voltage_v: vin.map(|v| v.volts()),
                    current_a: None,
                    power_w: None,
                },
                PowerMeasurement {
                    name: "core".into(),
                    voltage_v: vout.map(|v| v.volts()),
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
                vin_v = ?vin.map(|v| v.volts()),
                vout_v = ?vout.map(|v| v.volts()),
                "Board status"
            );
        }

        Ok(())
    }

    async fn shutdown(&mut self) {
        // The thread drops its shutdown receiver on exit, after
        // disabling the chain, so closed() means it has finished.
        // A failed send means it is already gone.
        const THREAD_EXIT_TIMEOUT: Duration = Duration::from_secs(2);
        let _ = self.thread_shutdown.send(());
        if time::timeout(THREAD_EXIT_TIMEOUT, self.thread_shutdown.closed())
            .await
            .is_err()
        {
            warn!("Timed out waiting for thread to exit");
        }

        if let Err(e) = self.reset_line.assert().await {
            warn!("Failed to hold chips in reset: {}", e);
        }

        match self.regulator.lock().await.disable_output().await {
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

        vin_on: Voltage::from_volts(4.8),
        vin_off: Voltage::from_volts(4.5),
        vin_uv_warn_limit: Voltage::from_volts(0.0), // Disabled due to TI bug
        vin_ov_fault_limit: Voltage::from_volts(6.5),
        vin_ov_fault_response: 0xB7,

        vout_scale_loop: 0.25,
        vout_min: Voltage::from_volts(1.0),
        vout_max: Voltage::from_volts(2.0),
        vout_command: Voltage::from_volts(1.15),

        vout_ov_fault_limit: Ratio::from_factor(1.25),
        vout_ov_warn_limit: Ratio::from_factor(1.16),
        vout_margin_high: Ratio::from_factor(1.10),
        vout_margin_low: Ratio::from_factor(0.90),
        vout_uv_warn_limit: Ratio::from_factor(0.90),
        vout_uv_fault_limit: Ratio::from_factor(0.75),

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

    const DEFAULT_VOUT: Voltage = Voltage::from_volts(1.15);
    tps546
        .set_vout_target(DEFAULT_VOUT)
        .await
        .context("failed to set core voltage target")?;
    tps546
        .clear_faults()
        .await
        .context("failed to clear faults")?;

    time::sleep(Duration::from_millis(500)).await;

    match tps546.get_vout().await {
        Ok(v) => debug!("Core voltage readback: {:.3}V", v.volts()),
        Err(e) => warn!("Failed to read core voltage: {}", e),
    }

    if let Err(e) = tps546.dump_configuration().await {
        warn!("Failed to dump TPS546 configuration: {}", e);
    }

    Ok(tps546)
}

/// GPIO-driven chip reset line that records when reset was last released.
#[derive(Clone)]
struct BitaxeResetLine {
    nrst_pin: BitaxeRawGpioPin,
    released_since: Arc<StdMutex<Option<Instant>>>,
}

impl BitaxeResetLine {
    /// When reset was last released, or `None` while reset is asserted.
    fn released_since(&self) -> Result<Option<Instant>> {
        self.released_since
            .lock()
            .map(|guard| *guard)
            .map_err(|_| anyhow!("reset line state lock poisoned"))
    }
}

#[async_trait]
impl ResetLine for BitaxeResetLine {
    async fn assert(&mut self) -> Result<()> {
        self.nrst_pin
            .write(PinValue::Low)
            .await
            .map_err(|e| anyhow!("failed to assert reset: {}", e))?;
        *self
            .released_since
            .lock()
            .map_err(|_| anyhow!("reset line state lock poisoned"))? = None;
        Ok(())
    }

    async fn release(&mut self) -> Result<()> {
        self.nrst_pin
            .write(PinValue::High)
            .await
            .map_err(|e| anyhow!("failed to release reset: {}", e))?;
        *self
            .released_since
            .lock()
            .map_err(|_| anyhow!("reset line state lock poisoned"))? = Some(Instant::now());
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backplane::BoardRegistry;

    fn osmu_device(vid: u16, pid: u16, product: &str) -> UsbDeviceInfo {
        UsbDeviceInfo {
            vid,
            pid,
            manufacturer: Some("OSMU".to_string()),
            product: Some(product.to_string()),
            ..Default::default()
        }
    }

    fn bitaxe_raw_device() -> UsbDeviceInfo {
        osmu_device(0xc0de, 0xcafe, "Bitaxe")
    }

    fn rhapd_device() -> UsbDeviceInfo {
        osmu_device(0x1209, 0x6102, "Bitaxe Gamma RHAP-D")
    }

    #[test]
    fn registry_finds_both_firmwares() {
        for device in [bitaxe_raw_device(), rhapd_device()] {
            let desc = BoardRegistry
                .find_descriptor(&device)
                .unwrap_or_else(|| panic!("no descriptor for {device:?}"));
            assert_eq!(desc.name, "Bitaxe Gamma");
        }
    }
}

