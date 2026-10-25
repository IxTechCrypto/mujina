//! Per-chip clock tuner for the Nano 3S chain, layered on the owner tuner.
//!
//! The 12 A3197S chips are a series voltage stack: a chip clocked lower draws
//! less and takes a larger share of the rail, so moving one chip moves every
//! other chip's voltage. The tuner therefore moves one chip at a time and
//! keeps a step only when the moved chip does more work AND the board as a
//! whole doesn't lose more than a small guard band. A clearly failing chip is
//! stepped down; otherwise the strongest low-fail chip is stepped up.
//!
//! Pure state machine (no hardware calls): the caller feeds one
//! [`ChipMetrics`] per tick while the owner tuner is locked, applies the
//! offsets [`ChipTuner::tick`] returns, and calls [`ChipTuner::interrupt`]
//! whenever the base point changes or ticking stops.

pub const CHIPS: usize = 12;

/// What a step is judged on at board level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChipGoal {
    /// Board hashrate must not drop more than the guard.
    Hashrate,
    /// Board TH/W must not drop more than the guard.
    Efficiency,
}

/// One tick of readings.
#[derive(Clone, Copy, Debug)]
pub struct ChipMetrics {
    pub chip_ghs: [f32; CHIPS],
    pub chip_fail_pct: [f32; CHIPS],
    pub board_ths: f32,
    pub board_w: f32,
    pub temp_max_c: f32,
}

#[derive(Clone, Copy, Debug)]
pub struct ChipTunerConfig {
    /// Ticks averaged per judgement (20 x 15 s = two SmartSpeed windows).
    pub settle_ticks: u32,
    pub step_mhz: i16,
    pub min_offset_mhz: i16,
    pub max_offset_mhz: i16,
    /// The moved chip must gain at least this fraction of its own work.
    pub chip_margin: f32,
    /// The board may lose at most this fraction of its hashrate (or TH/W).
    pub board_guard: f32,
    /// A chip is "failing" above this and above twice the median.
    pub weak_fail_pct: f32,
    /// Only chips below this fail rate are stepped up.
    pub strong_fail_pct: f32,
    /// Mean chip fail rate that reverts a trial at once.
    pub max_board_fail_pct: f32,
    /// Board power that reverts a trial at once.
    pub power_cap_w: f32,
    /// Hottest-chip shift that restarts the search after convergence.
    pub retune_temp_delta_c: f32,
    /// Settled windows after convergence before re-probing.
    pub reprobe_windows: u32,
}

impl Default for ChipTunerConfig {
    fn default() -> Self {
        Self {
            settle_ticks: 20,
            step_mhz: 10,
            min_offset_mhz: -60,
            max_offset_mhz: 40,
            chip_margin: 0.015,
            board_guard: 0.01,
            weak_fail_pct: 8.0,
            strong_fail_pct: 3.0,
            max_board_fail_pct: 10.0,
            power_cap_w: f32::INFINITY,
            retune_temp_delta_c: 5.0,
            reprobe_windows: 6,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChipPhase {
    /// Measuring the current offsets.
    Baseline,
    /// One chip moved; measuring the result.
    Trial { chip: usize, from: i16 },
    /// Nothing left to try; watching for a temperature shift.
    Converged,
}

#[derive(Clone, Copy, Default)]
struct Accum {
    n: u32,
    chip_ghs: [f32; CHIPS],
    chip_fail: [f32; CHIPS],
    ths: f32,
    w: f32,
    temp: f32,
}

impl Accum {
    fn add(&mut self, m: &ChipMetrics) {
        self.n += 1;
        for i in 0..CHIPS {
            self.chip_ghs[i] += m.chip_ghs[i];
            self.chip_fail[i] += m.chip_fail_pct[i];
        }
        self.ths += m.board_ths;
        self.w += m.board_w;
        self.temp += m.temp_max_c;
    }

    fn mean(&self) -> Window {
        let n = self.n.max(1) as f32;
        Window {
            chip_ghs: self.chip_ghs.map(|v| v / n),
            chip_fail: self.chip_fail.map(|v| v / n),
            ths: self.ths / n,
            w: self.w / n,
            temp: self.temp / n,
        }
    }
}

/// Means over one settle window.
#[derive(Clone, Copy, Debug)]
struct Window {
    chip_ghs: [f32; CHIPS],
    chip_fail: [f32; CHIPS],
    ths: f32,
    w: f32,
    temp: f32,
}

pub struct ChipTuner {
    cfg: ChipTunerConfig,
    goal: ChipGoal,
    offsets: [i16; CHIPS],
    phase: ChipPhase,
    acc: Accum,
    baseline: Option<Window>,
    /// Directions already tried and rejected since the last change.
    done_up: [bool; CHIPS],
    done_down: [bool; CHIPS],
    converged_windows: u32,
    ref_temp: Option<f32>,
    last_event: String,
}

impl ChipTuner {
    pub fn new(cfg: ChipTunerConfig, goal: ChipGoal, offsets: [i16; CHIPS]) -> Self {
        Self {
            cfg,
            goal,
            offsets,
            phase: ChipPhase::Baseline,
            acc: Accum::default(),
            baseline: None,
            done_up: [false; CHIPS],
            done_down: [false; CHIPS],
            converged_windows: 0,
            ref_temp: None,
            last_event: String::new(),
        }
    }

    pub fn offsets(&self) -> [i16; CHIPS] {
        self.offsets
    }

    pub fn phase(&self) -> ChipPhase {
        self.phase
    }

    pub fn last_event(&self) -> &str {
        &self.last_event
    }

    pub fn set_power_cap_w(&mut self, w: f32) {
        self.cfg.power_cap_w = w;
    }

    /// The base point changed or ticking stopped: drop the measurement, put a
    /// trial chip back, and search again from the current offsets. Returns
    /// offsets to apply when a trial was reverted.
    pub fn interrupt(&mut self) -> Option<[i16; CHIPS]> {
        let reverted = self.revert_trial();
        self.restart("interrupted");
        reverted.then_some(self.offsets)
    }

    pub fn tick(&mut self, m: &ChipMetrics) -> Option<[i16; CHIPS]> {
        // Fast safety: a trial that pushes the board unstable or over the
        // power cap is put back on this tick, not after the settle.
        if let ChipPhase::Trial { chip, .. } = self.phase {
            let board_fail = m.chip_fail_pct.iter().sum::<f32>() / CHIPS as f32;
            if board_fail > self.cfg.max_board_fail_pct || m.board_w > self.cfg.power_cap_w {
                let up = self.offsets[chip] > self.trial_from();
                self.revert_trial();
                self.mark_done(chip, up);
                self.last_event = format!(
                    "chip {chip}: reverted at once (fail {board_fail:.1}%, {:.1} W)",
                    m.board_w
                );
                self.restart_keeping_marks();
                return Some(self.offsets);
            }
        }

        self.acc.add(m);
        if self.acc.n < self.cfg.settle_ticks {
            return None;
        }
        let win = self.acc.mean();
        self.acc = Accum::default();

        match self.phase {
            ChipPhase::Baseline => {
                self.baseline = Some(win);
                self.ref_temp.get_or_insert(win.temp);
                self.start_next_trial(&win)
            }
            ChipPhase::Trial { chip, from } => self.judge(chip, from, win),
            ChipPhase::Converged => {
                self.converged_windows += 1;
                let drifted = self
                    .ref_temp
                    .is_some_and(|t| (win.temp - t).abs() > self.cfg.retune_temp_delta_c);
                if drifted || self.converged_windows >= self.cfg.reprobe_windows {
                    let why = if drifted { "temperature shifted" } else { "re-probing" };
                    self.restart(why);
                    // This window doubles as the new baseline.
                    self.baseline = Some(win);
                    self.ref_temp = Some(win.temp);
                    return self.start_next_trial(&win);
                }
                None
            }
        }
    }

    fn judge(&mut self, chip: usize, from: i16, win: Window) -> Option<[i16; CHIPS]> {
        let base = self.baseline.expect("trial always follows a baseline");
        let up = self.offsets[chip] > from;
        let chip_gain = win.chip_ghs[chip] / base.chip_ghs[chip].max(1e-3) - 1.0;
        let board_ratio = match self.goal {
            ChipGoal::Hashrate => win.ths / base.ths.max(1e-6),
            ChipGoal::Efficiency => (win.ths / win.w.max(1e-3)) / (base.ths / base.w.max(1e-3)).max(1e-9),
        };
        let keep = chip_gain >= self.cfg.chip_margin && board_ratio >= 1.0 - self.cfg.board_guard;
        if keep {
            self.last_event = format!(
                "chip {chip}: kept {:+} MHz (chip {:+.1}%, board {:+.1}%)",
                self.offsets[chip],
                chip_gain * 100.0,
                (board_ratio - 1.0) * 100.0
            );
            // The voltage share moved, so other chips get fresh chances; this
            // chip may not immediately undo the step.
            self.done_up = [false; CHIPS];
            self.done_down = [false; CHIPS];
            self.mark_done(chip, !up);
            self.baseline = Some(win);
            self.ref_temp = Some(win.temp);
            return self.start_next_trial(&win).or(Some(self.offsets));
        }
        self.offsets[chip] = from;
        self.mark_done(chip, up);
        self.last_event = format!(
            "chip {chip}: reverted to {from:+} MHz (chip {:+.1}%, board {:+.1}%)",
            chip_gain * 100.0,
            (board_ratio - 1.0) * 100.0
        );
        // Re-measure at the restored offsets before the next trial.
        self.phase = ChipPhase::Baseline;
        Some(self.offsets)
    }

    /// Picks the next chip to move and applies the step, or converges.
    fn start_next_trial(&mut self, win: &Window) -> Option<[i16; CHIPS]> {
        let Some((chip, step)) = self.pick(win) else {
            if self.phase != ChipPhase::Converged {
                self.last_event = format!("converged at {:?}", self.offsets);
            }
            self.phase = ChipPhase::Converged;
            self.converged_windows = 0;
            return None;
        };
        let from = self.offsets[chip];
        self.offsets[chip] = from + step;
        self.phase = ChipPhase::Trial { chip, from };
        self.last_event = format!("chip {chip}: trying {:+} MHz", self.offsets[chip]);
        Some(self.offsets)
    }

    fn pick(&self, win: &Window) -> Option<(usize, i16)> {
        let step = self.cfg.step_mhz;
        let mut sorted = win.chip_fail;
        sorted.sort_by(f32::total_cmp);
        let median = (sorted[CHIPS / 2 - 1] + sorted[CHIPS / 2]) / 2.0;

        let weak = (0..CHIPS)
            .filter(|&i| !self.done_down[i] && self.offsets[i] - step >= self.cfg.min_offset_mhz)
            .filter(|&i| win.chip_fail[i] > self.cfg.weak_fail_pct && win.chip_fail[i] > 2.0 * median)
            .max_by(|&a, &b| win.chip_fail[a].total_cmp(&win.chip_fail[b]));
        if let Some(i) = weak {
            return Some((i, -step));
        }
        (0..CHIPS)
            .filter(|&i| !self.done_up[i] && self.offsets[i] + step <= self.cfg.max_offset_mhz)
            .filter(|&i| win.chip_fail[i] < self.cfg.strong_fail_pct)
            .min_by(|&a, &b| {
                win.chip_fail[a]
                    .total_cmp(&win.chip_fail[b])
                    .then(win.chip_ghs[b].total_cmp(&win.chip_ghs[a]))
            })
            .map(|i| (i, step))
    }

    fn trial_from(&self) -> i16 {
        match self.phase {
            ChipPhase::Trial { from, .. } => from,
            _ => 0,
        }
    }

    /// Puts a trial chip back; true when one was reverted.
    fn revert_trial(&mut self) -> bool {
        if let ChipPhase::Trial { chip, from } = self.phase {
            self.offsets[chip] = from;
            return true;
        }
        false
    }

    fn mark_done(&mut self, chip: usize, up: bool) {
        if up {
            self.done_up[chip] = true;
        } else {
            self.done_down[chip] = true;
        }
    }

    fn restart(&mut self, why: &str) {
        self.done_up = [false; CHIPS];
        self.done_down = [false; CHIPS];
        self.ref_temp = None;
        self.restart_keeping_marks();
        self.last_event = why.to_owned();
    }

    fn restart_keeping_marks(&mut self) {
        self.phase = ChipPhase::Baseline;
        self.acc = Accum::default();
        self.baseline = None;
        self.converged_windows = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::nano3s_chain_sim::{ChainSim, SimReading};

    /// One weak chip and a spread like the unit's: 5 weakest, then 7-9.
    const UNIT: [f32; CHIPS] = [0.0, 4.0, 0.0, -6.0, 2.0, -20.0, 6.0, -14.0, -14.0, -14.0, 3.0, 8.0];

    fn metrics(r: &SimReading) -> ChipMetrics {
        ChipMetrics {
            chip_ghs: r.chip_ghs,
            chip_fail_pct: r.chip_fail_pct,
            board_ths: r.board_ths,
            board_w: r.board_w,
            temp_max_c: 77.0,
        }
    }

    fn flat(fail: f32) -> ChipMetrics {
        ChipMetrics {
            chip_ghs: [140.0; CHIPS],
            chip_fail_pct: [fail; CHIPS],
            board_ths: 1.68,
            board_w: 100.0,
            temp_max_c: 77.0,
        }
    }

    /// Feeds `ticks` noisy readings, applying whatever offsets come back.
    /// Returns true if the tuner converged at least once.
    fn run(t: &mut ChipTuner, sim: &ChainSim, ticks: u32, seed: u64) -> bool {
        let mut converged = false;
        for k in 0..ticks {
            let r = sim.measure_noisy(&t.offsets(), 0.02, seed + k as u64);
            t.tick(&metrics(&r));
            converged |= t.phase() == ChipPhase::Converged;
        }
        converged
    }

    fn settle(t: &mut ChipTuner, m: &ChipMetrics) -> Option<[i16; CHIPS]> {
        (0..t.cfg.settle_ticks).fold(None, |_, _| t.tick(m))
    }

    #[test]
    fn a_clearly_failing_chip_is_stepped_down_first() {
        let mut t = ChipTuner::new(ChipTunerConfig::default(), ChipGoal::Hashrate, [0; CHIPS]);
        let mut m = flat(1.0);
        m.chip_fail_pct[5] = 40.0;
        let o = settle(&mut t, &m).expect("a trial starts");
        assert_eq!(o[5], -10);
        assert_eq!(t.phase(), ChipPhase::Trial { chip: 5, from: 0 });
    }

    #[test]
    fn with_no_failing_chip_the_strongest_steps_up() {
        let mut t = ChipTuner::new(ChipTunerConfig::default(), ChipGoal::Hashrate, [0; CHIPS]);
        let mut m = flat(2.0);
        m.chip_fail_pct[8] = 0.5;
        let o = settle(&mut t, &m).expect("a trial starts");
        assert_eq!(o[8], 10);
    }

    #[test]
    fn a_trial_that_costs_the_board_is_reverted() {
        let mut t = ChipTuner::new(ChipTunerConfig::default(), ChipGoal::Hashrate, [0; CHIPS]);
        let mut m = flat(2.0);
        m.chip_fail_pct[8] = 0.5;
        settle(&mut t, &m);
        // Chip 8 gains 3 %, but the stolen voltage costs the others 2 %.
        let mut after = m;
        after.chip_ghs = [137.2; CHIPS];
        after.chip_ghs[8] = 144.2;
        after.board_ths = after.chip_ghs.iter().sum::<f32>() / 1000.0;
        let o = settle(&mut t, &after).expect("reverted");
        assert_eq!(o[8], 0);
        assert!(t.last_event().contains("reverted"), "{}", t.last_event());
    }

    #[test]
    fn an_unstable_or_over_cap_trial_is_reverted_on_the_next_tick() {
        for (fail, watts) in [(50.0, 100.0), (2.0, 130.0)] {
            let cfg = ChipTunerConfig { power_cap_w: 110.0, ..Default::default() };
            let mut t = ChipTuner::new(cfg, ChipGoal::Hashrate, [0; CHIPS]);
            let mut m = flat(2.0);
            m.chip_fail_pct[3] = 0.5;
            settle(&mut t, &m).expect("trial on chip 3");
            let bad = ChipMetrics { chip_fail_pct: [fail; CHIPS], board_w: watts, ..m };
            assert_eq!(t.tick(&bad), Some([0; CHIPS]));
            assert!(t.last_event().contains("at once"), "{}", t.last_event());
            assert_eq!(t.phase(), ChipPhase::Baseline);
        }
    }

    #[test]
    fn interrupt_puts_the_trial_chip_back() {
        let mut t = ChipTuner::new(ChipTunerConfig::default(), ChipGoal::Hashrate, [0; CHIPS]);
        let mut m = flat(1.0);
        m.chip_fail_pct[5] = 40.0;
        settle(&mut t, &m);
        assert_eq!(t.interrupt(), Some([0; CHIPS]));
        assert_eq!(t.phase(), ChipPhase::Baseline);
        assert_eq!(t.interrupt(), None, "nothing to revert outside a trial");
    }

    #[test]
    fn a_temperature_shift_restarts_a_converged_search() {
        let cfg = ChipTunerConfig { reprobe_windows: 1000, ..Default::default() };
        let mut t = ChipTuner::new(cfg, ChipGoal::Hashrate, [0; CHIPS]);
        let m = flat(5.0); // nobody failing, nobody strong: nothing to try
        settle(&mut t, &m);
        assert_eq!(t.phase(), ChipPhase::Converged);
        let mut hot = flat(5.0);
        hot.temp_max_c = 83.0;
        hot.chip_fail_pct[2] = 1.0;
        let o = settle(&mut t, &hot).expect("searching again");
        assert_eq!(o[2], 10);
    }

    #[test]
    fn on_the_simulated_unit_it_beats_uniform_clocks() {
        let sim = ChainSim::new(UNIT, 3678.0, 348.0);
        let uniform = sim.measure(&[0; CHIPS]);
        let mut t = ChipTuner::new(ChipTunerConfig::default(), ChipGoal::Hashrate, [0; CHIPS]);
        assert!(run(&mut t, &sim, 3000, 1), "never converged");
        let tuned = sim.measure(&t.offsets());
        let o = t.offsets();
        assert!(tuned.board_ths > uniform.board_ths * 1.03, "{} vs {} at {o:?}", tuned.board_ths, uniform.board_ths);
        assert!(o[5] < 0, "weakest chip slowed: {o:?}");
        assert!(o.iter().all(|&x| (-60..=40).contains(&x)), "{o:?}");
    }

    #[test]
    fn on_an_even_chain_it_does_no_harm() {
        let sim = ChainSim::new([0.0; CHIPS], 3678.0, 348.0);
        let uniform = sim.measure(&[0; CHIPS]);
        let mut t = ChipTuner::new(ChipTunerConfig::default(), ChipGoal::Hashrate, [0; CHIPS]);
        run(&mut t, &sim, 3000, 7);
        let tuned = sim.measure(&t.offsets());
        assert!(tuned.board_ths >= uniform.board_ths * 0.99, "{} vs {}", tuned.board_ths, uniform.board_ths);
    }

    #[test]
    fn once_tuned_it_does_not_drift_down() {
        let sim = ChainSim::new(UNIT, 3678.0, 348.0);
        let mut t = ChipTuner::new(ChipTunerConfig::default(), ChipGoal::Hashrate, [0; CHIPS]);
        run(&mut t, &sim, 3000, 11);
        let first = sim.measure(&t.offsets()).board_ths;
        run(&mut t, &sim, 3000, 50_000);
        let later = sim.measure(&t.offsets()).board_ths;
        assert!(later >= first * 0.99, "{later} after {first}");
    }
}
