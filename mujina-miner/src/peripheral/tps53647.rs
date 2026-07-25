//! TPS53647 multiphase buck controller driver.
//!
//! The TPS53647 is a TI 4-phase step-down controller with a PMBus
//! interface, used as the ASIC core rail regulator on multi-chip boards.
//! It differs from the [`Tps546`](super::tps546) the single-chip Bitaxe
//! uses in three ways that matter here:
//!
//! - Output voltage is commanded as an **8-bit VID code**, not Linear16.
//!   See [`Vid`].
//! - Phase count and current limit are set through manufacturer-specific
//!   registers rather than standard PMBus ones.
//! - `READ_VOUT` is not used for readback; the part exposes the measured
//!   output on a manufacturer register in a fixed-point format.
//!
//! Register values and the initialization order follow the reference
//! firmware for the NerdQAxe++, which is the only board using this part
//! today.
//!
//! Datasheet: <https://www.ti.com/lit/ds/symlink/tps53647.pdf>

use crate::{
    hw_trait::{HwError, Result, i2c::I2c},
    peripheral::pmbus::{Linear11, PmbusCommand},
    tracing::prelude::*,
};

/// I2C address of the regulator.
///
/// Set on the board by the ADDR_TRISE resistor divider, which encodes the
/// PMBus address and the soft-start slew rate together.
pub const DEFAULT_ADDRESS: u8 = 0x71;

/// Value the device-code register returns on a genuine TPS53647.
pub const DEVICE_CODE: u16 = 0x01F0;

/// Manufacturer-specific command codes.
///
/// PMBus reserves 0xD0..=0xFD for manufacturer use, so these numbers mean
/// nothing outside this part. They are defined here rather than added to
/// the shared [`PmbusCommand`] enum precisely because they collide with
/// other vendors' meanings at the same addresses.
mod mfr {
    /// Measured output voltage, unsigned fixed-point with 9 fractional bits.
    pub const VOUT_MEASURED: u8 = 0xD4;
    /// Maximum output current, in whole amps.
    pub const IMAX: u8 = 0xDA;
    /// Switching frequency selection.
    pub const SWITCHING_FREQUENCY: u8 = 0xDC;
    /// Operation mode: VR12 mode, phase shedding, slew rate.
    pub const OPERATION_MODE: u8 = 0xDD;
    /// Active phase count, encoded as `phases - 1`.
    pub const PHASE_COUNT: u8 = 0xE4;
    /// Device code, used to confirm the part before configuring it.
    pub const DEVICE_CODE: u8 = 0xFC;
}

/// Driver error.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// I2C bus error.
    #[error("I2C: {0}")]
    I2c(#[from] HwError),

    /// The device-code register did not identify a TPS53647. Either
    /// nothing is at this address or it is a different regulator.
    #[error("expected TPS53647 device code {DEVICE_CODE:#06x}, read {0:#06x}")]
    WrongDevice(u16),

    /// A requested output voltage fell outside the configured window.
    #[error("requested {requested:.3} V is outside the allowed {min:.3}-{max:.3} V")]
    VoltageOutOfRange { requested: f32, min: f32, max: f32 },

    /// A requested voltage did not fit the 8-bit VID range.
    #[error("{0:.3} V does not map to a valid VID code")]
    UnrepresentableVoltage(f32),

    /// Phase count outside what the controller supports.
    #[error("phase count {0} out of range 1-6")]
    PhaseCountOutOfRange(u8),
}

type DriverResult<T> = std::result::Result<T, Error>;

/// An 8-bit VR12-style voltage identification code.
///
/// The mapping is linear: code 1 is [`Vid::FLOOR_V`] and each further
/// step adds [`Vid::STEP_V`]. Code 0 is a distinguished "output off"
/// value, not 0.25 V.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vid(u8);

impl Vid {
    /// Voltage of VID code 1.
    pub const FLOOR_V: f32 = 0.25;
    /// Volts per VID step.
    pub const STEP_V: f32 = 0.005;

    /// The code that switches the output off.
    pub const OFF: Self = Self(0);

    /// Factory-default code, corresponding to 1.000 V.
    ///
    /// Firmware treats a readback of this exact value as "the regulator
    /// has reset and lost its configuration", so it must never be written
    /// deliberately. [`Vid::from_volts`] nudges it down one step, which
    /// is a 5 mV error and well inside the rail's regulation tolerance.
    const FACTORY_DEFAULT: u8 = 0x97;

    /// Encode a voltage, rounding to the nearest 5 mV step.
    pub fn from_volts(volts: f32) -> DriverResult<Self> {
        if volts == 0.0 {
            return Ok(Self::OFF);
        }
        let code = ((volts - Self::FLOOR_V) / Self::STEP_V).round() + 1.0;
        if !(1.0..=255.0).contains(&code) {
            return Err(Error::UnrepresentableVoltage(volts));
        }
        let code = code as u8;
        // Step away from the reset sentinel rather than refusing the
        // request; see FACTORY_DEFAULT.
        let code = if code == Self::FACTORY_DEFAULT {
            Self::FACTORY_DEFAULT - 1
        } else {
            code
        };
        Ok(Self(code))
    }

    /// Decode to volts. Code 0 means the output is off.
    pub fn to_volts(self) -> f32 {
        if self.0 == 0 {
            return 0.0;
        }
        (self.0 - 1) as f32 * Self::STEP_V + Self::FLOOR_V
    }

    /// The raw code.
    pub const fn raw(self) -> u8 {
        self.0
    }
}

/// Configuration applied by [`Tps53647::init`].
#[derive(Debug, Clone, Copy)]
pub struct Tps53647Config {
    /// Number of active phases. Phases beyond this are shed.
    pub phases: u8,
    /// Maximum output current in amps, written to the current-limit
    /// register. Must match the IMON resistor fitted on the board:
    /// `R_kohm = 6000 / imax_a`.
    pub imax_a: u8,
    /// Overcurrent warn and fault threshold, in amps.
    pub ifault_a: f32,
    /// Lowest output voltage [`set_vout`](Tps53647::set_vout) will accept.
    pub vout_min_v: f32,
    /// Highest output voltage [`set_vout`](Tps53647::set_vout) will accept.
    pub vout_max_v: f32,
    /// Overtemperature warning threshold, in Celsius.
    pub ot_warn_c: f32,
    /// Overtemperature fault threshold, in Celsius.
    pub ot_fault_c: f32,
}

impl Tps53647Config {
    /// Settings for the NerdQAxe++: three active phases feeding four
    /// BM1370 in parallel on one core rail.
    ///
    /// `imax_a` is 90 A to match the board's 66.5 kOhm IMON resistor, and
    /// the voltage window brackets the ~1.15 V the chips run at. The
    /// window is a driver-level clamp; the chip-level envelope still
    /// comes from `chip_profile`.
    pub const NERDQAXE_PP: Self = Self {
        phases: 3,
        imax_a: 90,
        ifault_a: 95.0,
        vout_min_v: 0.8,
        vout_max_v: 1.4,
        ot_warn_c: 95.0,
        ot_fault_c: 125.0,
    };
}

/// TPS53647 driver, generic over the I2C implementation.
pub struct Tps53647<I> {
    i2c: I,
    address: u8,
    config: Tps53647Config,
}

impl<I: I2c> Tps53647<I> {
    /// Create a driver bound to the default address.
    pub fn new(i2c: I, config: Tps53647Config) -> Self {
        Self {
            i2c,
            address: DEFAULT_ADDRESS,
            config,
        }
    }

    /// Read the device code without configuring anything.
    ///
    /// Safe to call before [`init`](Self::init) as a presence check.
    pub async fn device_code(&mut self) -> DriverResult<u16> {
        Ok(self.read_word(mfr::DEVICE_CODE).await?)
    }

    /// Identify the regulator and configure its phase, current and
    /// temperature limits.
    ///
    /// # This does not gate the output
    ///
    /// Whether the rail is live is decided by the ENABLE pin, which this
    /// driver does not own -- the board holds it. Calling `init` on a
    /// board that already has ENABLE asserted **will** leave a live rail.
    /// Callers must have the enable line low first.
    ///
    /// What `init` does guarantee is the *voltage* the rail would come up
    /// at: `RESTORE_DEFAULT_ALL` reloads VOUT_COMMAND from NVM, so the
    /// last step here overwrites it with the bottom of the configured
    /// window. Without that, enabling the rail later would bring four
    /// ASICs up at whatever voltage happened to be stored -- the factory
    /// default is 1.000 V, comfortably live. Coming up at the floor is
    /// recoverable; coming up at an unknown voltage is not.
    pub async fn init(&mut self) -> DriverResult<()> {
        let code = self.device_code().await?;
        if code != DEVICE_CODE {
            return Err(Error::WrongDevice(code));
        }
        if !(1..=6).contains(&self.config.phases) {
            return Err(Error::PhaseCountOutOfRange(self.config.phases));
        }
        debug!(device_code = format!("{code:#06x}"), "Found TPS53647");

        self.write_command(PmbusCommand::ClearFaults).await?;
        self.restore_defaults().await?;

        // Off first, so nothing downstream sees a rail while the current
        // and phase limits below are still at their NVM values.
        //
        // 0b0001_0111: output controlled by the OPERATION command and the
        // ENABLE pin together, turning off immediately rather than using
        // the programmed turn-off delay.
        const ON_OFF_CONFIG: u8 = 0b0001_0111;
        self.write_byte(PmbusCommand::OnOffConfig.as_u8(), ON_OFF_CONFIG)
            .await?;

        // 500 kHz switching.
        const SWITCHING_500KHZ: u8 = 0x20;
        self.write_byte(mfr::SWITCHING_FREQUENCY, SWITCHING_500KHZ)
            .await?;

        self.write_byte(mfr::IMAX, self.config.imax_a).await?;

        // VR12 mode, dynamic phase shedding enabled, 0.68 mV/us slew.
        const OPERATION_MODE: u8 = 0x89;
        self.write_byte(mfr::OPERATION_MODE, OPERATION_MODE).await?;

        // The reference firmware re-sends these two after the mode write,
        // which suggests the mode change disturbs them. Kept deliberately.
        self.write_byte(PmbusCommand::OnOffConfig.as_u8(), ON_OFF_CONFIG)
            .await?;
        self.write_byte(mfr::SWITCHING_FREQUENCY, SWITCHING_500KHZ)
            .await?;

        self.write_byte(mfr::PHASE_COUNT, self.config.phases - 1)
            .await?;

        self.write_linear11(PmbusCommand::OtWarnLimit.as_u8(), self.config.ot_warn_c)
            .await?;
        self.write_linear11(PmbusCommand::OtFaultLimit.as_u8(), self.config.ot_fault_c)
            .await?;
        self.write_linear11(PmbusCommand::IoutOcWarnLimit.as_u8(), self.config.ifault_a)
            .await?;
        self.write_linear11(PmbusCommand::IoutOcFaultLimit.as_u8(), self.config.ifault_a)
            .await?;

        // Leave a known voltage behind, overwriting whatever
        // RESTORE_DEFAULT_ALL loaded. See the note on this function.
        self.set_vout(self.config.vout_min_v).await?;

        debug!(
            phases = self.config.phases,
            imax_a = self.config.imax_a,
            ifault_a = self.config.ifault_a,
            vout_v = self.config.vout_min_v,
            "TPS53647 configured; rail will come up at the window floor when enabled"
        );
        Ok(())
    }

    /// Command the output voltage.
    ///
    /// Rejects anything outside the configured window rather than
    /// clamping: a caller asking for an out-of-range core voltage has a
    /// bug, and silently substituting a different voltage would hide it.
    pub async fn set_vout(&mut self, volts: f32) -> DriverResult<()> {
        if volts < self.config.vout_min_v || volts > self.config.vout_max_v {
            return Err(Error::VoltageOutOfRange {
                requested: volts,
                min: self.config.vout_min_v,
                max: self.config.vout_max_v,
            });
        }
        let vid = Vid::from_volts(volts)?;
        self.write_word(PmbusCommand::VoutCommand.as_u8(), vid.raw() as u16)
            .await?;
        debug!(
            requested_v = volts,
            vid = format!("{:#04x}", vid.raw()),
            actual_v = vid.to_volts(),
            "TPS53647 vout set"
        );
        Ok(())
    }

    /// Read the measured output voltage, in volts.
    ///
    /// Uses the manufacturer register rather than `READ_VOUT`: the value
    /// is unsigned fixed-point with 9 fractional bits.
    pub async fn vout(&mut self) -> DriverResult<f32> {
        const FRACTIONAL_BITS: i32 = 9;
        let raw = self.read_word(mfr::VOUT_MEASURED).await?;
        Ok(raw as f32 * 2.0f32.powi(-FRACTIONAL_BITS))
    }

    /// Read the input voltage, in volts.
    pub async fn vin(&mut self) -> DriverResult<f32> {
        self.read_linear11(PmbusCommand::ReadVin.as_u8()).await
    }

    /// Read the output current, in amps.
    pub async fn iout(&mut self) -> DriverResult<f32> {
        self.read_linear11(PmbusCommand::ReadIout.as_u8()).await
    }

    /// Read the controller temperature, in Celsius.
    pub async fn temperature(&mut self) -> DriverResult<f32> {
        self.read_linear11(PmbusCommand::ReadTemperature1.as_u8())
            .await
    }

    /// Reload all settings from NVM.
    async fn restore_defaults(&mut self) -> DriverResult<()> {
        /// RESTORE_DEFAULT_ALL. Not in the shared command enum yet.
        const RESTORE_DEFAULT_ALL: u8 = 0x12;
        self.i2c.write(self.address, &[RESTORE_DEFAULT_ALL]).await?;
        Ok(())
    }

    // -- transport helpers ------------------------------------------------
    //
    // PMBus words are little-endian on the wire.

    async fn write_command(&mut self, cmd: PmbusCommand) -> Result<()> {
        self.i2c.write(self.address, &[cmd.as_u8()]).await
    }

    async fn write_byte(&mut self, cmd: u8, value: u8) -> Result<()> {
        self.i2c.write(self.address, &[cmd, value]).await
    }

    async fn write_word(&mut self, cmd: u8, value: u16) -> Result<()> {
        let [lo, hi] = value.to_le_bytes();
        self.i2c.write(self.address, &[cmd, lo, hi]).await
    }

    async fn read_word(&mut self, cmd: u8) -> Result<u16> {
        let mut buf = [0u8; 2];
        self.i2c.write_read(self.address, &[cmd], &mut buf).await?;
        Ok(u16::from_le_bytes(buf))
    }

    async fn write_linear11(&mut self, cmd: u8, value: f32) -> DriverResult<()> {
        let encoded =
            Linear11::from_f32(value).map_err(|_| Error::UnrepresentableVoltage(value))?;
        self.write_word(cmd, encoded.0).await?;
        Ok(())
    }

    async fn read_linear11(&mut self, cmd: u8) -> DriverResult<f32> {
        let raw = self.read_word(cmd).await?;
        Ok(Linear11::new(raw).to_f32())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vid_matches_the_reference_encoding() {
        // vid = (V - 0.25) / 0.005 + 1
        assert_eq!(Vid::from_volts(0.25).unwrap().raw(), 1);
        assert_eq!(Vid::from_volts(1.15).unwrap().raw(), 181);
        assert_eq!(Vid::from_volts(1.20).unwrap().raw(), 191);
    }

    #[test]
    fn vid_round_trips_within_one_step() {
        for mv in 800..=1400 {
            let volts = mv as f32 / 1000.0;
            let vid = Vid::from_volts(volts).unwrap();
            // Codes near the factory-default sentinel get nudged a step
            // down, so they can land up to 1.5 steps low; every other
            // code is within half a step of the request.
            let tolerance = if vid.raw() == Vid::FACTORY_DEFAULT - 1 {
                Vid::STEP_V * 1.5
            } else {
                Vid::STEP_V / 2.0
            };
            assert!(
                (vid.to_volts() - volts).abs() <= tolerance + 1e-6,
                "{volts} V -> {:#04x} -> {} V",
                vid.raw(),
                vid.to_volts()
            );
        }
    }

    #[test]
    fn the_factory_default_nudge_never_overshoots() {
        // Rounding away from the sentinel must go *down*. A core rail
        // that comes up 5 mV high is worse than one 5 mV low, and this
        // is the one place the driver knowingly misses the request.
        for mv in [998, 999, 1000, 1001, 1002] {
            let volts = mv as f32 / 1000.0;
            let vid = Vid::from_volts(volts).unwrap();
            assert_ne!(vid.raw(), Vid::FACTORY_DEFAULT);
            if vid.raw() == Vid::FACTORY_DEFAULT - 1 {
                assert!(
                    vid.to_volts() <= volts,
                    "{volts} V nudged UP to {} V",
                    vid.to_volts()
                );
            }
        }
    }

    #[test]
    fn vid_avoids_the_factory_default_code() {
        // 1.000 V lands exactly on 0x97, the value firmware uses to
        // detect a regulator that has reset. It must be nudged.
        let vid = Vid::from_volts(1.000).unwrap();
        assert_ne!(vid.raw(), 0x97);
        assert_eq!(vid.raw(), 0x96);
        assert!((vid.to_volts() - 0.995).abs() < 1e-6);
    }

    #[test]
    fn vid_zero_is_off_not_a_voltage() {
        assert_eq!(Vid::from_volts(0.0).unwrap(), Vid::OFF);
        assert_eq!(Vid::OFF.to_volts(), 0.0);
        // And the floor voltage is code 1, not code 0.
        assert_ne!(Vid::from_volts(0.25).unwrap(), Vid::OFF);
    }

    #[test]
    fn vid_rejects_voltages_off_the_scale() {
        // Below the floor.
        assert!(Vid::from_volts(0.1).is_err());
        // Above what 8 bits can express: 0.25 + 254*0.005 = 1.52 V.
        assert!(Vid::from_volts(2.0).is_err());
    }

    #[test]
    fn nerdqaxe_config_matches_the_board() {
        let c = Tps53647Config::NERDQAXE_PP;
        assert_eq!(c.phases, 3, "CSP4 is strapped to 3V3, disabling phase 4");
        // R_IMON = 6000 / imax; the board fits 66.5k.
        assert!((6000.0 / c.imax_a as f32 - 66.5).abs() < 1.0);
        // The operating point has to sit inside the clamp.
        assert!(c.vout_min_v < 1.15 && 1.15 < c.vout_max_v);
        assert!(c.ifault_a > c.imax_a as f32);
    }
}
