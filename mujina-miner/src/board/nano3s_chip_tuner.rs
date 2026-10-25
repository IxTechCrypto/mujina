//! Per-chip clock tuner for the Nano 3S chain, layered on the owner tuner.
//!
//! The 12 A3197S chips are a series voltage stack: a chip clocked lower draws
//! less and takes a larger share of the rail, so moving one chip moves every
//! other chip's voltage. The tuner therefore moves one chip at a time and
//! keeps a step only when the moved chip does more work AND the board as a
//! whole doesn't lose more than a small guard band, per step and in total
//! since the search started. A clearly failing chip is stepped down;
//! otherwise the strongest low-fail chip is stepped up.
//!
//! Pure state machine (no hardware calls): the caller feeds one
//! [`ChipMetrics`] per tick while the owner tuner is locked, makes the
//! firmware match [`ChipTuner::offsets`] (retrying failed sends), persists
//! only [`ChipTuner::committed_offsets`] (never an unjudged trial), and calls
//! [`ChipTuner::interrupt`] whenever the base point changes or ticking stops.

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
    /// Ticks ignored after each offset change: the per-chip readings are
    /// SmartSpeed windows (~131 s), so the first ones still show the old
    /// offsets.
    pub discard_ticks: u32,
    pub step_mhz: i16,
    pub min_offset_mhz: i16,
    pub max_offset_mhz: i16,
    /// The moved chip must gain at least this fraction of its own work.
    pub chip_margin: f32,
    /// One step may cost the board at most this fraction of its hashrate
    /// (or TH/W)...
    pub board_guard: f32,
    /// ...and all kept steps together at most this, against the board when
    /// the search started.
    pub total_guard: f32,
    /// A chip is "failing" above this and above twice the median.
    pub weak_fail_pct: f32,
    /// Only chips below this fail rate are stepped up.
    pub strong_fail_pct: f32,
    /// Mean chip fail rate that reverts a trial at once, or restarts a
    /// converged search.
    pub max_board_fail_pct: f32,
    /// Board power that reverts a trial at once; no step-ups above
    /// `up_power_fraction` of it.
    pub power_cap_w: f32,
    pub up_power_fraction: f32,
    /// Hottest-chip shift that restarts the search after convergence.
    pub retune_temp_delta_c: f32,
    /// Settled windows after convergence before step-ups are retried;
    /// doubles each time a retry finds nothing, up to `max_reprobe_windows`.
    pub reprobe_windows: u32,
    pub max_reprobe_windows: u32,
}

impl Default for ChipTunerConfig {
    fn default() -> Self {
        Self {
            settle_ticks: 20,
            discard_ticks: 8,
            step_mhz: 10,
            min_offset_mhz: -60,
            max_offset_mhz: 40,
            chip_margin: 0.015,
            board_guard: 0.01,
            total_guard: 0.02,
            weak_fail_pct: 8.0,
            strong_fail_pct: 3.0,
            max_board_fail_pct: 10.0,
            power_cap_w: f32::INFINITY,
            up_power_fraction: 0.97,
            retune_temp_delta_c: 5.0,
            reprobe_windows: 6,
            max_reprobe_windows: 48,
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

/// A chip reading below this (GH/s) is treated as missing or dead.
const DEAD_GHS: f32 = 1.0;

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
    /// Ticks still to ignore after the last offset change.
    discard: u32,
    baseline: Option<Window>,
    /// The board when this search started (for `total_guard`).
    reference: Option<Window>,
    /// Directions already tried and rejected since the last change.
    done_up: [bool; CHIPS],
    done_down: [bool; CHIPS],
    converged_windows: u32,
    reprobe_after: u32,
    ref_temp: Option<f32>,
    last_event: String,
}

impl ChipTuner {
    pub fn new(cfg: ChipTunerConfig, goal: ChipGoal, offsets: [i16; CHIPS]) -> Self {
        let offsets = offsets.map(|o| o.clamp(cfg.min_offset_mhz, cfg.max_offset_mhz));
        Self {
            cfg,
            goal,
            offsets,
            phase: ChipPhase::Baseline,
            acc: Accum::default(),
            discard: 0,
            baseline: None,
            reference: None,
            done_up: [false; CHIPS],
            done_down: [false; CHIPS],
            converged_windows: 0,
            reprobe_after: cfg.reprobe_windows,
            ref_temp: None,
            last_event: String::new(),
        }
    }

    /// The offsets the firmware should be running now (a trial included).
    pub fn offsets(&self) -> [i16; CHIPS] {
        self.offsets
    }

    /// The offsets to persist: a trial chip counts at its pre-trial value.
    pub fn committed_offsets(&self) -> [i16; CHIPS] {
        let mut o = self.offsets;
        if let ChipPhase::Trial { chip, from } = self.phase {
            o[chip] = from;
        }
        o
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

    /// The base point changed or ticking stopped: put a trial chip back,
    /// drop the measurement, and search again from the committed offsets.
    pub fn interrupt(&mut self) {
        if self.revert_trial() {
            self.discard = self.cfg.discard_ticks;
        }
        self.restart("interrupted");
    }

    /// Feeds one tick. Returns true when [`Self::offsets`] changed.
    pub fn tick(&mut self, m: &ChipMetrics) -> bool {
        let board_fail = m.chip_fail_pct.iter().sum::<f32>() / CHIPS as f32;
        // Fast safety: a trial that pushes the board unstable or over the
        // power cap is put back on this tick, not after the settle.
        if let ChipPhase::Trial { chip, from } = self.phase
            && (board_fail > self.cfg.max_board_fail_pct || m.board_w > self.cfg.power_cap_w)
        {
            let up = self.offsets[chip] > from;
            self.offsets[chip] = from;
            self.mark_done(chip, up);
            self.restart_keeping_marks();
            self.discard = self.cfg.discard_ticks;
            self.last_event = format!(
                "chip {chip}: reverted at once (fail {board_fail:.1}%, {:.1} W)",
                m.board_w
            );
            return true;
        }
        // A converged board that goes unstable searches again (the weak
        // chips get stepped down).
        if self.phase == ChipPhase::Converged && board_fail > self.cfg.max_board_fail_pct {
            self.restart("board unstable while converged");
            return false;
        }
        if self.discard > 0 {
            self.discard -= 1;
            return false;
        }

        self.acc.add(m);
        if self.acc.n < self.cfg.settle_ticks {
            return false;
        }
        let win = self.acc.mean();
        self.acc = Accum::default();

        match self.phase {
            ChipPhase::Baseline => {
                self.baseline = Some(win);
                self.reference.get_or_insert(win);
                self.ref_temp.get_or_insert(win.temp);
                self.start_next_trial(&win)
            }
            ChipPhase::Trial { chip, from } => self.judge(chip, from, win),
            ChipPhase::Converged => {
                self.converged_windows += 1;
                if self.ref_temp.is_some_and(|t| (win.temp - t).abs() > self.cfg.retune_temp_delta_c) {
                    self.restart("temperature shifted");
                    self.baseline = Some(win);
                    self.reference = Some(win);
                    self.ref_temp = Some(win.temp);
                    return self.start_next_trial(&win);
                }
                if self.converged_windows >= self.reprobe_after {
                    // Conditions may have moved a little: step-ups get
                    // another chance; rejected step-downs stay rejected.
                    self.done_up = [false; CHIPS];
                    self.converged_windows = 0;
                    self.reprobe_after = (self.reprobe_after * 2).min(self.cfg.max_reprobe_windows);
                    self.baseline = Some(win);
                    self.last_event = "re-probing step-ups".to_owned();
                    return self.start_next_trial(&win);
                }
                false
            }
        }
    }

    fn board_ratio(&self, win: &Window, base: &Window) -> f32 {
        match self.goal {
            ChipGoal::Hashrate => win.ths / base.ths.max(1e-6),
            ChipGoal::Efficiency => (win.ths / win.w.max(1e-3)) / (base.ths / base.w.max(1e-3)).max(1e-9),
        }
    }

    fn judge(&mut self, chip: usize, from: i16, win: Window) -> bool {
        let base = self.baseline.expect("trial always follows a baseline");
        let reference = self.reference.unwrap_or(base);
        let up = self.offsets[chip] > from;
        let chip_gain = win.chip_ghs[chip] / base.chip_ghs[chip].max(DEAD_GHS) - 1.0;
        let step = self.board_ratio(&win, &base);
        let total = self.board_ratio(&win, &reference);
        let keep = chip_gain >= self.cfg.chip_margin
            && step >= 1.0 - self.cfg.board_guard
            && total >= 1.0 - self.cfg.total_guard;
        if keep {
            let kept = format!(
                "chip {chip}: kept {:+} MHz (chip {:+.1}%, board {:+.1}%)",
                self.offsets[chip],
                chip_gain * 100.0,
                (step - 1.0) * 100.0
            );
            // The voltage share moved, so other chips get fresh chances; this
            // chip may not immediately undo the step.
            self.done_up = [false; CHIPS];
            self.done_down = [false; CHIPS];
            self.mark_done(chip, !up);
            self.reprobe_after = self.cfg.reprobe_windows;
            self.baseline = Some(win);
            self.ref_temp = Some(win.temp);
            self.phase = ChipPhase::Baseline;
            self.start_next_trial(&win);
            self.last_event = format!("{kept}; {}", self.last_event);
            return true; // the trial value is now committed
        }
        self.offsets[chip] = from;
        self.mark_done(chip, up);
        self.discard = self.cfg.discard_ticks;
        self.last_event = format!(
            "chip {chip}: reverted to {from:+} MHz (chip {:+.1}%, board {:+.1}%)",
            chip_gain * 100.0,
            (step - 1.0) * 100.0
        );
        // Re-measure at the restored offsets before the next trial.
        self.phase = ChipPhase::Baseline;
        true
    }

    /// Picks the next chip to move and applies the step, or converges.
    /// Returns true when an offset changed.
    fn start_next_trial(&mut self, win: &Window) -> bool {
        let Some((chip, step)) = self.pick(win) else {
            if self.phase != ChipPhase::Converged {
                self.last_event = format!("converged at {:?}", self.offsets);
            }
            self.phase = ChipPhase::Converged;
            self.converged_windows = 0;
            return false;
        };
        let from = self.offsets[chip];
        self.offsets[chip] = from + step;
        self.phase = ChipPhase::Trial { chip, from };
        self.discard = self.cfg.discard_ticks;
        self.last_event = format!("chip {chip}: trying {:+} MHz", self.offsets[chip]);
        true
    }

    fn pick(&self, win: &Window) -> Option<(usize, i16)> {
        let step = self.cfg.step_mhz;
        let alive: Vec<usize> = (0..CHIPS).filter(|&i| win.chip_ghs[i] >= DEAD_GHS).collect();
        if alive.len() < 2 {
            return None;
        }
        let mut fails: Vec<f32> = alive.iter().map(|&i| win.chip_fail[i]).collect();
        fails.sort_by(f32::total_cmp);
        let median = fails[fails.len() / 2];

        let weak = alive
            .iter()
            .copied()
            .filter(|&i| !self.done_down[i] && self.offsets[i] - step >= self.cfg.min_offset_mhz)
            .filter(|&i| win.chip_fail[i] > self.cfg.weak_fail_pct && win.chip_fail[i] > 2.0 * median)
            .max_by(|&a, &b| win.chip_fail[a].total_cmp(&win.chip_fail[b]));
        if let Some(i) = weak {
            return Some((i, -step));
        }
        if win.w > self.cfg.power_cap_w * self.cfg.up_power_fraction {
            return None;
        }
        alive
            .iter()
            .copied()
            .filter(|&i| !self.done_up[i] && self.offsets[i] + step <= self.cfg.max_offset_mhz)
            .filter(|&i| win.chip_fail[i] < self.cfg.strong_fail_pct)
            .min_by(|&a, &b| {
                win.chip_fail[a]
                    .total_cmp(&win.chip_fail[b])
                    .then(win.chip_ghs[b].total_cmp(&win.chip_ghs[a]))
            })
            .map(|i| (i, step))
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
        self.reference = None;
        self.reprobe_after = self.cfg.reprobe_windows;
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

    /// A spread like the unit's: chip 5 weakest, then 7-9.
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

    fn tuner(cfg: ChipTunerConfig) -> ChipTuner {
        ChipTuner::new(cfg, ChipGoal::Hashrate, [0; CHIPS])
    }

    /// Feeds noisy simulator readings; true if it converged at least once.
    fn run(t: &mut ChipTuner, sim: &ChainSim, ticks: u32, seed: u64) -> bool {
        let mut converged = false;
        for k in 0..ticks {
            let r = sim.measure_noisy(&t.offsets(), 0.02, seed + k as u64);
            t.tick(&metrics(&r));
            converged |= t.phase() == ChipPhase::Converged;
        }
        converged
    }

    /// Feeds one full window (discard + settle) of the same reading.
    fn settle(t: &mut ChipTuner, m: &ChipMetrics) -> bool {
        let n = t.cfg.discard_ticks + t.cfg.settle_ticks;
        (0..n).fold(false, |changed, _| t.tick(m) | changed)
    }

    #[test]
    fn a_clearly_failing_chip_is_stepped_down_first() {
        let mut t = tuner(ChipTunerConfig::default());
        let mut m = flat(1.0);
        m.chip_fail_pct[5] = 40.0;
        assert!(settle(&mut t, &m));
        assert_eq!(t.offsets()[5], -10);
        assert_eq!(t.phase(), ChipPhase::Trial { chip: 5, from: 0 });
        // A trial is never what gets persisted.
        assert_eq!(t.committed_offsets(), [0; CHIPS]);
    }

    #[test]
    fn with_no_failing_chip_the_strongest_steps_up() {
        let mut t = tuner(ChipTunerConfig::default());
        let mut m = flat(2.0);
        m.chip_fail_pct[8] = 0.5;
        settle(&mut t, &m);
        assert_eq!(t.offsets()[8], 10);
    }

    #[test]
    fn a_dead_or_missing_chip_is_never_picked() {
        let mut t = tuner(ChipTunerConfig::default());
        let mut m = flat(2.0);
        m.chip_ghs[4] = 0.0;
        m.chip_fail_pct[4] = 0.0; // looks "strongest" by fail rate alone
        settle(&mut t, &m);
        assert_eq!(t.offsets()[4], 0);
    }

    #[test]
    fn a_trial_that_costs_the_board_is_reverted() {
        let mut t = tuner(ChipTunerConfig::default());
        let mut m = flat(2.0);
        m.chip_fail_pct[8] = 0.5;
        settle(&mut t, &m);
        // Chip 8 gains 3 %, but the stolen voltage costs the others 2 %.
        let mut after = m;
        after.chip_ghs = [137.2; CHIPS];
        after.chip_ghs[8] = 144.2;
        after.board_ths = after.chip_ghs.iter().sum::<f32>() / 1000.0;
        assert!(settle(&mut t, &after));
        assert_eq!(t.offsets()[8], 0);
        assert!(t.last_event().contains("reverted"), "{}", t.last_event());
    }

    #[test]
    fn small_losses_cannot_add_up_past_the_total_guard() {
        let mut t = tuner(ChipTunerConfig::default());
        let mut m = flat(2.0);
        m.chip_fail_pct[8] = 0.5;
        settle(&mut t, &m); // baseline, chip 8 trial starts
        // Every trial: chip 8 +2 %, board -0.8 %. Each passes the 1 % step
        // guard; the third would take the board past 2 % in total.
        for _ in 0..3 {
            m.chip_ghs[8] *= 1.02;
            m.board_ths *= 0.992;
            settle(&mut t, &m);
        }
        assert_eq!(t.committed_offsets()[8], 20, "{}", t.last_event());
    }

    #[test]
    fn efficiency_goal_rejects_a_step_that_costs_th_per_watt() {
        let mut t = ChipTuner::new(ChipTunerConfig::default(), ChipGoal::Efficiency, [0; CHIPS]);
        let mut m = flat(2.0);
        m.chip_fail_pct[8] = 0.5;
        settle(&mut t, &m);
        let mut after = m;
        after.chip_ghs[8] *= 1.03; // the chip gains
        after.board_ths *= 1.003; // the board barely does
        after.board_w *= 1.03; // and it costs 3 % more power
        settle(&mut t, &after);
        assert_eq!(t.offsets()[8], 0, "{}", t.last_event());
    }

    #[test]
    fn no_step_ups_near_the_power_cap() {
        let cfg = ChipTunerConfig { power_cap_w: 100.0, ..Default::default() };
        let mut t = tuner(cfg);
        let mut m = flat(2.0);
        m.board_w = 98.0; // over 97 % of the cap
        m.chip_fail_pct[8] = 0.5;
        assert!(!settle(&mut t, &m));
        assert_eq!(t.phase(), ChipPhase::Converged);
    }

    #[test]
    fn an_unstable_or_over_cap_trial_is_reverted_on_the_next_tick() {
        for (fail, watts) in [(50.0, 95.0), (2.0, 130.0)] {
            let cfg = ChipTunerConfig { power_cap_w: 110.0, ..Default::default() };
            let mut t = tuner(cfg);
            let mut m = flat(2.0);
            m.board_w = 95.0;
            m.chip_fail_pct[3] = 0.5;
            settle(&mut t, &m);
            assert_eq!(t.offsets()[3], 10);
            let bad = ChipMetrics { chip_fail_pct: [fail; CHIPS], board_w: watts, ..m };
            assert!(t.tick(&bad));
            assert_eq!(t.offsets(), [0; CHIPS]);
            assert!(t.last_event().contains("at once"), "{}", t.last_event());
            assert_eq!(t.phase(), ChipPhase::Baseline);
        }
    }

    #[test]
    fn interrupt_puts_the_trial_chip_back() {
        let mut t = tuner(ChipTunerConfig::default());
        let mut m = flat(1.0);
        m.chip_fail_pct[5] = 40.0;
        settle(&mut t, &m);
        t.interrupt();
        assert_eq!(t.offsets(), [0; CHIPS]);
        assert_eq!(t.committed_offsets(), [0; CHIPS]);
        assert_eq!(t.phase(), ChipPhase::Baseline);
    }

    #[test]
    fn a_temperature_shift_restarts_a_converged_search() {
        let cfg = ChipTunerConfig { reprobe_windows: 1000, ..Default::default() };
        let mut t = tuner(cfg);
        let m = flat(5.0); // nobody failing, nobody strong: nothing to try
        settle(&mut t, &m);
        assert_eq!(t.phase(), ChipPhase::Converged);
        let mut hot = flat(5.0);
        hot.temp_max_c = 83.0;
        hot.chip_fail_pct[2] = 1.0;
        // The first window still averages in pre-shift ticks; the second is
        // all hot and restarts the search.
        settle(&mut t, &hot);
        assert!(settle(&mut t, &hot));
        assert_eq!(t.offsets()[2], 10);
    }

    #[test]
    fn on_the_simulated_unit_it_beats_uniform_clocks() {
        let sim = ChainSim::new(UNIT, 3678.0, 348.0);
        let uniform = sim.measure(&[0; CHIPS]);
        let mut t = tuner(ChipTunerConfig::default());
        assert!(run(&mut t, &sim, 4000, 1), "never converged");
        let o = t.committed_offsets();
        let tuned = sim.measure(&o);
        assert!(tuned.board_ths > uniform.board_ths * 1.03, "{} vs {} at {o:?}", tuned.board_ths, uniform.board_ths);
        assert!(o[5] < 0, "weakest chip slowed: {o:?}");
        assert!(o.iter().all(|&x| (-60..=40).contains(&x)), "{o:?}");
    }

    #[test]
    fn on_an_even_chain_it_does_no_harm() {
        let sim = ChainSim::new([0.0; CHIPS], 3678.0, 348.0);
        let uniform = sim.measure(&[0; CHIPS]);
        let mut t = tuner(ChipTunerConfig::default());
        run(&mut t, &sim, 4000, 7);
        let tuned = sim.measure(&t.committed_offsets());
        assert!(tuned.board_ths >= uniform.board_ths * 0.99, "{} vs {}", tuned.board_ths, uniform.board_ths);
    }

    #[test]
    fn once_tuned_it_does_not_drift_down() {
        let sim = ChainSim::new(UNIT, 3678.0, 348.0);
        let mut t = tuner(ChipTunerConfig::default());
        run(&mut t, &sim, 4000, 11);
        let first = sim.measure(&t.committed_offsets()).board_ths;
        run(&mut t, &sim, 4000, 50_000);
        let later = sim.measure(&t.committed_offsets()).board_ths;
        assert!(later >= first * 0.99, "{later} after {first}");
    }
}
