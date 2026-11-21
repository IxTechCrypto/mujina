//! Stage-2 per-chip parallel-rail PLL optimizer for 4x BM1370.

pub const CHIPS: usize = 4;
pub const PLL_STEP_MHZ: f32 = 6.25;
pub const PLL_MAX_OFFSET_MHZ: f32 = 50.0;
const MAX_STEPS: i8 = 8; // 8 * 6.25 = 50
pub const RAMP_START_MHZ: f32 = 56.25;
pub const SAFE_FREQ_MHZ: f32 = 495.0;
pub const SAFE_MV: u32 = 1150;

/// BM1370 die sweet spot (`58..=61` deg C, nominal `60` C) and thermal limits (deg C).
pub const TEMP_TARGET_MIN_C: f32 = 58.0;
pub const TEMP_TARGET_NOMINAL_C: f32 = 60.0;
pub const TEMP_TARGET_C: f32 = 63.0;
pub const TEMP_HOT_C: f32 = 65.0;
pub const TEMP_CRIT_C: f32 = 75.0;
pub const VR_TEMP_LIMIT_C: f32 = 100.0;
pub const ERR_LIMIT_PCT: f32 = 2.0;
pub const ERR_GOOD_PCT: f32 = 0.5;

/// Reg 0x10 version-rolling value: `65536 * 3000 / hz` (15 MHz IO clock / 5000).
pub fn vr_reg_for_hz(hz: u32) -> u32 {
    if hz == 0 {
        return 0;
    }
    ((65536u64 * 3000 + hz as u64 / 2) / hz as u64) as u32
}

/// Number of ramp steps from 56.25 MHz up to `target_mhz` (inclusive of both ends).
pub fn ramp_len(target_mhz: f32) -> usize {
    if target_mhz <= RAMP_START_MHZ {
        return 1;
    }
    let n = (target_mhz - RAMP_START_MHZ) / PLL_STEP_MHZ;
    let mut k = n as usize;
    if (k as f32) < n {
        k += 1;
    }
    k + 1
}

/// Frequency of ramp step `idx` (never exceeds `target_mhz`).
pub fn ramp_freq(idx: usize, target_mhz: f32) -> f32 {
    let f = RAMP_START_MHZ + PLL_STEP_MHZ * idx as f32;
    if f > target_mhz {
        target_mhz
    } else {
        f
    }
}

/// Factory NerdQX operating point and tuner clock/voltage envelope.
pub const START_FREQ_MHZ: u16 = 777;
pub const START_MV: u16 = 1250;
/// Highest base clock the tuner/manual API may request (per-chip offsets add up to +50 on top).
pub const MAX_BASE_FREQ_MHZ: f32 = 825.0;
pub const MIN_BASE_FREQ_MHZ: f32 = 100.0;
pub const MIN_VCORE_MV: u16 = 950;
pub const MAX_VCORE_MV: u16 = 1280;
/// Lowest duty for the VR (VRM) fan header, and for the ASIC fan header.
pub const VR_FAN_MIN_PCT: u8 = 40;
pub const ASIC_FAN_MIN_PCT: u8 = 25;

/// Core voltage the tuner pairs with a base clock: 1150 mV up to 500 MHz, rising linearly
/// to the factory 1250 mV at 777 MHz and on to the 1280 mV rail limit. 5 mV VID steps.
pub fn vcore_for_freq_mv(freq_mhz: f32) -> u16 {
    let f = if freq_mhz.is_finite() { freq_mhz } else { 500.0 };
    let mv = 1150.0 + (f - 500.0) * (100.0 / 277.0);
    let stepped = ((mv / 5.0).round() * 5.0) as i32;
    stepped.clamp(SAFE_MV as i32, MAX_VCORE_MV as i32) as u16
}

/// VRM temperature the VR fan works to hold (deg C).
pub const VR_TARGET_C: f32 = 35.0;
/// VR fan reaches 100% this many degrees above the target.
pub const VR_FAN_FULL_ABOVE_C: f32 = 10.0;

/// VR fan duty from VRM temperature: `VR_FAN_MIN_PCT` at or below `VR_TARGET_C`, rising
/// linearly to 100% at `VR_TARGET_C + 10`, 100% when the reading is missing. Keeps the
/// regulators as cool as the fan allows.
pub fn vr_fan_duty_pct(vr_temp_c: Option<f32>) -> u8 {
    let Some(t) = vr_temp_c.filter(|t| t.is_finite()) else {
        return 100;
    };
    if t <= VR_TARGET_C {
        return VR_FAN_MIN_PCT;
    }
    if t >= VR_TARGET_C + VR_FAN_FULL_ABOVE_C {
        return 100;
    }
    let span = (100 - VR_FAN_MIN_PCT) as f32;
    (VR_FAN_MIN_PCT as f32 + (t - VR_TARGET_C) / VR_FAN_FULL_ABOVE_C * span).round() as u8
}

/// NerdQX (BM1370) tuner profiles as `(id, label, hint)`. The dashboard builds its profile
/// list from these, so NerdQX never shows Nano 3S wording or targets.
pub const NERDQX_PROFILES: [(&str, &str, &str); 5] = [
    ("quiet", "Quiet", "400 MHz @ 1150 mV, 90 W cap, lowest fan noise"),
    ("efficient", "Efficient", "450 MHz @ 1120 mV, 110 W cap, best J/TH"),
    ("balanced", "Balanced", "512 MHz @ 1150 mV, capped by the PSU tier"),
    ("max_hash", "Max hash", "Climbs toward 825 MHz while dies stay under 58-61 C and under the PSU tier"),
    ("target", "Target (power / hashrate)", "Holds a power (W) or hashrate (TH/s) setpoint"),
];

/// Operating point the board starts mining at, by tuner profile: max-hash starts at the factory
/// 777 MHz / 1250 mV, every other profile at the efficient point (450 MHz / 1120 mV).
pub fn boot_point_for_profile(profile: &str) -> (u16, u16) {
    if profile == "max_hash" {
        (START_FREQ_MHZ, START_MV)
    } else {
        let (f, mv, _) = nerdqx_profile_goal("efficient", 0.0);
        (f as u16, mv)
    }
}

/// Goal `(base MHz, core mV, power cap W)` for a NerdQX tuner profile.
pub fn nerdqx_profile_goal(profile: &str, eff_ceiling_w: f32) -> (f32, u16, f32) {
    match profile {
        "quiet" => (400.0, 1100, 90.0),
        "efficient" => (450.0, 1120, 110.0),
        "max_hash" => (MAX_BASE_FREQ_MHZ, vcore_for_freq_mv(MAX_BASE_FREQ_MHZ), eff_ceiling_w),
        _ => (512.5, 1150, eff_ceiling_w),
    }
}

/// When the TMUX/TMP451 mux is not detected, clamp to a conservative operating point.
pub fn tmux_safe_clamp(tmux_ok: bool, freq_mhz: f32, mv: u32) -> (f32, u32) {
    if tmux_ok {
        return (freq_mhz, mv);
    }
    (
        if freq_mhz > SAFE_FREQ_MHZ { SAFE_FREQ_MHZ } else { freq_mhz },
        if mv > SAFE_MV { SAFE_MV } else { mv },
    )
}

#[derive(Clone, Copy, Debug)]
pub struct TuneInput {
    pub chip_temps_c: [f32; CHIPS],
    pub chip_hashrates_ghs: [f32; CHIPS],
    pub chip_error_pct: [f32; CHIPS],
    pub pin_w: f32,
    pub effective_ceiling_w: f32,
    pub vr_temp_c: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TuneOutput {
    pub offsets_mhz: [f32; CHIPS],
    /// Board over ceiling / VR hot: all chips were stepped down.
    pub board_limited: bool,
    /// Chip needing emergency shutdown (temp >= critical).
    pub critical: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ChipTuneEngine {
    steps: [i8; CHIPS],
}

impl ChipTuneEngine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn offsets_mhz(&self) -> [f32; CHIPS] {
        let mut o = [0.0; CHIPS];
        for i in 0..CHIPS {
            o[i] = self.steps[i] as f32 * PLL_STEP_MHZ;
        }
        o
    }

    pub fn reset(&mut self) {
        self.steps = [0; CHIPS];
    }

    fn bump(&mut self, i: usize, d: i8) {
        let v = self.steps[i] + d;
        self.steps[i] = v.clamp(-MAX_STEPS, MAX_STEPS);
    }

    /// One tuning iteration (one step of at most 6.25 MHz per chip).
    pub fn step(&mut self, inp: &TuneInput) -> TuneOutput {
        // Fail closed: non-finite board readings count as over-limit.
        let bad_board = !inp.pin_w.is_finite() || !inp.effective_ceiling_w.is_finite() || !inp.vr_temp_c.is_finite();
        let over_power = bad_board || inp.pin_w > inp.effective_ceiling_w;
        let near_power = bad_board || inp.pin_w > inp.effective_ceiling_w * 0.97;
        let vr_hot = bad_board || inp.vr_temp_c >= VR_TEMP_LIMIT_C;
        let board_limited = over_power || vr_hot;
        let mut critical = false;

        // Mean hashrate across live chips, for slice attribution of laggards.
        let mut sum = 0.0;
        for h in inp.chip_hashrates_ghs {
            sum += h;
        }
        let mean = sum / CHIPS as f32;

        for i in 0..CHIPS {
            let t = inp.chip_temps_c[i];
            let e = inp.chip_error_pct[i];
            if t >= TEMP_CRIT_C {
                critical = true;
            }
            let bad_chip = !t.is_finite() || !e.is_finite() || t >= TEMP_HOT_C || e > ERR_LIMIT_PCT;
            if board_limited || bad_chip {
                self.bump(i, -1);
            } else if near_power {
                // hold
            } else if t < TEMP_TARGET_C
                && e < ERR_GOOD_PCT
                && inp.chip_hashrates_ghs[i] >= mean * 0.9
            {
                self.bump(i, 1);
            }
        }
        TuneOutput { offsets_mhz: self.offsets_mhz(), board_limited, critical }
    }
}

