//! Stage-2 per-chip parallel-rail PLL optimizer for 4x BM1370.

pub const CHIPS: usize = 4;
pub const PLL_STEP_MHZ: f32 = 6.25;
pub const PLL_MAX_OFFSET_MHZ: f32 = 50.0;
const MAX_STEPS: i8 = 8; // 8 * 6.25 = 50
pub const RAMP_START_MHZ: f32 = 56.25;
pub const SAFE_FREQ_MHZ: f32 = 495.0;
pub const SAFE_MV: u32 = 1150;

/// BM1370 die sweet spot / limits (deg C).
pub const TEMP_TARGET_C: f32 = 61.0;
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

