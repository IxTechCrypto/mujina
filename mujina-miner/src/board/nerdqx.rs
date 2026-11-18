//! NerdQX hash board support
//!
//! The NerdQX is a NerdQAxe++ derivative: four BM1370 on one parallel
//! 3-phase TPS53647 rail behind a bitaxe-raw ESP32-S3 bridge. Everything
//! shared with that board lives in [`super::nerdqaxe_pp`]; this module holds
//! what is different:
//!
//! - a TMP451 behind a 4-way GPIO mux ([`TempMux`]) reading each die's
//!   thermal diode, instead of one PCB-side TMP1075 estimate;
//! - the fan headers: only M2 (ASIC) and M1 (regulator) have PWM and tach,
//!   M3 is bare +12 V/GND;
//! - a higher regulator fault threshold for the upgraded power envelope.
//!
//! If the TMP451 does not answer at bring-up the board still runs, on the
//! TMP1075 estimate, but is clamped to [`SAFE_MAX_FREQ_MHZ`] /
//! [`SAFE_MAX_VOLTAGE_MV`]: nothing then measures the dies.

use anyhow::{Context as _, Result};
use std::time::Duration;
use tokio::time;

use super::{
    nerdqaxe_pp::{self, Variant},
    pattern::{BoardPattern, Match},
};
use crate::{
    mgmt_protocol::bitaxe_raw::{gpio::BitaxeRawGpioController, i2c::BitaxeRawI2c},
    peripheral::{
        emc2302::Channel,
        tmp451::{NERDQX_OFFSET, NERDQX_SCALE, Tmp451},
        tps53647::Tps53647Config,
    },
    tracing::prelude::*,
};

inventory::submit! {
    super::BoardDescriptor {
        pattern: BoardPattern {
            // VID:PID only, as for the NerdQAxe++. The PID (0xcaf2) is
            // distinct from the NerdQAxe++'s 0xcaf1 so the two stay
            // unambiguous.
            vid: Match::Specific(0xc0de),
            pid: Match::Specific(0xcaf2),
            bcd_device: Match::Any,
            manufacturer: Match::Any,
            product: Match::Any,
            serial_pattern: Match::Any,
        },
        name: "NerdQX",
        create_fn: |device| Box::pin(nerdqaxe_pp::create_from_usb(device, &VARIANT)),
    }
}

/// The NerdQX.
pub(super) const VARIANT: Variant = Variant {
    model: "NerdQX",
    board_prefix: "nerdqx",
    thread_prefix: "NerdQX",
    regulator: Tps53647Config::NERDQX,
    // M2 (ASIC) is wired to PWM2/TACH2 (Channel::Fan2) and M1 (VREG) to
    // PWM1/TACH1 (Channel::Fan1); M3 is bare +12 V/GND.
    fans: [("M2", Channel::Fan2), ("M1", Channel::Fan1)],
    tmux: true,
    psu_limited: true,
    // 35 kHz VR clock (`round(196_608_000 / 35_000) == 0x15F1`).
    hash_counting_number: Some(0x15F1),
};

/// Highest clock allowed without per-die temperatures, in MHz.
pub(super) const SAFE_MAX_FREQ_MHZ: f32 = 495.0;
/// Highest core voltage allowed without per-die temperatures, in mV.
pub(super) const SAFE_MAX_VOLTAGE_MV: u16 = 1150;

/// bitaxe-raw GPIO command index of the temperature mux select.
const TEMP_MUX_CMD: u8 = 0x06;
/// I2C address of the TMP451.
const TMP451_ADDRESS: u8 = 0x4C;
/// Wait after switching the mux before the diode is trustworthy.
const MUX_SETTLE: Duration = Duration::from_millis(50);
/// Wait for a conversion started after the mux switch.
const CONVERSION_WAIT: Duration = Duration::from_millis(75);
/// Dies behind the mux, one per BM1370.
pub(super) const DIES: usize = 4;

/// One TMP451 shared by the four ASIC thermal diodes through a GPIO mux.
pub(super) struct TempMux {
    gpio: BitaxeRawGpioController,
    sensor: Tmp451<BitaxeRawI2c>,
}

impl TempMux {
    /// Select channel 0 and verify the TMP451 answers.
    ///
    /// Selecting a channel doubles as a firmware check: bitaxe-raw builds
    /// without the mux command reject it, which lands in the same fallback
    /// as a missing sensor.
    pub(super) async fn new(mut gpio: BitaxeRawGpioController, i2c: BitaxeRawI2c) -> Result<Self> {
        gpio.set_mux_channel(TEMP_MUX_CMD, 0)
            .await
            .context("temperature mux select failed")?;
        let mut sensor = Tmp451::new(i2c, TMP451_ADDRESS);
        sensor.init().await.context("TMP451 init failed")?;
        Ok(Self { gpio, sensor })
    }

    /// Calibrated die temperatures, in Celsius. A die that cannot be read
    /// is `None` rather than failing the others.
    pub(super) async fn read_dies(&mut self) -> [Option<f32>; DIES] {
        let mut dies = [None; DIES];
        for (channel, die) in dies.iter_mut().enumerate() {
            match self.read_die(channel as u8).await {
                Ok(c) => *die = Some(c),
                Err(e) => warn!(die = channel, error = %e, "TMP451 die read failed"),
            }
        }
        dies
    }

    async fn read_die(&mut self, channel: u8) -> Result<f32> {
        self.gpio.set_mux_channel(TEMP_MUX_CMD, channel).await?;
        time::sleep(MUX_SETTLE).await;
        // A conversion started before the diode settled would report the
        // previous die; this one is discarded and the next one is read.
        // (Ignored by a sensor that is free-running.)
        self.sensor.trigger_one_shot().await?;
        time::sleep(CONVERSION_WAIT).await;
        Ok(self
            .sensor
            .read_remote_calibrated(NERDQX_SCALE, NERDQX_OFFSET)
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asic::bm13xx::chip_profile::BM1370;

    #[test]
    fn variant_matches_the_board() {
        // M3 has no PWM or tach: exactly two fans, M2 on Fan2 and M1 on Fan1.
        assert_eq!(VARIANT.fans[0], ("M2", Channel::Fan2));
        assert_eq!(VARIANT.fans[1], ("M1", Channel::Fan1));
        assert!(VARIANT.tmux && VARIANT.psu_limited);
        assert_eq!(VARIANT.regulator.ifault_a, 142.0);
        assert_eq!(VARIANT.hash_counting_number, Some(0x15F1));
        assert_eq!(DIES, 4);
    }

    #[test]
    fn safe_limits_sit_inside_the_chip_envelope() {
        assert!(SAFE_MAX_FREQ_MHZ >= BM1370.min_freq_mhz);
        assert!(SAFE_MAX_FREQ_MHZ < BM1370.max_freq_mhz);
        assert_eq!(SAFE_MAX_VOLTAGE_MV, BM1370.min_voltage_mv);
    }

    #[test]
    fn descriptor_registered_for_the_nerdqx_pid() {
        let desc = inventory::iter::<super::super::BoardDescriptor>()
            .find(|d| d.name == "NerdQX")
            .expect("NerdQX descriptor registered");
        assert!(matches!(desc.pattern.vid, Match::Specific(0xc0de)));
        assert!(matches!(desc.pattern.pid, Match::Specific(0xcaf2)));
    }
}
