//! EMC2302 dual-channel PWM fan controller driver.
//!
//! The EMC2302 drives two fans over I2C with independent PWM duty and
//! tachometer readback. Unlike the [`emc2101`](super::emc2101) it has no
//! temperature sensing at all -- boards using it read temperature from a
//! separate sensor.
//!
//! Register layout is per-channel: each fan has an identical block of
//! offsets based at its own base address, plus a handful of device-wide
//! configuration registers.
//!
//! Datasheet: <https://www.microchip.com/en-us/product/emc2302>

use crate::{
    hw_trait::{Result, i2c::I2c},
    tracing::prelude::*,
};

// The `Percent` duty type is shared with the EMC2101 rather than
// redefined: it is a plain 0-100 clamp with no controller-specific
// behaviour, and boards hand the same value to either chip.
pub use super::emc2101::Percent;

/// I2C address of the EMC2302-1 variant.
///
/// The address is fixed per order-code suffix rather than strapped, so it
/// is a property of the part fitted, not of the board wiring. The
/// NerdQAxe++ fits the `-1`.
pub const DEFAULT_ADDRESS: u8 = 0x2E;

// Only the registers this driver touches are named. The rest of the map
// is in the datasheet; transcribing it here would be unused constants the
// compiler cannot warn about.

/// PWM output polarity, one bit per channel.
const REG_POLARITY: u8 = 0x2A;
/// PWM output driver type (push-pull vs open-drain), one bit per channel.
const REG_OUTPUT_CONFIG: u8 = 0x2B;
/// PWM base frequency for channels 1/2/3.
const REG_BASE_F123: u8 = 0x2D;

/// Register base of the channel 1 block.
const FAN1_BASE: u8 = 0x30;
/// Register base of the channel 2 block.
const FAN2_BASE: u8 = 0x40;

/// PWM duty setting (8-bit, 0-255), offset from a channel base.
const OFS_FAN_SETTING: u8 = 0x00;
/// Fan configuration 1 (edge count, range, control mode).
const OFS_FAN_CONFIG1: u8 = 0x02;
/// TACH reading, high byte.
const OFS_TACH_READING_MSB: u8 = 0x0E;
/// TACH reading, low byte.
const OFS_TACH_READING_LSB: u8 = 0x0F;

/// Which of the two fan outputs to address.
///
/// The variants name the silicon's own channel numbering. Boards label
/// their headers independently -- on the NerdQAxe++, header M1 is
/// [`Channel::Fan1`] and header M2 is [`Channel::Fan2`] -- so board code
/// should map its labels to these explicitly rather than assuming an
/// index order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// Channel 1, register block at [`FAN1_BASE`].
    Fan1,
    /// Channel 2, register block at [`FAN2_BASE`].
    Fan2,
}

impl Channel {
    /// Register base address of this channel's block.
    pub const fn base(self) -> u8 {
        match self {
            Channel::Fan1 => FAN1_BASE,
            Channel::Fan2 => FAN2_BASE,
        }
    }
}

/// Tachometer reading converted to RPM.
///
/// The EMC2302 reports a tach *period* count, so RPM is inversely
/// proportional to the raw value and an absent fan reads as the maximum
/// count rather than zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tach(u16);

impl Tach {
    /// Tachometer clock, in Hz.
    const FTACH: u32 = 32_768;
    /// Edges sampled per measurement, set by FAN_CONFIG1 bits 4:3.
    const EDGES: u32 = 5;
    /// Fan pole count. Two poles is near-universal for 4-wire PC fans and
    /// matches the parts fitted to the boards using this driver.
    const POLES: u32 = 2;
    /// Range multiplier, set by the RANGE bits (1x at reset).
    const RANGE_MULTIPLIER: u32 = 1;

    /// Raw counts at or above this mean no fan is turning: the counter
    /// saturates its 13-bit range when it sees no edges. The datasheet
    /// puts the usable measurement floor at 480 RPM, which is roughly
    /// where this saturation lands, so a stopped fan and a very slow one
    /// are genuinely indistinguishable here.
    const SATURATED: u16 = 8191;

    /// Build from the raw 13-bit period count.
    pub const fn from_raw(raw: u16) -> Self {
        Self(raw)
    }

    /// Convert to RPM. Returns `None` when no fan is detected, rather
    /// than a fabricated zero -- the caller decides whether that means
    /// "stopped" or "not fitted".
    pub fn as_rpm(self) -> Option<u32> {
        if self.0 == 0 || self.0 >= Self::SATURATED {
            return None;
        }
        let rpm = 60 * Self::FTACH * Self::RANGE_MULTIPLIER * (Self::EDGES - 1)
            / Self::POLES
            / self.0 as u32;
        Some(rpm)
    }
}

/// EMC2302 driver.
pub struct Emc2302<I: I2c> {
    i2c: I,
    address: u8,
}

impl<I: I2c> Emc2302<I> {
    /// Full-scale value of the 8-bit PWM duty register.
    const PWM_MAX: u8 = 255;

    /// Create a driver bound to the default address.
    pub fn new(i2c: I) -> Self {
        Self {
            i2c,
            address: DEFAULT_ADDRESS,
        }
    }

    /// Configure both channels for manual PWM control.
    ///
    /// `invert_polarity` selects whether 0x00 drives the fan at full
    /// speed; boards with a plain PWM header want `false`.
    ///
    /// There is no ID register to check on this part, so a wrong address
    /// or a dead bus surfaces as an I2C error from the first write rather
    /// than as an identification failure.
    pub async fn init(&mut self, invert_polarity: bool) -> Result<()> {
        self.set_polarity(invert_polarity).await?;

        // Push-pull drivers on both channels.
        const OUTPUT_PUSH_PULL_BOTH: u8 = 0x03;
        self.write_register(REG_OUTPUT_CONFIG, OUTPUT_PUSH_PULL_BOTH)
            .await?;

        // 19.53 kHz PWM base frequency on both channels -- above audible.
        const BASE_FREQ_19_53KHZ: u8 = (0x01) | (0x01 << 3);
        self.write_register(REG_BASE_F123, BASE_FREQ_19_53KHZ)
            .await?;

        // Manual (open-loop) duty control, sampling 5 tach edges per
        // measurement to suit a 2-pole fan. This edge count is what
        // `Tach::EDGES` assumes when converting to RPM.
        const FAN_CONFIG1_5_EDGES: u8 = 0b01 << 3;
        for channel in [Channel::Fan1, Channel::Fan2] {
            self.write_register(channel.base() + OFS_FAN_CONFIG1, FAN_CONFIG1_5_EDGES)
                .await?;
        }

        debug!("EMC2302 initialized for manual PWM control on both channels");
        Ok(())
    }

    /// Set the PWM output polarity for both channels.
    async fn set_polarity(&mut self, invert: bool) -> Result<()> {
        const BOTH_CHANNELS: u8 = 0x03;
        let value = if invert { BOTH_CHANNELS } else { 0x00 };
        self.write_register(REG_POLARITY, value).await
    }

    /// Drive a channel at the given duty cycle.
    pub async fn set_fan_speed(&mut self, channel: Channel, speed: Percent) -> Result<()> {
        let duty = speed.of(Self::PWM_MAX);
        self.write_register(channel.base() + OFS_FAN_SETTING, duty)
            .await
    }

    /// Read a channel's raw tachometer count.
    pub async fn get_tach(&mut self, channel: Channel) -> Result<Tach> {
        let msb = self
            .read_register(channel.base() + OFS_TACH_READING_MSB)
            .await?;
        let lsb = self
            .read_register(channel.base() + OFS_TACH_READING_LSB)
            .await?;

        // 13-bit count split across the pair: the low 3 bits of LSB are
        // unused padding.
        let raw = ((msb as u16) << 5) | ((lsb as u16) >> 3);
        trace!(
            channel = ?channel,
            msb = format!("{msb:#04x}"),
            lsb = format!("{lsb:#04x}"),
            raw,
            "EMC2302 tach"
        );
        Ok(Tach::from_raw(raw))
    }

    /// Read a channel's speed in RPM, or `None` if no fan is detected.
    pub async fn get_rpm(&mut self, channel: Channel) -> Result<Option<u32>> {
        Ok(self.get_tach(channel).await?.as_rpm())
    }

    async fn read_register(&mut self, reg: u8) -> Result<u8> {
        let mut buf = [0u8; 1];
        self.i2c.write_read(self.address, &[reg], &mut buf).await?;
        Ok(buf[0])
    }

    async fn write_register(&mut self, reg: u8, value: u8) -> Result<()> {
        self.i2c.write(self.address, &[reg, value]).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_bases_match_the_datasheet_blocks() {
        assert_eq!(Channel::Fan1.base(), 0x30);
        assert_eq!(Channel::Fan2.base(), 0x40);
    }

    #[test]
    fn tach_converts_using_the_reference_constant() {
        // 60 * 32768 * 1 * 4 / 2 = 3_932_160 counts-per-minute numerator.
        assert_eq!(Tach::from_raw(1487).as_rpm(), Some(3_932_160 / 1487));
    }

    #[test]
    fn tach_matches_measured_noctua_reading() {
        // A Noctua NF-A9x14 at 100% reads ~1487 raw, which the datasheet
        // arithmetic turns into ~2644 RPM. The fan's own spec (87 Hz
        // tacho, 2 cycles/rev) independently gives 2610 RPM, so the
        // conversion is right to within measurement error.
        let rpm = Tach::from_raw(1487).as_rpm().unwrap();
        assert!((2600..=2700).contains(&rpm), "got {rpm} RPM");
    }

    #[test]
    fn saturated_tach_reports_no_fan() {
        // A disconnected fan saturates the 13-bit counter. Reporting
        // `None` keeps it distinguishable from a real slow reading, which
        // a fabricated 0 RPM would not be.
        assert_eq!(Tach::from_raw(8191).as_rpm(), None);
        assert_eq!(Tach::from_raw(0xFFFF).as_rpm(), None);
        // Zero would divide by zero.
        assert_eq!(Tach::from_raw(0).as_rpm(), None);
    }

    #[test]
    fn tach_just_below_saturation_still_reads() {
        let rpm = Tach::from_raw(8190).as_rpm().expect("should report");
        // ~480 RPM, the datasheet's stated measurement floor.
        assert!((470..=490).contains(&rpm), "got {rpm} RPM");
    }

    #[test]
    fn duty_scales_to_the_full_8bit_range() {
        // The EMC2302 duty register is 8-bit, unlike the EMC2101's 6-bit
        // one, so full scale is 255 rather than 63.
        const MAX: u8 = 255;
        assert_eq!(Percent::ZERO.of(MAX), 0);
        assert_eq!(Percent::FULL.of(MAX), 255);
        assert_eq!(Percent::new_clamped(50).of(MAX), 127);
    }
}
