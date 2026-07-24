//! NerdQAxe++ hash board support
//!
//! Four BM1370 ASICs on a shared, stacked core-voltage domain, driven by an
//! ESP32-S3 running bitaxe-raw. Same USB control pattern as the Bitaxe Gamma
//! and emberOne/00: the host owns all chip and peripheral logic, the ESP32 is
//! a transport bridge.
//!
//! Scope: this brings the board up far enough to enumerate the chain. Hashing
//! needs multi-chip enumeration in `BM13xxThread`, which is not implemented
//! yet, so no hash threads are handed back. Power (TPS53647) and fan/thermal
//! (EMC2302, TMP1075) control land with that work; the rails come up at their
//! hardware defaults here, which is enough for discovery but not for
//! sustained hashing.

use anyhow::{Context as _, Result, bail};
use std::time::Duration;
use tokio::{
    sync::watch,
    time::{self, Instant},
};
use tokio_serial::SerialPortBuilderExt;
use tokio_util::codec::{FramedRead, FramedWrite};

use super::{
    BackplaneConnector, BoardDescriptor, BoardInfo,
    bitaxe::{TracingReader, discover_chips},
    pattern::{BoardPattern, Match, StringMatch},
};
use crate::{
    api_client::types::BoardTelemetry,
    asic::bm13xx,
    hw_trait::gpio::{Gpio, GpioPin, PinValue},
    mgmt_protocol::{
        ControlChannel,
        bitaxe_raw::{ResponseFormat, gpio::BitaxeRawGpioController},
    },
    tracing::prelude::*,
    transport::{UsbDeviceInfo, serial::SerialStream},
};

inventory::submit! {
    BoardDescriptor {
        pattern: BoardPattern {
            vid: Match::Any,
            pid: Match::Any,
            bcd_device: Match::Any,
            manufacturer: Match::Specific(StringMatch::Exact("shufps")),
            product: Match::Specific(StringMatch::Exact("NerdQAxe++")),
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

    // Hold the chain in reset across the whole power-up.
    asic_resetn.write(PinValue::Low).await?;

    // IO rails before the core rail, so the chips never see IO driven while
    // unpowered.
    ldo_en.write(PinValue::High).await?;
    // MCP1824 settles in ~0.2 ms; round up for the pair.
    time::sleep(Duration::from_millis(5)).await;

    pwr_en.write(PinValue::High).await?;
    wait_for_vr_rdy(&mut vr_rdy).await?;

    let data_stream =
        SerialStream::new(&serial_ports[1], 115200).context("failed to open data port")?;
    let (data_reader, data_writer, _data_control) = data_stream.split();
    let mut data_reader =
        FramedRead::new(TracingReader::new(data_reader, "Data"), bm13xx::FrameCodec);
    let mut data_writer = FramedWrite::new(data_writer, bm13xx::FrameCodec);

    // Release the chain and let the chips come out of reset before probing.
    debug!("De-asserting ASIC nRST");
    asic_resetn.write(PinValue::High).await?;
    time::sleep(Duration::from_millis(200)).await;

    let chip_infos = discover_chips(&mut data_reader, &mut data_writer).await?;
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

    // Park the chain in reset until there is a hash thread to drive it.
    asic_resetn.write(PinValue::Low).await?;
    pwr_en.write(PinValue::Low).await?;
    ldo_en.write(PinValue::Low).await?;

    let info = BoardInfo {
        model: "NerdQAxe++".to_string(),
        firmware_version: Some("bitaxe-raw".to_string()),
        serial_number: device.serial_number.clone(),
    };

    let telemetry = BoardTelemetry {
        name: format!(
            "nerdqaxe-pp-{}",
            info.serial_number.as_deref().unwrap_or("unknown")
        ),
        model: info.model.clone(),
        serial: info.serial_number.clone(),
        ..Default::default()
    };
    let (_telemetry_tx, telemetry_rx) = watch::channel(telemetry);

    warn!("NerdQAxe++ hash threads not yet implemented (needs multi-chip chain support)");

    Ok(BackplaneConnector {
        info,
        threads: Vec::new(),
        telemetry_rx,
        // No runtime commands until fan/frequency control lands with the
        // EMC2302 and TPS53647 drivers.
        command_tx: None,
        shutdown: None,
    })
}

/// Wait for the TPS53647 to report power-good.
///
/// Proceeding without this risks releasing reset into a core rail that has
/// not settled.
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
