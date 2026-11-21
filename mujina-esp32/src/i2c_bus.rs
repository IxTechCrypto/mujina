//! ESP-IDF I2C0 hardware driver for NerdQX Rev 1.0 (TPS53647, TMP451 mux, TMP1075, EMC2302).

use esp_idf_svc::hal::delay::FreeRtos;
use esp_idf_svc::hal::gpio::{Gpio10, Gpio11, Gpio13, Gpio2, Gpio3, Gpio43, Gpio44, Input, Output, PinDriver};
use esp_idf_svc::hal::i2c::{I2cConfig, I2cDriver, I2C0};
use esp_idf_svc::hal::peripheral::Peripheral;
use esp_idf_svc::hal::units::Hertz;
use esp_idf_svc::sys::{EspError, ESP_ERR_INVALID_ARG, ESP_ERR_INVALID_RESPONSE, ESP_ERR_TIMEOUT};

use crate::hw_pins::{
    ADDR_EMC2302, ADDR_TMP1075, ADDR_TMP451, ADDR_TPS53647, I2C0_SCL, I2C0_SDA, LDO_EN, PWR_EN,
    TMUX_A0, TMUX_A1, VR_RDY,
};
use crate::power_cooling::{
    decode_linear11, decode_tmp451_remote_c, decode_tps53647_vout_measured, encode_linear11,
    mv_to_vr12_vid, TMP451_MFG_ID, TPS53647_DEVICE_CODE, VR_TEMP_CUTOFF_C,
};

// Compile-time guard tying concrete GPIO types to crate::hw_pins constants.
const _: () = {
    assert!(I2C0_SDA == 44);
    assert!(I2C0_SCL == 43);
    assert!(TMUX_A0 == 2);
    assert!(TMUX_A1 == 3);
    assert!(PWR_EN == 10);
    assert!(VR_RDY == 11);
    assert!(LDO_EN == 13);
};

const I2C_TIMEOUT_TICKS: u32 = 50;

// TPS53647 PMBus & MFR command codes (matching mujina-miner/src/peripheral/tps53647.rs).
const PMBUS_ON_OFF_CONFIG: u8 = 0x02;
const PMBUS_CLEAR_FAULTS: u8 = 0x03;
const PMBUS_RESTORE_DEFAULT_ALL: u8 = 0x12;
const PMBUS_VOUT_COMMAND: u8 = 0x21;
const PMBUS_IOUT_OC_FAULT_LIMIT: u8 = 0x46;
const PMBUS_IOUT_OC_WARN_LIMIT: u8 = 0x4A;
const PMBUS_OT_FAULT_LIMIT: u8 = 0x4F;
const PMBUS_OT_WARN_LIMIT: u8 = 0x51;
const PMBUS_STATUS_WORD: u8 = 0x79;
const PMBUS_STATUS_VOUT: u8 = 0x7A;
const PMBUS_STATUS_IOUT: u8 = 0x7B;
const PMBUS_STATUS_INPUT: u8 = 0x7C;
const PMBUS_STATUS_TEMP: u8 = 0x7D;
const PMBUS_READ_VIN: u8 = 0x88;
const PMBUS_READ_IIN: u8 = 0x89;
const PMBUS_READ_IOUT: u8 = 0x8C;
const PMBUS_READ_TEMP1: u8 = 0x8D;
const PMBUS_READ_PIN: u8 = 0x97;

const MFR_VOUT_MEASURED: u8 = 0xD4;
const MFR_IMAX: u8 = 0xDA;
const MFR_SWITCHING_FREQ: u8 = 0xDC;
const MFR_OPERATION_MODE: u8 = 0xDD;
const MFR_PHASE_COUNT: u8 = 0xE4;
const MFR_DEVICE_CODE: u8 = 0xFC;

// TMP451 remote die temperature registers.
const TMP451_REG_REMOTE_HI: u8 = 0x01;
const TMP451_REG_STATUS: u8 = 0x02;
const TMP451_REG_ONE_SHOT: u8 = 0x0F;
const TMP451_REG_REMOTE_LO: u8 = 0x10;
const TMP451_REG_MFG_ID: u8 = 0xFE;

// TMP1075 temperature register.
const TMP1075_REG_TEMP: u8 = 0x00;

// EMC2302 fan drive and tach registers.
const EMC2302_FAN1_DRIVE: u8 = 0x30;
const EMC2302_FAN2_DRIVE: u8 = 0x40;
const EMC2302_FAN1_TACH_HI: u8 = 0x3E;
const EMC2302_FAN1_TACH_LO: u8 = 0x3F;
const EMC2302_FAN2_TACH_HI: u8 = 0x4E;
const EMC2302_FAN2_TACH_LO: u8 = 0x4F;

/// Decoded telemetry snapshot from the TPS53647 VR controller.
#[derive(Debug, Clone, Copy, Default)]
pub struct Tps53647Telemetry {
    pub vin_v: f32,
    pub iin_a: f32,
    pub pin_w: f32,
    pub vout_v: f32,
    pub iout_a: f32,
    pub vr_temp_c: f32,
}

/// NerdQX Rev 1.0 I2C0 bus + TMUX1204 selector + power rail control GPIOs.
pub struct NerdqxI2c<'d> {
    i2c: I2cDriver<'d>,
    tmux_a0: PinDriver<'d, Gpio2, Output>,
    tmux_a1: PinDriver<'d, Gpio3, Output>,
    pwr_en: PinDriver<'d, Gpio10, Output>,
    ldo_en: PinDriver<'d, Gpio13, Output>,
    vr_rdy: PinDriver<'d, Gpio11, Input>,
    tmp451_present: bool,
    tmux_ch: u8,
    tmux_step: u8,
    cached_temps: [Option<f32>; 4],
    temp_fail_counts: [u8; 4],
}

impl<'d> NerdqxI2c<'d> {
    /// Initialize I2C0 at 400 kHz, probe TMP451, and configure TMUX / rail GPIOs (rails default off).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        i2c0: impl Peripheral<P = I2C0> + 'd,
        sda: impl Peripheral<P = Gpio44> + 'd,
        scl: impl Peripheral<P = Gpio43> + 'd,
        tmux_a0: impl Peripheral<P = Gpio2> + 'd,
        tmux_a1: impl Peripheral<P = Gpio3> + 'd,
        pwr_en: impl Peripheral<P = Gpio10> + 'd,
        ldo_en: impl Peripheral<P = Gpio13> + 'd,
        vr_rdy: impl Peripheral<P = Gpio11> + 'd,
    ) -> Result<Self, EspError> {
        let cfg = I2cConfig::new().baudrate(Hertz(400_000));
        let i2c = I2cDriver::new(i2c0, sda, scl, &cfg)?;

        let mut tmux_a0 = PinDriver::output(tmux_a0)?;
        let mut tmux_a1 = PinDriver::output(tmux_a1)?;
        let mut pwr_en = PinDriver::output(pwr_en)?;
        let mut ldo_en = PinDriver::output(ldo_en)?;
        let vr_rdy = PinDriver::input(vr_rdy)?;

        tmux_a0.set_low()?;
        tmux_a1.set_low()?;
        pwr_en.set_low()?;
        ldo_en.set_low()?;

        let mut bus = Self {
            i2c,
            tmux_a0,
            tmux_a1,
            pwr_en,
            ldo_en,
            vr_rdy,
            tmp451_present: false,
            tmux_ch: 0,
            tmux_step: 0,
            cached_temps: [None; 4],
            temp_fail_counts: [0; 4],
        };

        let _ = bus.select_tmux(0);
        bus.tmp451_present = matches!(bus.read_u8(ADDR_TMP451, TMP451_REG_MFG_ID), Ok(TMP451_MFG_ID));
        log::info!("TMP451 present: {}", bus.tmp451_present);
        Ok(bus)
    }

    /// Enable or disable main buck power (`PWR_EN`).
    pub fn set_pwr_en(&mut self, on: bool) -> Result<(), EspError> {
        if on {
            self.pwr_en.set_high()
        } else {
            self.pwr_en.set_low()
        }
    }

    /// Enable or disable ASIC LDO rail (`LDO_EN`).
    pub fn set_ldo_en(&mut self, on: bool) -> Result<(), EspError> {
        if on {
            self.ldo_en.set_high()
        } else {
            self.ldo_en.set_low()
        }
    }

    /// Read VR power-good status (`VR_RDY`).
    pub fn is_vr_ready(&self) -> bool {
        self.vr_rdy.is_high()
    }

    /// Configure TPS53647 (matching `mujina-miner/src/peripheral/tps53647.rs`),
    /// command initial VOUT, assert `LDO_EN` then `PWR_EN`, poll `VR_RDY` with a 2 s
    /// timeout, re-apply VOUT after output-enable NVM reload, and wait 50 ms to settle.
    pub fn init_tps53647_and_enable(&mut self, target_mv: u16) -> Result<(), &'static str> {
        let _ = self.set_pwr_en(false);
        let _ = self.set_ldo_en(false);
        FreeRtos::delay_ms(10);

        let dev_code = self
            .read_u16_le(ADDR_TPS53647, MFR_DEVICE_CODE)
            .map_err(|_| "tps53647_i2c_probe_failed")?;
        if dev_code != TPS53647_DEVICE_CODE {
            log::error!(
                "TPS53647 wrong device code: expected {TPS53647_DEVICE_CODE:#06x}, got {dev_code:#06x}"
            );
            return Err("tps53647_wrong_device");
        }

        // Clear any prior latched fault and reload NVM defaults while PWR_EN is low.
        self.write_cmd(ADDR_TPS53647, PMBUS_CLEAR_FAULTS)
            .map_err(|_| "tps53647_clear_faults_failed")?;
        self.write_cmd(ADDR_TPS53647, PMBUS_RESTORE_DEFAULT_ALL)
            .map_err(|_| "tps53647_restore_defaults_failed")?;
        FreeRtos::delay_ms(5);

        // Configure VR12 mode, 3 active phases, 500 kHz, 90 A IMAX, OT/OC limits.
        const ON_OFF_CONFIG: u8 = 0x17;
        const SWITCHING_500KHZ: u8 = 0x20;
        const OPERATION_MODE_VR12: u8 = 0x89;
        self.write_u8(ADDR_TPS53647, PMBUS_ON_OFF_CONFIG, ON_OFF_CONFIG)
            .map_err(|_| "tps53647_cfg_failed")?;
        self.write_u8(ADDR_TPS53647, MFR_SWITCHING_FREQ, SWITCHING_500KHZ)
            .map_err(|_| "tps53647_cfg_failed")?;
        self.write_u8(ADDR_TPS53647, MFR_IMAX, 90)
            .map_err(|_| "tps53647_cfg_failed")?;
        self.write_u8(ADDR_TPS53647, MFR_OPERATION_MODE, OPERATION_MODE_VR12)
            .map_err(|_| "tps53647_cfg_failed")?;
        // Re-send per NerdQAxe++/NerdQX reference sequence (mode change can disturb them).
        self.write_u8(ADDR_TPS53647, PMBUS_ON_OFF_CONFIG, ON_OFF_CONFIG)
            .map_err(|_| "tps53647_cfg_failed")?;
        self.write_u8(ADDR_TPS53647, MFR_SWITCHING_FREQ, SWITCHING_500KHZ)
            .map_err(|_| "tps53647_cfg_failed")?;
        self.write_u8(ADDR_TPS53647, MFR_PHASE_COUNT, 2) // 3 phases - 1
            .map_err(|_| "tps53647_cfg_failed")?;

        self.write_u16_le(ADDR_TPS53647, PMBUS_OT_WARN_LIMIT, encode_linear11(VR_TEMP_CUTOFF_C))
            .map_err(|_| "tps53647_limit_failed")?;
        self.write_u16_le(ADDR_TPS53647, PMBUS_OT_FAULT_LIMIT, encode_linear11(125.0))
            .map_err(|_| "tps53647_limit_failed")?;
        self.write_u16_le(ADDR_TPS53647, PMBUS_IOUT_OC_WARN_LIMIT, encode_linear11(142.0))
            .map_err(|_| "tps53647_limit_failed")?;
        self.write_u16_le(ADDR_TPS53647, PMBUS_IOUT_OC_FAULT_LIMIT, encode_linear11(142.0))
            .map_err(|_| "tps53647_limit_failed")?;

        // Set VOUT before enabling the core rail so it never powers into NVM UV fault.
        self.set_vcore_mv(target_mv)
            .map_err(|_| "tps53647_pre_vout_failed")?;

        // Assert IO LDO rail first, then core PWR_EN, then poll VR_RDY up to 2000 ms.
        self.set_ldo_en(true).map_err(|_| "ldo_en_failed")?;
        FreeRtos::delay_ms(5);
        self.set_pwr_en(true).map_err(|_| "pwr_en_failed")?;

        let mut vr_ready = false;
        for _ in 0..200 {
            if self.is_vr_ready() {
                vr_ready = true;
                break;
            }
            FreeRtos::delay_ms(10);
        }
        if !vr_ready {
            let sw = self.read_u16_le(ADDR_TPS53647, PMBUS_STATUS_WORD).unwrap_or(0xFFFF);
            let sv = self.read_u8(ADDR_TPS53647, PMBUS_STATUS_VOUT).unwrap_or(0xFF);
            let si = self.read_u8(ADDR_TPS53647, PMBUS_STATUS_IOUT).unwrap_or(0xFF);
            let sinp = self.read_u8(ADDR_TPS53647, PMBUS_STATUS_INPUT).unwrap_or(0xFF);
            let st = self.read_u8(ADDR_TPS53647, PMBUS_STATUS_TEMP).unwrap_or(0xFF);
            log::error!(
                "TPS53647 VR_RDY timeout after 2000ms: status_word={sw:#06x} vout={sv:#04x} iout={si:#04x} input={sinp:#04x} temp={st:#04x}"
            );
            let _ = self.set_pwr_en(false);
            let _ = self.set_ldo_en(false);
            return Err("vr_rdy_timeout");
        }

        // Re-apply VOUT_COMMAND after output-enable (TPS53647 reloads NVM on enable) + settle 50 ms.
        self.set_vcore_mv(target_mv)
            .map_err(|_| "tps53647_post_vout_failed")?;
        FreeRtos::delay_ms(50);
        log::info!("TPS53647 configured and VR_RDY high (vcore={target_mv} mV)");
        Ok(())
    }

    /// Emergency rail shutdown on overtemp / overpower / lost sensor fault.
    pub fn emergency_shutdown(&mut self) {
        let _ = self.set_pwr_en(false);
        let _ = self.set_ldo_en(false);
        let _ = self.set_fan_duty(100);
    }

    /// Read all core telemetry registers from the TPS53647 (`0x71`), decoding VOUT via `0xD4` / VID.
    pub fn read_tps53647(&mut self) -> Result<Tps53647Telemetry, EspError> {
        let vin_v = decode_linear11(self.read_u16_le(ADDR_TPS53647, PMBUS_READ_VIN)?);
        let iin_a = decode_linear11(self.read_u16_le(ADDR_TPS53647, PMBUS_READ_IIN)?);
        let pin_w = decode_linear11(self.read_u16_le(ADDR_TPS53647, PMBUS_READ_PIN)?);
        let iout_a = decode_linear11(self.read_u16_le(ADDR_TPS53647, PMBUS_READ_IOUT)?);
        let vr_temp_c = decode_linear11(self.read_u16_le(ADDR_TPS53647, PMBUS_READ_TEMP1)?);

        let raw_d4 = self.read_u16_le(ADDR_TPS53647, MFR_VOUT_MEASURED).unwrap_or(0);
        let cmd_vid = self
            .read_u16_le(ADDR_TPS53647, PMBUS_VOUT_COMMAND)
            .map(|w| w as u8)
            .unwrap_or(0);
        let vout_v = decode_tps53647_vout_measured(raw_d4, cmd_vid);

        Ok(Tps53647Telemetry {
            vin_v,
            iin_a,
            pin_w,
            vout_v,
            iout_a,
            vr_temp_c,
        })
    }

    /// Return true when the TMP451 remote diode mux sensor was detected on boot.
    pub fn is_using_tmp451(&self) -> bool {
        self.tmp451_present
    }

    /// Set TPS53647 core voltage in millivolts via PMBus `VOUT_COMMAND` (VR12 5 mV VID step)
    /// and verify readback matches the commanded VID.
    pub fn set_vcore_mv(&mut self, mv: u16) -> Result<(), EspError> {
        let Some(vid) = mv_to_vr12_vid(mv) else {
            return Err(EspError::from_infallible::<{ ESP_ERR_INVALID_ARG }>());
        };
        self.write_u16_le(ADDR_TPS53647, PMBUS_VOUT_COMMAND, vid as u16)?;
        let readback = self.read_u16_le(ADDR_TPS53647, PMBUS_VOUT_COMMAND)? as u8;
        if readback != vid {
            log::error!(
                "TPS53647 VOUT_COMMAND mismatch: wrote vid={vid:#04x}, read {readback:#04x}"
            );
            return Err(EspError::from_infallible::<{ ESP_ERR_INVALID_RESPONSE }>());
        }
        Ok(())
    }

    /// Clamp `target_mv` to the NerdQX BM1370 rail band (`950..=1280` mV) and apply `VOUT_COMMAND`.
    pub fn set_vcore_mv_clamped(&mut self, target_mv: u16) -> Result<u16, EspError> {
        let clamped = target_mv.clamp(crate::chip_tuner::MIN_VCORE_MV, crate::chip_tuner::MAX_VCORE_MV);
        self.set_vcore_mv(clamped)?;
        Ok(clamped)
    }

    /// Non-blocking TMP451 mux state machine called once per 50 ms main-loop tick.
    ///
    /// Waits 50 ms after mux switch before triggering one-shot conversion, then
    /// 100 ms (>= 75 ms conversion window) before checking BUSY/OPEN and caching
    /// the calibrated diode temperature, failing closed after 3 consecutive misses.
    pub fn step_temp_mux(&mut self) {
        if !self.tmp451_present {
            let board_temps = self.read_board_temps();
            let valid = board_temps.into_iter().flatten().find(|t| t.is_finite());
            self.cached_temps = [valid; 4];
            return;
        }

        let ch = (self.tmux_ch & 0x03) as usize;
        match self.tmux_step {
            0 => {
                // 50 ms after mux switch: trigger one-shot (effective if standby, no-op if free-run).
                let _ = self.write_u8(ADDR_TMP451, TMP451_REG_ONE_SHOT, 0x00);
                self.tmux_step = 1;
            }
            1 => {
                // Wait second 50 ms step so total settle+conversion >= 150 ms.
                self.tmux_step = 2;
            }
            _ => {
                let decoded = match (
                    self.read_u8(ADDR_TMP451, TMP451_REG_STATUS),
                    self.read_u8(ADDR_TMP451, TMP451_REG_REMOTE_HI),
                    self.read_u8(ADDR_TMP451, TMP451_REG_REMOTE_LO),
                ) {
                    (Ok(st), Ok(hi), Ok(lo)) => decode_tmp451_remote_c(st, hi, lo),
                    _ => None,
                };
                if let Some(c) = decoded {
                    self.cached_temps[ch] = Some(c);
                    self.temp_fail_counts[ch] = 0;
                } else {
                    self.temp_fail_counts[ch] = self.temp_fail_counts[ch].saturating_add(1);
                    if self.temp_fail_counts[ch] >= 3 {
                        self.cached_temps[ch] = None;
                    }
                }
                self.tmux_ch = (self.tmux_ch + 1) & 0x03;
                let _ = self.select_tmux(self.tmux_ch);
                self.tmux_step = 0;
            }
        }
    }

    /// Return the latest non-blocking calibrated die temperatures for all 4 BM1370 chips.
    pub fn asic_temps(&self) -> [Option<f32>; 4] {
        self.cached_temps
    }

    /// Read both TMP1075 board temperature sensors (`0x48`, `0x49`) in °C.
    pub fn read_board_temps(&mut self) -> [Option<f32>; 2] {
        let mut temps = [None; 2];
        for (i, &addr) in ADDR_TMP1075.iter().enumerate() {
            if let Ok(raw) = self.read_u16_be(addr, TMP1075_REG_TEMP) {
                temps[i] = Some(((raw as i16) >> 4) as f32 * 0.0625);
            }
        }
        temps
    }

    /// Set EMC2302 (`0x2E`) PWM duty cycle (0..=100%) on both fan channels.
    pub fn set_fan_duty(&mut self, pct: u8) -> Result<(), EspError> {
        let drive = ((pct.min(100) as u16 * 255) / 100) as u8;
        self.write_u8(ADDR_EMC2302, EMC2302_FAN1_DRIVE, drive)?;
        self.write_u8(ADDR_EMC2302, EMC2302_FAN2_DRIVE, drive)
    }

    /// Set the VR fan (EMC2302 channel 1) and ASIC fan (channel 2) duty independently.
    pub fn set_fan_duties(&mut self, vr_pct: u8, asic_pct: u8) -> Result<(), EspError> {
        let drive = |p: u8| ((p.min(100) as u16 * 255) / 100) as u8;
        self.write_u8(ADDR_EMC2302, EMC2302_FAN1_DRIVE, drive(vr_pct))?;
        self.write_u8(ADDR_EMC2302, EMC2302_FAN2_DRIVE, drive(asic_pct))
    }

    /// Read EMC2302 (`0x2E`) tachometer RPMs for both fan channels.
    pub fn read_fan_rpms(&mut self) -> [Option<u16>; 2] {
        let regs = [
            (EMC2302_FAN1_TACH_HI, EMC2302_FAN1_TACH_LO),
            (EMC2302_FAN2_TACH_HI, EMC2302_FAN2_TACH_LO),
        ];
        let mut rpms = [None; 2];
        for (i, &(hi_reg, lo_reg)) in regs.iter().enumerate() {
            if let (Ok(hi), Ok(lo)) = (
                self.read_u8(ADDR_EMC2302, hi_reg),
                self.read_u8(ADDR_EMC2302, lo_reg),
            ) {
                let count = ((hi as u16) << 5) | ((lo as u16) >> 3);
                let rpm = if count == 0 || count >= 0x1FFF {
                    0
                } else {
                    (3_932_160u32 / count as u32).min(u16::MAX as u32) as u16
                };
                rpms[i] = Some(rpm);
            }
        }
        rpms
    }

    fn select_tmux(&mut self, ch: u8) -> Result<(), EspError> {
        if (ch & 0x01) != 0 {
            self.tmux_a0.set_high()?;
        } else {
            self.tmux_a0.set_low()?;
        }
        if (ch & 0x02) != 0 {
            self.tmux_a1.set_high()?;
        } else {
            self.tmux_a1.set_low()?;
        }
        Ok(())
    }

    fn write_cmd(&mut self, addr: u8, cmd: u8) -> Result<(), EspError> {
        let _ = ESP_ERR_TIMEOUT;
        self.i2c.write(addr, &[cmd], I2C_TIMEOUT_TICKS)
    }

    fn read_u8(&mut self, addr: u8, reg: u8) -> Result<u8, EspError> {
        let mut buf = [0u8; 1];
        self.i2c.write_read(addr, &[reg], &mut buf, I2C_TIMEOUT_TICKS)?;
        Ok(buf[0])
    }

    fn write_u8(&mut self, addr: u8, reg: u8, val: u8) -> Result<(), EspError> {
        self.i2c.write(addr, &[reg, val], I2C_TIMEOUT_TICKS)
    }

    fn read_u16_le(&mut self, addr: u8, reg: u8) -> Result<u16, EspError> {
        let mut buf = [0u8; 2];
        self.i2c.write_read(addr, &[reg], &mut buf, I2C_TIMEOUT_TICKS)?;
        Ok(u16::from_le_bytes(buf))
    }

    fn read_u16_be(&mut self, addr: u8, reg: u8) -> Result<u16, EspError> {
        let mut buf = [0u8; 2];
        self.i2c.write_read(addr, &[reg], &mut buf, I2C_TIMEOUT_TICKS)?;
        Ok(u16::from_be_bytes(buf))
    }

    fn write_u16_le(&mut self, addr: u8, reg: u8, val: u16) -> Result<(), EspError> {
        let [lo, hi] = val.to_le_bytes();
        self.i2c.write(addr, &[reg, lo, hi], I2C_TIMEOUT_TICKS)
    }
}
