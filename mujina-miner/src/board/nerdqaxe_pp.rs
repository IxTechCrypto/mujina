//! NerdQAxe++ hash board support
//!
//! Four BM1370 ASICs sharing a single core rail in parallel, driven by an
//! ESP32-S3 running bitaxe-raw. Same USB control pattern as the Bitaxe Gamma
//! and emberOne/00: the host owns all chip and peripheral logic, the ESP32 is
//! a transport bridge.
//!
//! The core rail is a 3-phase TPS53647 at roughly 1.15 V and up to 90 A. It
//! is *not* a stacked/series voltage domain, so the commanded output is the
//! per-chip core voltage directly, not a multiple of it.
//!
//! The board is enumerated once at construction and then parked. From that
//! point the hash thread owns the power sequence: [`NerdQaxePpAsicEnable`]
//! commands the core voltage, brings the IO and core rails up in order,
//! waits for the regulator, and releases reset -- and unwinds all of it on
//! disable. Nothing else in this module energizes the rail, so there is a
//! single path to audit.
//!
//! The management peripherals (TPS53647, EMC2302, TMP1075) sit on the
//! always-on +3V3 rail and are polled whatever the core rail is doing.

use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use tokio::{
    sync::{Mutex, mpsc, watch},
    time::{self, Instant},
};
use tokio_serial::SerialPortBuilderExt;
use tokio_util::codec::{FramedRead, FramedWrite};
use tokio_util::sync::CancellationToken;

use super::{
    BackplaneConnector, BoardDescriptor, BoardInfo,
    bitaxe::{TracingReader, discover_chain},
    pattern::{BoardPattern, Match},
};
use crate::{
    api::{BoardCommand, commands::FanControlUpdate},
    api_client::types::{BoardTelemetry, Fan, PowerMeasurement, TemperatureSensor},
    asic::{
        bm13xx,
        bm13xx::thread::{BM13xxThread, FrequencyControl},
        hash_thread::{AsicEnable, BoardPeripherals, HashThread, ThreadRemovalSignal},
    },
    hw_trait::{
        gpio::{Gpio, GpioPin, PinValue},
        i2c::I2c as _,
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
        emc2302::{Emc2302, Percent},
        tmp1075::Tmp1075,
        tps53647::{Tps53647, Tps53647Config},
    },
    tracing::prelude::*,
    transport::{
        UsbDeviceInfo,
        serial::{SerialReader, SerialStream, SerialWriter},
    },
    types::Temperature,
};

inventory::submit! {
    BoardDescriptor {
        pattern: BoardPattern {
            // Match by VID:PID, same reasoning as the Bitaxe Gamma pattern:
            // manufacturer/product string descriptors are not reliably
            // readable on all platforms (observed on Windows), so VID:PID
            // is the only discriminator trustworthy enough to rely on. The
            // firmware uses a PID (0xcaf1) distinct from stock bitaxe-raw's
            // 0xcafe specifically so this stays unambiguous.
            vid: Match::Specific(0xc0de),
            pid: Match::Specific(0xcaf1),
            bcd_device: Match::Any,
            manufacturer: Match::Any,
            product: Match::Any,
            serial_pattern: Match::Any,
        },
        name: "NerdQAxe++",
        create_fn: |device| Box::pin(create_from_usb(device)),
    }
}

/// GPIO command indices exposed by the NerdQAxe++ bitaxe-raw firmware.
///
/// These are protocol indices, not ESP32 pin numbers. See the Command enum
/// in the firmware's `src/control/gpio.rs`.
mod gpio_cmd {
    pub const ASIC_RESETN: u8 = 0x00;
    pub const PWR_EN: u8 = 0x01;
    pub const LDO_EN: u8 = 0x02;
    pub const VR_RDY: u8 = 0x03;
}

const EXPECTED_CHIPS: usize = 4;
const EXPECTED_CHIP_ID: [u8; 2] = [0x13, 0x70];

async fn create_from_usb(device: UsbDeviceInfo) -> Result<BackplaneConnector> {
    let serial_ports = device.get_serial_ports(2).await?;

    debug!(
        serial = ?device.serial_number,
        control = %serial_ports[0],
        data = %serial_ports[1],
        "Opening NerdQAxe++ serial ports"
    );

    let control_port = tokio_serial::new(&serial_ports[0], 115200)
        .open_native_async()
        .context("failed to open control port")?;
    let control = ControlChannel::new(control_port, ResponseFormat::V0);

    let mut gpio = BitaxeRawGpioController::new(control.clone());
    let mut asic_resetn = gpio.pin(gpio_cmd::ASIC_RESETN).await?;
    let pwr_en = gpio.pin(gpio_cmd::PWR_EN).await?;
    let ldo_en = gpio.pin(gpio_cmd::LDO_EN).await?;
    let vr_rdy = gpio.pin(gpio_cmd::VR_RDY).await?;

    // Hold the chain in reset across the whole power-up. This write has
    // nothing to undo on failure, so it stays outside the guarded section.
    asic_resetn.write(PinValue::Low).await?;

    // Management peripherals come up before the core rail, not after.
    //
    // They live on the always-on +3V3 rail, so nothing here needs the core
    // rail -- but the regulator does need to be reachable *before* PWR_EN
    // goes high. `Tps53647::init` issues CLEAR_FAULTS, and that is the only
    // thing that recovers a controller left in a latched fault by a previous
    // session. The controller keeps its state as long as 12 V is present, so
    // doing this after the power-up would mean a single bad session wedged
    // the board until it was physically unplugged: VR_RDY would never
    // assert, bring-up would fail, and the code that clears the fault would
    // never be reached.
    let mut i2c = BitaxeRawI2c::new(control.clone());
    i2c.set_frequency(I2C_FREQUENCY_HZ).await?;
    let sensors = Sensors::new(i2c).await?;

    // The data port is opened once and kept: enumeration and the hash
    // thread share the same framed pair, so there is only ever one owner
    // of the chain's UART.
    let data_stream =
        SerialStream::new(&serial_ports[1], 115200).context("failed to open data port")?;
    let (data_reader, data_writer, _data_control) = data_stream.split();
    let mut data_reader =
        FramedRead::new(TracingReader::new(data_reader, "Data"), bm13xx::FrameCodec);
    let mut data_writer = FramedWrite::new(data_writer, bm13xx::FrameCodec);

    let mut power = NerdQaxePpAsicEnable {
        asic_resetn,
        pwr_en,
        ldo_en,
        vr_rdy,
        regulator: Arc::clone(&sensors.regulator),
        core_voltage_v: CORE_VOLTAGE_V,
    };

    let chain = enumerate_chain(&mut power, &mut data_reader, &mut data_writer).await;

    // A failed power-up is almost always the regulator refusing to start,
    // and from the outside every cause looks the same. Read the fault
    // registers while we still can, so the log says which one it was.
    if chain.is_err() {
        match sensors.regulator.lock().await.status().await {
            Ok(status) => warn!(%status, "TPS53647 status after failed bring-up"),
            Err(e) => warn!(error = %e, "could not read TPS53647 status after failed bring-up"),
        }
    }
    let chip_infos = chain?;

    let info = BoardInfo {
        model: "NerdQAxe++".to_string(),
        firmware_version: Some("bitaxe-raw".to_string()),
        serial_number: device.serial_number.clone(),
    };

    let board_name = format!(
        "nerdqaxe-pp-{}",
        info.serial_number.as_deref().unwrap_or("unknown")
    );

    // Hand the chain to a hash thread. It owns the power sequence from
    // here: the board is parked right now, and the thread brings it back
    // up through `NerdQaxePpAsicEnable` when the scheduler assigns work.
    let (thread_shutdown_tx, thread_shutdown_rx) = watch::channel(ThreadRemovalSignal::Running);
    let thread_name = match &device.serial_number {
        Some(serial) => format!("NerdQAxe-PP-{}", &serial[..8.min(serial.len())]),
        None => "NerdQAxe-PP".to_string(),
    };
    let peripherals = BoardPeripherals {
        asic_enable: Some(Box::new(power)),
        // The regulator is reachable, but nothing tunes voltage at runtime
        // on this board yet -- see the SetCoreVoltage arm of handle_command.
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

    let telemetry = BoardTelemetry {
        name: board_name.clone(),
        model: info.model.clone(),
        serial: info.serial_number.clone(),
        chip_model: Some("BM1370".into()),
        chip_count: Some(chip_infos.len() as u32),
        frequency_mhz: Some(bm13xx::thread::TARGET_FREQUENCY_MHZ),
        thread_count: threads.len() as u32,
        ..Default::default()
    };
    let (telemetry_tx, telemetry_rx) = watch::channel(telemetry);

    let (command_tx, command_rx) = mpsc::channel::<BoardCommand>(8);
    let cancel = CancellationToken::new();
    let monitor = NerdQaxePp {
        sensors,
        board_name,
        board_serial: info.serial_number.clone(),
        fan_percent: DEFAULT_FAN_PERCENT,
        freq_control,
        current_freq_mhz: bm13xx::thread::TARGET_FREQUENCY_MHZ,
        thread_shutdown: thread_shutdown_tx,
    };
    let monitor_handle = tokio::spawn(monitor.run(telemetry_tx, command_rx, cancel.clone()));

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

/// How long to hold the core rail down before bringing it back up, so the
/// bulk output capacitance actually discharges and the ASICs lose state.
/// Too short and the chips keep their chain addresses across a restart.
const POWER_DOWN_SETTLE: Duration = Duration::from_millis(500);

/// Core voltage commanded before the rail is enabled, in volts.
///
/// The reference firmware runs this board at 1.15 V for its four BM1370,
/// and that is what the chips are specified around. It is a deliberate
/// constant rather than something inherited from the regulator's NVM,
/// because this single number is what four ASICs in parallel are fed.
///
/// Runtime tuning does not touch it yet: `SetCoreVoltage` is refused, so
/// the rail only ever runs here.
const CORE_VOLTAGE_V: f32 = 1.15;

/// I2C bus speed. Standard mode; every device on this bus supports it and
/// nothing here needs the throughput of fast mode.
const I2C_FREQUENCY_HZ: u32 = 100_000;

/// Interval between sensor sweeps.
const MONITOR_INTERVAL: Duration = Duration::from_secs(5);

/// Fan duty applied at startup.
///
/// Deliberately high. The board is parked with its core rail off so it is
/// barely dissipating anything, but there is no automatic fan curve on
/// this board yet, so whatever is set here is what runs until someone
/// changes it. Erring loud is recoverable; erring quiet is not.
const DEFAULT_FAN_PERCENT: u8 = 80;

/// I2C addresses of the board's sensors.
mod i2c_addr {
    /// TMP1075N near the ASICs.
    pub const TMP1075_ASIC: u8 = 0x48;
    /// TMP1075N on the back, next to the regulator.
    pub const TMP1075_VR: u8 = 0x49;
}

/// Fan header to EMC2302 channel mapping.
///
/// The silkscreen labels and the silicon's channel numbering do not run
/// in the same order, so the mapping is spelled out rather than inferred
/// from an index.
mod fan {
    use crate::peripheral::emc2302::Channel;

    /// M1, the regulator fan.
    pub const M1: Channel = Channel::Fan1;
    /// M2, the ASIC fan.
    pub const M2: Channel = Channel::Fan2;
}

/// The board's management peripherals, once identified.
struct Sensors {
    fans: Emc2302<BitaxeRawI2c>,
    temp_asic: Tmp1075<BitaxeRawI2c>,
    temp_vr: Tmp1075<BitaxeRawI2c>,
    /// Shared with [`NerdQaxePpAsicEnable`], which commands the core
    /// voltage on the power-up path while the monitor reads measurements
    /// from the same part.
    regulator: Arc<Mutex<Tps53647<BitaxeRawI2c>>>,
}

impl Sensors {
    /// Identify and configure every peripheral on the I2C bus.
    ///
    /// The regulator is configured but its output is left **off**: this
    /// function must not energize the core rail.
    async fn new(i2c: BitaxeRawI2c) -> Result<Self> {
        // Temperature sensors first. They are the simplest transaction on
        // the bus, so a failure here means the bus itself is wrong rather
        // than any particular driver -- much easier to read in a log than
        // a PMBus timeout would be.
        //
        // These are TMP1075N parts, which have no Die ID register, so
        // `probe` is the identification path rather than `init`.
        let mut temp_asic = Tmp1075::new(i2c.clone(), i2c_addr::TMP1075_ASIC);
        let asic_c = temp_asic
            .probe()
            .await
            .context("TMP1075 (ASIC) not responding")?;

        let mut temp_vr = Tmp1075::new(i2c.clone(), i2c_addr::TMP1075_VR);
        let vr_c = temp_vr
            .probe()
            .await
            .context("TMP1075 (VR) not responding")?;

        debug!(
            asic_c = asic_c.as_degrees_c(),
            vr_c = vr_c.as_degrees_c(),
            "NerdQAxe++ temperature sensors online"
        );

        let mut fans = Emc2302::new(i2c.clone());
        fans.init(false).await.context("EMC2302 init failed")?;

        let mut regulator = Tps53647::new(i2c, Tps53647Config::NERDQAXE_PP);
        regulator.init().await.context("TPS53647 init failed")?;

        Ok(Self {
            fans,
            temp_asic,
            temp_vr,
            regulator: Arc::new(Mutex::new(regulator)),
        })
    }
}

/// State owned by the board monitor task.
struct NerdQaxePp {
    sensors: Sensors,
    board_name: String,
    board_serial: Option<String>,
    /// Commanded fan duty, applied to both headers.
    fan_percent: u8,
    /// Handle for retuning the chain's hash clock.
    freq_control: FrequencyControl,
    /// Last hash clock commanded, in MHz. Reported in telemetry.
    current_freq_mhz: f32,
    /// Removes the hash thread when the board goes away, so the chain is
    /// not left hashing against a board that no longer exists.
    thread_shutdown: watch::Sender<ThreadRemovalSignal>,
}

impl NerdQaxePp {
    async fn run(
        mut self,
        telemetry_tx: watch::Sender<BoardTelemetry>,
        mut command_rx: mpsc::Receiver<BoardCommand>,
        cancel: CancellationToken,
    ) {
        if let Err(e) = self.apply_fan_speed().await {
            warn!(error = %e, "failed to apply initial NerdQAxe++ fan speed");
        }

        let mut ticker = time::interval(MONITOR_INTERVAL);
        ticker.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
        ticker.tick().await; // discard the immediate first tick

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                Some(command) = command_rx.recv() => self.handle_command(command).await,
                _ = ticker.tick() => self.publish(&telemetry_tx).await,
            }
        }

        // Tear the hash thread down before returning. The thread owns the
        // enable path, so this is what actually de-energizes the rail: the
        // board disappearing must not leave four ASICs hashing unattended.
        let _ = self
            .thread_shutdown
            .send(ThreadRemovalSignal::BoardDisconnected);

        debug!("NerdQAxe++ monitor stopped");
    }

    async fn handle_command(&mut self, command: BoardCommand) {
        match command {
            BoardCommand::SetFanControl { update, reply } => {
                let result = match update {
                    // No automatic curve on this board yet: without hash
                    // threads there is no load to track, and inventing a
                    // curve now would mean rewriting it once there is.
                    FanControlUpdate { auto: true, .. } => Err(anyhow::anyhow!(
                        "automatic fan control is not implemented on the NerdQAxe++ yet"
                    )),
                    FanControlUpdate {
                        percent: Some(percent),
                        ..
                    } => {
                        self.fan_percent = percent.min(100);
                        self.apply_fan_speed().await
                    }
                    // Manual mode with no duty given: nothing to change.
                    FanControlUpdate { percent: None, .. } => Ok(()),
                };
                let _ = reply.send(result);
            }
            BoardCommand::SetCoreVoltage { reply, .. } => {
                // Refused deliberately. The rail is commanded once, at
                // CORE_VOLTAGE_V, on the power-up path. Retuning it means
                // moving four parallel ASICs on a 90 A rail while they are
                // hashing, and nothing here supervises that yet -- there is
                // no automatic fan curve to answer the extra heat with.
                let _ = reply.send(Err(anyhow::anyhow!(
                    "NerdQAxe++ core voltage is fixed at {CORE_VOLTAGE_V} V; \
                     runtime tuning is not implemented"
                )));
            }
            BoardCommand::SetFrequency { mhz, reply } => {
                let result = self.freq_control.set(mhz).await;
                if result.is_ok() {
                    self.current_freq_mhz = mhz;
                }
                let _ = reply.send(result);
            }
        }
    }

    async fn apply_fan_speed(&mut self) -> Result<()> {
        let percent = Percent::new_clamped(self.fan_percent);
        for channel in [fan::M1, fan::M2] {
            self.sensors
                .fans
                .set_fan_speed(channel, percent)
                .await
                .with_context(|| format!("failed to set {channel:?} speed"))?;
        }
        Ok(())
    }

    /// Read every sensor and publish a telemetry snapshot.
    ///
    /// Each read is independent: one failing sensor reports null for its
    /// own fields rather than blanking the whole snapshot, so a single
    /// flaky device does not make the board look disconnected.
    async fn publish(&mut self, tx: &watch::Sender<BoardTelemetry>) {
        let asic_c = self.read_temp(true).await;
        let vr_c = self.read_temp(false).await;

        let mut fans = Vec::with_capacity(2);
        for (name, channel) in [("M1", fan::M1), ("M2", fan::M2)] {
            let rpm = match self.sensors.fans.get_rpm(channel).await {
                Ok(rpm) => rpm,
                Err(e) => {
                    warn!(fan = name, error = %e, "EMC2302 tach read failed");
                    None
                }
            };
            fans.push(Fan {
                name: name.into(),
                rpm,
                percent: Some(self.fan_percent),
                target_percent: Some(self.fan_percent),
                // No automatic mode on this board yet, so this is always
                // manual rather than null -- null would imply the board
                // has no controllable policy at all.
                auto: Some(false),
                target_c: None,
                min_percent: None,
            });
        }

        // Held only for the duration of these four reads. The power-up path
        // takes the same lock to command the core voltage, and blocking a
        // rail bring-up behind a telemetry sweep would be a poor trade.
        let (vin, vout, iout, vr_internal_c, commanded_v) = {
            let mut regulator = self.sensors.regulator.lock().await;
            (
                regulator.vin().await.ok(),
                regulator.vout().await.ok(),
                regulator.iout().await.ok(),
                regulator.temperature().await.ok(),
                regulator.commanded_vout().await.ok(),
            )
        };

        // Commanded against measured, under whatever load is present. If
        // they track, the rail is doing what it was told; if measured sits
        // persistently below commanded, the difference is droop and the
        // chips are running at the lower number.
        if let (Some(cmd), Some(measured)) = (commanded_v, vout) {
            debug!(
                commanded_v = cmd.to_volts(),
                measured_v = measured,
                droop_mv = (cmd.to_volts() - measured) * 1000.0,
                iout_a = iout.unwrap_or(0.0),
                "NerdQAxe++ core rail"
            );
        }

        let powers = vec![
            PowerMeasurement {
                name: "input".into(),
                voltage_v: vin,
                current_a: None,
                power_w: None,
            },
            PowerMeasurement {
                name: "core".into(),
                voltage_v: vout,
                current_a: iout,
                // Computed rather than read: the part exposes input power,
                // not output power, and V*I at the output is the number
                // the dashboard's efficiency figure needs.
                power_w: vout.zip(iout).map(|(v, i)| v * i),
            },
        ];

        let mut temperatures = vec![
            TemperatureSensor {
                name: "asic".into(),
                temperature: asic_c,
            },
            TemperatureSensor {
                name: "vr".into(),
                temperature: vr_c,
            },
        ];
        if let Some(c) = vr_internal_c {
            temperatures.push(TemperatureSensor {
                name: "vr-internal".into(),
                temperature: Some(Temperature::from_celsius(c)),
            });
        }

        let _ = tx.send(BoardTelemetry {
            name: self.board_name.clone(),
            model: "NerdQAxe++".into(),
            serial: self.board_serial.clone(),
            chip_model: Some("BM1370".into()),
            chip_count: Some(EXPECTED_CHIPS as u32),
            frequency_mhz: Some(self.current_freq_mhz),
            fans,
            temperatures,
            powers,
            // Per-thread hashrate accounting does not exist yet, so this
            // stays empty and `thread_count` carries the fact instead.
            threads: Vec::new(),
            thread_count: 1,
        });
    }

    /// Read one temperature sensor, logging and nulling out on failure.
    async fn read_temp(&mut self, asic: bool) -> Option<Temperature> {
        let (label, sensor) = if asic {
            ("asic", &mut self.sensors.temp_asic)
        } else {
            ("vr", &mut self.sensors.temp_vr)
        };
        match sensor.read().await {
            Ok(reading) => Some(Temperature::from_celsius(reading.as_degrees_c())),
            Err(e) => {
                warn!(sensor = label, error = %e, "TMP1075 read failed");
                None
            }
        }
    }
}

/// Owns the board's power sequence and presents it to the hash thread as
/// a plain enable/disable.
///
/// The whole rail is behind this, not just the reset line the Bitaxe's
/// equivalent controls: on this board "enable the ASICs" means commanding
/// a core voltage, bringing up the IO and core rails in order, waiting for
/// the regulator, and only then releasing reset. Disabling unwinds all of
/// it. Keeping that in one place is what stops the rail being left live by
/// a path that forgot to tear it down.
#[derive(Clone)]
struct NerdQaxePpAsicEnable {
    asic_resetn: BitaxeRawGpioPin,
    pwr_en: BitaxeRawGpioPin,
    ldo_en: BitaxeRawGpioPin,
    vr_rdy: BitaxeRawGpioPin,
    regulator: Arc<Mutex<Tps53647<BitaxeRawI2c>>>,
    /// Core voltage commanded before the rail is enabled.
    core_voltage_v: f32,
}

#[async_trait]
impl AsicEnable for NerdQaxePpAsicEnable {
    async fn enable(&mut self) -> Result<()> {
        // Voltage first, while the output is still gated off.
        //
        // `Tps53647::init` deliberately leaves VOUT_COMMAND alone, so
        // without this the rail would come up at whatever the part holds
        // in NVM. This is the one place that decides what four ASICs in
        // parallel are fed, so it is explicit rather than inherited.
        self.regulator
            .lock()
            .await
            .set_vout(self.core_voltage_v)
            .await
            .context("failed to command NerdQAxe++ core voltage")?;

        // IO rails before the core rail, so the chips never see IO driven
        // while unpowered.
        self.ldo_en.write(PinValue::High).await?;
        // MCP1824 settles in ~0.2 ms; round up for the pair.
        time::sleep(Duration::from_millis(5)).await;

        self.pwr_en.write(PinValue::High).await?;
        wait_for_vr_rdy(&mut self.vr_rdy).await?;

        debug!(
            core_voltage_v = self.core_voltage_v,
            "NerdQAxe++ core rail up; releasing ASIC reset"
        );
        self.asic_resetn.write(PinValue::High).await?;
        Ok(())
    }

    async fn disable(&mut self) -> Result<()> {
        self.park().await;
        Ok(())
    }
}

impl NerdQaxePpAsicEnable {
    /// De-energize the board: assert reset, then drop the core rail and
    /// the IO rails.
    ///
    /// Best-effort: each write is attempted independently and a failure is
    /// logged rather than short-circuiting the rest, since this is the only
    /// thing standing between the board and being left powered and
    /// unsupervised. A `warn!` here means the board may still be live and
    /// needs a manual check (power-cycle the USB port).
    async fn park(&mut self) {
        if let Err(e) = self.asic_resetn.write(PinValue::Low).await {
            warn!(error = %e, "failed to assert ASIC reset while parking NerdQAxe++");
        }
        if let Err(e) = self.pwr_en.write(PinValue::Low).await {
            warn!(error = %e, "failed to disable core rail while parking NerdQAxe++");
        }
        if let Err(e) = self.ldo_en.write(PinValue::Low).await {
            warn!(error = %e, "failed to disable IO rails while parking NerdQAxe++");
        }
    }
}

/// Framed halves of the chain's UART, shared by enumeration and the hash
/// thread.
type ChainReader = FramedRead<TracingReader<SerialReader>, bm13xx::FrameCodec>;
type ChainWriter = FramedWrite<SerialWriter, bm13xx::FrameCodec>;

/// Enumerate the chain, leaving the board parked afterwards.
///
/// Runs the same power sequence the hash thread will use, so enumeration
/// exercises the real path rather than a parallel copy of it.
async fn enumerate_chain(
    power: &mut NerdQaxePpAsicEnable,
    data_reader: &mut ChainReader,
    data_writer: &mut ChainWriter,
) -> Result<Vec<crate::asic::ChipInfo>> {
    // Start from a known-off state rather than assuming one.
    //
    // The daemon does not always get to tear the board down on the way out
    // -- a kill, a crash, or simply exiting before the hash thread finishes
    // disabling leaves the rail up and the chips still carrying the chain
    // addresses from the previous session. Enumerating into that reports
    // the wrong chip count (observed: 8 on a 4-chip chain) and the board is
    // then rejected for as long as the daemon runs. Dropping the core rail
    // first makes the chips forget everything, so discovery always starts
    // from silicon reset.
    power.park().await;
    time::sleep(POWER_DOWN_SETTLE).await;

    // Everything from here on energizes the board, so a failure partway
    // through must not strand it live. `park` runs whatever the outcome.
    let result = async {
        power.enable().await?;

        // discover_chain waits for the chip UARTs to boot, sends the
        // version-mask preamble, and retries.
        let chip_infos = discover_chain(data_reader, data_writer).await?;
        debug!(count = chip_infos.len(), "Discovered chips");

        if let Some(first) = chip_infos.first()
            && first.chip_id != EXPECTED_CHIP_ID
        {
            bail!(
                "wrong chip type for NerdQAxe++: expected BM1370 ({:02x}{:02x}), found {:02x}{:02x}",
                EXPECTED_CHIP_ID[0],
                EXPECTED_CHIP_ID[1],
                first.chip_id[0],
                first.chip_id[1]
            );
        }

        // A short chain means a chip failed to enumerate. Report it rather
        // than running a partially-populated board.
        if chip_infos.len() != EXPECTED_CHIPS {
            bail!(
                "expected {EXPECTED_CHIPS} BM1370 on the chain, discovered {}",
                chip_infos.len()
            );
        }

        Ok(chip_infos)
    }
    .await;

    // Park regardless. The hash thread powers the board back up on its own
    // terms when the scheduler gives it work.
    power.park().await;

    result
}

/// Wait for the TPS53647 to report power-good.
///
/// Proceeding without this risks releasing reset into a core rail that has
/// not settled.
///
/// Each poll is a plain `read()` with no surrounding timeout on purpose. A
/// `read()` is one request/response transaction over the shared control
/// channel, and cancelling it mid-flight (which an outer `time::timeout`
/// does when it fires) leaves the response unread, desyncing every
/// subsequent transaction by one — observed on hardware as a persistent
/// "Response ID mismatch" once a poll ran long. The channel already bounds
/// each transaction internally (1s, see `ControlChannel::send_packet`), so a
/// stalled read fails cleanly rather than hanging. The `TIMEOUT` below is a
/// wall-clock budget checked between polls, not a per-read cap.
///
/// Measured on hardware: the rail asserts VR_RDY **30 ms** after PWR_EN.
/// The budget is several times that, since overshooting costs only a
/// slower failure path while undershooting costs a board that will not
/// start. The elapsed time is logged on success, so drift shows up in a
/// log rather than as a mystery timeout.
///
/// A timeout here means the regulator refused to start, not that it was
/// slow. Chasing that by raising the budget is a dead end -- when this
/// fired during bring-up of the TPS53647 driver the cause was an
/// overcurrent and overtemperature limit encoded as a *negative* number,
/// so the part faulted the instant it was enabled and would never have
/// asserted VR_RDY at any timeout. The caller logs the PMBus status
/// registers on failure for exactly this reason; read those first.
async fn wait_for_vr_rdy(pin: &mut impl GpioPin) -> Result<()> {
    const TIMEOUT: Duration = Duration::from_millis(250);
    const POLL: Duration = Duration::from_millis(2);

    let started = Instant::now();
    let deadline = started + TIMEOUT;
    while Instant::now() < deadline {
        if pin.read().await? == PinValue::High {
            debug!(
                elapsed_ms = started.elapsed().as_millis() as u64,
                "Core regulator asserted VR_RDY"
            );
            return Ok(());
        }
        time::sleep(POLL).await;
    }
    bail!("core regulator did not assert VR_RDY within {TIMEOUT:?}")
}
