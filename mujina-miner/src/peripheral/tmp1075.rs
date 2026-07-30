//! TMP1075 digital temperature sensor driver.
//!
//! Driver for the Texas Instruments TMP1075, a 12-bit I2C temperature
//! sensor with 0.0625 C resolution. Compatible with the LM75/TMP75
//! register interface.
//!
//! Datasheet: <https://www.ti.com/lit/ds/symlink/tmp1075.pdf>

use crate::hw_trait::{HwError, i2c::I2c};

/// Expected device ID value.
pub const DEVICE_ID: u16 = 0x7500;

/// TMP1075 register addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Register {
    /// Temperature result (read-only, 12-bit, two's complement)
    Temp = 0x00,
    /// Configuration
    Cfgr = 0x01,
    /// Low limit
    Llim = 0x02,
    /// High limit
    Hlim = 0x03,
    /// Device ID (read-only, not available on TMP1075N)
    DieId = 0x0F,
}

/// Driver error.
#[derive(Debug, thiserror::Error)]
pub enum Error<E> {
    /// I2C bus error
    #[error("I2C: {0}")]
    I2c(E),

    /// Device ID register returned an unexpected value
    #[error("unexpected device ID: 0x{0:04X}")]
    UnexpectedDeviceId(u16),

    /// The temperature register read back a value outside the part's
    /// rated range, which means the bus floated rather than a device
    /// answering.
    #[error("implausible temperature reading 0x{0:04X}; no sensor at this address")]
    NoSensor(u16),
}

/// A raw temperature reading from the TMP1075.
///
/// Wraps the 12-bit two's complement value (after shifting out the 4
/// unused low bits). Each LSB represents 0.0625 C.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reading(i16);

impl Reading {
    /// Resolution in degrees Celsius per LSB.
    const DEGREES_PER_LSB: f32 = 0.0625;

    /// Construct from a raw register value (16-bit, upper 12 bits are
    /// the temperature in two's complement, lower 4 bits unused).
    pub fn from_raw(raw: u16) -> Self {
        Self(raw as i16 >> 4)
    }

    /// Temperature in degrees Celsius.
    pub fn as_degrees_c(self) -> f32 {
        self.0 as f32 * Self::DEGREES_PER_LSB
    }
}

type Result<T> = std::result::Result<T, Error<HwError>>;

/// TMP1075 driver, generic over I2C implementation.
pub struct Tmp1075<I> {
    i2c: I,
    address: u8,
}

impl<I: I2c> Tmp1075<I> {
    /// Create a new driver instance.
    pub fn new(i2c: I, address: u8) -> Self {
        Self { i2c, address }
    }

    /// Verify the device ID register.
    ///
    /// Only valid on the base TMP1075. The TMP1075**N** has no Die ID
    /// register and will fail here -- use [`probe`](Self::probe) for that
    /// variant.
    pub async fn init(&mut self) -> Result<()> {
        let id = self.read_register(Register::DieId).await?;
        if id != DEVICE_ID {
            return Err(Error::UnexpectedDeviceId(id));
        }
        Ok(())
    }

    /// Confirm a sensor is present without reading the Die ID.
    ///
    /// The TMP1075N drops the Die ID register that [`init`](Self::init)
    /// checks, so identification has to come from the temperature
    /// register instead. A missing device NACKs its address and surfaces
    /// as an I2C error; a present one returns a reading inside the part's
    /// operating range. Values above the datasheet maximum mean the bus
    /// floated high rather than a device answering, which is the failure
    /// this catches that a bare ACK check would not.
    pub async fn probe(&mut self) -> Result<Reading> {
        /// Highest code the 12-bit register can hold, 127.9375 C. The
        /// part is only rated to 128 C, so anything at the rail is a
        /// floating bus, not a temperature.
        const MAX_VALID_RAW: u16 = 0x7FF0;

        let raw = self.read_register(Register::Temp).await?;
        if raw >= MAX_VALID_RAW {
            return Err(Error::NoSensor(raw));
        }
        Ok(Reading::from_raw(raw))
    }

    /// Read the current temperature.
    pub async fn read(&mut self) -> Result<Reading> {
        let raw = self.read_register(Register::Temp).await?;
        Ok(Reading::from_raw(raw))
    }

    async fn read_register(&mut self, reg: Register) -> Result<u16> {
        let mut buf = [0u8; 2];
        self.i2c
            .write_read(self.address, &[reg as u8], &mut buf)
            .await
            .map_err(Error::I2c)?;
        Ok(u16::from_be_bytes(buf))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Datasheet Table 7-1 test vectors.
    #[test]
    fn reading_from_raw() {
        let cases: &[(u16, f32)] = &[
            (0x7FF0, 127.9375),
            (0x6400, 100.0),
            (0x5000, 80.0),
            (0x4B00, 75.0),
            (0x3200, 50.0),
            (0x1900, 25.0),
            (0x0040, 0.25),
            (0x0010, 0.0625),
            (0x0000, 0.0),
            (0xFFF0, -0.0625),
            (0xFFC0, -0.25),
            (0xE700, -25.0),
            (0xCE00, -50.0),
            (0x8000, -128.0),
        ];
        for &(raw, expected) in cases {
            let reading = Reading::from_raw(raw);
            assert!(
                (reading.as_degrees_c() - expected).abs() < 1e-4,
                "raw=0x{:04X}: got {} C, expected {} C",
                raw,
                reading.as_degrees_c(),
                expected,
            );
        }
    }
}
