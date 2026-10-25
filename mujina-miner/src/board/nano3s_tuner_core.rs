//! Owner autotuner core, copied from IxTechCrypto/mujina
//! `mujina-miner/src/api/autotune.rs` (commit a607f1f) with only the pieces that
//! cannot compile in nano-mujina removed (API/BoardCommand/telemetry glue,
//! snapshot_boards, the AutoTuners map, persistence, ChipProfile lookups). See
//! docs/nano3s-efficient-tuner-plan.md. The decision logic is a pure function
//! of [`Metrics`] and tuner state, so it is unit-tested without hardware.
//!
//! Prior art credits are in the original file's header.
#![allow(dead_code)]

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Hard limits for one profile, before the board's chip count scales the
/// power cap.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CapLimits {
    pub temp_c: f32,
    pub power_w: f32,
}

/// Everything board-specific the tuner needs: frequency and voltage range
/// and step, voltages it must never command, settle timing, and the
/// thermal/power cap of each profile. Taken from a per-board value rather
/// than constants so a board with a very different shape (one rail for many
/// chips, a coarse voltage grid, a collapse voltage) can plug in without
/// touching the state machine. The default is the BM1370 envelope, which is
/// what every board used before this type existed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TuneEnvelope {
    /// Profile-mode safety-breach floor (all profiles) and fallback bound in
    /// `evaluate_target` when a board's chip model isn't recognized.
    pub min_freq_mhz: f32,
    pub max_freq_mhz: f32,
    pub freq_step_mhz: f32,
    pub min_voltage_mv: u16,
    /// Ceiling the tuner will raise voltage to on its own. Below the hard
    /// clamp: the tuner should never sit near the sustained-damage line.
    pub max_voltage_mv: u16,
    pub voltage_step_mv: u16,
    /// Voltages the tuner must never command; a step that lands on one takes
    /// one more step in the same direction.
    pub forbidden_voltages_mv: &'static [u16],
    /// Supervisor cycles between tuning steps. The scheduler's hashrate
    /// estimate is a ~300 s windowed average, so a tuning step must wait
    /// about that long before the measured hashrate reflects a clock change
    /// -- otherwise a just-changed clock is judged on stale data and the
    /// calibrated baseline ratchets. At the 2 s supervisor cadence, 150
    /// cycles = 300 s/step. Tuning is a slow background optimizer by design.
    pub settle_cycles: u32,
    /// Cycles between successive thermal/power back-off steps. Far shorter
    /// than a tuning step: a cap breach must be acted on promptly
    /// (temperature and power are instantaneous readings, not windowed), but
    /// not every 2 s or we overshoot before the previous drop takes effect.
    pub safety_settle_cycles: u32,
    /// Cycles between successive back-off steps for a power-only breach (a
    /// temperature breach always uses `safety_settle_cycles`). A profile's
    /// power cap is an efficiency limit; the board's own PSU trip is the
    /// safety one. Warming chips draw more (leakage) for minutes after a
    /// cut, so cutting every tick overshoots well below the cap.
    pub power_settle_cycles: u32,
    pub quiet: CapLimits,
    pub efficient: CapLimits,
    pub balanced: CapLimits,
    pub max_hash: CapLimits,
    /// Target mode: temperature ceiling. No per-chip thermal max is known, so
    /// by default this reuses MaxHash's rather than inventing one.
    pub target_temp_c: f32,
    /// Target mode, hashrate target: outer power bound, since that target's
    /// power draw isn't known in advance. Defaults to MaxHash's.
    pub target_hashrate_power_w: f32,
    /// Target mode, power target: the cap is `target * this`, a small
    /// headroom so the breach branch doesn't fight the convergence branch at
    /// setpoint.
    pub target_power_headroom: f32,
    /// Optional stability signal: a voltage trim whose settled fail rate
    /// exceeds this is undone and the tuner locks. `None` disables the check
    /// (it is also inert unless `Metrics::fail_rate_pct` is `Some`).
    pub max_fail_rate_pct: Option<f32>,
    /// Judge efficiency and fail rate on the MEAN of every evaluation since
    /// the last applied change instead of the single settled sample. The
    /// scheduler hashrate estimate is noisy between windows; safety checks
    /// (temp/power caps) always use the instantaneous reading regardless.
    pub average_judgement: bool,
    /// Profile mode: a settled point is only recorded as best (and a climb
    /// back to it only allowed) if its mean power is at most the cap minus
    /// this margin. 0.0 disables the hysteresis.
    pub best_point_margin_w: f32,
    /// Efficiency trims move this many voltage grid steps per judgement
    /// (1 = one step). If the larger move would cross the voltage floor or
    /// the `probe_floor`, the largest smaller move that fits is used, so a
    /// failed 3-step trim is refined with 2- then 1-step trims before the
    /// walk locks. A value above 1 also enables the power-at-equal-hashrate
    /// acceptance (see `evaluate_inner`).
    pub trim_steps_per_judgement: u8,
}

impl TuneEnvelope {
    /// The BM1370 envelope (the owner repo's default). `chip_profile` does not
    /// exist in nano-mujina, so its BM1370 numbers are inlined here.
    pub const fn bm1370() -> Self {
        Self {
            min_freq_mhz: 490.0,
            max_freq_mhz: 650.0,
            freq_step_mhz: 25.0,
            min_voltage_mv: 1150,
            max_voltage_mv: 1250,
            voltage_step_mv: 10,
            forbidden_voltages_mv: &[],
            settle_cycles: 150,
            safety_settle_cycles: 6,
            power_settle_cycles: 6,
            quiet: CapLimits {
                temp_c: 55.0,
                power_w: 12.0,
            },
            efficient: CapLimits {
                temp_c: 60.0,
                power_w: 13.0,
            },
            balanced: CapLimits {
                temp_c: 62.0,
                power_w: 15.0,
            },
            max_hash: CapLimits {
                temp_c: 68.0,
                power_w: 22.0,
            },
            target_temp_c: 68.0,
            target_hashrate_power_w: 22.0,
            target_power_headroom: 1.05,
            max_fail_rate_pct: None,
            average_judgement: false,
            best_point_margin_w: 0.0,
            trim_steps_per_judgement: 1,
        }
    }

    /// `n` voltage grid steps down from `mv`: the target is `mv - n*step`
    /// (passing over a forbidden value is fine); if it lands exactly on one,
    /// one more step is taken. `n == 1` equals [`Self::voltage_down`].
    fn voltage_down_n(&self, mv: u16, n: u8) -> u16 {
        let v = mv.saturating_sub(self.voltage_step_mv.saturating_mul(n as u16));
        if self.is_forbidden(v) {
            v.saturating_sub(self.voltage_step_mv)
        } else {
            v
        }
    }

    fn is_forbidden(&self, mv: u16) -> bool {
        self.forbidden_voltages_mv.contains(&mv)
    }

    /// One voltage step down from `mv`, taking one more step when it lands on
    /// a forbidden value. Saturates at 0; callers gate or clamp against
    /// `min_voltage_mv` themselves.
    fn voltage_down(&self, mv: u16) -> u16 {
        let mut v = mv.saturating_sub(self.voltage_step_mv);
        if self.is_forbidden(v) {
            v = v.saturating_sub(self.voltage_step_mv);
        }
        v
    }

    /// One voltage step up from `mv`, taking one more step when it lands on
    /// a forbidden value. Callers gate against `max_voltage_mv`.
    fn voltage_up(&self, mv: u16) -> u16 {
        let mut v = mv.saturating_add(self.voltage_step_mv);
        if self.is_forbidden(v) {
            v = v.saturating_add(self.voltage_step_mv);
        }
        v
    }

    fn profile_limits(&self, profile: TuneProfile) -> CapLimits {
        match profile {
            TuneProfile::Quiet => self.quiet,
            TuneProfile::Efficient => self.efficient,
            TuneProfile::Balanced => self.balanced,
            TuneProfile::MaxHash => self.max_hash,
        }
    }
}

impl Default for TuneEnvelope {
    fn default() -> Self {
        Self::bm1370()
    }
}

/// Relative margin a candidate point must beat the recorded best by (on
/// hashrate for hash-seeking profiles, on efficiency for efficiency-seeking
/// ones) to count as a genuine improvement rather than noise. Points judged
/// directly against real measurements from this chip, never an extrapolated
/// nameplate table.
const IMPROVEMENT_MARGIN: f32 = 0.03;
/// Power-at-equal-hashrate acceptance (multi-step envelopes only).
const POWER_SAVING_MARGIN: f32 = 0.015;
const HASHRATE_EQUAL_TOLERANCE: f32 = 0.01;
/// Below this hashrate the board is still warming up / not usefully hashing;
/// don't judge stability yet.
const MIN_HASHRATE_THS: f32 = 0.3;
/// Tolerance for comparing a live frequency reading to a recorded setpoint.
const FREQ_TOLERANCE_MHZ: f32 = 0.01;
/// Tolerance (mV) for deciding a live point is "at" a recorded voltage
/// setpoint. Absorbs regulator droop/ADC jitter (a measured vout lags the
/// commanded setpoint by up to ~one step); kept below the envelope's voltage step so an
/// intentionally-adjacent point is still distinct.
const VOLT_TOLERANCE_MV: u16 = 5;
/// Temperature (C) below the cap the die must fall to before a forced-full
/// fan episode is released. A hysteresis band so a die hovering at the cap
/// (its natural attractor) does not flap the fan every cycle.
const FAN_RELEASE_HYSTERESIS_C: f32 = 2.0;
/// Target mode: fraction of the target value the live measurement must be
/// within to count as "reached" and lock. A live power/hashrate reading
/// jitters cycle to cycle, so an exact match would never latch.
const TARGET_TOLERANCE_FRACTION: f32 = 0.02;
/// Target mode: consecutive settled cycles pinned at a frequency limit
/// (with the setpoint still unreached) before the target is declared
/// unreachable. Matches the two-strikes pattern `pending_reject_mhz` uses
/// elsewhere so a single noisy window doesn't trip it prematurely.
const UNREACHABLE_STRIKES: u32 = 2;

/// Most recent tuner events kept for the activity log.
const LOG_CAPACITY: usize = 40;

/// What the tuner optimizes toward, and the safety caps it respects.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TuneProfile {
    /// Low noise/heat: modest clock, tight temperature cap.
    Quiet,
    /// Best efficiency (J/TH): undervolt while stable.
    Efficient,
    /// A middle ground between efficiency and hashrate.
    #[default]
    Balanced,
    /// Push the clock to its stable limit within thermal/power caps.
    MaxHash,
}

/// Hard limits and search bias for a profile.
struct Caps {
    temp_c: f32,
    power_w: f32,
    /// Seek higher clock (raising voltage if needed) vs. hold conservative.
    seek_hash: bool,
    /// Trim voltage for efficiency while stable.
    seek_efficiency: bool,
}

impl TuneProfile {
    fn caps(self, env: &TuneEnvelope) -> Caps {
        let limits = env.profile_limits(self);
        let (seek_hash, seek_efficiency) = match self {
            TuneProfile::Quiet => (false, false),
            TuneProfile::Efficient => (false, true),
            TuneProfile::Balanced => (true, false),
            TuneProfile::MaxHash => (true, false),
        };
        Caps {
            temp_c: limits.temp_c,
            power_w: limits.power_w,
            seek_hash,
            seek_efficiency,
        }
    }
}

/// A Braiins-OS-style setpoint the tuner converges to and holds, rather
/// than the profiles' "climb as far as the caps allow" search. Kept as a
/// separate type from [`TuneProfile`] (not a variant of it) so the
/// existing profile enum keeps deriving `Eq` and its persisted-file
/// format is untouched.
#[derive(Clone, Copy, Debug, PartialEq, Deserialize, Serialize, ToSchema)]
#[serde(tag = "axis", content = "value", rename_all = "snake_case")]
pub enum TuneTarget {
    /// Target board power draw, in watts.
    Power(f32),
    /// Target aggregate hashrate, in TH/s.
    Hashrate(f32),
}

/// What the tuner is currently driving toward: a cap-based profile, or a
/// specific power/hashrate setpoint.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TuneMode {
    Profile(TuneProfile),
    Target(TuneTarget),
}

impl Default for TuneMode {
    fn default() -> Self {
        TuneMode::Profile(TuneProfile::default())
    }
}

impl TuneMode {
    /// Safety caps for this mode. For a profile these are the profile's
    /// own caps (unchanged behavior). For a target these are synthesized
    /// guardrails -- never the setpoint itself, which is handled by the
    /// convergence logic in `evaluate_target`, not by treating the target
    /// as a ceiling to climb toward.
    fn caps(self, env: &TuneEnvelope) -> Caps {
        match self {
            TuneMode::Profile(profile) => profile.caps(env),
            // No per-chip thermal max is known here, so the envelope's
            // target ceiling (MaxHash's by default) is used rather than an
            // invented one: target mode shouldn't be more thermally
            // restrictive than the highest-cap profile already validated.
            TuneMode::Target(TuneTarget::Power(watts)) => Caps {
                temp_c: env.target_temp_c,
                // Small headroom above the target so the breach branch
                // doesn't fight the convergence branch right at setpoint.
                power_w: watts * env.target_power_headroom,
                seek_hash: false,
                seek_efficiency: false,
            },
            TuneMode::Target(TuneTarget::Hashrate(_)) => Caps {
                temp_c: env.target_temp_c,
                // The target's power draw isn't known in advance; reuse
                // MaxHash's ceiling as the outer safety bound.
                power_w: env.target_hashrate_power_w,
                seek_hash: false,
                seek_efficiency: false,
            },
        }
    }
}

/// A frequency/voltage operating point.
#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize, Serialize, ToSchema)]
pub struct TuneSetpoint {
    pub frequency_mhz: f32,
    pub core_voltage_mv: u16,
}

/// What a profile search has learned, so a reboot resumes it instead of
/// re-walking the voltage down past points already ruled out.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SearchMemory {
    pub best: TuneSetpoint,
    pub best_hashrate_ths: f32,
    pub best_j_th: f32,
    /// Highest voltage ruled out (0 = none).
    pub probe_floor_mv: u16,
    /// Lowest clock ruled out (above the envelope max = none).
    pub probe_ceiling_mhz: f32,
}

/// Live inputs to one tuner decision.
#[derive(Clone, Copy, Debug)]
pub struct Metrics {
    pub asic_temp_c: f32,
    pub power_w: f32,
    pub hashrate_ths: f32,
    pub frequency_mhz: f32,
    pub core_voltage_mv: u16,
    pub fan_min_percent: Option<u8>,
    pub fan_auto: Option<bool>,
    /// Share of submitted work that failed (hardware errors / rejects) over
    /// the settled window, when the board can measure it. `None` for boards
    /// that can't; the fail-rate stability check is then inert.
    pub fail_rate_pct: Option<f32>,
}

/// A change the tuner wants applied to the board.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TuneAction {
    SetFrequency(f32),
    SetVoltage(u16),
    /// Force the fan to 100% duty. Issued once at the start of a
    /// temperature-cap breach, before the clock is touched, so the tuner
    /// never sacrifices hashrate while the fan still has headroom to cool
    /// the chip on its own.
    SetFanFull,
    /// Restore the fan to its automatic default curve once a
    /// temperature-cap breach has cleared.
    RestoreFanAuto,
    /// Slowly adjust the minimum fan speed in automatic mode to find a
    /// stable cooling state.
    SetFanMinPercent(u8),
}

/// Where the tuner is in its search.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TunePhase {
    Disabled,
    /// Waiting for the chip/hashrate to settle before the first judgement.
    Warmup,
    /// Actively searching for a better operating point.
    Seeking,
    /// Backed off after hitting a thermal/power cap.
    BackedOff,
    /// Converged; holding the best known-good point.
    Locked,
    /// Target mode: stepping the clock toward the setpoint.
    Converging,
    /// Target mode: pinned at the chip's frequency limit and still not at
    /// the setpoint. Holding rather than continuing to step -- the target
    /// is not achievable on this hardware.
    Unreachable,
}

/// One activity-log entry, surfaced in the dashboard.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct TuneEvent {
    pub message: String,
    pub frequency_mhz: f32,
    pub core_voltage_mv: u16,
    pub asic_temp_c: f32,
    pub efficiency_j_th: Option<f32>,
    pub power_w: f32,
    pub hashrate_ths: f32,
    /// `evaluate` call count when logged (the adapter turns it into age).
    pub cycle: u64,
}

/// Snapshot the API returns for `GET /boards/{name}/autotune`.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct AutoTuneStatus {
    pub enabled: bool,
    /// Set when `mode` is a cap-based profile; `None` in target mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<TuneProfile>,
    /// Set when `mode` is a power/hashrate setpoint; `None` in profile mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<TuneTarget>,
    pub phase: TunePhase,
    pub frequency_mhz: Option<f32>,
    pub core_voltage_mv: Option<u16>,
    pub efficiency_j_th: Option<f32>,
    pub best: Option<TuneSetpoint>,
    pub log: Vec<TuneEvent>,
}

/// Efficiency in joules per terahash, or `None` when not hashing.
fn efficiency_j_th(power_w: f32, hashrate_ths: f32) -> Option<f32> {
    (hashrate_ths > 0.0).then(|| power_w / hashrate_ths)
}

/// The pure tuning state machine.
///
/// Shared behind a mutex between the API handlers (which flip `enabled`
/// and `profile`) and the supervisor task (which calls [`Self::evaluate`]).
pub struct AutoTuner {
    enabled: bool,
    mode: TuneMode,
    /// Board limits and timing; BM1370's unless the supervisor sets another.
    envelope: TuneEnvelope,
    /// Count of chips on the board this tuner is driving, updated from
    /// telemetry each cycle. Defaults to 1.
    chip_count: u32,
    /// nano-mujina addition: live whole-board power ceiling (W) imposed by
    /// the board's PSU, applied as `min()` over the mode's power cap.
    power_ceiling_w: Option<f32>,
    phase: TunePhase,
    /// Cycles since the last applied change (settle gate).
    cycles_since_change: u32,
    /// Best point actually measured this session: the setpoint plus the
    /// hashrate and efficiency it delivered. Every later point is compared
    /// directly against this real measurement rather than an extrapolated
    /// nameplate that may not match this chip.
    best: Option<(TuneSetpoint, f32, f32)>,
    /// Lowest clock a completed tuning step found did NOT improve on `best`
    /// (even after exhausting the voltage-recovery step). The search will
    /// not climb back to it, so it can't oscillate between a losing clock
    /// and its predecessor. Deliberately set ONLY on a genuine
    /// no-improvement verdict — never on a thermal/power safety back-off,
    /// which is transient and must stay retryable. That conflation was the
    /// original bug that capped Max Hash below the manual default.
    probe_ceiling_mhz: f32,
    /// A clock that failed to improve on `best` ONCE and is being re-measured
    /// before it is written off. The ~300 s windowed hashrate has sampling
    /// noise that can exceed the improvement margin, so a single unlucky
    /// window must not permanently exclude a genuinely-good clock (that would
    /// re-introduce the original bug in statistical form). Only a second
    /// consecutive failure at the same clock lowers `probe_ceiling_mhz`.
    pending_reject_mhz: Option<f32>,
    /// Highest voltage an efficiency undervolt step found did NOT improve on
    /// `best` (efficiency got worse, i.e. the undervolt cost hashrate). The
    /// mirror of `probe_ceiling_mhz` for the efficiency walk: the search
    /// won't trim back down to it, so it can't oscillate. `0` means none
    /// found yet.
    probe_floor_mv: u16,
    /// Last voltage this tuner commanded, tracked independently of the
    /// board's measured vout (which lags/droops relative to the setpoint by
    /// about as much as one step), so successive raise/trim steps actually
    /// move the setpoint instead of repeating a no-op.
    last_commanded_voltage_mv: Option<u16>,
    /// Whether the fan has been forced to 100% for the current temperature
    /// back-off episode; restored to automatic once temperature clears the
    /// cap.
    fan_forced_full: bool,
    last_commanded_fan_min_pct: Option<u8>,
    last_efficiency: Option<f32>,
    /// Target mode: consecutive settled cycles pinned at the chip's
    /// frequency limit with the setpoint still unreached. Two consecutive
    /// (mirroring `pending_reject_mhz`'s noise tolerance elsewhere in this
    /// file) trips `TunePhase::Unreachable` rather than stepping forever.
    stuck_at_limit_cycles: u32,
    /// Voltage held before the most recent efficiency trim, until that trim
    /// has been judged. Lets the fail-rate check step back to the last good
    /// voltage.
    pending_trim_from_mv: Option<u16>,
    /// Running sums of hashrate/power/fail rate since the last applied change
    /// (only fed when `envelope.average_judgement`).
    acc: Accum,
    /// Number of `evaluate` calls while enabled.
    cycle: u64,
    log: VecDeque<TuneEvent>,
}

/// Running means for averaged judgement.
#[derive(Clone, Copy, Debug, Default)]
struct Accum {
    n: u32,
    hash: f32,
    power: f32,
    fail: f32,
    fail_n: u32,
}

impl Accum {
    fn add(&mut self, m: &Metrics) {
        self.n += 1;
        self.hash += m.hashrate_ths;
        self.power += m.power_w;
        if let Some(f) = m.fail_rate_pct {
            self.fail += f;
            self.fail_n += 1;
        }
    }

    /// `m` with hashrate, power and fail rate replaced by the means; temp,
    /// clock and voltage stay instantaneous.
    fn mean_of(&self, m: &Metrics) -> Metrics {
        if self.n == 0 {
            return *m;
        }
        let n = self.n as f32;
        Metrics {
            hashrate_ths: self.hash / n,
            power_w: self.power / n,
            fail_rate_pct: if self.fail_n > 0 { Some(self.fail / self.fail_n as f32) } else { m.fail_rate_pct },
            ..*m
        }
    }
}

impl Default for AutoTuner {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: TuneMode::default(),
            envelope: TuneEnvelope::default(),
            chip_count: 1,
            power_ceiling_w: None,
            phase: TunePhase::Disabled,
            cycles_since_change: 0,
            best: None,
            probe_ceiling_mhz: TuneEnvelope::default().max_freq_mhz
                + TuneEnvelope::default().freq_step_mhz,
            pending_reject_mhz: None,
            probe_floor_mv: 0,
            last_commanded_voltage_mv: None,
            fan_forced_full: false,
            last_commanded_fan_min_pct: None,
            last_efficiency: None,
            stuck_at_limit_cycles: 0,
            pending_trim_from_mv: None,
            acc: Accum::default(),
            cycle: 0,
            log: VecDeque::with_capacity(LOG_CAPACITY),
        }
    }
}

impl AutoTuner {
    /// Shared state reset for entering a fresh search, regardless of mode.
    ///
    /// Deliberately does NOT reset `fan_forced_full`: that flag tracks
    /// whether the *board* is currently sitting in a forced-100% fan
    /// override, not the tuning search, and switching modes (or
    /// disabling/re-enabling) does not touch the board's actual fan state.
    /// Clearing it here previously left the flag reporting "not forced"
    /// while the fan was still physically pinned at 100% from the prior
    /// profile's breach, so `evaluate` could never issue the
    /// `RestoreFanAuto` that would have handed it back — the fan stayed
    /// stuck at 100% until a fresh breach happened to re-arm the flag.
    fn reset_search(&mut self) {
        self.enabled = true;
        self.phase = TunePhase::Warmup;
        self.cycles_since_change = 0;
        self.acc = Accum::default();
        // Fresh search: drop any best/log from a previous mode so we never
        // persist a stale point under the newly-selected mode.
        self.best = None;
        self.probe_ceiling_mhz = self.envelope.max_freq_mhz + self.envelope.freq_step_mhz;
        self.pending_reject_mhz = None;
        self.probe_floor_mv = 0;
        self.last_commanded_voltage_mv = None;
        self.last_commanded_fan_min_pct = None;
        self.stuck_at_limit_cycles = 0;
        self.pending_trim_from_mv = None;
        self.log.clear();
    }

    /// Enable tuning with a cap-based profile, resetting the search.
    pub fn enable_profile(&mut self, profile: TuneProfile) {
        self.mode = TuneMode::Profile(profile);
        self.reset_search();
    }

    /// Enable tuning toward a power/hashrate setpoint, resetting the search.
    pub fn enable_target(&mut self, target: TuneTarget) {
        self.mode = TuneMode::Target(target);
        self.reset_search();
    }

    /// Set the board's tuning envelope, resetting the search state derived
    /// from the previous limits.
    pub fn set_envelope(&mut self, envelope: TuneEnvelope) {
        self.envelope = envelope;
        self.probe_ceiling_mhz = envelope.max_freq_mhz + envelope.freq_step_mhz;
        self.pending_reject_mhz = None;
        self.probe_floor_mv = 0;
        self.pending_trim_from_mv = None;
        self.acc = Accum::default();
    }

    /// Evaluate calls so far while enabled (event ages are measured in these).
    pub fn cycle(&self) -> u64 {
        self.cycle
    }

    /// Recent activity-log entries, oldest first.
    pub fn events(&self) -> impl Iterator<Item = &TuneEvent> {
        self.log.iter()
    }

    /// Best measured point and its efficiency (J/TH).
    pub fn best_point(&self) -> Option<(TuneSetpoint, f32)> {
        self.best.map(|(sp, _, eff)| (sp, eff))
    }

    /// The search state worth keeping across a reboot; `None` until a best
    /// point has been measured.
    pub fn memory(&self) -> Option<SearchMemory> {
        let (best, best_hashrate_ths, best_j_th) = self.best?;
        Some(SearchMemory {
            best,
            best_hashrate_ths,
            best_j_th,
            probe_floor_mv: self.probe_floor_mv,
            probe_ceiling_mhz: self.probe_ceiling_mhz,
        })
    }

    /// Restores a saved search after `enable_profile`. The first settled
    /// judgement at the best point re-measures it.
    pub fn restore_memory(&mut self, mem: SearchMemory) {
        self.best = Some((mem.best, mem.best_hashrate_ths, mem.best_j_th));
        self.probe_floor_mv = mem.probe_floor_mv;
        self.probe_ceiling_mhz = mem.probe_ceiling_mhz;
    }

    /// Averaging window progress: (samples accumulated, samples needed).
    pub fn averaging_progress(&self) -> (u32, u32) {
        (self.acc.n.min(self.envelope.settle_cycles), self.envelope.settle_cycles)
    }

    /// Update the chip count for the board this tuner is driving, looked up
    /// by the supervisor from the board's telemetry each cycle.
    pub fn set_chip_count(&mut self, count: u32) {
        self.chip_count = count;
    }

    /// nano-mujina addition: cap every mode's power limit at the PSU's live
    /// ceiling so a profile can never ask for more than the supply allows.
    pub fn set_power_ceiling(&mut self, watts: Option<f32>) {
        self.power_ceiling_w = watts;
    }

    /// nano-mujina addition: another controller (the PSU safety trip) changed
    /// the core voltage behind the tuner's back. Adopt it as the commanded
    /// voltage so the next step walks from the real value instead of
    /// restoring the stale one, and drop any trim still being judged.
    pub fn note_external_voltage(&mut self, mv: u16) {
        self.last_commanded_voltage_mv = Some(mv);
        self.pending_trim_from_mv = None;
    }

    /// nano-mujina addition: temperature cap (C) of the current mode.
    pub fn temp_cap_c(&self) -> f32 {
        self.caps().temp_c
    }

    /// nano-mujina addition: whether the tuner currently holds the fan at 100 %.
    pub fn fan_forced(&self) -> bool {
        self.fan_forced_full
    }

    pub fn phase(&self) -> TunePhase {
        self.phase
    }

    /// nano-mujina addition: most recent activity-log message, if any.
    pub fn last_event(&self) -> Option<&str> {
        self.log.back().map(|e| e.message.as_str())
    }

    /// Retrieve the safety caps for the current mode, scaled dynamically
    /// by the board's chip count.
    fn caps(&self) -> Caps {
        let mut caps = self.mode.caps(&self.envelope);
        // Scale profile caps and default hashrate target ceiling by chip count.
        // Power target mode is already board-wide and does not scale.
        if !matches!(self.mode, TuneMode::Target(TuneTarget::Power(_))) {
            caps.power_w *= self.chip_count as f32;
        }
        if let Some(ceiling) = self.power_ceiling_w {
            caps.power_w = caps.power_w.min(ceiling);
        }
        caps
    }

    /// Disable tuning. The board keeps whatever setpoint it is at,
    /// including the fan — see [`Self::reset_search`] for why
    /// `fan_forced_full` is not reset here either.
    pub fn disable(&mut self) {
        self.enabled = false;
        self.phase = TunePhase::Disabled;
        self.last_commanded_voltage_mv = None;
        self.acc = Accum::default();
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn status(&self, setpoint: Option<TuneSetpoint>) -> AutoTuneStatus {
        let (profile, target) = match self.mode {
            TuneMode::Profile(profile) => (Some(profile), None),
            TuneMode::Target(target) => (None, Some(target)),
        };
        AutoTuneStatus {
            enabled: self.enabled,
            profile,
            target,
            phase: self.phase,
            frequency_mhz: setpoint.map(|s| s.frequency_mhz),
            core_voltage_mv: setpoint.map(|s| s.core_voltage_mv),
            efficiency_j_th: self.last_efficiency,
            best: self.best.map(|(setpoint, ..)| setpoint),
            log: self.log.iter().cloned().collect(),
        }
    }

    fn log_event(&mut self, message: impl Into<String>, m: &Metrics) {
        if self.log.len() == LOG_CAPACITY {
            self.log.pop_front();
        }
        self.log.push_back(TuneEvent {
            message: message.into(),
            frequency_mhz: m.frequency_mhz,
            core_voltage_mv: m.core_voltage_mv,
            asic_temp_c: m.asic_temp_c,
            efficiency_j_th: efficiency_j_th(m.power_w, m.hashrate_ths),
            power_w: m.power_w,
            hashrate_ths: m.hashrate_ths,
            cycle: self.cycle,
        });
    }

    /// The core voltage this tuner believes is currently commanded. The
    /// board's measured vout droops below the commanded setpoint (by up to
    /// ~one step) and jitters, so all voltage decisions — recording `best`,
    /// matching `at_best`, computing the next raise/trim — work in this
    /// commanded domain. Falls back to the measured reading only before the
    /// tuner has issued its first voltage command.
    fn commanded_voltage(&self, m: &Metrics) -> u16 {
        self.last_commanded_voltage_mv.unwrap_or(m.core_voltage_mv)
    }

    /// Decide the next action from the latest metrics. Returns `None` when
    /// holding (settling, locked, or disabled). Pure aside from internal
    /// state; the caller applies the returned action to the hardware.
    pub fn evaluate(&mut self, m: &Metrics) -> Option<TuneAction> {
        if self.enabled {
            self.cycle += 1;
        }
        if self.enabled && self.envelope.average_judgement {
            self.acc.add(m);
        }
        let action = self.evaluate_inner(m);
        if action.is_some() {
            self.acc = Accum::default();
        }
        action
    }

    fn evaluate_inner(&mut self, m: &Metrics) -> Option<TuneAction> {
        if !self.enabled {
            return None;
        }
        self.last_efficiency = efficiency_j_th(m.power_w, m.hashrate_ths);
        self.cycles_since_change += 1;

        let caps = self.caps();
        let env = self.envelope;

        // 1. Safety: over a hard cap -> back off on a fast cadence that does
        //    NOT wait for the long tuning settle (temperature and power are
        //    instantaneous). A breach is treated as a transient condition,
        //    never as permanent evidence the silicon can't run this clock —
        //    nothing here narrows the search space for future attempts.
        if m.asic_temp_c > caps.temp_c || m.power_w > caps.power_w {
            self.phase = TunePhase::BackedOff;
            // A breach supersedes any trim still being judged: its fail rate
            // is no longer evidence about that trim, and reverting would
            // raise voltage right after a thermal or power breach.
            self.pending_trim_from_mv = None;

            // Temperature breaches get the fan pushed to 100% first — the
            // automatic curve may still have headroom left (e.g. it only
            // reaches 100% well above where a profile's cap sits), so the
            // tuner should not sacrifice clock while the fan still has more
            // to give. Power breaches skip this: more airflow doesn't reduce
            // watts drawn.
            // If the fan is in automatic mode, we slowly increase the min fan
            // speed first to find a stable speed, avoiding thermal oscillation.
            if m.asic_temp_c > caps.temp_c {
                let mut fan_adjusted = false;
                if m.fan_auto == Some(true) {
                    if let Some(min_pct) = m.fan_min_percent {
                        if min_pct < 100 {
                            fan_adjusted = true;
                            let new_min = (min_pct + 5).min(100);
                            if self.last_commanded_fan_min_pct != Some(new_min)
                                || self.cycles_since_change >= env.safety_settle_cycles
                            {
                                self.last_commanded_fan_min_pct = Some(new_min);
                                self.cycles_since_change = 0;
                                self.log_event(
                                    format!(
                                        "temp {:.1}C over {:.0}C cap: slowly increasing fan min to {new_min}%",
                                        m.asic_temp_c, caps.temp_c
                                    ),
                                    m,
                                );
                                return Some(TuneAction::SetFanMinPercent(new_min));
                            }
                        }
                    }
                }

                if !fan_adjusted && !self.fan_forced_full {
                    self.fan_forced_full = true;
                    self.cycles_since_change = 0;
                    self.log_event(
                        format!(
                            "temp {:.1}C over {:.0}C cap: forcing fan to 100%",
                            m.asic_temp_c, caps.temp_c
                        ),
                        m,
                    );
                    return Some(TuneAction::SetFanFull);
                }
            }

            let settle = if m.asic_temp_c > caps.temp_c {
                env.safety_settle_cycles
            } else {
                env.power_settle_cycles
            };
            if self.cycles_since_change < settle {
                return None;
            }
            self.cycles_since_change = 0;
            let why = if m.asic_temp_c > caps.temp_c {
                format!("temp {:.1}C over {:.0}C cap", m.asic_temp_c, caps.temp_c)
            } else {
                format!("power {:.1}W over {:.0}W cap", m.power_w, caps.power_w)
            };
            // Cap hysteresis: a clock above best that breaches the POWER cap
            // is a real ceiling (power is deterministic in clock), so the
            // climb must not return to it and oscillate across the cap.
            if env.best_point_margin_w > 0.0
                && m.power_w > caps.power_w
                && let Some((b, ..)) = self.best
                && m.frequency_mhz > b.frequency_mhz + FREQ_TOLERANCE_MHZ
            {
                self.probe_ceiling_mhz = self.probe_ceiling_mhz.min(m.frequency_mhz);
            }
            if m.frequency_mhz > env.min_freq_mhz {
                let target = (m.frequency_mhz - env.freq_step_mhz).max(env.min_freq_mhz);
                self.log_event(format!("{why}: lowering to {target:.0} MHz"), m);
                return Some(TuneAction::SetFrequency(target));
            }
            if m.core_voltage_mv > env.min_voltage_mv {
                let v = env.voltage_down(m.core_voltage_mv).max(env.min_voltage_mv);
                self.last_commanded_voltage_mv = Some(v);
                self.log_event(
                    format!("{why}: at clock floor, trimming voltage to {v} mV"),
                    m,
                );
                return Some(TuneAction::SetVoltage(v));
            }
            // At the clock floor and minimum voltage: nothing more to give.
            // The board's own thermal watchdog is the backstop.
            self.log_event(format!("{why}: at floor, holding"), m);
            return None;
        }

        // Not breached: release a forced-full fan episode only once the die
        // has fallen a hysteresis band below the cap. A die parked right at
        // the cap is the natural attractor for a hash-seeking profile, so
        // releasing on the first sub-cap reading would flap the fan (and, by
        // zeroing the counter each flip, stall the tuner). Don't reset the
        // settle counter here: a breach resolved by airflow alone shouldn't
        // also delay the next tuning judgement.
        if self.fan_forced_full && m.asic_temp_c < caps.temp_c - FAN_RELEASE_HYSTERESIS_C {
            self.fan_forced_full = false;
            self.log_event("temp back under cap: restoring automatic fan curve", m);
            return Some(TuneAction::RestoreFanAuto);
        }

        // 2. Tuning steps are slow: wait a full settle so the ~300 s windowed
        //    hashrate reflects the last change before it is judged.
        if self.cycles_since_change < env.settle_cycles {
            return None;
        }
        self.cycles_since_change = 0;

        // Averaged judgement: from here on efficiency, fail rate and the
        // headroom checks see the window means; the safety branch above
        // already ran on the instantaneous reading.
        let judged = if self.envelope.average_judgement { self.acc.mean_of(m) } else { *m };
        self.acc = Accum::default();
        let m = &judged;

        // Target mode converges to a fixed setpoint rather than climbing as
        // far as the caps allow -- entirely different logic from the
        // profile hill-climb below, so it's handled separately.
        if let TuneMode::Target(target) = self.mode {
            return self.evaluate_target(m, target);
        }

        // Not usefully hashing: genuine first-time warmup holds. A collapse
        // after `best` was already established falls through instead — it is
        // judged (and reverted) below like any other bad candidate, rather
        // than holding here forever.
        if m.hashrate_ths < MIN_HASHRATE_THS && self.best.is_none() {
            self.phase = TunePhase::Warmup;
            return None;
        }

        // A voltage trim has now settled. If the board reports a fail rate
        // over the envelope's limit, the trim went past the stability cliff:
        // step back to the last good voltage and lock there. `probe_floor_mv`
        // keeps the efficiency walk from trimming to it again.
        if let Some(prev_mv) = self.pending_trim_from_mv.take()
            && let (Some(max), Some(rate)) = (env.max_fail_rate_pct, m.fail_rate_pct)
            && rate > max
        {
            let failed_mv = self.commanded_voltage(m);
            self.probe_floor_mv = self.probe_floor_mv.max(failed_mv);
            self.last_commanded_voltage_mv = Some(prev_mv);
            self.phase = TunePhase::Locked;
            self.log_event(
                format!(
                    "fail rate {rate:.1}% over {max:.1}% at {failed_mv} mV: back to {prev_mv} mV and locking"
                ),
                m,
            );
            return Some(TuneAction::SetVoltage(prev_mv));
        }

        // A settled point over the fail-rate limit is unstable whatever its
        // hashrate or J/TH: never record it as best or climb from it. Raise
        // voltage a step (power permitting), else drop the clock a step. The
        // efficiency walk also stays above it from now on.
        if let (Some(max), Some(rate)) = (env.max_fail_rate_pct, m.fail_rate_pct)
            && rate > max
        {
            let commanded_v = self.commanded_voltage(m);
            self.probe_floor_mv = self.probe_floor_mv.max(commanded_v);
            self.phase = TunePhase::Seeking;
            let up = env.voltage_up(commanded_v);
            if up <= env.max_voltage_mv && m.power_w < caps.power_w - 1.0 {
                self.last_commanded_voltage_mv = Some(up);
                self.log_event(
                    format!("fail rate {rate:.1}% over {max:.1}%: raising voltage to {up} mV"),
                    m,
                );
                return Some(TuneAction::SetVoltage(up));
            }
            let down = m.frequency_mhz - env.freq_step_mhz;
            if down >= env.min_freq_mhz {
                self.probe_ceiling_mhz = self.probe_ceiling_mhz.min(m.frequency_mhz);
                self.log_event(
                    format!("fail rate {rate:.1}% over {max:.1}%: lowering clock to {down:.0} MHz"),
                    m,
                );
                return Some(TuneAction::SetFrequency(down));
            }
            return None;
        }

        let hashrate = m.hashrate_ths;
        let efficiency = efficiency_j_th(m.power_w, hashrate).unwrap_or(f32::INFINITY);
        // All voltage reasoning is in the commanded domain (see helper), so a
        // recorded `best`, an `at_best` match, and the next raise/trim all
        // agree instead of chasing regulator droop.
        let commanded_v = self.commanded_voltage(m);
        // Cap hysteresis: a point whose mean power sits inside the margin
        // below the cap is "at cap" and never becomes the known-good point.
        let near_cap = env.best_point_margin_w > 0.0
            && m.power_w > caps.power_w - env.best_point_margin_w;

        // 3. At the setpoint recorded as `best`: this is a confirmation, not a
        //    new candidate (e.g. we just returned here after a transient
        //    thermal/power backoff resolved). Refresh the measurement and keep
        //    trying to extend the search from here — this is what lets the
        //    tuner retry climbing past a point it was once knocked away from,
        //    instead of getting stuck exactly where the interruption left it.
        let at_best = self.best.is_some_and(|(b, _, _)| {
            (m.frequency_mhz - b.frequency_mhz).abs() <= FREQ_TOLERANCE_MHZ
                && commanded_v.abs_diff(b.core_voltage_mv) <= VOLT_TOLERANCE_MV
        });

        if at_best {
            // A transient collapse while sitting at the known-good point
            // (e.g. a pool hiccup) must not overwrite the good recorded
            // measurement — hold here rather than poisoning `best`.
            if hashrate < MIN_HASHRATE_THS {
                return None;
            }
            self.pending_reject_mhz = None;
            if !near_cap {
                self.best = Some((
                    TuneSetpoint {
                        frequency_mhz: m.frequency_mhz,
                        core_voltage_mv: commanded_v,
                    },
                    hashrate,
                    efficiency,
                ));
            }
            return self.seek_further_or_lock(m, &caps);
        }

        // 4. A genuinely new candidate point: judge it directly against the
        //    best ever measured on this chip, never an extrapolated table.
        let at_cap_candidate = near_cap && self.best.is_some();
        if at_cap_candidate
            && let Some((b, ..)) = self.best
            && m.frequency_mhz > b.frequency_mhz + FREQ_TOLERANCE_MHZ
        {
            // Deterministic (power, not noisy hashrate): exclude at once.
            self.probe_ceiling_mhz = self.probe_ceiling_mhz.min(m.frequency_mhz);
            self.pending_reject_mhz = Some(m.frequency_mhz);
        }
        let strictly_improved = !at_cap_candidate && match self.best {
            None => true,
            Some((_, best_hash, best_eff)) => {
                if caps.seek_efficiency {
                    // Multi-step envelopes also accept equal-hashrate power
                    // savings: mean power down >= 1.5 % with mean hashrate
                    // within -1 % (power is far less noisy than hashrate).
                    let best_power = best_eff * best_hash;
                    let power_win = env.trim_steps_per_judgement > 1
                        && m.power_w <= best_power * (1.0 - POWER_SAVING_MARGIN)
                        && hashrate >= best_hash * (1.0 - HASHRATE_EQUAL_TOLERANCE);
                    power_win || efficiency < best_eff * (1.0 - IMPROVEMENT_MARGIN)
                } else {
                    hashrate > best_hash * (1.0 + IMPROVEMENT_MARGIN)
                }
            }
        };

        if strictly_improved {
            self.pending_reject_mhz = None;
            self.best = Some((
                TuneSetpoint {
                    frequency_mhz: m.frequency_mhz,
                    core_voltage_mv: commanded_v,
                },
                hashrate,
                efficiency,
            ));
            return self.seek_further_or_lock(m, &caps);
        }

        // Not an improvement. `best` is guaranteed set here — `None` forces
        // `strictly_improved`, so we can't reach this branch without one.
        let (best, ..) = self
            .best
            .expect("a non-improving candidate implies a recorded best");

        // Hash-seeking profiles get one chance to recover a clock ABOVE best
        // with more voltage before giving up on it. Only a climb can pay off
        // this way; a point at or below best just reverts (no point spending
        // ~5 min/step raising voltage at a clock that can't beat best, adding
        // heat the whole time).
        if caps.seek_hash
            && m.frequency_mhz > best.frequency_mhz + FREQ_TOLERANCE_MHZ
            && hashrate >= MIN_HASHRATE_THS
            && !near_cap
            && env.voltage_up(commanded_v) <= env.max_voltage_mv
            && m.power_w < caps.power_w - 1.0
        {
            let v = env.voltage_up(commanded_v);
            self.last_commanded_voltage_mv = Some(v);
            self.phase = TunePhase::Seeking;
            self.log_event(format!("no improvement: raising voltage to {v} mV"), m);
            return Some(TuneAction::SetVoltage(v));
        }

        // Reconcile back to the best point actually found, rather than holding
        // wherever the search ended.
        if (m.frequency_mhz - best.frequency_mhz).abs() > FREQ_TOLERANCE_MHZ {
            // A clock above best that lost even after voltage recovery is a
            // dead end — but the ~300 s hashrate estimate is noisy, so require
            // a SECOND consecutive failure at this clock before excluding it,
            // lest one unlucky window permanently cap a good clock. A collapse
            // below best just reverts upward without excluding anything.
            if m.frequency_mhz > best.frequency_mhz {
                let confirmed = self
                    .pending_reject_mhz
                    .is_some_and(|p| (p - m.frequency_mhz).abs() <= FREQ_TOLERANCE_MHZ);
                if confirmed {
                    self.probe_ceiling_mhz = self.probe_ceiling_mhz.min(m.frequency_mhz);
                    self.pending_reject_mhz = None;
                } else {
                    self.pending_reject_mhz = Some(m.frequency_mhz);
                    // A usefully-hashing point that merely underperformed might
                    // just be an unlucky window: hold and re-measure once before
                    // committing to exclude it. A collapse is unambiguous — don't
                    // sit at it, revert now (still only excluded on a second one).
                    if hashrate >= MIN_HASHRATE_THS {
                        self.phase = TunePhase::Seeking;
                        self.log_event(
                            format!(
                                "no improvement at {:.0} MHz: re-measuring before excluding",
                                m.frequency_mhz
                            ),
                            m,
                        );
                        return None;
                    }
                }
            }
            self.phase = TunePhase::Seeking;
            self.log_event(
                format!("reverting to best known-good {:.0} MHz", best.frequency_mhz),
                m,
            );
            return Some(TuneAction::SetFrequency(best.frequency_mhz));
        }
        if commanded_v.abs_diff(best.core_voltage_mv) > VOLT_TOLERANCE_MV {
            // An undervolt below best that lost efficiency/hashrate: record
            // it so the efficiency walk won't trim back down to it.
            if commanded_v < best.core_voltage_mv {
                self.probe_floor_mv = self.probe_floor_mv.max(commanded_v);
            }
            self.phase = TunePhase::Seeking;
            self.last_commanded_voltage_mv = Some(best.core_voltage_mv);
            self.log_event(
                format!("reverting to best known-good {} mV", best.core_voltage_mv),
                m,
            );
            return Some(TuneAction::SetVoltage(best.core_voltage_mv));
        }
        if self.phase != TunePhase::Locked {
            self.phase = TunePhase::Locked;
            self.log_event("converged: holding best known-good point", m);
        }
        None
    }

    /// Target-mode convergence: step frequency toward the setpoint and hold
    /// once within tolerance. Unlike the profile hill-climb this never
    /// records a "best" to climb past -- it always compares the live
    /// measurement directly against the fixed target value, matching the
    /// same principle the profile search uses (judge against reality, not
    /// an extrapolated table): the target itself is the only reference
    /// point, and convergence is driven purely by live `m.power_w` /
    /// `m.hashrate_ths`.
    fn evaluate_target(&mut self, m: &Metrics, target: TuneTarget) -> Option<TuneAction> {
        let (target_value, current) = match target {
            TuneTarget::Power(watts) => (watts, m.power_w),
            TuneTarget::Hashrate(ths) => (ths, m.hashrate_ths),
        };

        // Not usefully hashing yet: hold rather than judge a target against
        // a chip that hasn't spun up.
        if m.hashrate_ths < MIN_HASHRATE_THS {
            self.phase = TunePhase::Warmup;
            return None;
        }

        let error = target_value - current;
        let tolerance = (target_value.abs() * TARGET_TOLERANCE_FRACTION).max(f32::EPSILON);

        if error.abs() <= tolerance {
            self.stuck_at_limit_cycles = 0;
            if self.phase != TunePhase::Locked {
                self.phase = TunePhase::Locked;
                self.log_event(
                    format!("target reached: holding at {:.0} MHz", m.frequency_mhz),
                    m,
                );
            }
            return None;
        }

        let (min_freq, max_freq) = (self.envelope.min_freq_mhz, self.envelope.max_freq_mhz);
        let freq_step = self.envelope.freq_step_mhz;
        let step = if error > 0.0 { freq_step } else { -freq_step };
        let next = (m.frequency_mhz + step).clamp(min_freq, max_freq);

        // Pinned at a limit (the clamp landed back where we started) with
        // the setpoint still unreached: this is what "unreachable" means.
        if (next - m.frequency_mhz).abs() <= FREQ_TOLERANCE_MHZ {
            self.stuck_at_limit_cycles += 1;
            if self.stuck_at_limit_cycles >= UNREACHABLE_STRIKES {
                if self.phase != TunePhase::Unreachable {
                    self.phase = TunePhase::Unreachable;
                    self.log_event(
                        format!(
                            "target not reachable: holding at {:.0} MHz limit",
                            m.frequency_mhz
                        ),
                        m,
                    );
                }
            } else {
                self.phase = TunePhase::Converging;
            }
            return None;
        }

        self.stuck_at_limit_cycles = 0;
        self.phase = TunePhase::Converging;
        self.log_event(format!("converging: stepping to {next:.0} MHz"), m);
        Some(TuneAction::SetFrequency(next))
    }

    /// Having just confirmed or improved on `best`, try to extend the search
    /// (trim voltage for efficiency profiles, climb the clock for
    /// hash-seeking ones); lock if there's nothing further to try.
    fn seek_further_or_lock(&mut self, m: &Metrics, caps: &Caps) -> Option<TuneAction> {
        self.phase = TunePhase::Seeking;

        // Trim from the commanded voltage, not the drooping measured one, so a
        // step moves the setpoint by exactly one voltage step rather than
        // droop+step (which would overshoot the stability cliff).
        let env = self.envelope;
        let current_v = self.commanded_voltage(m);
        // Largest trim (up to `trim_steps_per_judgement` grid steps) that
        // stays on or above the floor and above the failed-trim `probe_floor`.
        let trimmed = (1..=env.trim_steps_per_judgement.max(1))
            .rev()
            .map(|k| env.voltage_down_n(current_v, k))
            .find(|t| *t >= env.min_voltage_mv && *t > self.probe_floor_mv)
            .unwrap_or_else(|| env.voltage_down(current_v));
        if caps.seek_efficiency && trimmed >= env.min_voltage_mv && trimmed > self.probe_floor_mv {
            self.pending_trim_from_mv = Some(current_v);
            self.last_commanded_voltage_mv = Some(trimmed);
            self.log_event(
                format!("trimming voltage to {trimmed} mV for efficiency"),
                m,
            );
            return Some(TuneAction::SetVoltage(trimmed));
        }

        let next = m.frequency_mhz + env.freq_step_mhz;
        let has_headroom = m.asic_temp_c < caps.temp_c - 3.0
            && m.power_w < caps.power_w - 1.0 - env.best_point_margin_w;
        if caps.seek_hash
            && next <= env.max_freq_mhz
            && next < self.probe_ceiling_mhz
            && has_headroom
        {
            self.log_event(format!("headroom: raising to {next:.0} MHz"), m);
            return Some(TuneAction::SetFrequency(next));
        }

        if self.phase != TunePhase::Locked {
            self.phase = TunePhase::Locked;
            self.log_event("converged: holding best known-good point", m);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The pre-envelope constants, now read from the BM1370 envelope, so the
    // existing tests keep asserting against the original numbers.
    const SETTLE_CYCLES: u32 = TuneEnvelope::bm1370().settle_cycles;
    const SAFETY_SETTLE_CYCLES: u32 = TuneEnvelope::bm1370().safety_settle_cycles;
    const MIN_FREQ_MHZ: f32 = TuneEnvelope::bm1370().min_freq_mhz;
    const MAX_FREQ_MHZ: f32 = TuneEnvelope::bm1370().max_freq_mhz;
    const FREQ_STEP_MHZ: f32 = TuneEnvelope::bm1370().freq_step_mhz;
    const AUTO_VOLT_CEIL_MV: u16 = TuneEnvelope::bm1370().max_voltage_mv;

    fn m(temp: f32, power: f32, hash_ths: f32, freq: f32, volt: u16) -> Metrics {
        Metrics {
            asic_temp_c: temp,
            power_w: power,
            hashrate_ths: hash_ths,
            frequency_mhz: freq,
            core_voltage_mv: volt,
            fan_min_percent: None,
            fan_auto: None,
            fail_rate_pct: None,
        }
    }

    /// Drive `evaluate` past the settle gate and return the action taken on
    /// the acting cycle.
    fn step_to_action(t: &mut AutoTuner, metrics: &Metrics) -> Option<TuneAction> {
        let mut last = None;
        for _ in 0..SETTLE_CYCLES {
            last = t.evaluate(metrics);
        }
        last
    }

    #[test]
    fn disabled_tuner_does_nothing() {
        let mut t = AutoTuner::default();
        assert_eq!(t.evaluate(&m(60.0, 12.0, 1.2, 525.0, 1150)), None);
    }

    #[test]
    fn settles_before_acting() {
        let mut t = AutoTuner::default();
        t.enable_profile(TuneProfile::MaxHash);
        let metrics = m(50.0, 12.0, 1.2, 525.0, 1150);
        // No action until the settle gate elapses.
        for _ in 0..SETTLE_CYCLES - 1 {
            assert_eq!(t.evaluate(&metrics), None);
        }
        assert!(matches!(
            t.evaluate(&metrics),
            Some(TuneAction::SetFrequency(_))
        ));
    }

    /// Drive `evaluate` through the (short) safety cadence and return the last
    /// action — over-cap back-off does not wait for the long tuning settle.
    fn safety_step(t: &mut AutoTuner, metrics: &Metrics) -> Option<TuneAction> {
        let mut last = None;
        for _ in 0..SAFETY_SETTLE_CYCLES {
            last = t.evaluate(metrics);
        }
        last
    }

    #[test]
    fn temp_breach_forces_fan_before_dropping_clock() {
        let mut t = AutoTuner::default();
        t.enable_profile(TuneProfile::Balanced); // temp cap 62
        let hot = m(70.0, 14.0, 1.3, 550.0, 1150);
        // First reaction to a temperature breach is to max the fan, NOT to
        // give up clock while the fan curve may still have headroom.
        assert_eq!(t.evaluate(&hot), Some(TuneAction::SetFanFull));
        assert_eq!(t.phase, TunePhase::BackedOff);
        assert!(t.fan_forced_full);
        // Fan already full and still over cap after the safety settle: only
        // now does the clock drop.
        let mut action = None;
        for _ in 0..SAFETY_SETTLE_CYCLES {
            action = t.evaluate(&hot);
        }
        assert_eq!(action, Some(TuneAction::SetFrequency(525.0)));
    }

    #[test]
    fn power_breach_drops_clock_without_touching_fan() {
        let mut t = AutoTuner::default();
        t.enable_profile(TuneProfile::Quiet); // power cap 12
        // Power breaches skip the fan (airflow doesn't cut watts) and drop
        // the clock directly.
        let action = safety_step(&mut t, &m(50.0, 15.0, 1.2, 525.0, 1150));
        assert_eq!(action, Some(TuneAction::SetFrequency(500.0)));
        assert!(!t.fan_forced_full);
    }

    #[test]
    fn restores_fan_when_temp_clears() {
        let mut t = AutoTuner::default();
        t.enable_profile(TuneProfile::MaxHash); // temp cap 68
        assert_eq!(
            t.evaluate(&m(70.0, 15.0, 1.0, 550.0, 1150)),
            Some(TuneAction::SetFanFull)
        );
        // Temperature back under cap: restore the automatic curve before
        // resuming normal tuning.
        assert_eq!(
            t.evaluate(&m(60.0, 15.0, 1.0, 550.0, 1150)),
            Some(TuneAction::RestoreFanAuto)
        );
        assert!(!t.fan_forced_full);
    }

    #[test]
    fn profile_switch_does_not_strand_a_forced_full_fan() {
        // Regression test: switching profiles while the fan is forced to
        // 100% from a prior breach must not lose track of that override.
        // Losing it meant the fan stayed physically pinned at 100% forever
        // (or until a fresh breach happened to re-arm the flag), even once
        // temperature was comfortably under the new profile's cap.
        let mut t = AutoTuner::default();
        t.enable_profile(TuneProfile::Balanced); // temp cap 62
        assert_eq!(
            t.evaluate(&m(70.0, 14.0, 1.3, 525.0, 1150)),
            Some(TuneAction::SetFanFull)
        );
        assert!(t.fan_forced_full);

        // Operator switches profile while the fan is still forced full.
        t.enable_profile(TuneProfile::Efficient); // temp cap 60
        assert!(
            t.fan_forced_full,
            "switching profiles must not forget the board's fan is still forced full"
        );

        // Comfortably under Efficient's release threshold (60 - 2 = 58): the
        // tuner must still be able to hand the fan back to the auto curve.
        assert_eq!(
            t.evaluate(&m(55.0, 10.0, 1.0, 400.0, 1100)),
            Some(TuneAction::RestoreFanAuto)
        );
        assert!(!t.fan_forced_full);
    }

    #[test]
    fn thermal_backoff_does_not_permanently_cap_climb() {
        // Regression test for the reported bug: a thermal excursion must not
        // teach the tuner that the clock is unreachable. The probe ceiling
        // (which gates future climbing) stays wide open through a full
        // temperature back-off.
        let mut t = AutoTuner::default();
        t.enable_profile(TuneProfile::MaxHash); // temp cap 68
        let hot = m(70.0, 15.0, 1.0, 550.0, 1150);
        assert_eq!(t.evaluate(&hot), Some(TuneAction::SetFanFull));
        let mut dropped = None;
        for _ in 0..SAFETY_SETTLE_CYCLES {
            dropped = t.evaluate(&hot);
        }
        assert_eq!(dropped, Some(TuneAction::SetFrequency(525.0)));
        assert_eq!(t.probe_ceiling_mhz, MAX_FREQ_MHZ + FREQ_STEP_MHZ);
    }

    #[test]
    fn over_cap_at_clock_floor_trims_voltage() {
        let mut t = AutoTuner::default();
        t.enable_profile(TuneProfile::Quiet); // power cap 12
        // Already at the clock floor and over the power cap: shed voltage.
        // Starts one step above the voltage floor so the trim has somewhere
        // to land (the floor itself equals BM1370's nominal-stock voltage,
        // so there is no headroom below it).
        let action = safety_step(&mut t, &m(50.0, 15.0, 1.0, MIN_FREQ_MHZ, 1160));
        assert_eq!(action, Some(TuneAction::SetVoltage(1150)));
    }

    #[test]
    fn maxhash_climbs_with_headroom() {
        let mut t = AutoTuner::default();
        t.enable_profile(TuneProfile::MaxHash);
        // Cool, low power, first solid point -> raise clock.
        let action = step_to_action(&mut t, &m(55.0, 13.0, 1.25, 550.0, 1150));
        assert_eq!(action, Some(TuneAction::SetFrequency(575.0)));
    }

    #[test]
    fn stable_point_with_headroom_climbs_the_clock() {
        // Judged on the measured hashrate directly (no nameplate table): a
        // solid point with thermal/power headroom pushes the clock up.
        let mut t = AutoTuner::default();
        t.enable_profile(TuneProfile::MaxHash);
        let action = step_to_action(&mut t, &m(55.0, 11.5, 1.0, 525.0, 1150));
        assert_eq!(action, Some(TuneAction::SetFrequency(550.0)));
    }

    #[test]
    fn reverts_to_best_when_higher_clock_does_not_improve() {
        let mut t = AutoTuner::default();
        t.enable_profile(TuneProfile::MaxHash);
        // Baseline 525 at the voltage ceiling, ~1.2 TH/s.
        step_to_action(&mut t, &m(55.0, 14.0, 1.2, 525.0, AUTO_VOLT_CEIL_MV));
        // A higher clock delivers no more hashrate and voltage is already
        // maxed (no recovery). First failure only flags it for re-measure —
        // one noisy window must not exclude a clock.
        let first = step_to_action(&mut t, &m(60.0, 15.0, 1.2, 600.0, AUTO_VOLT_CEIL_MV));
        assert_eq!(first, None);
        assert_eq!(t.probe_ceiling_mhz, MAX_FREQ_MHZ + FREQ_STEP_MHZ);
        // A second consecutive failure confirms it: revert to best and mark
        // 600 as the probe ceiling.
        let second = step_to_action(&mut t, &m(60.0, 15.0, 1.2, 600.0, AUTO_VOLT_CEIL_MV));
        assert_eq!(second, Some(TuneAction::SetFrequency(525.0)));
        assert_eq!(t.probe_ceiling_mhz, 600.0);
    }

    #[test]
    fn converges_reapplies_best_then_locks() {
        let mut t = AutoTuner::default();
        t.enable_profile(TuneProfile::MaxHash);
        let v = AUTO_VOLT_CEIL_MV;
        // 525 solid -> climb to 550.
        let a1 = step_to_action(&mut t, &m(55.0, 14.0, 1.2, 525.0, v));
        assert_eq!(a1, Some(TuneAction::SetFrequency(550.0)));
        // 550 no better, voltage maxed: first failure re-measures, second
        // reverts to 525.
        assert_eq!(step_to_action(&mut t, &m(56.0, 15.0, 1.2, 550.0, v)), None);
        let a2 = step_to_action(&mut t, &m(56.0, 15.0, 1.2, 550.0, v));
        assert_eq!(a2, Some(TuneAction::SetFrequency(525.0)));
        // Back at best; 550 is now the probe ceiling, so it locks here
        // instead of climbing into the known-bad clock again.
        let a3 = step_to_action(&mut t, &m(55.0, 14.0, 1.2, 525.0, v));
        assert_eq!(a3, None);
        assert_eq!(t.phase, TunePhase::Locked);
        assert_eq!(t.best.map(|(sp, ..)| sp.frequency_mhz), Some(525.0));
    }

    #[test]
    fn one_noisy_window_does_not_exclude_a_good_clock() {
        // The noise guard: a single unlucky hashrate window at a higher clock
        // flags it for re-measure but must NOT lower the probe ceiling. If the
        // clock then measures well, it becomes the new best and climbing
        // continues — the original bug (permanent exclusion) cannot recur.
        let mut t = AutoTuner::default();
        t.enable_profile(TuneProfile::MaxHash);
        // Baseline 525 ~1.0 TH/s at the voltage ceiling (no recovery ladder).
        step_to_action(&mut t, &m(55.0, 14.0, 1.0, 525.0, AUTO_VOLT_CEIL_MV));
        // 550 reads low once (unlucky window): re-measure, ceiling untouched.
        assert_eq!(
            step_to_action(&mut t, &m(55.0, 14.0, 1.0, 550.0, AUTO_VOLT_CEIL_MV)),
            None
        );
        assert_eq!(t.probe_ceiling_mhz, MAX_FREQ_MHZ + FREQ_STEP_MHZ);
        // 550 now measures well: it becomes best and the tuner keeps climbing.
        let action = step_to_action(&mut t, &m(55.0, 14.0, 1.1, 550.0, AUTO_VOLT_CEIL_MV));
        assert_eq!(action, Some(TuneAction::SetFrequency(575.0)));
        assert_eq!(t.best.map(|(sp, ..)| sp.frequency_mhz), Some(550.0));
    }

    #[test]
    fn best_voltage_tracks_commanded_not_measured_vout() {
        // Regression for the vout-droop domain confusion: after the tuner
        // raises voltage, `best` and the `at_best` match use the commanded
        // value, so a measured reading that droops below it still locks
        // instead of livelocking in a no-op revert.
        let mut t = AutoTuner::default();
        t.enable_profile(TuneProfile::MaxHash);
        // Baseline 525/1150 -> climb to 550.
        step_to_action(&mut t, &m(55.0, 13.0, 1.0, 525.0, 1150));
        // 550 no gain -> raise voltage to 1160 (commanded).
        assert_eq!(
            step_to_action(&mut t, &m(55.0, 13.0, 1.0, 550.0, 1150)),
            Some(TuneAction::SetVoltage(1160))
        );
        // 1160 improves and becomes best; measured vout reads 1152 (droops
        // below the 1160 commanded). The tuner must still recognise it as best
        // and not thrash.
        let action = step_to_action(&mut t, &m(58.0, 14.0, 1.1, 550.0, 1152));
        // Improved -> record best at commanded 1160 and keep climbing.
        assert_eq!(action, Some(TuneAction::SetFrequency(575.0)));
        assert_eq!(t.best.map(|(sp, ..)| sp.core_voltage_mv), Some(1160));
    }

    #[test]
    fn fan_episode_does_not_flap_at_the_cap() {
        // Hysteresis: with the die hovering just below the cap after a forced
        // -full episode, the fan is NOT released (which would flap it every
        // cycle and stall the tuner). It stays forced until a clear margin.
        let mut t = AutoTuner::default();
        t.enable_profile(TuneProfile::MaxHash); // temp cap 68
        assert_eq!(
            t.evaluate(&m(70.0, 15.0, 1.0, 550.0, 1150)),
            Some(TuneAction::SetFanFull)
        );
        // 67C is under the 68 cap but inside the 2C hysteresis band: hold the
        // fan full, do not flap.
        assert_eq!(t.evaluate(&m(67.0, 15.0, 1.0, 550.0, 1150)), None);
        assert!(t.fan_forced_full);
        // Only a clear drop below cap-hysteresis releases it.
        assert_eq!(
            t.evaluate(&m(65.0, 15.0, 1.0, 550.0, 1150)),
            Some(TuneAction::RestoreFanAuto)
        );
        assert!(!t.fan_forced_full);
    }

    #[test]
    fn voltage_steps_accumulate_despite_lagging_measurement() {
        // Regression test for the voltage-drift defect: raise steps are
        // computed from the tuner's own last-commanded value, not the
        // regulator's lagging measured vout.
        let mut t = AutoTuner::default();
        t.enable_profile(TuneProfile::MaxHash);
        // Baseline at 525/1150 -> climb to 550.
        step_to_action(&mut t, &m(55.0, 13.0, 1.0, 525.0, 1150));
        // 550 no hashrate gain -> first voltage raise to 1160.
        let a1 = step_to_action(&mut t, &m(55.0, 13.0, 1.0, 550.0, 1150));
        assert_eq!(a1, Some(TuneAction::SetVoltage(1160)));
        // Measurement still reads the old 1150 (regulator lag), but the next
        // raise builds on the commanded 1160 -> 1170, not another 1160.
        let a2 = step_to_action(&mut t, &m(55.0, 13.0, 1.0, 550.0, 1150));
        assert_eq!(a2, Some(TuneAction::SetVoltage(1170)));
    }

    #[test]
    fn collapse_after_best_reverts_instead_of_holding() {
        // Regression test for the warmup trap: once a good point exists, a
        // hashrate collapse must revert toward it, not sit in warmup forever.
        let mut t = AutoTuner::default();
        t.enable_profile(TuneProfile::MaxHash);
        step_to_action(&mut t, &m(55.0, 13.0, 1.0, 525.0, 1150));
        let action = step_to_action(&mut t, &m(55.0, 13.0, 0.1, 575.0, 1150));
        assert_eq!(action, Some(TuneAction::SetFrequency(525.0)));
    }

    #[test]
    fn efficient_trims_voltage_when_stable() {
        let mut t = AutoTuner::default();
        t.enable_profile(TuneProfile::Efficient);
        // Starts one step above the voltage floor so the trim has somewhere
        // to land (the floor itself equals BM1370's nominal-stock voltage).
        let action = step_to_action(&mut t, &m(55.0, 12.0, 1.2, 525.0, 1160));
        assert_eq!(action, Some(TuneAction::SetVoltage(1150)));
    }

    #[test]
    fn efficient_reverts_and_locks_when_undervolt_hurts() {
        // The efficiency walk must not oscillate: an undervolt that worsens
        // efficiency is reverted and its voltage marked as the probe floor.
        let mut t = AutoTuner::default();
        t.enable_profile(TuneProfile::Efficient); // temp cap 60, power cap 13
        // 1160 solid -> trim to 1150 (the voltage floor, one step below).
        let a1 = step_to_action(&mut t, &m(55.0, 12.0, 1.2, 525.0, 1160));
        assert_eq!(a1, Some(TuneAction::SetVoltage(1150)));
        // 1150 is worse (same hashrate, but we assert efficiency didn't beat
        // best: lower power at same hashrate WOULD be better, so make power
        // higher to model an undervolt that cost hashrate). Model the
        // undervolt hurting: hashrate drops so efficiency worsens.
        let a2 = step_to_action(&mut t, &m(55.0, 12.0, 1.0, 525.0, 1150));
        assert_eq!(a2, Some(TuneAction::SetVoltage(1160)));
        assert_eq!(t.probe_floor_mv, 1150);
        // Back at best; the floor blocks re-trimming to 1150, so it locks.
        let a3 = step_to_action(&mut t, &m(55.0, 12.0, 1.2, 525.0, 1160));
        assert_eq!(a3, None);
        assert_eq!(t.phase, TunePhase::Locked);
    }

    #[test]
    fn quiet_locks_when_stable_and_no_headroom_sought() {
        let mut t = AutoTuner::default();
        t.enable_profile(TuneProfile::Quiet); // not seek_hash, not efficiency
        let action = step_to_action(&mut t, &m(50.0, 11.0, 1.2, 525.0, 1150));
        assert_eq!(action, None);
        assert_eq!(t.phase, TunePhase::Locked);
    }

    // --- target mode ---------------------------------------------------

    #[test]
    fn power_target_steps_toward_setpoint_then_locks() {
        let mut t = AutoTuner::default();
        t.enable_target(TuneTarget::Power(15.0));
        // Below target: step up.
        let a1 = step_to_action(&mut t, &m(50.0, 12.0, 1.2, 525.0, 1150));
        assert_eq!(a1, Some(TuneAction::SetFrequency(550.0)));
        assert_eq!(t.phase, TunePhase::Converging);
        // Within 2% tolerance of 15.0 W: locks, no further action.
        let a2 = step_to_action(&mut t, &m(50.0, 14.9, 1.3, 550.0, 1150));
        assert_eq!(a2, None);
        assert_eq!(t.phase, TunePhase::Locked);
    }

    #[test]
    fn hashrate_target_steps_down_when_above_setpoint() {
        let mut t = AutoTuner::default();
        t.enable_target(TuneTarget::Hashrate(1.2));
        // Above target: step down.
        let action = step_to_action(&mut t, &m(50.0, 15.0, 1.4, 550.0, 1150));
        assert_eq!(action, Some(TuneAction::SetFrequency(525.0)));
        assert_eq!(t.phase, TunePhase::Converging);
    }

    #[test]
    fn unreachable_hashrate_target_pins_at_chip_max_and_reports_unreachable() {
        let mut t = AutoTuner::default();
                t.enable_target(TuneTarget::Hashrate(999.0)); // far beyond any real chip
        // First step: climbs toward max, pinned at the chip ceiling
        // (650 MHz) but still far short -> Converging (first strike).
        let a1 = step_to_action(&mut t, &m(50.0, 15.0, 1.0, 650.0, 340));
        assert_eq!(a1, None);
        assert_eq!(t.phase, TunePhase::Converging);
        // Second consecutive settled cycle still pinned at the ceiling ->
        // declared unreachable rather than looping forever.
        let a2 = step_to_action(&mut t, &m(50.0, 15.0, 1.0, 650.0, 340));
        assert_eq!(a2, None);
        assert_eq!(t.phase, TunePhase::Unreachable);
    }

    #[test]
    fn target_mode_safety_breach_overrides_convergence() {
        // The breach branch runs before any mode dispatch, so a target
        // setpoint must not prevent the same thermal back-off profiles get.
        let mut t = AutoTuner::default();
        t.enable_target(TuneTarget::Power(30.0)); // caps.power_w = 31.5
        assert_eq!(
            t.evaluate(&m(70.0, 15.0, 1.0, 550.0, 1150)), // over the 62C safety cap
            Some(TuneAction::SetFanFull)
        );
        assert_eq!(t.phase, TunePhase::BackedOff);
    }

    #[test]
    fn multi_chip_power_caps_scale_correctly() {
        let mut t = AutoTuner::default();
        t.set_chip_count(4);
        t.enable_profile(TuneProfile::MaxHash); // base cap 22W * 4 = 88W

        // 60 W is under the 88W cap, should not trigger breach on evaluation.
        let action = t.evaluate(&m(55.0, 60.0, 4.0, 525.0, 1150));
        assert_eq!(action, None);
        assert_eq!(t.phase, TunePhase::Warmup);

        // Reset cycles_since_change to 0.
        t.enable_profile(TuneProfile::MaxHash);
        // 90 W is over the 88W cap, should trigger a safety breach and back off.
        let action = safety_step(&mut t, &m(55.0, 90.0, 4.0, 525.0, 1150));
        assert_eq!(t.phase, TunePhase::BackedOff);
        assert_eq!(action, Some(TuneAction::SetFrequency(500.0)));
    }

    #[test]
    fn fan_auto_slowly_ramps_instead_of_forcing_100() {
        let mut t = AutoTuner::default();
        t.enable_profile(TuneProfile::Balanced); // temp cap 62

        // Metrics with fan in Auto mode and min_percent at 33%
        let hot_metrics = Metrics {
            asic_temp_c: 70.0,
            power_w: 14.0,
            hashrate_ths: 1.3,
            frequency_mhz: 550.0,
            core_voltage_mv: 1150,
            fan_min_percent: Some(33),
            fan_auto: Some(true),
            fail_rate_pct: None,
        };

        // First breach should slowly increase the fan minimum instead of forcing 100%
        assert_eq!(
            t.evaluate(&hot_metrics),
            Some(TuneAction::SetFanMinPercent(38))
        );
        assert_eq!(t.phase, TunePhase::BackedOff);
        assert!(!t.fan_forced_full);

        // Next evaluation within safety settle cycles returns None
        assert_eq!(t.evaluate(&hot_metrics), None);

        // Advance past safety settle cycles
        for _ in 1..SAFETY_SETTLE_CYCLES {
            t.evaluate(&hot_metrics);
        }

        // Under second breach with new telemetry min_percent at 38%, it should increment again to 43%
        let hot_metrics_updated = Metrics {
            fan_min_percent: Some(38),
            ..hot_metrics
        };
        assert_eq!(
            t.evaluate(&hot_metrics_updated),
            Some(TuneAction::SetFanMinPercent(43))
        );
        assert!(!t.fan_forced_full);
    }

    // --- tuning envelope ---------------------------------------------------

    /// A Nano 3S-shaped envelope: one rail for many chips, a coarse 26 mV
    /// voltage grid, and a 3600 mV value the board collapses at.
    fn nano_env() -> TuneEnvelope {
        TuneEnvelope {
            min_freq_mhz: 210.0,
            max_freq_mhz: 440.0,
            freq_step_mhz: 10.0,
            min_voltage_mv: 3392,
            max_voltage_mv: 3800,
            voltage_step_mv: 26,
            forbidden_voltages_mv: &[3600],
            settle_cycles: 150,
            safety_settle_cycles: 6,
            power_settle_cycles: 6,
            quiet: CapLimits {
                temp_c: 55.0,
                power_w: 60.0,
            },
            efficient: CapLimits {
                temp_c: 60.0,
                power_w: 90.0,
            },
            balanced: CapLimits {
                temp_c: 62.0,
                power_w: 110.0,
            },
            max_hash: CapLimits {
                temp_c: 68.0,
                power_w: 137.0,
            },
            target_temp_c: 68.0,
            target_hashrate_power_w: 137.0,
            target_power_headroom: 1.05,
            max_fail_rate_pct: Some(10.0),
            average_judgement: false,
            best_point_margin_w: 0.0,
            trim_steps_per_judgement: 1,
        }
    }

    fn nano_tuner(profile: TuneProfile) -> AutoTuner {
        let mut t = AutoTuner::default();
        t.set_envelope(nano_env());
        t.enable_profile(profile);
        t
    }

    fn nano_m(power: f32, volt: u16, fail: Option<f32>) -> Metrics {
        Metrics {
            fail_rate_pct: fail,
            ..m(50.0, power, 1.0, 300.0, volt)
        }
    }

    #[test]
    fn bm1370_envelope_matches_the_old_constants() {
        let e = TuneEnvelope::bm1370();
        assert_eq!(e.min_freq_mhz, 490.0);
        assert_eq!(e.max_freq_mhz, 650.0);
        assert_eq!(e.freq_step_mhz, 25.0);
        assert_eq!(e.min_voltage_mv, 1150);
        assert_eq!(
            e.max_voltage_mv,
            1250
        );
        assert_eq!(e.voltage_step_mv, 10);
        assert!(e.forbidden_voltages_mv.is_empty());
        assert_eq!(e.settle_cycles, 150);
        assert_eq!(e.safety_settle_cycles, 6);
        assert_eq!((e.quiet.temp_c, e.quiet.power_w), (55.0, 12.0));
        assert_eq!((e.efficient.temp_c, e.efficient.power_w), (60.0, 13.0));
        assert_eq!((e.balanced.temp_c, e.balanced.power_w), (62.0, 15.0));
        assert_eq!((e.max_hash.temp_c, e.max_hash.power_w), (68.0, 22.0));
        assert_eq!(e.target_temp_c, 68.0);
        assert_eq!(e.target_hashrate_power_w, 22.0);
        assert_eq!(e.target_power_headroom, 1.05);
        assert_eq!(e.max_fail_rate_pct, None);
        assert_eq!(TuneEnvelope::default(), e);
    }

    #[test]
    fn voltage_steps_land_on_the_grid_and_skip_forbidden() {
        let e = nano_env();
        let mut v = 3704;
        let mut seen = Vec::new();
        for _ in 0..5 {
            v = e.voltage_down(v);
            seen.push(v);
        }
        // 3600 is forbidden, so 3626 steps straight to 3574.
        assert_eq!(seen, [3678, 3652, 3626, 3574, 3548]);
        assert_eq!(e.voltage_up(3574), 3626);
        assert_eq!(e.voltage_up(3548), 3574);
    }

    #[test]
    fn efficient_trim_walks_the_grid_past_the_forbidden_voltage() {
        let mut t = nano_tuner(TuneProfile::Efficient);
        let (mut v, mut power) = (3704u16, 80.0f32);
        let mut seen = Vec::new();
        for _ in 0..5 {
            let action = step_to_action(&mut t, &nano_m(power, v, None));
            let Some(TuneAction::SetVoltage(next)) = action else {
                panic!("expected a voltage trim, got {action:?}");
            };
            seen.push(next);
            v = next;
            power *= 0.95; // each trim improves efficiency past the margin
        }
        assert_eq!(seen, [3678, 3652, 3626, 3574, 3548]);
    }

    #[test]
    fn trim_stops_at_the_voltage_floor() {
        let mut t = nano_tuner(TuneProfile::Efficient);
        // 3400 - 26 = 3374 is below 3392: no trim, the search just locks.
        assert_eq!(step_to_action(&mut t, &nano_m(80.0, 3400, None)), None);
        assert_eq!(t.phase, TunePhase::Locked);
    }

    #[test]
    fn safety_backoff_clamps_voltage_at_the_floor() {
        let mut t = nano_tuner(TuneProfile::Efficient);
        let hot = m(70.0, 80.0, 1.0, 210.0, 3400); // clock already at the floor
        assert_eq!(t.evaluate(&hot), Some(TuneAction::SetFanFull));
        let mut action = None;
        for _ in 0..SAFETY_SETTLE_CYCLES {
            action = t.evaluate(&hot);
        }
        assert_eq!(action, Some(TuneAction::SetVoltage(3392)));
    }

    #[test]
    fn fail_rate_over_max_after_a_trim_steps_back_and_locks() {
        let mut t = nano_tuner(TuneProfile::Efficient);
        assert_eq!(
            step_to_action(&mut t, &nano_m(80.0, 3704, Some(2.0))),
            Some(TuneAction::SetVoltage(3678))
        );
        // The trim settled with 15% failures: back to the last good voltage.
        assert_eq!(
            step_to_action(&mut t, &nano_m(76.0, 3678, Some(15.0))),
            Some(TuneAction::SetVoltage(3704))
        );
        assert_eq!(t.phase, TunePhase::Locked);
        assert!(t.log.back().unwrap().message.contains("fail rate"));
        // Back at the good voltage it stays put rather than trimming again.
        assert_eq!(step_to_action(&mut t, &nano_m(80.0, 3704, Some(2.0))), None);
        assert_eq!(t.phase, TunePhase::Locked);
    }

    #[test]
    fn breach_after_a_trim_cancels_the_fail_rate_revert() {
        let mut t = nano_tuner(TuneProfile::Efficient);
        assert_eq!(
            step_to_action(&mut t, &nano_m(80.0, 3704, Some(2.0))),
            Some(TuneAction::SetVoltage(3678))
        );
        // Overheats before the trim is judged.
        let hot = Metrics {
            fail_rate_pct: Some(2.0),
            ..m(70.0, 76.0, 1.0, 300.0, 3678)
        };
        assert_eq!(t.evaluate(&hot), Some(TuneAction::SetFanFull));
        // Cooled again with a high fail rate: no revert-and-lock on the
        // strength of the old trim. The stability gate raises one step
        // because this point is unstable, and keeps seeking.
        step_to_action(&mut t, &nano_m(76.0, 3678, Some(15.0)));
        assert_ne!(t.phase, TunePhase::Locked);
        assert!(t.log.back().unwrap().message.contains("raising voltage"));
    }

    #[test]
    fn unstable_point_raises_voltage_instead_of_climbing() {
        // The 2026-10-24 failure: Balanced at 72 % fail kept raising the clock.
        let mut t = nano_tuner(TuneProfile::Balanced);
        assert_eq!(
            step_to_action(&mut t, &nano_m(85.0, 3444, Some(72.0))),
            Some(TuneAction::SetVoltage(3470))
        );
        assert!(t.best_point().is_none());
        // Stable now: the efficiency walk never trims back to 3444.
        let a = step_to_action(&mut t, &nano_m(88.0, 3470, Some(3.0)));
        assert_ne!(a, Some(TuneAction::SetVoltage(3444)));
    }

    #[test]
    fn restored_memory_keeps_the_trim_above_the_old_floor() {
        // Learned: best 3652 mV, 3626 ruled out.
        let mut t = nano_tuner(TuneProfile::Efficient);
        step_to_action(&mut t, &nano_m(80.0, 3652, Some(1.0)));
        let mut mem = t.memory().expect("best measured");
        mem.probe_floor_mv = 3626;
        // Reboot: fresh tuner, same profile, memory restored.
        let mut r = nano_tuner(TuneProfile::Efficient);
        r.restore_memory(mem);
        assert_eq!(r.memory(), Some(mem));
        // At best again: the efficiency walk may not trim to 3626 or below.
        let a = step_to_action(&mut r, &nano_m(80.0, 3652, Some(1.0)));
        assert!(
            !matches!(a, Some(TuneAction::SetVoltage(v)) if v <= 3626),
            "trimmed to a ruled-out voltage: {a:?}"
        );
        // Without the memory the same tuner trims straight to 3626.
        let mut f = nano_tuner(TuneProfile::Efficient);
        assert_eq!(
            step_to_action(&mut f, &nano_m(80.0, 3652, Some(1.0))),
            Some(TuneAction::SetVoltage(3626))
        );
    }

    #[test]
    fn power_breach_waits_power_settle_between_cuts() {
        let env = TuneEnvelope { safety_settle_cycles: 1, power_settle_cycles: 4, ..nano_env() };
        let mut t = AutoTuner::default();
        t.set_envelope(env);
        t.enable_profile(TuneProfile::Efficient); // power cap 90
        let over = m(50.0, 95.0, 4.0, 300.0, 3704);
        let cuts = (0..8)
            .filter(|_| matches!(t.evaluate(&over), Some(TuneAction::SetFrequency(_))))
            .count();
        assert_eq!(cuts, 2);
        // A temperature breach still acts at once (fan first).
        assert_eq!(t.evaluate(&m(70.0, 80.0, 4.0, 300.0, 3704)), Some(TuneAction::SetFanFull));
    }

    #[test]
    fn unstable_point_at_power_cap_lowers_the_clock() {
        let mut t = nano_tuner(TuneProfile::Balanced); // power cap 110
        assert_eq!(
            step_to_action(&mut t, &nano_m(109.5, 3678, Some(40.0))),
            Some(TuneAction::SetFrequency(290.0))
        );
    }

    #[test]
    fn fail_rate_none_or_unlimited_leaves_behavior_unchanged() {
        // Board reports no fail rate: keeps trimming despite a max being set.
        let mut t = nano_tuner(TuneProfile::Efficient);
        step_to_action(&mut t, &nano_m(80.0, 3704, None));
        assert_eq!(
            step_to_action(&mut t, &nano_m(76.0, 3678, None)),
            Some(TuneAction::SetVoltage(3652))
        );

        // Envelope sets no max (every current board): a reported rate is inert.
        let mut t = AutoTuner::default();
        t.enable_profile(TuneProfile::Efficient);
        let hi = |power, volt| Metrics {
            fail_rate_pct: Some(90.0),
            ..m(50.0, power, 1.0, 525.0, volt)
        };
        step_to_action(&mut t, &hi(12.0, 1200));
        assert_eq!(
            step_to_action(&mut t, &hi(11.0, 1190)),
            Some(TuneAction::SetVoltage(1180))
        );
    }

    // --- averaged judgement + cap hysteresis ---------------------------------

    fn avg_env() -> TuneEnvelope {
        TuneEnvelope {
            settle_cycles: 4,
            safety_settle_cycles: 1,
            power_settle_cycles: 1,
            average_judgement: true,
            ..nano_env()
        }
    }

    fn avg_tuner(profile: TuneProfile, env: TuneEnvelope) -> AutoTuner {
        let mut t = AutoTuner::default();
        t.set_envelope(env);
        t.enable_profile(profile);
        t
    }

    /// Feeds `n` samples alternating +/- `noise` (fraction) on hashrate.
    fn noisy(t: &mut AutoTuner, power: f32, hash: f32, noise: f32, volt: u16, n: u32) -> Option<TuneAction> {
        let mut last = None;
        for k in 0..n {
            let sign = if k % 2 == 0 { 1.0 } else { -1.0 };
            last = t.evaluate(&m(50.0, power, hash * (1.0 + sign * noise), 300.0, volt));
        }
        last
    }

    #[test]
    fn averaged_judgement_accepts_a_real_trim_despite_noise() {
        // Baseline 80 W / 4.88 TH/s, then a trim to 76 W (5 % better J/TH).
        // The judging sample reads 4 % low; alone it would look like a 1 %
        // gain and be reverted. The window mean sees the real gain.
        let mut t = avg_tuner(TuneProfile::Efficient, avg_env());
        assert_eq!(noisy(&mut t, 80.0, 4.88, 0.0, 3704, 4), Some(TuneAction::SetVoltage(3678)));
        assert_eq!(noisy(&mut t, 76.0, 4.88, 0.04, 3678, 4), Some(TuneAction::SetVoltage(3652)));

        // Same samples with averaging off: the trim is (wrongly) reverted.
        let mut u = avg_tuner(TuneProfile::Efficient, TuneEnvelope { average_judgement: false, ..avg_env() });
        assert_eq!(noisy(&mut u, 80.0, 4.88, 0.0, 3704, 4), Some(TuneAction::SetVoltage(3678)));
        assert_eq!(noisy(&mut u, 76.0, 4.88, 0.04, 3678, 4), Some(TuneAction::SetVoltage(3704)));
    }

    #[test]
    fn averaged_judgement_uses_mean_fail_rate_and_reports_progress() {
        let mut t = avg_tuner(TuneProfile::Efficient, avg_env());
        noisy(&mut t, 80.0, 4.88, 0.0, 3704, 4);
        assert_eq!(t.averaging_progress(), (0, 4));
        // Fail rate 12, 2, 12, 2: mean 7 < 10 -> trim stands.
        let mut last = None;
        for (i, f) in [12.0, 2.0, 12.0, 2.0].into_iter().enumerate() {
            last = t.evaluate(&Metrics { fail_rate_pct: Some(f), ..m(50.0, 76.0, 4.88, 300.0, 3678) });
            assert_eq!(t.averaging_progress().0, if i < 3 { i as u32 + 1 } else { 0 });
        }
        assert_eq!(last, Some(TuneAction::SetVoltage(3652)));
    }

    #[test]
    fn averaged_judgement_still_trips_safety_on_one_hot_sample() {
        let mut t = avg_tuner(TuneProfile::Efficient, avg_env()); // temp cap 60
        noisy(&mut t, 80.0, 4.88, 0.0, 3704, 2);
        let hot = m(75.0, 80.0, 4.88, 300.0, 3704);
        assert_eq!(t.evaluate(&hot), Some(TuneAction::SetFanFull));
        // Fan already full, next hot sample after the 1-cycle safety settle.
        assert_eq!(t.evaluate(&hot), Some(TuneAction::SetFrequency(290.0)));
        // Power breach is also instantaneous.
        let mut t = avg_tuner(TuneProfile::Efficient, avg_env()); // power cap 90
        noisy(&mut t, 80.0, 4.88, 0.0, 3704, 2);
        t.evaluate(&m(50.0, 80.0, 4.88, 300.0, 3704));
        assert_eq!(t.evaluate(&m(50.0, 95.0, 4.88, 300.0, 3704)), Some(TuneAction::SetFrequency(290.0)));
    }

    #[test]
    fn default_envelope_does_not_average() {
        let e = TuneEnvelope::bm1370();
        assert!(!e.average_judgement);
        assert_eq!(e.best_point_margin_w, 0.0);
        let mut t = AutoTuner::default();
        t.enable_profile(TuneProfile::Efficient);
        t.evaluate(&m(50.0, 12.0, 1.0, 525.0, 1200));
        assert_eq!(t.averaging_progress().0, 0);
    }

    /// Climb sim: power is deterministic in clock (101 W + 0.4 W/MHz above
    /// 300), hashrate grows with clock, power drifts +/-0.5 W sample to
    /// sample. Returns the iteration of the last frequency change.
    fn climb_sim(margin: f32) -> (usize, f32) {
        let env = TuneEnvelope { best_point_margin_w: margin, ..avg_env() };
        let mut t = avg_tuner(TuneProfile::MaxHash, env); // cap 137 W
        let mut freq = 300.0_f32;
        let mut last_change = 0;
        for i in 0..400 {
            let drift = if i % 2 == 0 { 0.5 } else { -0.5 };
            let power = 101.0 + 0.4 * (freq - 300.0) + drift;
            let hash = (freq / 100.0).powi(2) * 0.5;
            match t.evaluate(&m(50.0, power, hash, freq, 3704)) {
                Some(TuneAction::SetFrequency(f)) => {
                    freq = f;
                    last_change = i;
                }
                _ => {}
            }
        }
        (last_change, freq)
    }

    #[test]
    fn cap_hysteresis_stops_oscillation_across_the_power_cap() {
        // 380 MHz = 133.2 W (inside cap - 3 margin = 134), 390 MHz = 137.2 W.
        let (last, freq) = climb_sim(3.0);
        assert!(last < 200, "still changing clock at iteration {last}");
        assert_eq!(freq, 380.0);
        // Without the margin the same plant keeps hopping across the cap.
        let (last, _) = climb_sim(0.0);
        assert!(last >= 300, "control case should oscillate, last change {last}");
    }

    #[test]
    fn point_inside_the_margin_is_not_recorded_as_best() {
        let env = TuneEnvelope { best_point_margin_w: 3.0, ..avg_env() };
        let mut t = avg_tuner(TuneProfile::MaxHash, env);
        // First point 135 W (inside the margin, cap 137): nothing to revert
        // to, so it is recorded anyway.
        noisy(&mut t, 135.0, 5.0, 0.0, 3704, 4);
        assert!(t.best.is_some());
        // A later improving point inside the margin is NOT adopted.
        let before = t.best;
        let a = t.evaluate(&m(50.0, 135.5, 6.0, 310.0, 3704));
        assert_eq!(a, None);
        for _ in 0..3 {
            t.evaluate(&m(50.0, 135.5, 6.0, 310.0, 3704));
        }
        assert_eq!(t.best, before);
    }

    // --- multi-step trims ------------------------------------------------------

    fn v3_env() -> TuneEnvelope {
        TuneEnvelope { trim_steps_per_judgement: 3, ..avg_env() }
    }

    #[test]
    fn three_step_trim_path_skips_3600() {
        let e = v3_env();
        assert_eq!(e.voltage_down_n(3704, 3), 3626);
        // 3626 - 78 = 3548: passes over 3600 without landing on it.
        assert_eq!(e.voltage_down_n(3626, 3), 3548);
        // Landing exactly on 3600 takes one more step.
        assert_eq!(e.voltage_down_n(3678, 3), 3574 - 0); // 3600 -> 3574
        assert_eq!(e.voltage_down_n(3704, 1), e.voltage_down(3704));
        // bm1370 is unchanged.
        let b = TuneEnvelope::bm1370();
        assert_eq!(b.trim_steps_per_judgement, 1);
        assert_eq!(b.voltage_down_n(1200, 1), b.voltage_down(1200));
    }

    #[test]
    fn efficient_trims_three_steps_at_once_then_again() {
        let mut t = avg_tuner(TuneProfile::Efficient, v3_env());
        assert_eq!(noisy(&mut t, 80.0, 4.88, 0.0, 3704, 4), Some(TuneAction::SetVoltage(3626)));
        // 6 % less power, same hashrate: accepted, trims again.
        assert_eq!(noisy(&mut t, 75.0, 4.88, 0.0, 3626, 4), Some(TuneAction::SetVoltage(3548)));
    }

    #[test]
    fn failed_multi_step_trim_reverts_then_refines_with_smaller_steps() {
        let mut t = avg_tuner(TuneProfile::Efficient, v3_env());
        noisy(&mut t, 80.0, 4.88, 0.0, 3704, 4); // -> 3626
        // Mean fail rate 15 % > 10 %: back to the last good voltage.
        let mut last = None;
        for _ in 0..4 {
            last = t.evaluate(&Metrics { fail_rate_pct: Some(15.0), ..m(50.0, 75.0, 4.88, 300.0, 3626) });
        }
        assert_eq!(last, Some(TuneAction::SetVoltage(3704)));
        assert_eq!(t.phase, TunePhase::Locked);
        // At the good point again: 3 steps would reach the failed 3626, so
        // the refinement is a 2-step trim, then (if it fails) 1 step.
        assert_eq!(noisy(&mut t, 80.0, 4.88, 0.0, 3704, 4), Some(TuneAction::SetVoltage(3652)));
    }

    #[test]
    fn power_at_equal_hashrate_is_accepted_for_multi_step_only() {
        // 2 % less power, hashrate -0.5 %: J/TH gain only ~1.5 % (< 3 %).
        let run = |env: TuneEnvelope| {
            let mut t = avg_tuner(TuneProfile::Efficient, env);
            noisy(&mut t, 80.0, 4.88, 0.0, 3704, 4);
            noisy(&mut t, 78.4, 4.856, 0.0, 3626, 4)
        };
        assert_eq!(run(v3_env()), Some(TuneAction::SetVoltage(3548)));
        // Single-step envelope keeps the old criterion: reverts.
        assert_eq!(run(TuneEnvelope { trim_steps_per_judgement: 1, ..avg_env() }), Some(TuneAction::SetVoltage(3704)));
        // Hashrate down 3 %: not "equal hashrate", power win rejected.
        let mut t = avg_tuner(TuneProfile::Efficient, v3_env());
        noisy(&mut t, 80.0, 4.88, 0.0, 3704, 4);
        assert_eq!(noisy(&mut t, 78.4, 4.73, 0.0, 3626, 4), Some(TuneAction::SetVoltage(3704)));
    }
}
