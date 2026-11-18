//! TMP451 remote and local temperature sensor driver.
//!
//! Driver for the Texas Instruments TMP451, a dual-channel 12-bit I2C
//! temperature sensor with a local channel (die temperature) and a
//! remote channel (external diode). Both channels have 0.0625 C
//! resolution.
//!
//! Datasheet: <https://www.ti.com/lit/ds/symlink/tmp451.pdf>

use crate::hw_trait::{HwError, i2c::I2c};

/// Expected manufacturer ID (register 0xFE).
pub const MANUFACTURER_ID: u8 = 0x55;

/// Status register BUSY bit: an ADC conversion is in progress.
pub const STATUS_BUSY: u8 = 0x80;

/// Remote-diode gain correction measured on the NerdQX BM1370 thermal diodes.
pub const NERDQX_SCALE: f32 = 1.09;
/// Remote-diode offset correction for the NerdQX, in Celsius.
pub const NERDQX_OFFSET: f32 = -29.5;

/// TMP451 register addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Register {
    /// Local temperature, high byte (integer, read-only)
    LocalTempHigh = 0x00,
    /// Remote temperature, high byte (integer, read-only)
    RemoteTempHigh = 0x01,
    /// Status (read-only)
    Status = 0x02,
    /// Configuration
    Config = 0x03,
    /// One-shot trigger (write-only; any data starts a conversion)
    OneShot = 0x0F,
    /// Remote temperature, low byte (fractional, read-only)
    RemoteTempLow = 0x10,
    /// Remote temperature offset, high byte (1 C/LSB, read/write)
    RemoteOffsetHigh = 0x11,
    /// Local temperature, low byte (fractional, read-only)
    LocalTempLow = 0x15,
    /// N-factor correction (two's complement, read/write)
    NFactorCorrection = 0x23,
    /// Manufacturer ID (read-only)
    MfgId = 0xFE,
}

/// Driver error.
#[derive(Debug, thiserror::Error)]
pub enum Error<E> {
    /// I2C bus error
    #[error("I2C: {0}")]
    I2c(E),

    /// Manufacturer ID register returned an unexpected value
    #[error("unexpected manufacturer ID: 0x{0:02X}")]
    UnexpectedMfgId(u8),

    /// Remote diode is open or faulted
    #[error("remote diode open")]
    RemoteDiodeOpen,
}

/// A raw temperature reading from the TMP451.
///
/// Wraps a 12-bit value assembled from the high byte (integer part)
/// and the upper 4 bits of the low byte (fractional part). In
/// standard binary mode, values below 0 C read as zero. Each LSB
/// represents 0.0625 C.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reading(i16);

impl Reading {
    const DEGREES_PER_LSB: f32 = 0.0625;

    /// Assemble from separate high and low register bytes.
    ///
    /// High byte is the integer part (unsigned in standard mode).
    /// Low byte upper 4 bits are the fractional part.
    pub fn from_high_low(high: u8, low: u8) -> Self {
        let raw = ((high as i16) << 4) | ((low >> 4) as i16);
        Self(raw)
    }

    /// Temperature in degrees Celsius.
    pub fn as_degrees_c(self) -> f32 {
        self.0 as f32 * Self::DEGREES_PER_LSB
    }
}

type Result<T> = std::result::Result<T, Error<HwError>>;

/// TMP451 driver, generic over I2C implementation.
pub struct Tmp451<I> {
    i2c: I,
    address: u8,
}

impl<I: I2c> Tmp451<I> {
    /// Create a new driver instance.
    pub fn new(i2c: I, address: u8) -> Self {
        Self { i2c, address }
    }

    /// Verify the manufacturer ID register.
    pub async fn init(&mut self) -> Result<()> {
        let id = self.read_byte(Register::MfgId).await?;
        if id != MANUFACTURER_ID {
            return Err(Error::UnexpectedMfgId(id));
        }
        Ok(())
    }

    /// Read the local (on-die) temperature.
    pub async fn read_local(&mut self) -> Result<Reading> {
        let high = self.read_byte(Register::LocalTempHigh).await?;
        let low = self.read_byte(Register::LocalTempLow).await?;
        Ok(Reading::from_high_low(high, low))
    }

    /// Read the remote (external diode) temperature.
    ///
    /// Returns `Error::RemoteDiodeOpen` if the diode is disconnected
    /// or faulted.
    pub async fn read_remote(&mut self) -> Result<Reading> {
        const REMOTE_OPEN: u8 = 0x04;
        let s = self.read_byte(Register::Status).await?;
        if s & REMOTE_OPEN != 0 {
            return Err(Error::RemoteDiodeOpen);
        }
        let high = self.read_byte(Register::RemoteTempHigh).await?;
        let low = self.read_byte(Register::RemoteTempLow).await?;
        Ok(Reading::from_high_low(high, low))
    }

    /// Read the remote temperature as `reading * scale + offset`, in Celsius.
    pub async fn read_remote_calibrated(&mut self, scale: f32, offset: f32) -> Result<f32> {
        Ok(self.read_remote().await?.as_degrees_c() * scale + offset)
    }

    /// Start a single conversion (effective in standby mode, ignored while
    /// the sensor free-runs).
    pub async fn trigger_one_shot(&mut self) -> Result<()> {
        self.write_byte(Register::OneShot, 0).await
    }

    /// Whether a conversion is in progress ([STATUS_BUSY]).
    pub async fn is_busy(&mut self) -> Result<bool> {
        Ok(self.read_byte(Register::Status).await? & STATUS_BUSY != 0)
    }

    /// Set the remote temperature offset (high byte, 1 C/LSB).
    ///
    /// The offset is added to every remote ADC conversion result.
    /// Use a negative value to reduce a reading that is too high.
    pub async fn set_remote_offset(&mut self, degrees: i8) -> Result<()> {
        self.write_byte(Register::RemoteOffsetHigh, degrees as u8)
            .await
    }

    async fn read_byte(&mut self, reg: Register) -> Result<u8> {
        let mut buf = [0u8; 1];
        self.i2c
            .write_read(self.address, &[reg as u8], &mut buf)
            .await
            .map_err(Error::I2c)?;
        Ok(buf[0])
    }

    async fn write_byte(&mut self, reg: Register, value: u8) -> Result<()> {
        self.i2c
            .write(self.address, &[reg as u8, value])
            .await
            .map_err(Error::I2c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reading_from_high_low() {
        // Datasheet Table 7-1 / 7-2 values
        let r = Reading::from_high_low(0x00, 0x00);
        assert!((r.as_degrees_c() - 0.0).abs() < 1e-4);

        // Fractional bits: 25 + one LSB
        let r = Reading::from_high_low(0x19, 0x10);
        assert!((r.as_degrees_c() - 25.0625).abs() < 1e-4);

        // Max: 127.9375 C
        let r = Reading::from_high_low(0x7F, 0xF0);
        assert!((r.as_degrees_c() - 127.9375).abs() < 1e-4);
    }

    #[test]
    fn low_byte_bottom_nibble_ignored() {
        // Lower 4 bits of low byte are unused
        let a = Reading::from_high_low(0x19, 0x80);
        let b = Reading::from_high_low(0x19, 0x8F);
        assert_eq!(a, b);
    }

    #[test]
    fn nerdqx_calibration_constants() {
        assert_eq!(Register::OneShot as u8, 0x0F);
        assert_eq!(STATUS_BUSY, 0x80);
        // Raw 60 C diode reading maps to ~35.9 C, a plausible die temperature.
        let t = 60.0 * NERDQX_SCALE + NERDQX_OFFSET;
        assert!((t - 35.9).abs() < 1e-3);
    }

    /// Register-file I2C mock that records writes.
    struct Mock {
        regs: [u8; 256],
        writes: Vec<Vec<u8>>,
    }

    #[async_trait::async_trait]
    impl I2c for Mock {
        async fn write(&mut self, _addr: u8, data: &[u8]) -> crate::hw_trait::Result<()> {
            self.writes.push(data.to_vec());
            Ok(())
        }
        async fn read(&mut self, _addr: u8, _buf: &mut [u8]) -> crate::hw_trait::Result<()> {
            Err(HwError::NotSupported("plain read".into()))
        }
        async fn write_read(
            &mut self,
            _addr: u8,
            write: &[u8],
            read: &mut [u8],
        ) -> crate::hw_trait::Result<()> {
            read[0] = self.regs[write[0] as usize];
            Ok(())
        }
        async fn set_frequency(&mut self, _hz: u32) -> crate::hw_trait::Result<()> {
            Ok(())
        }
    }

    fn mock() -> Tmp451<Mock> {
        Tmp451::new(
            Mock {
                regs: [0; 256],
                writes: Vec::new(),
            },
            0x4C,
        )
    }

    #[tokio::test]
    async fn one_shot_writes_the_trigger_register() {
        let mut t = mock();
        t.trigger_one_shot().await.unwrap();
        assert_eq!(t.i2c.writes, vec![vec![0x0F, 0x00]]);
    }

    #[tokio::test]
    async fn calibrated_remote_applies_scale_then_offset() {
        let mut t = mock();
        t.i2c.regs[0x01] = 60; // 60.0 C
        t.i2c.regs[0x10] = 0x80; // + 0.5 C
        let c = t.read_remote_calibrated(NERDQX_SCALE, NERDQX_OFFSET).await;
        assert!((c.unwrap() - (60.5 * 1.09 - 29.5)).abs() < 1e-3);
    }

    #[tokio::test]
    async fn busy_flag_follows_status_bit() {
        let mut t = mock();
        assert!(!t.is_busy().await.unwrap());
        t.i2c.regs[0x02] = STATUS_BUSY;
        assert!(t.is_busy().await.unwrap());
    }
}