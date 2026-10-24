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
    AutoTuner, CapLimits, Metrics, TuneAction, TuneEnvelope, TunePhase, TuneProfile, TuneSetpoint,
};

/// How often the owner tuner is evaluated. The status block of `run_worker`
/// runs on every IPC status refresh (~15 s per the existing comments); the
/// adapter gates on wall clock so a faster refresh cannot shorten a settle.
pub(crate) const OWNER_TICK: Duration = Duration::from_secs(15);

/// Settle = 10 ticks x 15 s = 150 s, the same as POWER_TARGET_CHECK_INTERVAL
/// and one SmartSpeed window (~131.1 s) plus margin, so a step is judged on
/// hashrate/fail-rate that reflect it.
pub(crate) const OWNER_SETTLE_CYCLES: u32 = 10;
/// One tick (15 s) between thermal/power back-off steps (spec: ~12-15 s).
pub(crate) const OWNER_SAFETY_SETTLE_CYCLES: u32 = 1;

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
    quiet: CapLimits { temp_c: 70.0, power_w: 60.0 },
    efficient: CapLimits { temp_c: 80.0, power_w: 90.0 },
    balanced: CapLimits { temp_c: 85.0, power_w: 110.0 },
    max_hash: CapLimits { temp_c: 88.0, power_w: 137.0 },
    target_temp_c: 88.0,
    target_hashrate_power_w: 137.0,
    target_power_headroom: 1.05,
    max_fail_rate_pct: Some(10.0),
};

// --- API types -------------------------------------------------------------

/// Body of `PATCH /api/v0/boards/{name}/owner-tune`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct OwnerTuneRequest {
    /// `quiet`, `efficient`, `balanced` or `max_hash`; `null` turns the owner
    /// tuner off.
    #[serde(default)]
    pub profile: Option<TuneProfile>,
}

/// Body of `GET /api/v0/boards/{name}/owner-tune`.
#[derive(Clone, Debug, Default, Serialize, ToSchema)]
pub struct OwnerTuneStatus {
    pub enabled: bool,
    pub profile: Option<TuneProfile>,
    pub phase: Option<TunePhase>,
    pub last_event: Option<String>,
    pub setpoint: Option<TuneSetpoint>,
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

// --- pure helpers ----------------------------------------------------------

/// Snaps a requested core voltage to the DC/DC's 26 mV grid (anchored at
/// 3600), never returns 3600 (steps one grid step down), then clamps to the
/// envelope range. 3800 is off-grid but is the project's hard limit, so it is
/// reached by the final clamp, as in POWER_TARGET_MAX_MV.
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
pub(crate) fn owner_power_ceiling(effective_safety_w: f64) -> f32 {
    ((effective_safety_w as f32) - OWNER_POWER_MARGIN_W).max(0.0)
}

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
}

/// Maps a tuner action to hardware commands. Min-fan adjustments are ignored:
/// the harness has no minimum-duty control and `fan_auto` is reported as
/// `None`, so the tuner never emits them in practice.
pub(crate) fn action_to_cmds(action: TuneAction) -> Vec<OwnerCmd> {
    match action {
        TuneAction::SetFrequency(base) => vec![OwnerCmd::SetMode(domain_freqs(base))],
        TuneAction::SetVoltage(mv) => vec![OwnerCmd::SetVoltage(snap_voltage_mv(mv as i64) as i32)],
        TuneAction::SetFanFull => vec![OwnerCmd::Fan("fan:manual,100")],
        TuneAction::RestoreFanAuto => vec![OwnerCmd::Fan("fan:auto,")],
        TuneAction::SetFanMinPercent(_) => vec![],
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
}

impl OwnerTune {
    pub(crate) fn new() -> Self {
        let mut tuner = AutoTuner::default();
        tuner.set_envelope(NANO3S_ENVELOPE);
        Self { tuner, profile: None, next_tick: None }
    }

    pub(crate) fn profile(&self) -> Option<TuneProfile> {
        self.profile
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.profile.is_some()
    }

    pub(crate) fn enable(&mut self, profile: TuneProfile) {
        self.tuner.enable_profile(profile);
        self.profile = Some(profile);
        self.next_tick = None;
    }

    /// Disables the tuner. Returns the harness command that hands the fan
    /// back to auto if the tuner had forced it to 100 %.
    pub(crate) fn disable(&mut self) -> Option<OwnerCmd> {
        let restore = self.tuner.fan_forced().then_some(OwnerCmd::Fan("fan:auto,"));
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
        self.tuner.set_power_ceiling(Some(w));
    }

    pub(crate) fn evaluate(&mut self, m: &Metrics) -> Vec<OwnerCmd> {
        self.tuner.evaluate(m).map(action_to_cmds).unwrap_or_default()
    }

    pub(crate) fn publish(&self, setpoint: Option<TuneSetpoint>) {
        publish_status(OwnerTuneStatus {
            enabled: self.profile.is_some(),
            profile: self.profile,
            phase: Some(self.tuner.phase()),
            last_event: self.tuner.last_event().map(str::to_owned),
            setpoint,
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
        assert_eq!(action_to_cmds(TuneAction::SetFanFull), vec![OwnerCmd::Fan("fan:manual,100")]);
        assert_eq!(action_to_cmds(TuneAction::RestoreFanAuto), vec![OwnerCmd::Fan("fan:auto,")]);
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
        assert_eq!(r.profile, None);
        let r: OwnerTuneRequest = serde_json::from_str(r#"{"profile":"max_hash"}"#).unwrap();
        assert_eq!(r.profile, Some(TuneProfile::MaxHash));
        assert!(serde_json::from_str::<OwnerTuneRequest>(r#"{"profile":"nope"}"#).is_err());
    }

    #[test]
    fn power_ceiling_and_backstop() {
        assert_eq!(owner_power_ceiling(137.0), 134.0);
        assert_eq!(owner_power_ceiling(1.0), 0.0);
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
        assert_eq!(e.settle_cycles as u64 * OWNER_TICK.as_secs(), 150);
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
}
