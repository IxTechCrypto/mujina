//! Test-only model of the Nano 3S hash chain: 12 A3197S chips in a series
//! voltage stack. Used to exercise the per-chip tuner without hardware.
//!
//! All chips carry the same current, and a chip draws current proportional
//! to clock times voltage, so each chip sees V_i = K / f_i. A chip clocked
//! lower therefore gets a larger share of the rail.

pub(crate) const SIM_CHIPS: usize = 12;

const CORES: f32 = 152.0;
const GHS_PER_CORE_MHZ: f32 = 0.0027;
/// Voltage a nominal chip needs: `NEED_BASE_MV + NEED_PER_MHZ * f`.
const NEED_BASE_MV: f32 = 200.0;
const NEED_PER_MHZ: f32 = 0.25;
/// Width of the fail-rate sigmoid, mV.
const FAIL_SLOPE_MV: f32 = 4.0;
const MIN_MHZ: f32 = 100.0;
/// Watts per (V * K): puts a uniform chain at 3678 mV / 348 MHz near 100 W.
const W_SCALE: f32 = 2.549e-4;

/// One steady-state measurement of the chain.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SimReading {
    pub chip_ghs: [f32; SIM_CHIPS],
    pub chip_fail_pct: [f32; SIM_CHIPS],
    pub chip_mv: [f32; SIM_CHIPS],
    pub board_ths: f32,
    pub board_w: f32,
}

pub(crate) struct ChainSim {
    strength_mv: [f32; SIM_CHIPS],
    rail_mv: f32,
    base_mhz: f32,
}

#[allow(dead_code)] // setters are used by the tuner tests
impl ChainSim {
    /// `strength_mv[i]`: how many mV less than a nominal chip chip i needs
    /// at a given clock (positive = strong, negative = weak).
    pub fn new(strength_mv: [f32; SIM_CHIPS], rail_mv: f32, base_mhz: f32) -> Self {
        Self { strength_mv, rail_mv, base_mhz }
    }

    pub fn set_rail_mv(&mut self, mv: f32) {
        self.rail_mv = mv;
    }

    pub fn set_base_mhz(&mut self, mhz: f32) {
        self.base_mhz = mhz;
    }

    /// Deterministic steady-state reading for these per-chip offsets.
    pub fn measure(&self, offsets_mhz: &[i16; SIM_CHIPS]) -> SimReading {
        let f: [f32; SIM_CHIPS] =
            std::array::from_fn(|i| (self.base_mhz + f32::from(offsets_mhz[i])).max(MIN_MHZ));
        let k = self.rail_mv / f.iter().map(|f| 1.0 / f).sum::<f32>();
        let mut r = SimReading {
            chip_ghs: [0.0; SIM_CHIPS],
            chip_fail_pct: [0.0; SIM_CHIPS],
            chip_mv: [0.0; SIM_CHIPS],
            board_ths: 0.0,
            board_w: self.rail_mv / 1000.0 * k * W_SCALE,
        };
        for i in 0..SIM_CHIPS {
            let v = k / f[i];
            let need = NEED_BASE_MV + NEED_PER_MHZ * f[i] - self.strength_mv[i];
            let p = (1.0 / (1.0 + ((v - need) / FAIL_SLOPE_MV).exp())).clamp(0.002, 1.0);
            r.chip_mv[i] = v;
            r.chip_fail_pct[i] = p * 100.0;
            r.chip_ghs[i] = CORES * f[i] * GHS_PER_CORE_MHZ * (1.0 - p);
        }
        r.board_ths = r.chip_ghs.iter().sum::<f32>() / 1000.0;
        r
    }

    /// Same, with seeded multiplicative noise of +/- `frac` on each
    /// chip_ghs and on board_w (xorshift, no external crates).
    pub fn measure_noisy(&self, offsets_mhz: &[i16; SIM_CHIPS], frac: f32, seed: u64) -> SimReading {
        let mut r = self.measure(offsets_mhz);
        let mut state = seed.max(1);
        let mut noise = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let u = (state >> 40) as f32 / (1u64 << 24) as f32;
            1.0 + frac * (2.0 * u - 1.0)
        };
        for g in &mut r.chip_ghs {
            *g *= noise();
        }
        r.board_w *= noise();
        r.board_ths = r.chip_ghs.iter().sum::<f32>() / 1000.0;
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ZERO: [i16; SIM_CHIPS] = [0; SIM_CHIPS];

    #[test]
    fn lowering_one_clock_shifts_voltage_to_it() {
        let sim = ChainSim::new([0.0; SIM_CHIPS], 3678.0, 348.0);
        let before = sim.measure(&ZERO);
        let mut off = ZERO;
        off[3] = -40;
        let after = sim.measure(&off);
        assert!(after.chip_mv[3] > before.chip_mv[3]);
        for i in (0..SIM_CHIPS).filter(|&i| i != 3) {
            assert!(after.chip_mv[i] < before.chip_mv[i]);
        }
    }

    #[test]
    fn chip_voltages_sum_to_rail() {
        let sim = ChainSim::new([0.0; SIM_CHIPS], 3678.0, 348.0);
        let mut off = ZERO;
        off[0] = 30;
        off[7] = -25;
        let r = sim.measure(&off);
        assert!((r.chip_mv.iter().sum::<f32>() - 3678.0).abs() < 0.5);
    }

    #[test]
    fn calibration() {
        let sim = ChainSim::new([0.0; SIM_CHIPS], 3678.0, 348.0);
        let r = sim.measure(&ZERO);
        assert!(r.chip_fail_pct.iter().all(|&p| p < 5.0));
        assert!((90.0..110.0).contains(&r.board_w), "{}", r.board_w);
        let mut s = [0.0; SIM_CHIPS];
        s[5] = -20.0;
        let r = ChainSim::new(s, 3678.0, 348.0).measure(&ZERO);
        assert!(r.chip_fail_pct[5] > 30.0);
    }

    #[test]
    fn noise_is_seeded_and_zero_frac_is_exact() {
        let sim = ChainSim::new([0.0; SIM_CHIPS], 3678.0, 348.0);
        assert_eq!(sim.measure_noisy(&ZERO, 0.0, 7), sim.measure(&ZERO));
        assert_eq!(sim.measure_noisy(&ZERO, 0.05, 7), sim.measure_noisy(&ZERO, 0.05, 7));
        assert_ne!(sim.measure_noisy(&ZERO, 0.05, 7), sim.measure_noisy(&ZERO, 0.05, 8));
    }
}
