//! BM1370 4-chip ASIC chain UART1 driver for NerdQX Rev 1.0 (ESP32-S3R8).
//!
//! Protocol helpers (CRC5, CRC16, packet builders, nonce parsing) are pure
//! `no_std` functions for host unit testing. The hardware `Bm1370Chain` driver
//! is gated behind `#[cfg(feature = "esp32s3")]`.

/// Number of BM1370 ASICs in the NerdQX Rev 1.0 chain.
pub const NUM_CHIPS: u8 = 4;

/// Initial UART baud rate after ASIC hardware reset.
pub const BAUD_INIT: u32 = 115_200;

/// Operational UART baud rate after chain initialization.
pub const BAUD_OPERATIONAL: u32 = 1_000_000;

/// Address spacing between chips in a 4-chip BM1370 chain (`256 / 4 = 0x40`).
pub const CHIP_ADDR_INTERVAL: u8 = 0x40;

/// BM1370 command/work frame preamble.
pub const PREAMBLE: [u8; 2] = [0x55, 0xAA];

/// BM1370 response frame preamble.
pub const RESP_PREAMBLE: [u8; 2] = [0xAA, 0x55];

// Packet type/command bits.
const TYPE_JOB: u8 = 0x21;
const TYPE_CMD: u8 = 0x40;
const CMD_ALL: u8 = 0x10;
const CMD_SET_ADDR: u8 = 0x00;
const CMD_WRITE_REG: u8 = 0x01;
const CMD_READ_REG: u8 = 0x02;
const CMD_CHAIN_INACTIVE: u8 = 0x03;

// Key BM1370 registers.
pub const REG_CHIP_ID: u8 = 0x00;
pub const REG_PLL0_PARAM: u8 = 0x08;
pub const REG_CHIP_NONCE_OFFSET: u8 = 0x0C;
pub const REG_HASH_COUNTING_NUMBER: u8 = 0x10;
pub const REG_TICKET_MASK: u8 = 0x14;
pub const REG_MISC_CONTROL: u8 = 0x18;
pub const REG_FAST_UART_CFG: u8 = 0x28;
pub const REG_CORE_REG_CTRL: u8 = 0x3C;
pub const REG_ANALOG_MUX: u8 = 0x54;
pub const REG_IO_DRIVER_STRENGTH: u8 = 0x58;
pub const REG_VERSION_ROLLING: u8 = 0xA4;
pub const REG_INIT_CONTROL: u8 = 0xA8;
pub const REG_ADC_CTRL1: u8 = 0xB9;

/// NerdQX HashCountingNumber for 35 kHz VR clock (`round(196_608_000 / 35_000) == 0x15F1`).
pub const REG_HASH_COUNTING_NERDQX: u32 = 0x0000_15F1;

/// Decoded nonce hit returned by a BM1370 chip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NonceHit {
    pub nonce: u32,
    pub job_id: u8,
    pub version_bits: u16,
    pub chip_index: u8,
}

/// Map a 32-bit nonce to its owning chip index (`0..4`) in a 4-chip chain.
#[inline]
pub const fn chip_index_for_nonce(nonce: u32) -> u8 {
    ((nonce as u64 * 4) >> 32) as u8
}

/// Sliding window duration (seconds) for per-chip and total hashrate estimation.
pub const HR_WINDOW_SECS: usize = 120;

/// Hardware ticket-mask difficulty (`REG_TICKET_MASK = 0x0000_00FF` -> diff 256).
pub const ASIC_TICKET_DIFF: f32 = 256.0;

/// Ring-buffered 120-second sliding-window hashrate & error-rate estimator across 4 BM1370 chips.
#[derive(Debug, Clone)]
pub struct ChainHashrateWindow {
    hit_buckets: [[u16; HR_WINDOW_SECS]; 4],
    err_buckets: [[u16; HR_WINDOW_SECS]; 4],
    cur_hits: [u16; 4],
    cur_errs: [u16; 4],
    cursor: usize,
    filled: usize,
    pub chip_ghs: [f32; 4],
    pub chip_err_pct: [f32; 4],
    pub board_err_pct: f32,
}

impl Default for ChainHashrateWindow {
    fn default() -> Self {
        Self::new()
    }
}

impl ChainHashrateWindow {
    pub fn new() -> Self {
        Self {
            hit_buckets: [[0; HR_WINDOW_SECS]; 4],
            err_buckets: [[0; HR_WINDOW_SECS]; 4],
            cur_hits: [0; 4],
            cur_errs: [0; 4],
            cursor: 0,
            filled: 0,
            chip_ghs: [0.0; 4],
            chip_err_pct: [0.0; 4],
            board_err_pct: 0.0,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    #[inline]
    pub fn record_valid(&mut self, chip_idx: usize) {
        let c = chip_idx & 3;
        self.cur_hits[c] = self.cur_hits[c].saturating_add(1);
    }

    #[inline]
    pub fn record_error(&mut self, chip_idx: usize) {
        let c = chip_idx & 3;
        self.cur_errs[c] = self.cur_errs[c].saturating_add(1);
    }

    /// Commit the current 1-second slice into the 120-second ring and recompute averages.
    pub fn commit_second(&mut self) {
        for c in 0..4 {
            self.hit_buckets[c][self.cursor] = self.cur_hits[c];
            self.err_buckets[c][self.cursor] = self.cur_errs[c];
            self.cur_hits[c] = 0;
            self.cur_errs[c] = 0;
        }
        self.cursor = (self.cursor + 1) % HR_WINDOW_SECS;
        if self.filled < HR_WINDOW_SECS {
            self.filled += 1;
        }
        // Avoid inflated 1-second startup spikes before >= 4s of samples exist.
        let secs = self.filled.max(4) as f32;
        let mut total_valid: u32 = 0;
        let mut total_err: u32 = 0;
        for c in 0..4 {
            let valid_sum: u32 = self.hit_buckets[c][..self.filled].iter().map(|&x| x as u32).sum();
            let err_sum: u32 = self.err_buckets[c][..self.filled].iter().map(|&x| x as u32).sum();
            total_valid += valid_sum;
            total_err += err_sum;
            self.chip_ghs[c] = (valid_sum as f32) * ASIC_TICKET_DIFF * 4.294_967_3 / secs;
            let denom = valid_sum + err_sum;
            self.chip_err_pct[c] = if denom > 0 {
                (err_sum as f32) * 100.0 / (denom as f32)
            } else {
                0.0
            };
        }
        let all_denom = total_valid + total_err;
        self.board_err_pct = if all_denom > 0 {
            (total_err as f32) * 100.0 / (all_denom as f32)
        } else {
            0.0
        };
    }

    pub fn total_ghs(&self) -> f32 {
        self.chip_ghs.iter().sum()
    }
}

/// Bitmain CRC-5 (polynomial `x^5 + x^2 + 1`, init `0x1F`) over `bit_len` MSB-first bits.
pub fn crc5_bits(data: &[u8], bit_len: usize) -> u8 {
    let mut crc = 0x1Fu8;
    for bit_idx in 0..bit_len {
        let byte = data.get(bit_idx / 8).copied().unwrap_or(0);
        let din = (byte >> (7 - (bit_idx % 8))) & 1;
        let msb = (crc >> 4) & 1;
        let fb = din ^ msb;
        crc = ((crc << 1) & 0x1F) ^ if fb != 0 { 0x05 } else { 0x00 };
    }
    crc & 0x1F
}

/// Convenience CRC-5 over all bytes of `data`.
pub fn crc5(data: &[u8]) -> u8 {
    crc5_bits(data, data.len() * 8)
}

/// CRC-16/CCITT-FALSE (polynomial `0x1021`, init `0xFFFF`) used for BM1370 work packets.
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            if (crc & 0x8000) != 0 {
                crc = (crc << 1) ^ 0x1021;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

/// Build an 11-byte BM1370 register write command (`0x55 0xAA ...`).
pub fn build_write_reg(all: bool, chip_addr: u8, reg: u8, val: u32) -> [u8; 11] {
    let mut pkt = [0u8; 11];
    pkt[0] = PREAMBLE[0];
    pkt[1] = PREAMBLE[1];
    pkt[2] = TYPE_CMD | CMD_WRITE_REG | if all { CMD_ALL } else { 0 };
    pkt[3] = 9;
    pkt[4] = if all { 0 } else { chip_addr };
    pkt[5] = reg;
    pkt[6..10].copy_from_slice(&val.to_be_bytes());
    pkt[10] = crc5(&pkt[2..10]);
    pkt
}

/// Build a 7-byte BM1370 register read command (`0x55 0xAA ...`).
pub fn build_read_reg(all: bool, chip_addr: u8, reg: u8) -> [u8; 7] {
    let mut pkt = [0u8; 7];
    pkt[0] = PREAMBLE[0];
    pkt[1] = PREAMBLE[1];
    pkt[2] = TYPE_CMD | CMD_READ_REG | if all { CMD_ALL } else { 0 };
    pkt[3] = 5;
    pkt[4] = if all { 0 } else { chip_addr };
    pkt[5] = reg;
    pkt[6] = crc5(&pkt[2..6]);
    pkt
}

/// Build a 7-byte BM1370 set-chip-address command.
pub fn build_set_addr(chip_addr: u8) -> [u8; 7] {
    let mut pkt = [0u8; 7];
    pkt[0] = PREAMBLE[0];
    pkt[1] = PREAMBLE[1];
    pkt[2] = TYPE_CMD | CMD_SET_ADDR;
    pkt[3] = 5;
    pkt[4] = chip_addr;
    pkt[5] = 0;
    pkt[6] = crc5(&pkt[2..6]);
    pkt
}

/// Build a 7-byte BM1370 chain-inactive broadcast command.
pub fn build_chain_inactive() -> [u8; 7] {
    let mut pkt = [0u8; 7];
    pkt[0] = PREAMBLE[0];
    pkt[1] = PREAMBLE[1];
    pkt[2] = TYPE_CMD | CMD_ALL | CMD_CHAIN_INACTIVE;
    pkt[3] = 5;
    pkt[4] = 0;
    pkt[5] = 0;
    pkt[6] = crc5(&pkt[2..6]);
    pkt
}

/// Build an 88-byte BM1370 work packet (`0x55 0xAA 0x21 ... CRC16`).
pub fn build_work_packet(
    job_id: u8,
    version: u32,
    prev_hash: &[u8; 32],
    merkle_root: &[u8; 32],
    ntime: u32,
    nbits: u32,
) -> [u8; 88] {
    let mut pkt = [0u8; 88];
    pkt[0] = PREAMBLE[0];
    pkt[1] = PREAMBLE[1];
    pkt[2] = TYPE_JOB;
    pkt[3] = 86; // payload length excluding 2-byte preamble
    pkt[4] = job_id;
    pkt[5] = 1; // num_midstates = 1 (version rolling enabled)
    pkt[6..10].copy_from_slice(&0u32.to_le_bytes()); // starting nonce
    pkt[10..14].copy_from_slice(&nbits.to_le_bytes());
    pkt[14..18].copy_from_slice(&ntime.to_le_bytes());
    pkt[18..50].copy_from_slice(merkle_root);
    pkt[50..82].copy_from_slice(prev_hash);
    pkt[82..86].copy_from_slice(&version.to_le_bytes());
    let crc = crc16(&pkt[2..86]);
    pkt[86..88].copy_from_slice(&crc.to_be_bytes());
    pkt
}

/// Parse an 11-byte BM1370 `ChipId` (`0x00`) read-register response frame.
/// Returns `Some(chip_address)` when preamble, ReadRegister frame type (`0`),
/// CRC-5 checksum, and `[0x13, 0x70]` silicon ID all validate.
pub fn parse_chip_id_frame(frame: &[u8; 11]) -> Option<u8> {
    if frame[0] != RESP_PREAMBLE[0] || frame[1] != RESP_PREAMBLE[1] {
        return None;
    }
    if (frame[10] & 0xE0) != 0 || crc5(&frame[2..11]) != 0 {
        return None;
    }
    if frame[2] != 0x13 || frame[3] != 0x70 {
        return None;
    }
    Some(frame[6])
}

/// Advance a shared 11-byte `0xAA 0x55` response-frame accumulator by one byte.
#[cfg(feature = "esp32s3")]
fn feed_rx_byte(rx_buf: &mut [u8; 11], rx_len: &mut usize, b: u8) -> Option<[u8; 11]> {
    match *rx_len {
        0 => {
            if b == RESP_PREAMBLE[0] {
                rx_buf[0] = b;
                *rx_len = 1;
            }
            None
        }
        1 => {
            if b == RESP_PREAMBLE[1] {
                rx_buf[1] = b;
                *rx_len = 2;
            } else if b == RESP_PREAMBLE[0] {
                rx_buf[0] = b;
                *rx_len = 1;
            } else {
                *rx_len = 0;
            }
            None
        }
        n => {
            rx_buf[n] = b;
            *rx_len += 1;
            if *rx_len == 11 {
                *rx_len = 0;
                Some(*rx_buf)
            } else {
                None
            }
        }
    }
}

/// Parse an 11-byte BM1370 nonce response frame (`0xAA 0x55 ...`).
pub fn parse_nonce_frame(frame: &[u8; 11]) -> Option<NonceHit> {
    if frame[0] != RESP_PREAMBLE[0] || frame[1] != RESP_PREAMBLE[1] {
        return None;
    }
    if (frame[10] & 0x80) == 0 {
        return None;
    }
    let nonce = u32::from_le_bytes([frame[2], frame[3], frame[4], frame[5]]);
    let job_id = frame[7];
    let version_bits = u16::from_be_bytes([frame[8], frame[9]]);
    Some(NonceHit {
        nonce,
        job_id,
        version_bits,
        chip_index: chip_index_for_nonce(nonce),
    })
}

/// Compute a BM1370 PLL0 register word for `freq_mhz` (25 MHz reference crystal),
/// keeping `fb_div` within `[160, 239]` and VCO within `[1600, 3200]` MHz.
pub fn pll_reg_for_freq(freq_mhz: u16) -> u32 {
    let target = freq_mhz.clamp(56, 900) as f32;
    let mut best_word: Option<u32> = None;
    let mut min_error = f32::MAX;
    let mut best_vco = f32::MAX;

    for ref_div in [2u8, 1u8] {
        for post_div1 in (1u8..=7).rev() {
            for post_div2 in (1u8..=post_div1).rev() {
                let fb_f =
                    (post_div1 * post_div2) as f32 * target * ref_div as f32 / 25.0;
                let fb_div = fb_f.round() as u32;
                if !(0xA0..=0xEF).contains(&fb_div) {
                    continue;
                }
                let actual =
                    25.0 * fb_div as f32 / (ref_div as f32 * post_div1 as f32 * post_div2 as f32);
                let error = (target - actual).abs();
                let vco = 25.0 * fb_div as f32 / ref_div as f32;
                if !(1600.0..=3200.0).contains(&vco) {
                    continue;
                }
                if error < 2.5 && (error < min_error || (error == min_error && vco < best_vco)) {
                    min_error = error;
                    best_vco = vco;
                    let vco_flag = if vco >= 2400.0 { 0x5000_0000 } else { 0x4000_0000 };
                    let post_div = (((post_div1 - 1) as u32) << 4) | ((post_div2 - 1) as u32);
                    best_word = Some(
                        vco_flag | (fb_div << 16) | ((ref_div as u32) << 8) | post_div,
                    );
                }
            }
        }
    }

    best_word.unwrap_or(0x40A2_0255) // safe 56.25 MHz fallback (fb=162, ref=2, pd=6x6, VCO=2025MHz)
}

#[cfg(feature = "esp32s3")]
mod esp32s3_driver {
    use super::*;
    use crate::hw_pins::{ASIC_RST, UART1_RX, UART1_TX};
    use esp_idf_svc::hal::delay::FreeRtos;
    use esp_idf_svc::hal::gpio::{AnyIOPin, AnyOutputPin, Output, PinDriver};
    use esp_idf_svc::hal::uart::{config::Config, UartDriver, UART1};
    use esp_idf_svc::hal::units::Hertz;
    use esp_idf_svc::sys::EspError;

    /// ESP-IDF UART1 driver for the 4-chip BM1370 chain on NerdQX Rev 1.0.
    ///
    /// Expected NerdQX Rev 1.0 pins from `crate::hw_pins`:
    /// - `UART1_TX` = GPIO17
    /// - `UART1_RX` = GPIO18
    /// - `ASIC_RST` = GPIO1
    pub struct Bm1370Chain<'d> {
        uart: UartDriver<'d>,
        asic_rst: PinDriver<'d, AnyOutputPin, Output>,
        rx_buf: [u8; 11],
        rx_len: usize,
    }

    impl<'d> Bm1370Chain<'d> {
        /// Create a new BM1370 chain driver on UART1 at `BAUD_INIT` (115200 baud).
        pub fn new(
            uart1: UART1,
            tx: AnyIOPin,
            rx: AnyIOPin,
            rst: AnyOutputPin,
        ) -> Result<Self, EspError> {
            debug_assert_eq!(UART1_TX, 17);
            debug_assert_eq!(UART1_RX, 18);
            debug_assert_eq!(ASIC_RST, 1);

            let cfg = Config::new().baudrate(Hertz(BAUD_INIT));
            let uart = UartDriver::new(
                uart1,
                tx,
                rx,
                Option::<AnyIOPin>::None,
                Option::<AnyIOPin>::None,
                &cfg,
            )?;
            let asic_rst = PinDriver::output(rst)?;
            Ok(Self {
                uart,
                asic_rst,
                rx_buf: [0u8; 11],
                rx_len: 0,
            })
        }

        /// Assert `ASIC_RST` low to halt hashing on safety cutoff.
        pub fn hold_reset_low(&mut self) {
            let _ = self.asic_rst.set_low();
        }

        fn drain_rx(&mut self) {
            let mut dump = [0u8; 64];
            while let Ok(n) = self.uart.read(&mut dump, 0) {
                if n == 0 {
                    break;
                }
            }
            self.rx_len = 0;
        }

        fn count_chip_id_responses(&mut self, wait_ms: u32) -> u8 {
            let mut count = 0u8;
            let mut byte = [0u8; 1];
            let slices = wait_ms.div_ceil(5);
            for _ in 0..slices {
                loop {
                    match self.uart.read(&mut byte, 0) {
                        Ok(1) => {
                            if let Some(frame) =
                                feed_rx_byte(&mut self.rx_buf, &mut self.rx_len, byte[0])
                            {
                                if parse_chip_id_frame(&frame).is_some() {
                                    count = count.saturating_add(1);
                                }
                            }
                        }
                        _ => break,
                    }
                }
                FreeRtos::delay_ms(5);
            }
            count
        }

        /// Hardware-reset the 4-chip BM1370 chain, enumerate addresses and verify
        /// `ChipId` readbacks, enable hashing cores, switch to 1_000_000 baud, ramp
        /// the PLL to `freq_mhz`, and return the post-init detected chip count.
        pub fn reset_and_init(&mut self, freq_mhz: u16) -> Result<u8, EspError> {
            // Reset at 115200 baud.
            self.uart.change_baudrate(Hertz(BAUD_INIT))?;
            self.asic_rst.set_low()?;
            FreeRtos::delay_ms(100);
            self.asic_rst.set_high()?;
            FreeRtos::delay_ms(100);
            self.drain_rx();

            // Enable full version rolling (11-byte response format) before enumeration,
            // matching mujina-miner/src/asic/bm13xx/thread.rs.
            for _ in 0..3 {
                self.write_all(&build_write_reg(true, 0, REG_VERSION_ROLLING, 0x9000_FFFF))?;
                FreeRtos::delay_ms(5);
            }
            FreeRtos::delay_ms(10);

            // Enumerate pre-config chips via broadcast ChipId (0x00) read.
            self.write_all(&build_read_reg(true, 0, REG_CHIP_ID))?;
            let pre_count = self.count_chip_id_responses(80);
            log::info!("BM1370 pre-init ChipId broadcast replies: {pre_count}/{NUM_CHIPS}");

            // Pre-configuration broadcast registers (BM1370 defaults + reporting enabled).
            self.write_all(&build_write_reg(true, 0, REG_INIT_CONTROL, 0x0007_0000))?;
            self.write_all(&build_write_reg(true, 0, REG_MISC_CONTROL, 0xF000_C100))?;
            FreeRtos::delay_ms(10);

            // Put chain inactive and assign addresses (interval 2: 0x00, 0x02, 0x04, 0x06).
            self.write_all(&build_chain_inactive())?;
            FreeRtos::delay_ms(5);
            for chip in 0..NUM_CHIPS {
                self.write_all(&build_set_addr(chip * 2))?;
                FreeRtos::delay_ms(2);
            }

            // Broadcast core clock select/delay, ticket mask (diff 256), and IO drive strength.
            self.write_all(&build_write_reg(true, 0, REG_CORE_REG_CTRL, 0x8000_8B00))?;
            self.write_all(&build_write_reg(true, 0, REG_CORE_REG_CTRL, 0x8000_800C))?;
            self.write_all(&build_write_reg(true, 0, REG_TICKET_MASK, 0x0000_00FF))?;
            self.write_all(&build_write_reg(true, 0, REG_IO_DRIVER_STRENGTH, 0x0001_1111))?;

            // Per-chip core reset (0x0007_01F0 from Bitaxe/S21 Pro capture), misc reporting,
            // clock select/delay, and core array enable.
            for chip in 0..NUM_CHIPS {
                let addr = chip * 2;
                self.write_all(&build_write_reg(false, addr, REG_INIT_CONTROL, 0x0007_01F0))?;
                self.write_all(&build_write_reg(false, addr, REG_MISC_CONTROL, 0xF000_C100))?;
                self.write_all(&build_write_reg(false, addr, REG_CORE_REG_CTRL, 0x8000_8B00))?;
                self.write_all(&build_write_reg(false, addr, REG_CORE_REG_CTRL, 0x8000_800C))?;
                self.write_all(&build_write_reg(false, addr, REG_CORE_REG_CTRL, 0x8000_82AA))?;
                FreeRtos::delay_ms(2);
            }

            // ADC / analog mux / nonce bin overflow setup before baud switch.
            self.write_all(&build_write_reg(true, 0, REG_ADC_CTRL1, 0x0000_4480))?;
            self.write_all(&build_write_reg(true, 0, REG_ANALOG_MUX, 0x0000_0002))?;
            self.write_all(&build_write_reg(true, 0, REG_ADC_CTRL1, 0x0000_4480))?;
            self.write_all(&build_write_reg(true, 0, REG_CORE_REG_CTRL, 0x8000_8DEE))?;
            FreeRtos::delay_ms(5);

            // Switch ASIC UART divider to 1 Mbps, then update ESP32 UART1 baud rate.
            self.write_all(&build_write_reg(true, 0, REG_FAST_UART_CFG, 0x1130_0200))?;
            FreeRtos::delay_ms(10);
            self.uart.change_baudrate(Hertz(BAUD_OPERATIONAL))?;
            FreeRtos::delay_ms(10);

            // Ramp PLL frequency smoothly in 6.25 MHz steps to avoid TPS53647 voltage droop.
            let target_f = freq_mhz.clamp(60, 800) as f32;
            let steps = crate::chip_tuner::ramp_len(target_f);
            for idx in 0..steps {
                let step_mhz = crate::chip_tuner::ramp_freq(idx, target_f).round() as u16;
                self.write_all(&build_write_reg(
                    true,
                    0,
                    REG_PLL0_PARAM,
                    pll_reg_for_freq(step_mhz),
                ))?;
                FreeRtos::delay_ms(15);
            }
            FreeRtos::delay_ms(20);

            // Program NerdQX nonce sweep deadline (0x15F1 for 35 kHz VR clock) and full version rolling.
            self.write_all(&build_write_reg(
                true,
                0,
                REG_HASH_COUNTING_NUMBER,
                REG_HASH_COUNTING_NERDQX,
            ))?;
            self.write_all(&build_write_reg(true, 0, REG_VERSION_ROLLING, 0x9000_FFFF))?;
            FreeRtos::delay_ms(10);

            // Verify operational chain at 1 Mbps by reading ChipId back from all 4 chips.
            self.drain_rx();
            self.write_all(&build_read_reg(true, 0, REG_CHIP_ID))?;
            let post_count = self.count_chip_id_responses(80);
            if post_count != NUM_CHIPS {
                log::error!(
                    "BM1370 initialization check FAILED: {post_count}/{NUM_CHIPS} chips answered at 1 Mbps (pre-init={pre_count})"
                );
            }
            Ok(post_count)
        }

        /// Ramp broadcast `REG_PLL0_PARAM` in 6.25 MHz steps from `from_mhz` to `to_mhz`.
        pub fn set_chain_frequency(&mut self, from_mhz: f32, to_mhz: f32) -> Result<(), EspError> {
            let from = from_mhz.clamp(56.25, 900.0);
            let to = to_mhz.clamp(56.25, 900.0);
            let step = crate::chip_tuner::PLL_STEP_MHZ;
            let mut cur = from;
            if to >= from {
                while cur + step < to {
                    cur += step;
                    self.write_all(&build_write_reg(
                        true,
                        0,
                        REG_PLL0_PARAM,
                        pll_reg_for_freq(cur.round() as u16),
                    ))?;
                    FreeRtos::delay_ms(15);
                }
            } else {
                while cur - step > to {
                    cur -= step;
                    self.write_all(&build_write_reg(
                        true,
                        0,
                        REG_PLL0_PARAM,
                        pll_reg_for_freq(cur.round() as u16),
                    ))?;
                    FreeRtos::delay_ms(15);
                }
            }
            self.write_all(&build_write_reg(
                true,
                0,
                REG_PLL0_PARAM,
                pll_reg_for_freq(to.round() as u16),
            ))?;
            FreeRtos::delay_ms(15);
            Ok(())
        }

        /// Command an individual BM1370 chip's `REG_PLL0_PARAM` (`chip_idx` in `0..4`).
        pub fn set_chip_frequency(&mut self, chip_idx: u8, freq_mhz: f32) -> Result<(), EspError> {
            let chip_addr = (chip_idx & 3) * 2;
            let target = (freq_mhz.round() as i32).clamp(56, 900) as u16;
            self.write_all(&build_write_reg(
                false,
                chip_addr,
                REG_PLL0_PARAM,
                pll_reg_for_freq(target),
            ))
        }

        /// Send a mining work packet to the 4-chip BM1370 chain.
        pub fn send_work(
            &mut self,
            job_id: u8,
            version: u32,
            prev_hash: &[u8; 32],
            merkle_root: &[u8; 32],
            ntime: u32,
            nbits: u32,
        ) -> Result<(), EspError> {
            let pkt = build_work_packet(job_id, version, prev_hash, merkle_root, ntime, nbits);
            self.write_all(&pkt)
        }

        /// Non-blocking poll for a decoded `NonceHit` from UART1 RX.
        pub fn poll_nonce(&mut self) -> Option<NonceHit> {
            let mut byte = [0u8; 1];
            loop {
                match self.uart.read(&mut byte, 0) {
                    Ok(1) => {
                        if let Some(frame) =
                            feed_rx_byte(&mut self.rx_buf, &mut self.rx_len, byte[0])
                        {
                            if let Some(hit) = parse_nonce_frame(&frame) {
                                return Some(hit);
                            }
                        }
                    }
                    _ => return None,
                }
            }
        }

        fn write_all(&mut self, data: &[u8]) -> Result<(), EspError> {
            let mut offset = 0;
            while offset < data.len() {
                let n = self.uart.write(&data[offset..])?;
                if n == 0 {
                    break;
                }
                offset += n;
            }
            Ok(())
        }
    }
}

#[cfg(feature = "esp32s3")]
pub use esp32s3_driver::Bm1370Chain;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc5_known_vectors() {
        // Empty input yields init 0x1F.
        assert_eq!(crc5(&[]), 0x1F);
        // Deterministic 5-bit output range.
        let c = crc5(&[0x51, 0x09, 0x00, 0x08, 0x40, 0xA0, 0x02, 0x00]);
        assert!(c < 32);
        // Flipping any bit changes CRC5.
        let c2 = crc5(&[0x51, 0x09, 0x00, 0x08, 0x40, 0xA0, 0x02, 0x01]);
        assert_ne!(c, c2);
    }

    #[test]
    fn crc16_ccitt_false_standard_vector() {
        // Standard CRC-16/CCITT-FALSE check value for "123456789" is 0x29B1.
        assert_eq!(crc16(b"123456789"), 0x29B1);
    }

    #[test]
    fn chip_index_for_nonce_quarters() {
        assert_eq!(chip_index_for_nonce(0x0000_0000), 0);
        assert_eq!(chip_index_for_nonce(0x3FFF_FFFF), 0);
        assert_eq!(chip_index_for_nonce(0x4000_0000), 1);
        assert_eq!(chip_index_for_nonce(0x7FFF_FFFF), 1);
        assert_eq!(chip_index_for_nonce(0x8000_0000), 2);
        assert_eq!(chip_index_for_nonce(0xBFFF_FFFF), 2);
        assert_eq!(chip_index_for_nonce(0xC000_0000), 3);
        assert_eq!(chip_index_for_nonce(0xFFFF_FFFF), 3);
    }

    #[test]
    fn packet_builders_and_nonce_parser() {
        let wr = build_write_reg(true, 0, REG_PLL0_PARAM, 0x40A0_0200);
        assert_eq!(&wr[0..2], &PREAMBLE);
        assert_eq!(wr[2], 0x51);
        assert_eq!(wr[3], 9);
        assert_eq!(wr[5], REG_PLL0_PARAM);
        assert!(wr[10] < 32);

        let rd = build_read_reg(false, 0x40, REG_MISC_CONTROL);
        assert_eq!(&rd[0..2], &PREAMBLE);
        assert_eq!(rd[2], 0x42);
        assert_eq!(rd[3], 5);
        assert_eq!(rd[4], 0x40);

        // Verified hardware captures from mujina-miner/src/asic/bm13xx/crc.rs
        assert_eq!(
            build_read_reg(true, 0, REG_CHIP_ID),
            [0x55, 0xAA, 0x52, 0x05, 0x00, 0x00, 0x0A]
        );
        assert_eq!(
            build_set_addr(0x04),
            [0x55, 0xAA, 0x40, 0x05, 0x04, 0x00, 0x03]
        );
        assert_eq!(
            build_chain_inactive(),
            [0x55, 0xAA, 0x53, 0x05, 0x00, 0x00, 0x03]
        );
        assert_eq!(
            build_write_reg(true, 0, REG_VERSION_ROLLING, 0x9000_FFFF),
            [0x55, 0xAA, 0x51, 0x09, 0x00, 0xA4, 0x90, 0x00, 0xFF, 0xFF, 0x1C]
        );
        assert_eq!(
            build_write_reg(false, 0x00, REG_INIT_CONTROL, 0x0007_01F0),
            [0x55, 0xAA, 0x41, 0x09, 0x00, 0xA8, 0x00, 0x07, 0x01, 0xF0, 0x15]
        );

        let mut resp_crc = [0xAA, 0x55, 0x13, 0x70, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00];
        resp_crc[10] = crc5_bits(&resp_crc[2..10], 64 + 3);
        assert_eq!(parse_chip_id_frame(&resp_crc), Some(0x02));
        let cap_resp = [0xAA, 0x55, 0x13, 0x70, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10];
        assert_eq!(parse_chip_id_frame(&cap_resp), Some(0x00));
        // Nonce frames must NOT parse as ChipId frames
        assert_eq!(
            parse_chip_id_frame(&[0xAA, 0x55, 0x13, 0x70, 0, 0, 0, 0, 0, 0, 0x80]),
            None
        );

        let work = build_work_packet(7, 0x2000_0000, &[0x11; 32], &[0x22; 32], 1234, 5678);
        assert_eq!(&work[0..2], &PREAMBLE);
        assert_eq!(work[2], TYPE_JOB);
        assert_eq!(work[4], 7);
        let expected_crc = crc16(&work[2..86]).to_be_bytes();
        assert_eq!(&work[86..88], &expected_crc);

        let nonce = 0x8000_0042u32;
        let nb = nonce.to_le_bytes();
        let frame = [0xAA, 0x55, nb[0], nb[1], nb[2], nb[3], 0x00, 0x07, 0x12, 0x34, 0x80];
        let hit = parse_nonce_frame(&frame).expect("valid nonce frame");
        assert_eq!(
            hit,
            NonceHit {
                nonce,
                job_id: 7,
                version_bits: 0x1234,
                chip_index: 2,
            }
        );
    }

    #[test]
    fn pll_reg_for_freq_keeps_bm1370_vco_in_range() {
        for freq in [60u16, 200, 400, 495, 525, 777, 825, 875] {
            let word = pll_reg_for_freq(freq);
            let fb_div = ((word >> 16) & 0xFF) as f32;
            let ref_div = ((word >> 8) & 0xFF) as f32;
            let pd1 = (((word >> 4) & 0x0F) + 1) as f32;
            let pd2 = ((word & 0x0F) + 1) as f32;
            let vco = 25.0 * fb_div / ref_div;
            let actual = vco / (pd1 * pd2);
            assert!(
                (1600.0..=3200.0).contains(&vco),
                "freq {freq}: VCO {vco} out of BM1370 range"
            );
            assert!(
                (actual - freq as f32).abs() < 2.5,
                "freq {freq}: actual {actual} too far"
            );
        }
    }
}
