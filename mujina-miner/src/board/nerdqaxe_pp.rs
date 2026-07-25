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
//! Scope: the chain is enumerated, then **parked** -- reset asserted, core
//! and IO rails off. Hashing needs a real `BM13xxThread` over the chain,
//! which also has to keep those rails up, and that is not implemented yet,
//! so no hash threads are handed back.
//!
//! The management peripherals (TPS53647, EMC2302, TMP1075) are brought up
//! and polled regardless, because they sit on the always-on +3V3 rail and
//! stay readable with the core rail parked. The regulator is identified and
//! configured but its output is deliberately left off.

use anyhow::{Context as _, Result, bail};
use std::time::Duration;
use tokio::{
    sync::{mpsc, watch},
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
    asic::bm13xx,
    hw_trait::{
        gpio::{Gpio, GpioPin, PinValue},
        i2c::I2c as _,
    },
    mgmt_protocol::{
        ControlChannel,
        bitaxe_raw::{ResponseFormat, gpio::BitaxeRawGpioController, i2c::BitaxeRawI2c},
    },
    peripheral::{
        emc2302::{Emc2302, Percent},
        tmp1075::Tmp1075,
        tps53647::{Tps53647, Tps53647Config},
    },
    tracing::prelude::*,
    transport::{UsbDeviceInfo, serial::SerialStream},
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
    let mut pwr_en = gpio.pin(gpio_cmd::PWR_EN).await?;
    let mut ldo_en = gpio.pin(gpio_cmd::LDO_EN).await?;
    let mut vr_rdy = gpio.pin(gpio_cmd::VR_RDY).await?;

    // Hold the chain in reset across the whole power-up. This write has
    // nothing to undo on failure, so it stays outside the guarded section.
    asic_resetn.write(PinValue::Low).await?;

    // Everything from here on energizes the board or takes the chain out of
    // reset, so a failure partway through must not strand it live and
    // unsupervised. `park()` always runs before this function returns,
    // whether bring-up succeeded or failed.
    let bring_up = bring_up_chain(
        &serial_ports[1],
        &mut asic_resetn,
        &mut pwr_en,
        &mut ldo_en,
        &mut vr_rdy,
    )
    .await;

    park(&mut asic_resetn, &mut pwr_en, &mut ldo_en).await;

    // Chip details aren't consumed yet; discovery already logged the count.
    // They'll feed hash-thread construction once chain support lands.
    bring_up?;

    let info = BoardInfo {
        model: "NerdQAxe++".to_string(),
        firmware_version: Some("bitaxe-raw".to_string()),
        serial_number: device.serial_number.clone(),
    };

    let board_name = format!(
        "nerdqaxe-pp-{}",
        info.serial_number.as_deref().unwrap_or("unknown")
    );

    let telemetry = BoardTelemetry {
        name: board_name.clone(),
        model: info.model.clone(),
        serial: info.serial_number.clone(),
        chip_model: Some("BM1370".into()),
        chip_count: Some(EXPECTED_CHIPS as u32),
        // No hash threads yet (`threads: Vec::new()` below), so this board
        // contributes nothing to the aggregate hashrate. Stated explicitly
        // rather than left to `Default` so it has to be revisited when
        // multi-chip chain support lands.
        thread_count: 0,
        ..Default::default()
    };
    let (telemetry_tx, telemetry_rx) = watch::channel(telemetry);

    warn!("NerdQAxe++ hash threads not yet implemented (needs multi-chip chain support)");

    // Bring up the management peripherals. These all sit on the always-on
    // +3V3 rail, so they are readable with the ASIC core rail parked --
    // which is exactly the state this board is left in until hash threads
    // land. Sensor telemetry therefore works without energizing anything.
    let mut i2c = BitaxeRawI2c::new(control.clone());
    i2c.set_frequency(I2C_FREQUENCY_HZ).await?;

    let sensors = Sensors::new(i2c).await?;

    let (command_tx, command_rx) = mpsc::channel::<BoardCommand>(8);
    let cancel = CancellationToken::new();
    let monitor = NerdQaxePp {
        sensors,
        board_name,
        board_serial: info.serial_number.clone(),
        fan_percent: DEFAULT_FAN_PERCENT,
    };
    let monitor_handle = tokio::spawn(monitor.run(telemetry_tx, command_rx, cancel.clone()));

    let shutdown = Box::pin(async move {
        cancel.cancel();
        let _ = monitor_handle.await;
    });

    Ok(BackplaneConnector {
        info,
        threads: Vec::new(),
        telemetry_rx,
        command_tx: Some(command_tx),
        shutdown: Some(shutdown),
    })
}

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
    regulator: Tps53647<BitaxeRawI2c>,
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
            regulator,
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
                // Refused rather than applied: the rail is parked off and
                // nothing is hashing, so setting a core voltage would only
                // energize four ASICs with no thermal supervision.
                let _ = reply.send(Err(anyhow::anyhow!(
                    "NerdQAxe++ core rail stays off until hash threads are implemented"
                )));
            }
            BoardCommand::SetFrequency { reply, .. } => {
                let _ = reply.send(Err(anyhow::anyhow!(
                    "NerdQAxe++ has no hash threads to retune yet"
                )));
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

        let regulator = &mut self.sensors.regulator;
        let vin = regulator.vin().await.ok();
        let vout = regulator.vout().await.ok();
        let iout = regulator.iout().await.ok();
        let vr_internal_c = regulator.temperature().await.ok();

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
            // No hash clock is being driven while the chain is parked.
            frequency_mhz: None,
            fans,
            temperatures,
            powers,
            threads: Vec::new(),
            thread_count: 0,
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

/// Power up the core and IO rails, release the chain from reset, and
/// enumerate the chips.
///
/// Callers must always run [`park`] after this returns, `Ok` or `Err`: on
/// error the rails may be partway through power-up (any point from IO rails
/// on through chain enumeration) and must still be de-energized.
async fn bring_up_chain(
    data_port: &str,
    asic_resetn: &mut impl GpioPin,
    pwr_en: &mut impl GpioPin,
    ldo_en: &mut impl GpioPin,
    vr_rdy: &mut impl GpioPin,
) -> Result<Vec<crate::asic::ChipInfo>> {
    // IO rails before the core rail, so the chips never see IO driven while
    // unpowered.
    ldo_en.write(PinValue::High).await?;
    // MCP1824 settles in ~0.2 ms; round up for the pair.
    time::sleep(Duration::from_millis(5)).await;

    pwr_en.write(PinValue::High).await?;
    wait_for_vr_rdy(vr_rdy).await?;

    let data_stream = SerialStream::new(data_port, 115200).context("failed to open data port")?;
    let (data_reader, data_writer, _data_control) = data_stream.split();
    let mut data_reader =
        FramedRead::new(TracingReader::new(data_reader, "Data"), bm13xx::FrameCodec);
    let mut data_writer = FramedWrite::new(data_writer, bm13xx::FrameCodec);

    // Release the chain; discover_chain waits for the chip UARTs to boot,
    // sends the version-mask preamble, and retries.
    debug!("De-asserting ASIC nRST");
    asic_resetn.write(PinValue::High).await?;

    let chip_infos = discover_chain(&mut data_reader, &mut data_writer).await?;
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

    // A short chain means a chip failed to enumerate. Report it rather than
    // running a partially-populated board.
    if chip_infos.len() != EXPECTED_CHIPS {
        bail!(
            "expected {EXPECTED_CHIPS} BM1370 on the chain, discovered {}",
            chip_infos.len()
        );
    }

    Ok(chip_infos)
}

/// De-energize the board: assert reset, then drop the core rail and the IO
/// rails.
///
/// Best-effort: each write is attempted independently and a failure is
/// logged rather than short-circuiting the rest, since after a failed
/// bring-up this is the only thing standing between the board and being
/// left powered and unsupervised. A `warn!` here means the board may still
/// be live and needs a manual check (power-cycle the USB port).
async fn park(
    asic_resetn: &mut impl GpioPin,
    pwr_en: &mut impl GpioPin,
    ldo_en: &mut impl GpioPin,
) {
    if let Err(e) = asic_resetn.write(PinValue::Low).await {
        warn!(error = %e, "failed to assert ASIC reset while parking NerdQAxe++");
    }
    if let Err(e) = pwr_en.write(PinValue::Low).await {
        warn!(error = %e, "failed to disable core rail while parking NerdQAxe++");
    }
    if let Err(e) = ldo_en.write(PinValue::Low).await {
        warn!(error = %e, "failed to disable IO rails while parking NerdQAxe++");
    }
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
/// `TIMEOUT` is a starting guess, not a measured value — confirm against
/// real hardware once the TPS53647 soft-start timing is known.
async fn wait_for_vr_rdy(pin: &mut impl GpioPin) -> Result<()> {
    const TIMEOUT: Duration = Duration::from_millis(100);
    const POLL: Duration = Duration::from_millis(2);

    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        if pin.read().await? == PinValue::High {
            return Ok(());
        }
        time::sleep(POLL).await;
    }
    bail!("core regulator did not assert VR_RDY within {TIMEOUT:?}")
}
