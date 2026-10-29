//! Nano 3S adapter for the owner autotuner ("owner tuner").
//!
//! Everything here is pure (no IPC, no files) so it tests on the host without
//! the `nano3s` feature. `nano3s.rs::run_worker` owns the hardware side: it
//! builds [`Metrics`] with [`metrics_from_readings`], ticks [`OwnerTune`], and
//! executes the returned [`OwnerCmd`]s over IPC.
#![allow(dead_code)]

use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::nano3s_tuner_core::{
    AutoTuner, CapLimits, Metrics, SearchMemory, TuneAction, TuneEnvelope, TunePhase, TuneProfile, TuneSetpoint,
};

/// How often the owner tuner is evaluated. The status block of `run_worker`
/// runs on every IPC status refresh (~15 s per the existing comments); the
/// adapter gates on wall clock so a faster refresh cannot shorten a settle.
pub(crate) const OWNER_TICK: Duration = Duration::from_secs(15);

/// Settle = 20 ticks x 15 s = 300 s, two SmartSpeed windows (~131.1 s each).
/// The tuner judges the MEAN of every tick since the last change (SmartSpeed
/// hashrate is 3-5 % noisy window to window), so a step is judged on
/// hashrate/fail-rate that reflect it.
pub(crate) const OWNER_SETTLE_CYCLES: u32 = 20;
/// One tick (15 s) between thermal back-off steps (spec: ~12-15 s).
pub(crate) const OWNER_SAFETY_SETTLE_CYCLES: u32 = 1;
/// 4 ticks (60 s) between power-cap back-off steps. At one tick, warming
/// chips (leakage) kept power over Efficient's 95 W cap while the clock was
/// cut 348 -> 258 MHz in 4 min, landing 11 W under the cap (2026-10-24).
pub(crate) const OWNER_POWER_SETTLE_CYCLES: u32 = 4;

/// Margin (W) the tuner's own power cap sits below `effective_safety_w`, so
/// the tuner backs the clock off before aadhi's 137 W / PD safety trip fires.
pub(crate) const OWNER_POWER_MARGIN_W: f32 = 3.0;

/// The four PLL domains sit 20 MHz apart (210/230/250/270 ... 420/440/460/480).
pub(crate) const DOMAIN_SPACING_MHZ: u32 = 20;
/// Per-domain ceiling enforced by `patch_board_tuning`'s validator: base 440
/// + 3*20 = 500.
pub(crate) const MAX_DOMAIN_MHZ: u32 = 500;
/// Voltage grid anchor: the DC/DC quantizes as 3600 +/- n*26 mV
/// (POWER_TARGET_STEP_MV's comment in nano3s.rs).
const VOLT_GRID_ANCHOR_MV: i32 = 3600;
const VOLT_STEP_MV: i32 = 26;
/// Hard backstop: a chip at or above this temperature gets an immediate
/// emergency clock drop regardless of profile. Same number as
/// AUTOTUNE_TEMP_CEILING_C in nano3s.rs (97 C), which previously only
/// guarded the hashrate autotune.
pub(crate) const BACKSTOP_TEMP_C: f64 = 97.0;
pub(crate) const BACKSTOP_STEP_MHZ: u32 = 30;
pub(crate) const BACKSTOP_FLOOR_MHZ: u32 = 100;

/// Nano 3S envelope. Sources:
/// - frequency 210..440 base, step 10: aadhi's `tune:` clamp (LOW anchor
///   base 210 in hashrate_cali.ini; 440 so base+60 = 500 MHz = per-domain cap).
/// - voltage 3392..3800, step 26, forbid 3600: LOW anchor 3392, POWER_TARGET_MAX_MV
///   3800, DC/DC step 26 mV, 3600 = core-domain migration collapse
///   (POWER_TARGET_AVOID_MV).
/// - temp caps (hottest chip, `temp_max`): hashrate_cali.ini limits LOW 80 /
///   MED 85 / HIGH 90 C; fan PID targets ~90 C and its emergency is 100 C;
///   the 97 C autotune ceiling is the absolute backstop. Quiet 70, Efficient
///   80 (LOW limit), Balanced 85 (MED limit), MaxHash 88 (under HIGH's 90 so
///   the tuner backs off before the fan PID is at its target).
/// - power caps are WHOLE-BOARD watts (INA226 input power): Quiet 60,
///   Efficient 90, Balanced 110 bracket the baselines (50.6 W LOW, 95.8 W
///   MED/338-398). MaxHash 137 = POWER_TARGET_SAFETY_W; every cap is further
///   clamped at runtime by `owner_power_ceiling(effective_safety_w)`.
/// - max_fail_rate_pct 10: the baselines show fail < ~10 % marks usable
///   points (14.5 % at 3496/LOW, 2.8-3.2 % at the good points).
pub(crate) const NANO3S_ENVELOPE: TuneEnvelope = TuneEnvelope {
    min_freq_mhz: 210.0,
    max_freq_mhz: 440.0,
    freq_step_mhz: 10.0,
    min_voltage_mv: 3392,
    max_voltage_mv: 3800,
    voltage_step_mv: 26,
    forbidden_voltages_mv: &[3600],
    settle_cycles: OWNER_SETTLE_CYCLES,
    safety_settle_cycles: OWNER_SAFETY_SETTLE_CYCLES,
    power_settle_cycles: OWNER_POWER_SETTLE_CYCLES,
    quiet: CapLimits { temp_c: 70.0, power_w: 60.0 },
    efficient: CapLimits { temp_c: 80.0, power_w: 90.0 },
    balanced: CapLimits { temp_c: 88.0, power_w: 90.0 },
    max_hash: CapLimits { temp_c: 88.0, power_w: 137.0 },
    target_temp_c: 88.0,
    target_hashrate_power_w: 137.0,
    target_power_headroom: 1.05,
    max_fail_rate_pct: Some(10.0),
    average_judgement: true,
    // 3 W below the cap: a point inside it counts as "at cap" and is never
    // recorded as known-good (the 308<->318 MHz oscillation of run 1).
    best_point_margin_w: 3.0,
    // Run 2: one 26 mV step moves J/TH < 1 %, below the acceptance margin.
    trim_steps_per_judgement: 3,
};

/// Where the device's factory calibration lives.
pub(crate) const CALI_PATH: &str = "/data/factory/hashrate_cali.ini";

/// One calibrated operating mode: `cali_param0 = P-T-MV-MHZ-SPACING`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CaliMode {
    pub power_w: f32,
    pub temp_c: f32,
    pub mv: u16,
    pub mhz: u32,
    pub spacing_mhz: u32,
}

/// Parses `hashrate_cali.ini` text (sections `[mode0]`..`[mode2]`, key
/// `cali_param0`). `None` if any mode is missing or malformed, or a value is
/// outside what this board can run (voltage must stay under the 3800 mV hard
/// limit, temperature under the 97 C backstop).
pub(crate) fn parse_cali(text: &str) -> Option<[CaliMode; 3]> {
    let mut modes: [Option<CaliMode>; 3] = [None; 3];
    let mut cur: Option<usize> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(sec) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            cur = sec
                .trim()
                .to_ascii_lowercase()
                .strip_prefix("mode")
                .and_then(|n| n.parse::<usize>().ok())
                .filter(|n| *n < 3);
            continue;
        }
        let (Some(i), Some((k, v))) = (cur, line.split_once('=')) else { continue };
        if k.trim() != "cali_param0" {
            continue;
        }
        let f: Vec<&str> = v.trim().split('-').map(str::trim).collect();
        if f.len() != 5 {
            return None;
        }
        let mode = CaliMode {
            power_w: f[0].parse().ok()?,
            temp_c: f[1].parse().ok()?,
            mv: f[2].parse().ok()?,
            mhz: f[3].parse().ok()?,
            spacing_mhz: f[4].parse().ok()?,
        };
        let ok = mode.power_w > 0.0
            && mode.power_w <= 300.0
            && mode.temp_c >= 30.0
            && mode.temp_c < BACKSTOP_TEMP_C as f32
            && (3000..=NANO3S_ENVELOPE.max_voltage_mv).contains(&mode.mv)
            && (100..=MAX_DOMAIN_MHZ).contains(&mode.mhz);
        if !ok {
            return None;
        }
        modes[i] = Some(mode);
    }
    let [Some(a), Some(b), Some(c)] = modes else { return None };
    // Calibration points must rise with power; anything else is a bad file.
    (a.power_w < b.power_w && b.power_w < c.power_w).then_some([a, b, c])
}

/// Builds the owner-tuner envelope from the device calibration: voltage floor
/// = mode0, Quiet = mode0 (P, T), Efficient = (mode1 P, mode0 T), Balanced =
/// (mode1 P, mode2 T), MaxHash = mode2 (P, T). mode1 is Canaan's Medium:
/// Balanced gets stock Medium's power budget and a cap high enough for its
/// fan target (cap - 5) to match stock Medium's ~85 C chips. Frequency
/// range, voltage ceiling (3800), step and forbidden voltage stay the
/// constants; the PSU ceiling is applied on top at runtime.
pub(crate) fn envelope_from_cali(c: &[CaliMode; 3]) -> TuneEnvelope {
    TuneEnvelope {
        min_voltage_mv: c[0].mv,
        quiet: CapLimits { temp_c: c[0].temp_c, power_w: c[0].power_w },
        efficient: CapLimits { temp_c: c[0].temp_c, power_w: c[1].power_w },
        balanced: CapLimits { temp_c: c[2].temp_c, power_w: c[1].power_w },
        max_hash: CapLimits { temp_c: c[2].temp_c, power_w: c[2].power_w },
        target_temp_c: c[2].temp_c,
        target_hashrate_power_w: c[2].power_w,
        ..NANO3S_ENVELOPE
    }
}

/// Envelope for this device: from `path` when it parses, else the constants.
/// The bool is true when the file was used.
pub(crate) fn load_envelope(path: &str) -> (TuneEnvelope, bool) {
    match std::fs::read_to_string(path).ok().as_deref().and_then(parse_cali) {
        Some(c) => (envelope_from_cali(&c), true),
        None => (NANO3S_ENVELOPE, false),
    }
}

// --- API types -------------------------------------------------------------

/// Body of `PATCH /api/v0/boards/{name}/owner-tune`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct OwnerTuneRequest {
    /// `quiet`, `efficient`, `balanced` or `max_hash`; `null` turns the owner
    /// tuner off. Absent (with `chip_temp_target_c` present) leaves the
    /// profile unchanged; `{}` still means off.
    #[serde(default, deserialize_with = "present_or_null")]
    #[schema(value_type = Option<TuneProfile>)]
    pub profile: Option<Option<TuneProfile>>,
    /// Hottest-chip temperature (60..88 C) the fan loop holds while the owner
    /// tuner is on. Default: the profile's temp cap minus 5 C. Persisted.
    #[serde(default)]
    pub chip_temp_target_c: Option<f32>,
    /// Per-chip clock tuner on/off while the owner tuner is on (it runs only
    /// once the owner tuner has locked). Absent leaves it unchanged. Persisted.
    #[serde(default)]
    pub per_chip: Option<bool>,
    /// Continuous dynamic tuning: continuously probes upward when thermal/cooling headroom opens up.
    #[serde(default)]
    pub continuous: Option<bool>,
}

fn present_or_null<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Option<TuneProfile>>, D::Error> {
    Option::<TuneProfile>::deserialize(d).map(Some)
}

/// Fan-target range accepted by the API.
pub(crate) const FAN_TARGET_RANGE_C: std::ops::RangeInclusive<f32> = 60.0..=88.0;

impl OwnerTuneRequest {
    /// `Err` (-> 400) when `chip_temp_target_c` is outside 60..88.
    pub(crate) fn validate(&self) -> Result<(), ()> {
        match self.chip_temp_target_c {
            Some(c) if !FAN_TARGET_RANGE_C.contains(&c) => Err(()),
            _ => Ok(()),
        }
    }

    /// The profile change to queue: `Some(None)` = off, `None` = keep.
    pub(crate) fn profile_change(&self) -> Option<Option<TuneProfile>> {
        match (
            self.profile,
            self.chip_temp_target_c.is_some() || self.per_chip.is_some() || self.continuous.is_some(),
        ) {
            (None, true) => None,
            (None, false) => Some(None),
            (p, _) => p,
        }
    }
}

/// Averaging window progress for the status block.
#[derive(Clone, Copy, Debug, Default, Serialize, ToSchema)]
pub struct AveragingProgress {
    pub ticks: u32,
    pub needed: u32,
}

/// Body of `GET /api/v0/boards/{name}/owner-tune`.
#[derive(Clone, Debug, Default, Serialize, ToSchema)]
pub struct OwnerTuneStatus {
    pub enabled: bool,
    pub profile: Option<TuneProfile>,
    pub phase: Option<TunePhase>,
    pub last_event: Option<String>,
    pub setpoint: Option<TuneSetpoint>,
    /// Duty the chip-temp fan loop last commanded (None until it has).
    pub fan_duty_percent: Option<u8>,
    /// Hottest-chip temperature the fan loop is holding.
    pub fan_target_c: Option<f32>,
    pub averaging_progress: Option<AveragingProgress>,
    /// Whole-board watts the tuner may draw continuously (PSU-derived).
    pub continuous_ceiling_w: Option<f32>,
    /// Continuous dynamic tuning enabled.
    pub continuous: bool,
    /// Last 20 tuner log entries, newest last.
    pub events: Vec<OwnerEvent>,
    /// Best measured point, if any.
    pub best: Option<OwnerBest>,
    /// Per-chip tuner state; `None` while it is off.
    pub per_chip: Option<ChipTuneStatus>,
}

#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct ChipTuneStatus {
    /// `baseline`, `trial` or `converged`; `waiting` until the owner tuner locks.
    pub phase: String,
    pub last_event: String,
    pub offsets_mhz: Vec<i16>,
}

#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct OwnerEvent {
    /// Approximate age (15 s per tuner tick).
    pub t_secs_ago: u64,
    pub message: String,
    pub freq_mhz: f32,
    pub voltage_mv: u16,
    pub power_w: f32,
    pub hashrate_ths: f32,
}

#[derive(Clone, Copy, Debug, Serialize, ToSchema)]
pub struct OwnerBest {
    pub freq_mhz: f32,
    pub voltage_mv: u16,
    pub j_th: f32,
}

static OWNER_TUNE_STATUS: Mutex<Option<OwnerTuneStatus>> = Mutex::new(None);

pub(crate) fn publish_status(s: OwnerTuneStatus) {
    *OWNER_TUNE_STATUS.lock().unwrap_or_else(|e| e.into_inner()) = Some(s);
}

pub(crate) fn read_status() -> OwnerTuneStatus {
    OWNER_TUNE_STATUS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_default()
}

/// Control-file body for a profile (`None` = off).
pub(crate) fn directive_for(profile: Option<TuneProfile>) -> String {
    let name = match profile {
        None => "off",
        Some(TuneProfile::Quiet) => "quiet",
        Some(TuneProfile::Efficient) => "efficient",
        Some(TuneProfile::Balanced) => "balanced",
        Some(TuneProfile::MaxHash) => "max_hash",
    };
    format!("owner_tune:{name}")
}

/// Parses the part after `owner_tune:`.
pub(crate) fn parse_directive(v: &str) -> Result<Option<TuneProfile>, ()> {
    match v.trim() {
        "off" => Ok(None),
        "quiet" => Ok(Some(TuneProfile::Quiet)),
        "efficient" => Ok(Some(TuneProfile::Efficient)),
        "balanced" => Ok(Some(TuneProfile::Balanced)),
        "max_hash" => Ok(Some(TuneProfile::MaxHash)),
        _ => Err(()),
    }
}

/// Parsed `owner_tune:` control line: `<profile|off|keep>[;fan=<C>]`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct OwnerDirective {
    /// `Some(None)` = off, `None` = keep (`keep`).
    pub profile: Option<Option<TuneProfile>>,
    pub fan_target_c: Option<f32>,
    /// `chip=on|off`: per-chip tuner switch; `None` = unchanged.
    pub chip_tune: Option<bool>,
    /// `continuous=on|off`: continuous dynamic tuning switch; `None` = unchanged.
    pub continuous: Option<bool>,
}

pub(crate) fn directive_for_request(profile: Option<Option<TuneProfile>>, fan_target_c: Option<f32>) -> String {
    let head = match profile {
        None => "owner_tune:keep".to_string(),
        Some(p) => directive_for(p),
    };
    match fan_target_c {
        Some(c) => format!("{head};fan={c}"),
        None => head,
    }
}

pub(crate) fn parse_owner_directive(v: &str) -> Result<OwnerDirective, ()> {
    let mut parts = v.split(';');
    let head = parts.next().unwrap_or("").trim();
    let profile = if head == "keep" { None } else { Some(parse_directive(head)?) };
    let mut fan_target_c = None;
    let mut chip_tune = None;
    let mut continuous = None;
    for extra in parts {
        let extra = extra.trim();
        if let Some(v) = extra.strip_prefix("chip=") {
            chip_tune = Some(match v.trim() {
                "on" => true,
                "off" => false,
                _ => return Err(()),
            });
            continue;
        }
        if let Some(v) = extra.strip_prefix("continuous=") {
            continuous = Some(match v.trim() {
                "on" => true,
                "off" => false,
                _ => return Err(()),
            });
            continue;
        }
        let c: f32 = extra.strip_prefix("fan=").ok_or(())?.trim().parse().map_err(|_| ())?;
        if !FAN_TARGET_RANGE_C.contains(&c) {
            return Err(());
        }
        fan_target_c = Some(c);
    }
    Ok(OwnerDirective { profile, fan_target_c, chip_tune, continuous })
}

// --- chip-temp fan loop ------------------------------------------------------

pub(crate) const FAN_DUTY_MIN: f32 = 20.0;
/// Duty change (%) per degree C of error, per 15 s tick.
pub(crate) const FAN_KP: f32 = 2.0;
/// Only write the harness when the duty moved by at least this much (or hits 100).
pub(crate) const FAN_MIN_WRITE_DELTA: u8 = 3;
/// A forced-full episode ends only once the die is this far under the cap.
pub(crate) const FAN_RELEASE_C: f32 = 5.0;
/// Default target = profile temp cap minus this.
pub(crate) const FAN_TARGET_BELOW_CAP_C: f32 = 5.0;
const TEMP_PLAUSIBLE_MAX_C: f32 = 150.0;

/// Default chip-temp fan target: Efficient 68 C, Quiet 65 C (cooler is the
/// point of those profiles), Balanced/MaxHash cap - 5 C.
pub(crate) fn default_fan_target_c(profile: Option<TuneProfile>, cap: f32) -> f32 {
    match profile {
        Some(TuneProfile::Efficient) => 68.0,
        Some(TuneProfile::Quiet) => 65.0,
        _ => cap - FAN_TARGET_BELOW_CAP_C,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct FanState {
    duty: f32,
    /// Last duty written to the harness; `None` = nothing sent (fan not ours).
    pub sent: Option<u8>,
    /// Forced to 100 % until the die is `FAN_RELEASE_C` under the cap.
    hold_full: bool,
}

impl Default for FanState {
    fn default() -> Self {
        // Start from 100 %: a fan we have just taken over from auto ramps down.
        Self { duty: 100.0, sent: None, hold_full: false }
    }
}

impl FanState {
    /// Tuner `RestoreFanAuto`: back to PI control, but only once the die is
    /// really under the release band (this loop owns that decision).
    pub(crate) fn restore(&mut self, temp: Option<f32>, cap: f32) {
        if let Some(t) = plausible(temp)
            && t < cap - FAN_RELEASE_C
        {
            self.hold_full = false;
        }
    }

    pub(crate) fn duty_percent(&self) -> u8 {
        self.duty.round() as u8
    }

    /// Fan state for a takeover from `current` duty (the fan's measured
    /// percent), so the loop doesn't start at 100 % and chill the chips.
    /// Unknown duty keeps the safe 100 % start.
    fn starting_at(current: Option<u8>) -> Self {
        let duty = current.map_or(100.0, |d| (d as f32).clamp(FAN_DUTY_MIN, 100.0));
        Self { duty, ..Self::default() }
    }
}

fn plausible(temp: Option<f32>) -> Option<f32> {
    temp.filter(|t| t.is_finite() && *t > 0.0 && *t <= TEMP_PLAUSIBLE_MAX_C)
}

/// One fan-loop step (one per 15 s tick). Returns the `fan:manual` duty to
/// write, or `None` to leave the harness alone.
///
/// Safety rules, in priority order:
/// - temp missing or implausible (<= 0, > 150, NaN): 100 %.
/// - `force_full` (tuner SetFanFull) or temp >= `cap`: 100 % and held there
///   until temp < `cap` - 5 C.
/// - otherwise PI: duty += `FAN_KP` * (temp - target), clamped 20..100.
pub(crate) fn fan_step(st: &mut FanState, temp: Option<f32>, cap: f32, target: f32, force_full: bool) -> Option<u8> {
    let t = plausible(temp);
    if force_full {
        st.hold_full = true;
    }
    match t {
        None => st.duty = 100.0,
        Some(t) => {
            if t >= cap {
                st.hold_full = true;
            } else if st.hold_full && !force_full && t < cap - FAN_RELEASE_C {
                st.hold_full = false;
            }
            if st.hold_full {
                st.duty = 100.0;
            } else {
                st.duty = (st.duty + FAN_KP * (t - target)).clamp(FAN_DUTY_MIN, 100.0);
            }
        }
    }
    let d = st.duty_percent();
    let send = match st.sent {
        None => true,
        Some(s) => (d == 100 && s != 100) || d.abs_diff(s) >= FAN_MIN_WRITE_DELTA,
    };
    send.then(|| {
        st.sent = Some(d);
        d
    })
}

// --- pure helpers ----------------------------------------------------------

/// Snaps a requested core voltage to the DC/DC's 26 mV grid (anchored at
/// 3600), never returns 3600 (steps one grid step down), then clamps to the
/// highest/lowest on-grid values inside the envelope (3782 / 3392 mV).
pub(crate) fn snap_voltage_mv(mv: i64) -> u16 {
    let rel = mv - VOLT_GRID_ANCHOR_MV as i64;
    let step = VOLT_STEP_MV as i64;
    let n = if rel >= 0 { (rel + step / 2) / step } else { -((-rel + step / 2) / step) };
    let mut snapped = VOLT_GRID_ANCHOR_MV as i64 + n * step;
    if snapped == VOLT_GRID_ANCHOR_MV as i64 {
        snapped -= step;
    }
    // Clamp to the highest/lowest values ON the grid inside the envelope:
    // 3800 itself isn't a value the DC/DC can represent (3600 + k*26), so
    // commanding it would rely on the shim rounding.
    let lo = VOLT_GRID_ANCHOR_MV as i64
        - (VOLT_GRID_ANCHOR_MV as i64 - NANO3S_ENVELOPE.min_voltage_mv as i64) / step * step;
    let hi = VOLT_GRID_ANCHOR_MV as i64
        + (NANO3S_ENVELOPE.max_voltage_mv as i64 - VOLT_GRID_ANCHOR_MV as i64) / step * step;
    snapped.clamp(lo, hi) as u16
}

/// Clamps a requested base clock to the envelope and spreads it over the four
/// PLL domains 20 MHz apart.
pub(crate) fn domain_freqs(base_mhz: f32) -> [u32; 4] {
    let b = base_mhz.round().clamp(NANO3S_ENVELOPE.min_freq_mhz, NANO3S_ENVELOPE.max_freq_mhz) as u32;
    let mut out = [0u32; 4];
    for (i, f) in out.iter_mut().enumerate() {
        *f = (b + DOMAIN_SPACING_MHZ * i as u32).min(MAX_DOMAIN_MHZ);
    }
    out
}

/// PSU-derived power cap handed to the tuner each tick.
///
/// Continuous ceiling = min(80 % of the detected PD contract, safety trip -
/// 3 W). On the 140 W EPR supply that is 112 W. An undetected contract is
/// `INFINITY`, which leaves the safety-trip bound.
pub(crate) fn owner_power_ceiling(pd_contract_w: f64, effective_safety_w: f64) -> f32 {
    let by_pd = (pd_contract_w * PD_CONTINUOUS_FRACTION) as f32;
    by_pd.min((effective_safety_w as f32) - OWNER_POWER_MARGIN_W).max(0.0)
}

/// Fraction of the PD contract the tuner may draw continuously.
pub(crate) const PD_CONTINUOUS_FRACTION: f64 = 0.8;

/// Emergency clock drop at/above the 97 C backstop.
pub(crate) fn backstop_freqs(cur: [u32; 4]) -> [u32; 4] {
    let mut out = cur;
    for f in out.iter_mut() {
        *f = f.saturating_sub(BACKSTOP_STEP_MHZ).max(BACKSTOP_FLOOR_MHZ);
    }
    out
}

/// Builds tuner [`Metrics`] from the worker's readings. `None` when the chain
/// reports no usable temperature or power (not hashing yet), so the tuner is
/// not fed zeros. `ghsspd_gh` is GH/s; `fail_rate_pct` is `spd_dh` passed
/// through.
pub(crate) fn metrics_from_readings(
    temp_max_c: f32,
    power_w: f64,
    ghsspd_gh: u32,
    freq0_mhz: u32,
    voltage_mv: u32,
    fail_rate_pct: f32,
) -> Option<Metrics> {
    if !(temp_max_c > 0.0) || !(power_w > 0.0) {
        return None;
    }
    Some(Metrics {
        asic_temp_c: temp_max_c,
        power_w: power_w as f32,
        hashrate_ths: ghsspd_gh as f32 / 1000.0,
        frequency_mhz: freq0_mhz as f32,
        core_voltage_mv: voltage_mv.min(u16::MAX as u32) as u16,
        // Harness has no min-fan or auto/manual readback: None keeps the
        // tuner on the "force 100 % then restore auto" fan path.
        fan_min_percent: None,
        fan_auto: None,
        fail_rate_pct: Some(fail_rate_pct).filter(|f| f.is_finite() && *f >= 0.0),
    })
}

/// A hardware command the worker must perform for a tuner action.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum OwnerCmd {
    /// `nano3s_ipc_set_mode(freqs)`.
    SetMode([u32; 4]),
    /// `nano3s_ipc_set_voltage_raw(mv)`.
    SetVoltage(i32),
    /// Text for `/tmp/harness_control`.
    Fan(&'static str),
    /// `fan:manual,<duty>` from the chip-temp fan loop.
    FanManual(u8),
}

/// Maps a tuner action to hardware commands. Fan actions map to nothing here:
/// `OwnerTune::evaluate` hands them to the fan loop.
pub(crate) fn action_to_cmds(action: TuneAction) -> Vec<OwnerCmd> {
    match action {
        TuneAction::SetFrequency(base) => vec![OwnerCmd::SetMode(domain_freqs(base))],
        TuneAction::SetVoltage(mv) => vec![OwnerCmd::SetVoltage(snap_voltage_mv(mv as i64) as i32)],
        // Fan actions are routed through the fan loop (`OwnerTune`), which
        // owns the fan while the owner tuner is on.
        TuneAction::SetFanFull | TuneAction::RestoreFanAuto | TuneAction::SetFanMinPercent(_) => vec![],
    }
}

/// Exclusivity: enabling the owner tuner clears aadhi's three loops.
/// Returns true if any was actually set.
pub(crate) fn clear_other_loops(
    power_target_w: &mut Option<f64>,
    temp_target_c: &mut Option<f64>,
    autotune_hashrate_ths: &mut Option<f64>,
) -> bool {
    let any = power_target_w.is_some() || temp_target_c.is_some() || autotune_hashrate_ths.is_some();
    *power_target_w = None;
    *temp_target_c = None;
    *autotune_hashrate_ths = None;
    any
}

/// At startup, the owner tuner wins over any persisted/env loop.
pub(crate) fn startup_loop_allowed(owner_profile: Option<TuneProfile>, autotune_hashrate: Option<f64>) -> bool {
    owner_profile.is_none() && autotune_hashrate.is_none()
}

// --- stateful wrapper ------------------------------------------------------

pub(crate) struct OwnerTune {
    tuner: AutoTuner,
    profile: Option<TuneProfile>,
    next_tick: Option<std::time::Instant>,
    continuous_ceiling_w: Option<f32>,
    fan: FanState,
    fan_target_override: Option<f32>,
    fan_force_pending: bool,
    fan_restore_pending: bool,
    chip_status: Option<ChipTuneStatus>,
}

impl OwnerTune {
    pub(crate) fn new() -> Self {
        Self::with_envelope(NANO3S_ENVELOPE)
    }

    pub(crate) fn with_envelope(envelope: TuneEnvelope) -> Self {
        let mut tuner = AutoTuner::default();
        tuner.set_envelope(envelope);
        Self {
            tuner,
            profile: None,
            next_tick: None,
            continuous_ceiling_w: None,
            fan: FanState::default(),
            fan_target_override: None,
            fan_force_pending: false,
            fan_restore_pending: false,
            chip_status: None,
        }
    }

    /// True once the profile search has locked onto a point.
    pub(crate) fn is_locked(&self) -> bool {
        self.profile.is_some() && self.tuner.phase() == TunePhase::Locked
    }

    /// Power cap (W) of the running profile, PSU ceiling included.
    pub(crate) fn power_cap_w(&self) -> f32 {
        self.tuner.power_cap_w()
    }

    /// Set continuous dynamic tuning mode.
    pub(crate) fn set_continuous(&mut self, c: bool) {
        self.tuner.set_continuous(c);
    }

    /// Whether continuous dynamic tuning mode is enabled.
    pub(crate) fn continuous(&self) -> bool {
        self.tuner.continuous()
    }

    /// Shown in the published status as `per_chip`.
    pub(crate) fn set_chip_status(&mut self, s: Option<ChipTuneStatus>) {
        self.chip_status = s;
    }

    /// `None` restores the default (profile temp cap - 5 C).
    pub(crate) fn set_fan_target(&mut self, c: Option<f32>) {
        self.fan_target_override = c.filter(|c| FAN_TARGET_RANGE_C.contains(c));
    }

    /// Effective fan target: the override (never within 2 C of the cap) or
    /// cap - 5 C.
    pub(crate) fn fan_target_c(&self) -> f32 {
        let cap = self.tuner.temp_cap_c();
        match self.fan_target_override {
            Some(c) => c.min(cap - 2.0),
            None => default_fan_target_c(self.profile, cap).min(cap - 2.0),
        }
    }

    /// One fan-loop step; at most one harness write per tick. `temp_max_c`
    /// is the raw hottest-chip reading (`None`/<=0 = missing -> 100 %).
    pub(crate) fn fan_tick(&mut self, temp_max_c: Option<f32>) -> Option<OwnerCmd> {
        if self.profile.is_none() {
            return None;
        }
        let cap = self.tuner.temp_cap_c();
        if std::mem::take(&mut self.fan_restore_pending) {
            self.fan.restore(temp_max_c, cap);
        }
        let force = std::mem::take(&mut self.fan_force_pending);
        let target = self.fan_target_c();
        fan_step(&mut self.fan, temp_max_c, cap, target, force).map(OwnerCmd::FanManual)
    }

    /// The running profile's search memory (see [`SearchMemory`]).
    pub(crate) fn memory(&self) -> Option<SearchMemory> {
        self.profile?;
        self.tuner.memory()
    }

    /// Restores a saved search for the running profile. Call right after
    /// [`Self::enable`] on a boot-time resume.
    pub(crate) fn restore_memory(&mut self, mem: SearchMemory) {
        if self.profile.is_some() {
            self.tuner.restore_memory(mem);
        }
    }

    /// Starts the fan loop from the fan's current duty instead of 100 %.
    /// Call right after [`Self::enable`].
    pub(crate) fn seed_fan_duty(&mut self, current: Option<u8>) {
        if self.profile.is_some() && self.fan.sent.is_none() {
            self.fan = FanState::starting_at(current);
        }
    }

    pub(crate) fn profile(&self) -> Option<TuneProfile> {
        self.profile
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.profile.is_some()
    }

    /// A profile picked from the API/UI: a fresh search (no remembered best)
    /// and the profile's default fan target unless this request set one.
    pub(crate) fn pick_profile(&mut self, profile: TuneProfile, fan_target_c: Option<f32>) {
        self.enable(profile);
        self.set_fan_target(fan_target_c);
    }

    pub(crate) fn enable(&mut self, profile: TuneProfile) {
        self.tuner.enable_profile(profile);
        self.profile = Some(profile);
        self.next_tick = None;
        self.fan = FanState::default();
        self.fan_force_pending = false;
        self.fan_restore_pending = false;
    }

    /// Disables the tuner. Returns the harness command that hands the fan
    /// back to auto if this tuner ever took it over.
    pub(crate) fn disable(&mut self) -> Option<OwnerCmd> {
        let restore = (self.fan.sent.is_some() || self.tuner.fan_forced()).then_some(OwnerCmd::Fan("fan:auto,"));
        self.fan = FanState::default();
        self.fan_force_pending = false;
        self.fan_restore_pending = false;
        self.tuner.disable();
        self.profile = None;
        self.next_tick = None;
        restore
    }

    /// True when a tick is due (and arms the next one).
    pub(crate) fn tick_due(&mut self, now: std::time::Instant) -> bool {
        if self.profile.is_none() {
            return false;
        }
        match self.next_tick {
            Some(t) if now < t => false,
            _ => {
                self.next_tick = Some(now + OWNER_TICK);
                true
            }
        }
    }

    pub(crate) fn note_external_voltage(&mut self, mv: u16) {
        self.tuner.note_external_voltage(mv);
    }

    pub(crate) fn set_power_ceiling(&mut self, w: f32) {
        self.continuous_ceiling_w = Some(w);
        self.tuner.set_power_ceiling(Some(w));
    }

    pub(crate) fn evaluate(&mut self, m: &Metrics) -> Vec<OwnerCmd> {
        let action = self.tuner.evaluate(m);
        match action {
            Some(TuneAction::SetFanFull) => self.fan_force_pending = true,
            Some(TuneAction::RestoreFanAuto) => self.fan_restore_pending = true,
            _ => {}
        }
        action.map(action_to_cmds).unwrap_or_default()
    }

    fn events(&self) -> Vec<OwnerEvent> {
        let now = self.tuner.cycle();
        let all: Vec<_> = self.tuner.events().collect();
        all[all.len().saturating_sub(20)..]
            .iter()
            .map(|e| OwnerEvent {
                t_secs_ago: now.saturating_sub(e.cycle) * OWNER_TICK.as_secs(),
                message: e.message.clone(),
                freq_mhz: e.frequency_mhz,
                voltage_mv: e.core_voltage_mv,
                power_w: e.power_w,
                hashrate_ths: e.hashrate_ths,
            })
            .collect()
    }

    pub(crate) fn publish(&self, setpoint: Option<TuneSetpoint>) {
        publish_status(OwnerTuneStatus {
            enabled: self.profile.is_some(),
            profile: self.profile,
            phase: Some(self.tuner.phase()),
            last_event: self.tuner.last_event().map(str::to_owned),
            setpoint,
            fan_duty_percent: self.profile.and(self.fan.sent),
            fan_target_c: self.profile.map(|_| self.fan_target_c()),
            averaging_progress: self.profile.map(|_| {
                let (ticks, needed) = self.tuner.averaging_progress();
                AveragingProgress { ticks, needed }
            }),
            continuous_ceiling_w: self.profile.and(self.continuous_ceiling_w),
            continuous: self.tuner.continuous(),
            events: self.events(),
            best: self.tuner.best_point().map(|(sp, j)| OwnerBest {
                freq_mhz: sp.frequency_mhz,
                voltage_mv: sp.core_voltage_mv,
                j_th: j,
            }),
            per_chip: self.chip_status.clone(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voltage_snaps_to_the_grid_and_skips_3600() {
        assert_eq!(snap_voltage_mv(3392), 3392);
        assert_eq!(snap_voltage_mv(3496), 3496);
        assert_eq!(snap_voltage_mv(3704), 3704);
        assert_eq!(snap_voltage_mv(3600), 3574);
        assert_eq!(snap_voltage_mv(3605), 3574); // rounds to 3600, then steps down
        assert_eq!(snap_voltage_mv(3614), 3626);
        assert_eq!(snap_voltage_mv(3500), 3496);
    }

    #[test]
    fn voltage_clamps_to_the_envelope() {
        assert_eq!(snap_voltage_mv(1000), 3392);
        assert_eq!(snap_voltage_mv(3380), 3392);
        assert_eq!(snap_voltage_mv(3790), 3782);
        assert_eq!(snap_voltage_mv(3805), 3782); // 3808 snapped, clamped to the top grid value
        assert_eq!(snap_voltage_mv(9000), 3782);
    }

    #[test]
    fn domain_freqs_are_clamped_and_spaced() {
        assert_eq!(domain_freqs(210.0), [210, 230, 250, 270]);
        assert_eq!(domain_freqs(338.0), [338, 358, 378, 398]);
        assert_eq!(domain_freqs(440.0), [440, 460, 480, 500]);
        assert_eq!(domain_freqs(100.0), [210, 230, 250, 270]);
        assert_eq!(domain_freqs(900.0), [440, 460, 480, 500]);
    }

    #[test]
    fn metrics_map_units_and_reject_idle_chain() {
        let m = metrics_from_readings(68.5, 95.8, 4880, 338, 3704, 3.2).unwrap();
        assert_eq!(m.asic_temp_c, 68.5);
        assert_eq!(m.power_w, 95.8_f64 as f32);
        assert!((m.hashrate_ths - 4.88).abs() < 1e-6);
        assert_eq!(m.frequency_mhz, 338.0);
        assert_eq!(m.core_voltage_mv, 3704);
        assert_eq!(m.fail_rate_pct, Some(3.2));
        assert_eq!(m.fan_auto, None);
        assert!(metrics_from_readings(0.0, 95.0, 4000, 338, 3704, 3.0).is_none());
        assert!(metrics_from_readings(60.0, 0.0, 4000, 338, 3704, 3.0).is_none());
        assert_eq!(
            metrics_from_readings(60.0, 50.0, 4000, 338, 3704, f32::NAN).unwrap().fail_rate_pct,
            None
        );
    }

    #[test]
    fn actions_map_to_commands() {
        assert_eq!(
            action_to_cmds(TuneAction::SetFrequency(300.0)),
            vec![OwnerCmd::SetMode([300, 320, 340, 360])]
        );
        assert_eq!(action_to_cmds(TuneAction::SetVoltage(3600)), vec![OwnerCmd::SetVoltage(3574)]);
        assert!(action_to_cmds(TuneAction::SetFanFull).is_empty());
        assert!(action_to_cmds(TuneAction::RestoreFanAuto).is_empty());
        assert!(action_to_cmds(TuneAction::SetFanMinPercent(40)).is_empty());
    }

    #[test]
    fn enabling_clears_the_other_loops() {
        let (mut p, mut t, mut a) = (Some(90.0), Some(80.0), Some(4.0));
        assert!(clear_other_loops(&mut p, &mut t, &mut a));
        assert_eq!((p, t, a), (None, None, None));
        assert!(!clear_other_loops(&mut p, &mut t, &mut a));
    }

    #[test]
    fn startup_exclusivity_prefers_owner_and_autotune() {
        assert!(startup_loop_allowed(None, None));
        assert!(!startup_loop_allowed(Some(TuneProfile::Efficient), None));
        assert!(!startup_loop_allowed(None, Some(4.0)));
    }

    #[test]
    fn directive_round_trips() {
        for p in [
            None,
            Some(TuneProfile::Quiet),
            Some(TuneProfile::Efficient),
            Some(TuneProfile::Balanced),
            Some(TuneProfile::MaxHash),
        ] {
            let d = directive_for(p);
            assert_eq!(parse_directive(d.strip_prefix("owner_tune:").unwrap()), Ok(p));
        }
        assert!(parse_directive("turbo").is_err());
    }

    #[test]
    fn request_accepts_null_and_names() {
        let r: OwnerTuneRequest = serde_json::from_str(r#"{"profile":null}"#).unwrap();
        assert_eq!(r.profile, Some(None));
        assert_eq!(r.profile_change(), Some(None));
        let r: OwnerTuneRequest = serde_json::from_str(r#"{"profile":"max_hash"}"#).unwrap();
        assert_eq!(r.profile, Some(Some(TuneProfile::MaxHash)));
        // `{}` is still "off"; a bare fan target leaves the profile alone.
        let r: OwnerTuneRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(r.profile_change(), Some(None));
        let r: OwnerTuneRequest = serde_json::from_str(r#"{"chip_temp_target_c":75}"#).unwrap();
        assert_eq!(r.profile_change(), None);
        assert!(r.validate().is_ok());
        for bad in ["59.9", "88.1", "0", "200"] {
            let r: OwnerTuneRequest =
                serde_json::from_str(&format!(r#"{{"chip_temp_target_c":{bad}}}"#)).unwrap();
            assert!(r.validate().is_err(), "{bad}");
        }
        assert!(serde_json::from_str::<OwnerTuneRequest>(r#"{"profile":"nope"}"#).is_err());
    }

    #[test]
    fn power_ceiling_and_backstop() {
        // 140 W EPR supply: 80 % = 112 W beats safety trip 137 - 3.
        assert_eq!(owner_power_ceiling(140.0, 137.0), 112.0);
        // 100 W SPR contract: 80 W, safety trip (100 - margin) is higher.
        assert_eq!(owner_power_ceiling(100.0, 97.0), 80.0);
        // Override/contract looser than the trip: the trip - 3 W wins.
        assert_eq!(owner_power_ceiling(200.0, 137.0), 134.0);
        // No PD reading (INFINITY): trip - 3 W.
        assert_eq!(owner_power_ceiling(f64::INFINITY, 137.0), 134.0);
        assert_eq!(owner_power_ceiling(1.0, 1.0), 0.0);
        assert_eq!(backstop_freqs([338, 358, 378, 398]), [308, 328, 348, 368]);
        assert_eq!(backstop_freqs([110, 120, 130, 140]), [100, 100, 100, 110]);
    }

    #[test]
    fn voltage_clamps_stay_on_the_grid() {
        assert_eq!(snap_voltage_mv(3800), 3782);
        assert_eq!(snap_voltage_mv(9999), 3782);
        assert_eq!(snap_voltage_mv(3392), 3392);
        assert_eq!(snap_voltage_mv(1000), 3392);
        assert_eq!(snap_voltage_mv(3600), 3574);
        assert_eq!(snap_voltage_mv(3704), 3704);
    }

    #[test]
    fn envelope_is_nano_shaped() {
        let e = NANO3S_ENVELOPE;
        assert_eq!(e.settle_cycles as u64 * OWNER_TICK.as_secs(), 300);
        assert_eq!(e.power_settle_cycles as u64 * OWNER_TICK.as_secs(), 60);
        assert!(e.average_judgement);
        assert_eq!(e.best_point_margin_w, 3.0);
        assert_eq!(e.voltage_step_mv, 26);
        assert!(e.forbidden_voltages_mv.contains(&3600));
        assert_eq!(e.max_fail_rate_pct, Some(10.0));
    }

    #[test]
    fn wrapper_ticks_on_wall_clock_and_power_ceiling_wins() {
        let mut o = OwnerTune::new();
        let t0 = std::time::Instant::now();
        assert!(!o.tick_due(t0)); // disabled
        o.enable(TuneProfile::MaxHash);
        assert!(o.tick_due(t0));
        assert!(!o.tick_due(t0 + Duration::from_secs(5)));
        assert!(o.tick_due(t0 + OWNER_TICK));
        // A PSU ceiling of 80 W makes 90 W a breach: the clock must come down.
        o.set_power_ceiling(80.0);
        let m = Metrics {
            asic_temp_c: 60.0,
            power_w: 90.0,
            hashrate_ths: 4.0,
            frequency_mhz: 338.0,
            core_voltage_mv: 3704,
            fan_min_percent: None,
            fan_auto: None,
            fail_rate_pct: Some(2.0),
        };
        let mut cmds = vec![];
        for _ in 0..OWNER_SETTLE_CYCLES {
            cmds = o.evaluate(&m);
            if !cmds.is_empty() {
                break;
            }
        }
        assert!(matches!(cmds.first(), Some(OwnerCmd::SetMode(f)) if f[0] < 338));
        assert_eq!(o.disable(), None);
        assert!(!o.is_enabled());
    }

    const SAMPLE_CALI: &str = "[mode0]\ncali_param0 = 62-80-3392-210-20\ncali_param1 = x\n\n[mode1]\ncali_param0 = 95-85-3496-338-20\n[mode2]\n# comment\ncali_param0 = 133-90-3704-420-20\n";

    #[test]
    fn cali_parses_the_sample_file() {
        let c = parse_cali(SAMPLE_CALI).unwrap();
        assert_eq!(c[0], CaliMode { power_w: 62.0, temp_c: 80.0, mv: 3392, mhz: 210, spacing_mhz: 20 });
        assert_eq!(c[1].mv, 3496);
        assert_eq!(c[2].mhz, 420);
    }

    #[test]
    fn cali_rejects_malformed_or_missing() {
        assert_eq!(parse_cali(""), None);
        assert_eq!(parse_cali("[mode0]\ncali_param0 = 62-80-3392-210-20\n"), None); // modes missing
        assert_eq!(parse_cali(&SAMPLE_CALI.replace("62-80-3392-210-20", "62-80-3392-210")), None); // 4 fields
        assert_eq!(parse_cali(&SAMPLE_CALI.replace("62-80-3392-210-20", "x-80-3392-210-20")), None);
        assert_eq!(parse_cali(&SAMPLE_CALI.replace("133-90-3704-420-20", "133-90-3900-420-20")), None); // > 3800 mV
        assert_eq!(parse_cali(&SAMPLE_CALI.replace("133-90-3704-420-20", "133-98-3704-420-20")), None); // >= backstop
        assert_eq!(parse_cali(&SAMPLE_CALI.replace("133-90", "50-90")), None); // power not rising
        assert_eq!(load_envelope("/nonexistent/hashrate_cali.ini"), (NANO3S_ENVELOPE, false));
    }

    #[test]
    fn envelope_from_the_sample_cali() {
        let e = envelope_from_cali(&parse_cali(SAMPLE_CALI).unwrap());
        assert_eq!(e.min_voltage_mv, 3392);
        assert_eq!((e.quiet.temp_c, e.quiet.power_w), (80.0, 62.0));
        assert_eq!((e.efficient.temp_c, e.efficient.power_w), (80.0, 95.0));
        assert_eq!((e.balanced.temp_c, e.balanced.power_w), (90.0, 95.0));
        assert_eq!((e.max_hash.temp_c, e.max_hash.power_w), (90.0, 133.0));
        // Hard limits and tuner behavior come from the constants.
        assert_eq!(e.max_voltage_mv, 3800);
        assert_eq!(e.forbidden_voltages_mv, &[3600]);
        assert!(e.average_judgement);
        assert_eq!(e.best_point_margin_w, 3.0);
        assert!(e.max_hash.temp_c < BACKSTOP_TEMP_C as f32);
    }

    // --- fan loop ------------------------------------------------------------

    fn st_at(duty: f32) -> FanState {
        FanState { duty, sent: Some(duty.round() as u8), hold_full: false }
    }

    #[test]
    fn fan_step_response_and_write_threshold() {
        // 3 C over target, Kp 2: +6 %.
        let mut st = st_at(50.0);
        assert_eq!(fan_step(&mut st, Some(78.0), 85.0, 75.0, false), Some(56));
        // 1 C under target: -2 %, below the 3 % write threshold -> no write,
        // but the internal duty still moved.
        assert_eq!(fan_step(&mut st, Some(74.0), 85.0, 75.0, false), None);
        assert_eq!(st.duty_percent(), 54);
        // Another -2 % accumulates past the threshold -> write.
        assert_eq!(fan_step(&mut st, Some(74.0), 85.0, 75.0, false), Some(52));
    }

    #[test]
    fn fan_step_clamps_to_20_and_100() {
        let mut st = st_at(30.0);
        assert_eq!(fan_step(&mut st, Some(60.0), 85.0, 75.0, false), Some(20)); // -30 -> clamp
        assert_eq!(fan_step(&mut st, Some(60.0), 85.0, 75.0, false), None);
        assert_eq!(st.duty_percent(), 20);
        let mut st = st_at(95.0);
        assert_eq!(fan_step(&mut st, Some(82.0), 85.0, 75.0, false), Some(100));
        assert_eq!(fan_step(&mut st, Some(82.0), 85.0, 75.0, false), None); // already 100
    }

    #[test]
    fn owner_directive_parses_the_chip_switch() {
        let d = parse_owner_directive("keep;chip=on").unwrap();
        assert_eq!((d.profile, d.chip_tune, d.fan_target_c), (None, Some(true), None));
        let d = parse_owner_directive("balanced;fan=80;chip=off").unwrap();
        assert_eq!(d.chip_tune, Some(false));
        assert_eq!(d.fan_target_c, Some(80.0));
        assert!(parse_owner_directive("keep;chip=maybe").is_err());
        // A per-chip switch alone keeps the profile.
        let r = OwnerTuneRequest { per_chip: Some(true), ..Default::default() };
        assert_eq!(r.profile_change(), None);
    }

    #[test]
    fn owner_directive_parses_the_continuous_switch() {
        let d = parse_owner_directive("keep;continuous=on").unwrap();
        assert_eq!((d.profile, d.continuous, d.fan_target_c), (None, Some(true), None));
        let d = parse_owner_directive("balanced;continuous=off").unwrap();
        assert_eq!(d.continuous, Some(false));
        assert!(parse_owner_directive("keep;continuous=maybe").is_err());
        // A continuous switch alone keeps the profile.
        let r = OwnerTuneRequest { continuous: Some(true), ..Default::default() };
        assert_eq!(r.profile_change(), None);
    }

    #[test]
    fn fan_takeover_starts_from_current_duty() {
        let mut st = FanState::starting_at(Some(30));
        // At target: first write is the current duty, not 100 %.
        assert_eq!(fan_step(&mut st, Some(75.0), 85.0, 75.0, false), Some(30));
        assert_eq!(FanState::starting_at(Some(5)).duty_percent(), 20);
        assert_eq!(FanState::starting_at(None).duty_percent(), 100);
    }

    #[test]
    fn fan_step_first_write_and_small_moves() {
        let mut st = FanState::default();
        // Nothing sent yet: the first step always writes.
        assert_eq!(fan_step(&mut st, Some(75.0), 85.0, 75.0, false), Some(100));
        assert_eq!(fan_step(&mut st, Some(75.0), 85.0, 75.0, false), None);
    }

    #[test]
    fn fan_hot_sample_goes_to_100_and_holds_until_cap_minus_5() {
        let mut st = st_at(40.0);
        assert_eq!(fan_step(&mut st, Some(85.0), 85.0, 80.0, false), Some(100)); // temp >= cap
        // Cooled to 82 (< cap but not < cap-5): still held at 100.
        assert_eq!(fan_step(&mut st, Some(82.0), 85.0, 80.0, false), None);
        assert_eq!(st.duty_percent(), 100);
        // 79.9 < 80: released, PI resumes from 100 (target 80 -> -0.2).
        assert_eq!(fan_step(&mut st, Some(79.0), 85.0, 80.0, false), None);
        assert_eq!(st.duty_percent(), 98);
        assert_eq!(fan_step(&mut st, Some(70.0), 85.0, 80.0, false), Some(78));
    }

    #[test]
    fn fan_missing_or_implausible_temp_forces_100() {
        for bad in [None, Some(0.0), Some(-5.0), Some(150.1), Some(f32::NAN), Some(f32::INFINITY)] {
            let mut st = st_at(30.0);
            assert_eq!(fan_step(&mut st, bad, 85.0, 80.0, false), Some(100), "{bad:?}");
        }
        // Back to PI once a plausible reading returns (no latch for a glitch).
        let mut st = st_at(30.0);
        fan_step(&mut st, None, 85.0, 80.0, false);
        assert_eq!(fan_step(&mut st, Some(70.0), 85.0, 80.0, false), Some(80));
    }

    #[test]
    fn fan_tuner_full_is_forced_and_restore_goes_back_to_pi() {
        let mut st = st_at(40.0);
        assert_eq!(fan_step(&mut st, Some(60.0), 85.0, 80.0, true), Some(100));
        // Cool, but the latch stays until temp < cap - 5.
        assert_eq!(fan_step(&mut st, Some(83.0), 85.0, 80.0, false), None);
        assert_eq!(st.duty_percent(), 100);
        st.restore(Some(83.0), 85.0); // too warm: ignored
        assert!(st.hold_full);
        st.restore(Some(70.0), 85.0);
        assert!(!st.hold_full);
        // Restore with a missing reading does not release.
        st.hold_full = true;
        st.restore(None, 85.0);
        assert!(st.hold_full);
    }

    fn fan_metrics(temp: f32) -> Metrics {
        Metrics {
            asic_temp_c: temp,
            power_w: 80.0,
            hashrate_ths: 4.0,
            frequency_mhz: 338.0,
            core_voltage_mv: 3704,
            fan_min_percent: None,
            fan_auto: None,
            fail_rate_pct: Some(2.0),
        }
    }

    #[test]
    fn owner_fan_loop_target_default_override_and_disable_restores_auto() {
        let mut o = OwnerTune::new();
        assert_eq!(o.fan_tick(Some(70.0)), None); // disabled: never touches the fan
        o.enable(TuneProfile::Efficient); // NANO3S_ENVELOPE efficient temp cap 80
        assert_eq!(o.fan_target_c(), 68.0); // Efficient default
        o.set_fan_target(Some(70.0));
        assert_eq!(o.fan_target_c(), 70.0);
        o.set_fan_target(Some(87.0)); // never closer than 2 C to the cap
        assert_eq!(o.fan_target_c(), 78.0);
        o.set_fan_target(Some(30.0)); // out of range: ignored
        assert_eq!(o.fan_target_c(), 68.0);
        for (p, want) in [
            (TuneProfile::Quiet, 65.0),
            (TuneProfile::Balanced, 83.0), // cap 88 - 5 (MaxHash's cap, Medium power)
            (TuneProfile::MaxHash, 83.0),  // cap 88 - 5
        ] {
            let mut o = OwnerTune::new();
            o.enable(p);
            let expect = default_fan_target_c(Some(p), o.tuner.temp_cap_c());
            assert_eq!(o.fan_target_c(), expect);
            assert_eq!(expect, want, "{p:?}");
        }
        assert_eq!(o.fan_tick(Some(75.0)), Some(OwnerCmd::FanManual(100)));
        assert_eq!(o.fan_tick(Some(75.0)), None);
        assert_eq!(o.disable(), Some(OwnerCmd::Fan("fan:auto,")));
        assert_eq!(o.fan_tick(Some(90.0)), None);
        // Never took the fan -> nothing to hand back.
        let mut o = OwnerTune::new();
        o.enable(TuneProfile::Efficient);
        assert_eq!(o.disable(), None);
    }

    #[test]
    fn tuner_temp_breach_routes_through_the_fan_loop() {
        let mut o = OwnerTune::new();
        o.enable(TuneProfile::Efficient); // temp cap 80
        o.fan_tick(Some(70.0));
        o.fan.duty = 40.0;
        o.fan.sent = Some(40);
        // 81 C breaches: the core asks for SetFanFull; no harness cmd from
        // evaluate itself, the fan loop writes 100.
        assert!(o.evaluate(&fan_metrics(81.0)).is_empty());
        assert_eq!(o.fan_tick(Some(81.0)), Some(OwnerCmd::FanManual(100)));
        // 76 C: under the cap but inside the release band -> stays 100.
        assert_eq!(o.fan_tick(Some(76.0)), None);
        assert_eq!(o.fan.duty_percent(), 100);
        // 60 C: released, PI ramps down from 100 (target 68).
        o.fan_tick(Some(60.0));
        assert!(o.fan.duty_percent() < 100);
    }

    #[test]
    fn owner_directive_round_trips_with_fan_target() {
        let d = parse_owner_directive("efficient;fan=72.5").unwrap();
        assert_eq!(d, OwnerDirective { profile: Some(Some(TuneProfile::Efficient)), fan_target_c: Some(72.5), chip_tune: None, continuous: None });
        let d = parse_owner_directive("keep;fan=75").unwrap();
        assert_eq!(d, OwnerDirective { profile: None, fan_target_c: Some(75.0), chip_tune: None, continuous: None });
        assert_eq!(parse_owner_directive("off").unwrap().profile, Some(None));
        assert!(parse_owner_directive("off;fan=95").is_err());
        assert!(parse_owner_directive("off;bogus=1").is_err());
        for (p, f) in [(None, Some(70.0)), (Some(None), None), (Some(Some(TuneProfile::Quiet)), Some(65.0))] {
            let line = directive_for_request(p, f);
            let d = parse_owner_directive(line.strip_prefix("owner_tune:").unwrap()).unwrap();
            assert_eq!((d.profile, d.fan_target_c), (p, f));
        }
    }

    #[test]
    fn status_exposes_events_best_and_ceiling() {
        let mut o = OwnerTune::new();
        o.enable(TuneProfile::Efficient);
        o.set_power_ceiling(112.0);
        for _ in 0..OWNER_SETTLE_CYCLES {
            o.evaluate(&fan_metrics(60.0));
        }
        o.publish(None);
        let s = read_status();
        assert_eq!(s.continuous_ceiling_w, Some(112.0));
        assert!(!s.continuous);
        assert!(s.events.len() <= 20 && !s.events.is_empty());
        assert!(s.events.iter().all(|e| e.power_w == 80.0 && e.hashrate_ths == 4.0));
        assert!(s.best.is_some_and(|b| (b.j_th - 20.0).abs() < 1e-3));

        o.set_continuous(true);
        o.publish(None);
        assert!(read_status().continuous);
    }

    #[test]
    fn profile_pick_resets_fan_override_and_memory() {
        let env = envelope_from_cali(&parse_cali(SAMPLE_CALI).unwrap());
        let mut o = OwnerTune::with_envelope(env);
        o.set_fan_target(Some(80.0));
        o.enable(TuneProfile::Balanced);
        o.restore_memory(SearchMemory {
            best: TuneSetpoint { frequency_mhz: 308.0, core_voltage_mv: 3756 },
            best_hashrate_ths: 5.26,
            best_j_th: 20.21,
            probe_floor_mv: 0,
            probe_ceiling_mhz: 450.0,
        });
        assert!(o.memory().is_some());

        // pick_profile with None resets fan target to Balanced default (cap 90 - 5 = 85.0) and clears memory
        o.pick_profile(TuneProfile::Balanced, None);
        assert_eq!(o.fan_target_c(), 85.0);
        assert!(o.memory().is_none());

        // pick_profile with Some(78.0) sets fan target to 78.0
        o.pick_profile(TuneProfile::Balanced, Some(78.0));
        assert_eq!(o.fan_target_c(), 78.0);
    }

    #[test]
    fn pick_profile_cross_profile_switch_adopts_new_profile_default_fan_target() {
        let env = envelope_from_cali(&parse_cali(SAMPLE_CALI).unwrap());
        let mut o = OwnerTune::with_envelope(env);
        // Start on Efficient (default target 68.0 C)
        o.pick_profile(TuneProfile::Efficient, None);
        assert_eq!(o.fan_target_c(), 68.0);

        // Switch Efficient -> Balanced with None fan target: must land on Balanced's default (85.0 C),
        // not keep the old profile's 68.0 C
        o.pick_profile(TuneProfile::Balanced, None);
        assert_eq!(o.fan_target_c(), 85.0);
    }
}
