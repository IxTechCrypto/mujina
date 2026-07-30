//! I2C implementation using bitaxe-raw control protocol.

use async_trait::async_trait;

use super::channel::ControlChannel;
use super::{I2CCommand, Packet, Page};
use crate::hw_trait::i2c::{I2c, I2cError};
use crate::hw_trait::{HwError, Result};

/// I2C bus implementation using bitaxe-raw control protocol.
#[derive(Clone)]
pub struct BitaxeRawI2c {
    channel: ControlChannel,
    mock: bool,
}

impl BitaxeRawI2c {
    /// Create a new I2C bus using the given control channel.
    pub fn new(channel: ControlChannel) -> Self {
        Self { channel, mock: false }
    }

    /// Set mock mode.
    pub fn set_mock(&mut self, mock: bool) {
        self.mock = mock;
    }
}

#[async_trait]
impl I2c for BitaxeRawI2c {
    async fn write(&mut self, addr: u8, data: &[u8]) -> Result<()> {
        if self.mock {
            return Ok(());
        }

        let packet = Packet::new(
            Page::I2C,
            I2CCommand::Write as u8,
            [vec![addr], data.to_vec()].concat(),
        );

        self.channel
            .send_packet(packet)
            .await
            .map_err(|e| HwError::I2c(I2cError::Other(format!("Write failed: {}", e))))?;

        Ok(())
    }

    async fn read(&mut self, addr: u8, buffer: &mut [u8]) -> Result<()> {
        if self.mock {
            buffer.fill(0);
            return Ok(());
        }

        let packet = Packet::new(
            Page::I2C,
            I2CCommand::Read as u8,
            vec![addr, buffer.len() as u8],
        );

        let response = self
            .channel
            .send_packet(packet)
            .await
            .map_err(|e| HwError::I2c(I2cError::Other(format!("Read failed: {}", e))))?;

        if response.data.len() != buffer.len() {
            return Err(HwError::I2c(I2cError::Other(format!(
                "Expected {} bytes, got {}",
                buffer.len(),
                response.data.len()
            ))));
        }

        buffer.copy_from_slice(&response.data);
        Ok(())
    }

    async fn write_read(&mut self, addr: u8, write: &[u8], read: &mut [u8]) -> Result<()> {
        println!("DEBUG: write_read: addr = {:02x}, write = {:?}, mock = {}", addr, write, self.mock);
        if self.mock {
            read.fill(0);
            if addr == 0x4c { // EMC2101
                if write.len() == 1 {
                    match write[0] {
                        0xFE => { if read.len() >= 1 { read[0] = 0x5D; } } // MFG_ID
                        0xFD => { if read.len() >= 1 { read[0] = 0x28; } } // PRODUCT_ID
                        0xFF => { if read.len() >= 1 { read[0] = 0x01; } } // REVISION
                        0x03 => { if read.len() >= 1 { read[0] = 0x00; } } // CONFIG
                        0x01 => { if read.len() >= 1 { read[0] = 45; } }   // EXT_TEMP_HIGH
                        0x10 => { if read.len() >= 1 { read[0] = 0; } }    // EXT_TEMP_LOW
                        0x4C => { if read.len() >= 1 { read[0] = 63; } }   // FAN_SETTING (100%)
                        0x47 => { if read.len() >= 1 { read[0] = 0x0f; } } // TACH_HIGH
                        0x46 => { if read.len() >= 1 { read[0] = 0xff; } } // TACH_LOW
                        _ => {}
                    }
                }
            } else if addr == 0x1b { // TPS546
                if write.len() == 1 {
                    match write[0] {
                        0x99 => { // MfrId
                            let id = &[0x54, 0x49, 0x54, 0x6B, 0x24, 0x41];
                            let len = read.len().min(id.len());
                            read[..len].copy_from_slice(&id[..len]);
                        }
                        0x20 => { if read.len() >= 1 { read[0] = 0x97; } } // VOUT_MODE (Linear -9)
                        0x79 => { if read.len() >= 2 { read[0] = 0; read[1] = 0; } } // STATUS_WORD
                        0x88 => { // READ_VIN (Linear11: 12.0V)
                            if read.len() >= 2 { read[0] = 0x0c; read[1] = 0x00; }
                        }
                        0x8B => { // READ_VOUT (Linear16: 1.2V with exp -9)
                            if read.len() >= 2 { read[0] = 0x66; read[1] = 0x02; }
                        }
                        0x8C => { // READ_IOUT (Linear11: 5.0A)
                            if read.len() >= 2 { read[0] = 0x05; read[1] = 0x00; }
                        }
                        0x96 => { // READ_POUT (Linear11: 6.0W)
                            if read.len() >= 2 { read[0] = 0x06; read[1] = 0x00; }
                        }
                        0x8D => { // READ_TEMPERATURE_1 (Linear11: 40C)
                            if read.len() >= 2 { read[0] = 0x28; read[1] = 0x00; }
                        }
                        _ => {}
                    }
                }
            }
            return Ok(());
        }

        let mut data = vec![addr];
        data.extend_from_slice(write);
        data.push(read.len() as u8);

        let packet = Packet::new(Page::I2C, I2CCommand::WriteRead as u8, data);

        let response = self
            .channel
            .send_packet(packet)
            .await
            .map_err(|e| HwError::I2c(I2cError::Other(format!("WriteRead failed: {}", e))))?;

        if response.data.len() != read.len() {
            return Err(HwError::I2c(I2cError::Other(format!(
                "Expected {} bytes, got {}",
                read.len(),
                response.data.len()
            ))));
        }

        read.copy_from_slice(&response.data);
        Ok(())
    }

    async fn set_frequency(&mut self, hz: u32) -> Result<()> {
        if self.mock {
            return Ok(());
        }

        let packet = Packet::new(
            Page::I2C,
            I2CCommand::SetFrequency as u8,
            hz.to_le_bytes().to_vec(),
        );

        self.channel
            .send_packet(packet)
            .await
            .map_err(|e| HwError::I2c(I2cError::Other(format!("SetFrequency failed: {}", e))))?;

        Ok(())
    }
}
