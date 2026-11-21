//! Power & cooling presets for the NerdQX (15 A stock fuse tiers, optional 20 A upgraded air / AiO tiers).

/// 15 A stock fuse hard cap (W) and default.
pub const STOCK_DEFAULT_W: f32 = 144.0;
pub const STOCK_MAX_W: f32 = 180.0;
pub const UPGRADED_DEFAULT_W: f32 = 192.0;
pub const UPGRADED_MAX_W: f32 = 240.0;
pub const AIO_DEFAULT_W: f32 = 240.0;
pub const AIO_MAX_W: f32 = 240.0;
/// Stock-fuse dynamic cap: watts per input volt (15 A).
pub const STOCK_W_PER_VIN: f32 = 15.0;
/// M3 pump/fan (+12V/GND only) is not metered by the TPS53647.
pub const PUMP_HEADROOM_MIN_W: f32 = 10.0;
pub const PUMP_HEADROOM_MAX_W: f32 = 15.0;
pub const MIN_CEILING_W: f32 = 60.0;

/// TPS53647 MFR_DEVICE_CODE (`0xFC`) expected response.
pub const TPS53647_DEVICE_CODE: u16 = 0x01F0;
/// TPS53647 factory-default VID sentinel (1.000 V); nudged down 1 step on write.
pub const VID_FACTORY_DEFAULT: u8 = 0x97;

/// TMP451 expected manufacturer ID (`0xFE`).
pub const TMP451_MFG_ID: u8 = 0x55;
pub const TMP451_STATUS_BUSY: u8 = 0x80;
pub const TMP451_STATUS_OPEN: u8 = 0x04;
/// Calibrated BM1370 remote thermal diode curve on NerdQX.
pub const NERDQX_DIODE_SCALE: f32 = 1.09;
pub const NERDQX_DIODE_OFFSET_C: f32 = -29.5;

/// Fail-closed hard safety cutoff thresholds (°C).
pub const ASIC_TEMP_CUTOFF_C: f32 = 85.0;
pub const VR_TEMP_CUTOFF_C: f32 = 95.0;

/// `no_std`-safe `val * 2^exp`.
#[inline]
pub fn ldexp_f32(mut val: f32, exp: i32) -> f32 {
    if exp >= 0 {
        let mut i = 0;
        while i < exp {
            val *= 2.0;
            i += 1;
        }
    } else {
        let mut i = 0;
        let n = -exp;
        while i < n {
            val *= 0.5;
            i += 1;
        }
    }
    val
}

/// Decode a PMBus LINEAR11 16-bit word (`exp[15:11]` signed, `mant[10:0]` signed).
#[inline]
pub fn decode_linear11(raw: u16) -> f32 {
    let exp = ((raw as i16) >> 11) as i32;
    let mant = (((raw << 5) as i16) >> 5) as i32;
    ldexp_f32(mant as f32, exp)
}

/// Encode `val` into PMBus LINEAR11 (smallest exponent keeping signed 11-bit mantissa).
pub fn encode_linear11(val: f32) -> u16 {
    if !val.is_finite() || val == 0.0 {
        return 0;
    }
    let mut exp = -16i32;
    while exp <= 15 {
        let scaled = ldexp_f32(val, -exp);
        if scaled >= -1024.4 && scaled <= 1023.4 {
            let rounded = if scaled >= 0.0 {
                (scaled + 0.5) as i32
            } else {
                (scaled - 0.5) as i32
            };
            if (-1024..=1023).contains(&rounded) {
                let e_bits = ((exp as u16) & 0x1F) << 11;
                let m_bits = (rounded as u16) & 0x07FF;
                return e_bits | m_bits;
            }
        }
        exp += 1;
    }
    0
}

/// Convert `mv` (`800..=1400` mV) to a TPS53647 VR12 5 mV VID code (`0x01..=0xFF`),
/// avoiding the `0x97` (1.000 V) factory-reset sentinel by stepping down to `0x96`.
pub fn mv_to_vr12_vid(mv: u16) -> Option<u8> {
    if mv < 800 || mv > 1400 {
        return None;
    }
    let raw = (((mv - 250) + 2) / 5) + 1;
    if raw == 0 || raw > 255 {
        return None;
    }
    let code = raw as u8;
    Some(if code == VID_FACTORY_DEFAULT {
        VID_FACTORY_DEFAULT - 1
    } else {
        code
    })
}

/// Decode a TPS53647 VR12 VID code (`0` = rail off) to volts.
#[inline]
pub fn vr12_vid_to_volts(code: u8) -> f32 {
    if code == 0 {
        return 0.0;
    }
    0.25 + (code - 1) as f32 * 0.005
}

/// Decode `TPS53647` `MFR_VOUT_MEASURED` (`0xD4`, unsigned Q7.9 fixed-point),
/// falling back to the commanded VID if invalid (`0` or `0xFFFF`).
pub fn decode_tps53647_vout_measured(raw_d4: u16, commanded_vid: u8) -> f32 {
    let volts = (raw_d4 as f32) / 512.0;
    if (0.2..=2.0).contains(&volts) {
        volts
    } else {
        vr12_vid_to_volts(commanded_vid)
    }
}

/// Decode a TMP451 remote-diode reading using NerdQX diode calibration constants.
/// Returns `None` if BUSY or REMOTE_OPEN is set, or if calibrated °C is out of range.
pub fn decode_tmp451_remote_c(status: u8, hi: u8, lo: u8) -> Option<f32> {
    if (status & (TMP451_STATUS_BUSY | TMP451_STATUS_OPEN)) != 0 {
        return None;
    }
    let raw = ((hi as i16) << 4) | ((lo >> 4) as i16);
    let raw_c = (raw as f32) * 0.0625;
    let cal_c = raw_c * NERDQX_DIODE_SCALE + NERDQX_DIODE_OFFSET_C;
    if (-10.0..=150.0).contains(&cal_c) {
        Some(cal_c)
    } else {
        None
    }
}

/// Fail-closed hard cutoff evaluator checked on every telemetry tick in `main.rs`.
pub fn evaluate_safety_cutoff(
    warmup_done: bool,
    vin_v: f32,
    pin_w: f32,
    effective_ceiling_w: f32,
    vr_temp_c: Option<f32>,
    chip_temps_c: &[Option<f32>; 4],
    tps_comm_failures: u8,
) -> Option<&'static str> {
    if tps_comm_failures >= 3 {
        return Some("tps53647_i2c_lost");
    }
    match vr_temp_c {
        Some(v) if !v.is_finite() || v >= VR_TEMP_CUTOFF_C => {
            return Some("vr_overtemp_or_invalid");
        }
        Some(v) if warmup_done && v <= 0.0 => {
            return Some("vr_overtemp_or_invalid");
        }
        None if warmup_done => {
            return Some("vr_overtemp_or_invalid");
        }
        _ => {}
    }
    let mut have_chip_temp = false;
    for slot in chip_temps_c.iter().copied() {
        if let Some(t) = slot {
            have_chip_temp = true;
            if !t.is_finite() || t >= ASIC_TEMP_CUTOFF_C {
                return Some("asic_overtemp");
            }
        }
    }
    if warmup_done && !have_chip_temp {
        return Some("all_temp_sensors_missing");
    }
    if warmup_done
        && (!vin_v.is_finite()
            || vin_v < 10.5
            || !pin_w.is_finite()
            || !effective_ceiling_w.is_finite()
            || pin_w > effective_ceiling_w)
    {
        return Some("power_or_vin_fault");
    }
    None
}

/// Compute EMC2302 fan duty (`25..=100%`) targeting the 58–61 °C BM1370 die sweet spot,
/// failing closed to `100%` on missing/faulted temps or extreme ASIC/VR temperatures.
pub fn compute_fan_duty_pct(
    max_asic_c: Option<f32>,
    vr_temp_c: Option<f32>,
    fault_latched: bool,
) -> u8 {
    if fault_latched {
        return 100;
    }
    let (Some(asic), Some(vr)) = (max_asic_c, vr_temp_c) else {
        return 100;
    };
    if !asic.is_finite() || !vr.is_finite() || vr >= 80.0 || asic >= 72.0 {
        return 100;
    }
    if asic <= 58.0 {
        return 25;
    }
    if asic <= 61.0 {
        let frac = (asic - 58.0) / 3.0;
        return (25.0 + frac * 20.0) as u8;
    }
    let frac = (asic - 61.0) / 11.0;
    (45.0 + frac * 55.0) as u8
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preset {
    /// #1A Stock air, 15 A fuse.
    StockAir,
    /// #1B Upgraded air, >=20 A fuse.
    UpgradedAir,
    /// #2 AiO liquid, >=20 A fuse.
    AiO,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PowerCoolingConfig {
    pub preset: Preset,
    pub fuse_amps: u8,
    pub fuse_upgrade_confirmed: bool,
    pub power_ceiling_w: f32,
    /// Estimated un-metered M3 pump/fan load (W).
    pub pump_w: f32,
}

impl PowerCoolingConfig {
    pub fn for_preset(preset: Preset, fuse_amps: u8, confirmed: bool) -> Self {
        let (ceiling, pump) = match preset {
            Preset::StockAir => (STOCK_DEFAULT_W, 0.0),
            Preset::UpgradedAir => (UPGRADED_DEFAULT_W, 0.0),
            Preset::AiO => (AIO_DEFAULT_W, PUMP_HEADROOM_MAX_W),
        };
        Self { preset, fuse_amps, fuse_upgrade_confirmed: confirmed, power_ceiling_w: ceiling, pump_w: pump }
    }

    pub fn default_stock() -> Self {
        Self::for_preset(Preset::StockAir, 15, false)
    }

    pub fn preset_default_w(&self) -> f32 {
        match self.preset {
            Preset::StockAir => STOCK_DEFAULT_W,
            Preset::UpgradedAir => UPGRADED_DEFAULT_W,
            Preset::AiO => AIO_DEFAULT_W,
        }
    }

    /// True when the >180 W tiers are legitimately unlocked.
    pub fn unlocked(&self) -> bool {
        self.preset != Preset::StockAir && self.fuse_amps >= 20 && self.fuse_upgrade_confirmed
    }

    /// Max configurable ceiling; falls back to 180 W unless unlocked.
    pub fn max_ceiling_w(&self) -> f32 {
        if !self.unlocked() {
            return STOCK_MAX_W;
        }
        match self.preset {
            Preset::UpgradedAir => UPGRADED_MAX_W,
            _ => AIO_MAX_W,
        }
    }

    /// Ceiling actually enforced: user value clamped to [60, max] and to the wall budget.
    pub fn effective_ceiling_w(&self, vin_v: f32) -> f32 {
        // Fail closed on a bad input-voltage reading.
        if !vin_v.is_finite() || vin_v <= 0.0 {
            return MIN_CEILING_W;
        }
        let mut c = self.power_ceiling_w;
        if !(c >= MIN_CEILING_W) {
            c = MIN_CEILING_W;
        }
        let max = self.max_ceiling_w();
        if c > max {
            c = max;
        }
        // Wall budget: 15*Vin on stock fuse; fuse*Vin minus un-metered pump otherwise.
        let dyn_cap = if self.unlocked() {
            self.fuse_amps as f32 * vin_v - self.pump_w
        } else {
            STOCK_W_PER_VIN * vin_v
        };
        if dyn_cap < c {
            c = dyn_cap;
        }
        if c < MIN_CEILING_W {
            c = MIN_CEILING_W;
        }
        c
    }
    /// Extra headroom (W) the wall budget (`fuse*vin`) leaves beyond board input
    /// power and the un-metered pump. Negative means overdrawn.
    pub fn pump_headroom_w(&self, pin_w: f32, vin_v: f32) -> f32 {
        self.fuse_amps as f32 * vin_v - pin_w - self.pump_w
    }

    /// True while the wall budget leaves room for the pump (at least 10 W, or
    /// the configured pump estimate if larger).
    pub fn pump_guard_ok(&self, pin_w: f32, vin_v: f32) -> bool {
        if self.pump_w <= 0.0 {
            return true;
        }
        let need = if self.pump_w > PUMP_HEADROOM_MIN_W { self.pump_w } else { PUMP_HEADROOM_MIN_W };
        self.fuse_amps as f32 * vin_v - pin_w >= need
    }
}
impl Default for PowerCoolingConfig {
    fn default() -> Self {
        Self::default_stock()
    }
}


