//! Supervisor-level ASIC auto-tuning.
//!
//! Runs as a task alongside the API server. Each cycle it reads a board's
//! telemetry (ASIC temperature, power, clock, core voltage) plus the
//! measured hashrate, then nudges frequency and voltage toward a profile's
//! goal — always inside hard thermal and power limits — by driving the
//! board through the same [`BoardCommand`] channel the manual tuning API
//! uses.
//!
//! The decision logic ([`AutoTuner::evaluate`]) is a pure function of the
//! current [`Metrics`] and tuner state, so it is unit-tested without any
//! hardware. The [`run`] task wires it to live telemetry and commands.
//!
//! # Prior art / acknowledgements
//!
//! The approach here is informed by the open-source Bitaxe auto-tuning
//! community. Unlike those tools — external scripts that drive the AxeOS
//! HTTP API — this runs inside the miner, but it borrows their proven
//! ideas. With thanks to:
//!
//! - **BitaxePID** by kha1n3vol3 — dual PID control, per-model profiles,
//!   snapshot persistence, and the tuning activity log.
//!   <https://github.com/kha1n3vol3/BitaxePID>
//! - **bitaxe-gamma-oc-script** by terminally-challenged — the sweep +
//!   coefficient-of-variation stability check.
//!   <https://github.com/terminally-challenged/bitaxe-gamma-oc-script>
//! - **bitaxe_frequency_sweeper** by andelorean — stepwise climb with
//!   temperature/VR/power thresholds and a values lookup table.
//!   <https://github.com/andelorean/bitaxe_frequency_sweeper>
//! - **bitaxe-temp-monitor** by Hurllz and **Bitaxe-Hashrate-Benchmark** /
//!   **Bitaxe-Temperature-Control** by WhiteyCookie — thermal-governor
//!   tuning and hashrate benchmarking.
//!   <https://github.com/Hurllz/bitaxe-temp-monitor>,
//!   <https://github.com/WhiteyCookie/Bitaxe-Hashrate-Benchmark>
//! - **AxeBench** — the Quiet / Efficient / Balanced / Max-Hash profile
//!   framing.
//! - **D-Central's** Bitaxe overclocking and auto-tuning guides — safe
//!   frequency/voltage ranges, 24/7 temperature targets, and the
//!   ~15 W / 25 W power limits used for the profile caps.
//!   <https://d-central.tech/bitaxe-auto-tuning-scripts-guide/>

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, watch};
use tokio::time::{self, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use utoipa::ToSchema;

use super::commands::{BoardCommand, FanControlUpdate};
use super::registry::BoardRegistry;
use crate::api_client::types::MinerTelemetry;
use crate::tracing::prelude::*;

// --- tuning envelope (shared with the manual clamps in board/bitaxe.rs
// and asic/bm13xx/thread.rs; kept conservative on purpose) ---
const MIN_FREQ_MHZ: f32 = 400.0;
const MAX_FREQ_MHZ: f32 = 650.0;
const MIN_VOLT_MV: u16 = 1000;
/// Ceiling the tuner will raise voltage to on its own. Below the 1300 mV
/// hard clamp: the tuner should never sit near the sustained-damage line.
const AUTO_VOLT_CEIL_MV: u16 = 1250;

const FREQ_STEP_MHZ: f32 = 25.0;
const VOLT_STEP_MV: u16 = 10;

/// Supervisor cycles between tuning steps. The scheduler's hashrate estimate
/// is a ~300 s windowed average, so a tuning step must wait about that long
/// before the measured hashrate reflects a clock change — otherwise a
/// just-changed clock is judged on stale data and the calibrated baseline
/// ratchets. At the 2 s cadence, 150 cycles ≈ 5 min/step (matching the
/// 5–10 min/point the manual overclocking guides use). Tuning is a slow
/// background optimizer by design.
const SETTLE_CYCLES: u32 = 150;
/// Cycles between successive thermal/power back-off steps. Far shorter than
/// a tuning step: a cap breach must be acted on promptly (temperature and
/// power are instantaneous readings, not windowed), but not every 2 s or we
/// overshoot before the previous drop takes effect.
const SAFETY_SETTLE_CYCLES: u32 = 6;
/// Relative margin a candidate point must beat the recorded best by (on
/// hashrate for hash-seeking profiles, on efficiency for efficiency-seeking
/// ones) to count as a genuine improvement rather than noise. Points judged
/// directly against real measurements from this chip, never an extrapolated
/// nameplate table.
const IMPROVEMENT_MARGIN: f32 = 0.03;
/// Below this hashrate the board is still warming up / not usefully hashing;
/// don't judge stability yet.
const MIN_HASHRATE_THS: f32 = 0.3;
/// Tolerance for comparing a live frequency reading to a recorded setpoint.
const FREQ_TOLERANCE_MHZ: f32 = 0.01;
/// Tolerance (mV) for deciding a live point is "at" a recorded voltage
/// setpoint. Absorbs regulator droop/ADC jitter (a measured vout lags the
/// commanded setpoint by up to ~one step); kept below `VOLT_STEP_MV` so an
/// intentionally-adjacent point is still distinct.
const VOLT_TOLERANCE_MV: u16 = 5;
/// Temperature (C) below the cap the die must fall to before a forced-full
/// fan episode is released. A hysteresis band so a die hovering at the cap
/// (its natural attractor) does not flap the fan every cycle.
const FAN_RELEASE_HYSTERESIS_C: f32 = 2.0;

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
    fn caps(self) -> Caps {
        match self {
            TuneProfile::Quiet => Caps {
                temp_c: 55.0,
                power_w: 12.0,
                seek_hash: false,
                seek_efficiency: false,
            },
            TuneProfile::Efficient => Caps {
                temp_c: 60.0,
                power_w: 13.0,
                seek_hash: false,
                seek_efficiency: true,
            },
            TuneProfile::Balanced => Caps {
                temp_c: 62.0,
                power_w: 15.0,
                seek_hash: true,
                seek_efficiency: false,
            },
            TuneProfile::MaxHash => Caps {
                temp_c: 68.0,
                power_w: 22.0,
                seek_hash: true,
                seek_efficiency: false,
            },
        }
    }
}

/// A frequency/voltage operating point.
#[derive(Clone, Copy, Debug, PartialEq, Deserialize, Serialize, ToSchema)]
pub struct TuneSetpoint {
    pub frequency_mhz: f32,
    pub core_voltage_mv: u16,
}

/// Live inputs to one tuner decision.
#[derive(Clone, Copy, Debug)]
pub struct Metrics {
    pub asic_temp_c: f32,
    pub power_w: f32,
    pub hashrate_ths: f32,
    pub frequency_mhz: f32,
    pub core_voltage_mv: u16,
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
}

/// One activity-log entry, surfaced in the dashboard.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct TuneEvent {
    pub message: String,
    pub frequency_mhz: f32,
    pub core_voltage_mv: u16,
    pub asic_temp_c: f32,
    pub efficiency_j_th: Option<f32>,
}

/// Snapshot the API returns for `GET /boards/{name}/autotune`.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct AutoTuneStatus {
    pub enabled: bool,
    pub profile: TuneProfile,
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
    profile: TuneProfile,
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
    last_efficiency: Option<f32>,
    log: VecDeque<TuneEvent>,
}

impl Default for AutoTuner {
    fn default() -> Self {
        Self {
            enabled: false,
            profile: TuneProfile::default(),
            phase: TunePhase::Disabled,
            cycles_since_change: 0,
            best: None,
            probe_ceiling_mhz: MAX_FREQ_MHZ + FREQ_STEP_MHZ,
            pending_reject_mhz: None,
            probe_floor_mv: 0,
            last_commanded_voltage_mv: None,
            fan_forced_full: false,
            last_efficiency: None,
            log: VecDeque::with_capacity(LOG_CAPACITY),
        }
    }
}

impl AutoTuner {
    /// Enable tuning with a profile, resetting the search.
    ///
    /// Deliberately does NOT reset `fan_forced_full`: that flag tracks
    /// whether the *board* is currently sitting in a forced-100% fan
    /// override, not the tuning search, and switching profiles (or
    /// disabling/re-enabling) does not touch the board's actual fan state.
    /// Clearing it here previously left the flag reporting "not forced"
    /// while the fan was still physically pinned at 100% from the prior
    /// profile's breach, so `evaluate` could never issue the
    /// `RestoreFanAuto` that would have handed it back — the fan stayed
    /// stuck at 100% until a fresh breach happened to re-arm the flag.
    pub fn enable(&mut self, profile: TuneProfile) {
        self.enabled = true;
        self.profile = profile;
        self.phase = TunePhase::Warmup;
        self.cycles_since_change = 0;
        // Fresh search: drop any best/log from a previous profile so we never
        // persist a stale point under the newly-selected profile.
        self.best = None;
        self.probe_ceiling_mhz = MAX_FREQ_MHZ + FREQ_STEP_MHZ;
        self.pending_reject_mhz = None;
        self.probe_floor_mv = 0;
        self.last_commanded_voltage_mv = None;
        self.log.clear();
    }

    /// Disable tuning. The board keeps whatever setpoint it is at,
    /// including the fan — see [`Self::enable`] for why `fan_forced_full`
    /// is not reset here either.
    pub fn disable(&mut self) {
        self.enabled = false;
        self.phase = TunePhase::Disabled;
        self.last_commanded_voltage_mv = None;
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn status(&self, setpoint: Option<TuneSetpoint>) -> AutoTuneStatus {
        AutoTuneStatus {
            enabled: self.enabled,
            profile: self.profile,
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
        if !self.enabled {
            return None;
        }
        self.last_efficiency = efficiency_j_th(m.power_w, m.hashrate_ths);
        self.cycles_since_change += 1;

        let caps = self.profile.caps();

        // 1. Safety: over a hard cap -> back off on a fast cadence that does
        //    NOT wait for the long tuning settle (temperature and power are
        //    instantaneous). A breach is treated as a transient condition,
        //    never as permanent evidence the silicon can't run this clock —
        //    nothing here narrows the search space for future attempts.
        if m.asic_temp_c > caps.temp_c || m.power_w > caps.power_w {
            self.phase = TunePhase::BackedOff;

            // Temperature breaches get the fan pushed to 100% first — the
            // automatic curve may still have headroom left (e.g. it only
            // reaches 100% well above where a profile's cap sits), so the
            // tuner should not sacrifice clock while the fan still has more
            // to give. Power breaches skip this: more airflow doesn't reduce
            // watts drawn.
            if m.asic_temp_c > caps.temp_c && !self.fan_forced_full {
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

            if self.cycles_since_change < SAFETY_SETTLE_CYCLES {
                return None;
            }
            self.cycles_since_change = 0;
            let why = if m.asic_temp_c > caps.temp_c {
                format!("temp {:.1}C over {:.0}C cap", m.asic_temp_c, caps.temp_c)
            } else {
                format!("power {:.1}W over {:.0}W cap", m.power_w, caps.power_w)
            };
            if m.frequency_mhz > MIN_FREQ_MHZ {
                let target = (m.frequency_mhz - FREQ_STEP_MHZ).max(MIN_FREQ_MHZ);
                self.log_event(format!("{why}: lowering to {target:.0} MHz"), m);
                return Some(TuneAction::SetFrequency(target));
            }
            if m.core_voltage_mv > MIN_VOLT_MV {
                let v = m
                    .core_voltage_mv
                    .saturating_sub(VOLT_STEP_MV)
                    .max(MIN_VOLT_MV);
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
        if self.cycles_since_change < SETTLE_CYCLES {
            return None;
        }
        self.cycles_since_change = 0;

        // Not usefully hashing: genuine first-time warmup holds. A collapse
        // after `best` was already established falls through instead — it is
        // judged (and reverted) below like any other bad candidate, rather
        // than holding here forever.
        if m.hashrate_ths < MIN_HASHRATE_THS && self.best.is_none() {
            self.phase = TunePhase::Warmup;
            return None;
        }

        let hashrate = m.hashrate_ths;
        let efficiency = efficiency_j_th(m.power_w, hashrate).unwrap_or(f32::INFINITY);
        // All voltage reasoning is in the commanded domain (see helper), so a
        // recorded `best`, an `at_best` match, and the next raise/trim all
        // agree instead of chasing regulator droop.
        let commanded_v = self.commanded_voltage(m);

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

        // 4. A genuinely new candidate point: judge it directly against the
        //    best ever measured on this chip, never an extrapolated table.
        let strictly_improved = match self.best {
            None => true,
            Some((_, best_hash, best_eff)) => {
                if caps.seek_efficiency {
                    efficiency < best_eff * (1.0 - IMPROVEMENT_MARGIN)
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
            && commanded_v + VOLT_STEP_MV <= AUTO_VOLT_CEIL_MV
            && m.power_w < caps.power_w - 1.0
        {
            let v = commanded_v + VOLT_STEP_MV;
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

    /// Having just confirmed or improved on `best`, try to extend the search
    /// (trim voltage for efficiency profiles, climb the clock for
    /// hash-seeking ones); lock if there's nothing further to try.
    fn seek_further_or_lock(&mut self, m: &Metrics, caps: &Caps) -> Option<TuneAction> {
        self.phase = TunePhase::Seeking;

        // Trim from the commanded voltage, not the drooping measured one, so a
        // step moves the setpoint by exactly VOLT_STEP_MV rather than
        // droop+step (which would overshoot the stability cliff).
        let trimmed = self.commanded_voltage(m).saturating_sub(VOLT_STEP_MV);
        if caps.seek_efficiency && trimmed >= MIN_VOLT_MV && trimmed > self.probe_floor_mv {
            self.last_commanded_voltage_mv = Some(trimmed);
            self.log_event(
                format!("trimming voltage to {trimmed} mV for efficiency"),
                m,
            );
            return Some(TuneAction::SetVoltage(trimmed));
        }

        let next = m.frequency_mhz + FREQ_STEP_MHZ;
        let has_headroom = m.asic_temp_c < caps.temp_c - 3.0 && m.power_w < caps.power_w - 1.0;
        if caps.seek_hash && next <= MAX_FREQ_MHZ && next < self.probe_ceiling_mhz && has_headroom {
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

// --- persistence -----------------------------------------------------------

/// Persisted auto-tune state for a board, keyed by serial.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct SavedProfile {
    /// Whether the tuner was actively enabled when this was saved. Only then
    /// is tuning resumed on the next boot, so a board the user left on manual
    /// control is never silently retuned.
    #[serde(default)]
    enabled: bool,
    profile: TuneProfile,
    setpoint: TuneSetpoint,
}

fn state_path() -> PathBuf {
    // Next to the daemon by default; overridable for tests/deployments.
    std::env::var_os("MUJINA_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("mujina-autotune.json")
}

fn load_saved() -> HashMap<String, SavedProfile> {
    match std::fs::read(state_path()) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(map) => map,
            Err(e) => {
                // Don't silently wipe on a corrupt file; surface it and start
                // empty rather than overwriting whatever is there.
                warn!(error = %e, "Ignoring unreadable autotune state file");
                Default::default()
            }
        },
        Err(_) => Default::default(),
    }
}

fn save_profile(serial: &str, saved: SavedProfile) {
    let mut all = load_saved();
    all.insert(serial.to_string(), saved);
    let Ok(bytes) = serde_json::to_vec_pretty(&all) else {
        return;
    };
    // Atomic write: a crash mid-write must not truncate the file and lose
    // other boards' saved profiles. Write a temp file, then rename over.
    let path = state_path();
    let tmp = path.with_extension("json.tmp");
    if let Err(e) = std::fs::write(&tmp, &bytes) {
        warn!(error = %e, path = %tmp.display(), "Failed to write autotune state");
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        warn!(error = %e, path = %path.display(), "Failed to commit autotune state");
    }
}

// --- supervisor task -------------------------------------------------------

/// Shared handle: the API reads/writes the manager, the task drives it.
pub type SharedAutoTuner = Arc<Mutex<AutoTuner>>;

/// Run the auto-tuning supervisor until cancelled.
///
/// Reads the first connected board's telemetry and the aggregate hashrate,
/// evaluates the tuner, and applies any action through the board command
/// channel. Persists the best point when the tuner locks.
pub async fn run(
    tuner: SharedAutoTuner,
    board_registry: Arc<Mutex<BoardRegistry>>,
    miner_telemetry_rx: watch::Receiver<MinerTelemetry>,
    cancel: CancellationToken,
) {
    let mut tick = time::interval(Duration::from_secs(2));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    // Track what we last persisted so a lock only writes once.
    let mut saved_best: Option<TuneSetpoint> = None;
    // Last enabled state we persisted, so an enable/disable is written through
    // and a reboot resumes only what the user left running.
    let mut last_enabled: Option<bool> = None;
    // Serials we have already considered for boot-time resume, so it happens
    // at most once per board per run.
    let mut resumed: HashSet<String> = Default::default();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tick.tick() => {}
        }

        // Gather the board's telemetry plus the aggregate hashrate. The tuner
        // supports a single board: with more than one connected, the aggregate
        // hashrate can't be attributed, so hold off rather than mis-tune.
        let hashrate_ths = miner_telemetry_rx.borrow().hashrate as f32 / 1e12;
        let (name, serial, metrics, sender) = {
            let mut reg = board_registry.lock().unwrap_or_else(|e| e.into_inner());
            let boards = reg.boards();
            if boards.len() != 1 {
                continue;
            }
            let board = boards.into_iter().next().unwrap();
            let Some(sender) = reg.command_sender(&board.name) else {
                continue;
            };
            let temp = board
                .temperatures
                .iter()
                .find(|t| t.name == "asic")
                .and_then(|t| t.temperature)
                .map(|t| t.as_degrees_c());
            let core = board.powers.iter().find(|p| p.name == "core");
            // Require temperature, clock, power AND voltage: a missing reading
            // must never default to a value that disables a cap or underflows
            // a step. Wait for a complete snapshot instead.
            let (Some(temp), Some(freq), Some(core)) = (temp, board.frequency_mhz, core) else {
                continue;
            };
            let (Some(power_w), Some(voltage_v)) = (core.power_w, core.voltage_v) else {
                continue;
            };
            let metrics = Metrics {
                asic_temp_c: temp,
                power_w,
                hashrate_ths,
                frequency_mhz: freq,
                core_voltage_mv: (voltage_v * 1000.0).round() as u16,
            };
            (board.name.clone(), board.serial.clone(), metrics, sender)
        };

        // Boot-time resume: if this board was actively auto-tuning when it was
        // last saved, re-enable the tuner so it converges again. We do NOT
        // blindly re-apply a stored setpoint — a board left on manual control
        // keeps its manual settings.
        if let Some(serial) = serial.as_ref()
            && resumed.insert(serial.clone())
            && let Some(saved) = load_saved().get(serial).cloned()
            && saved.enabled
        {
            let mut t = tuner.lock().unwrap_or_else(|e| e.into_inner());
            if !t.is_enabled() {
                t.enable(saved.profile);
                info!(board = %name, profile = ?saved.profile, "Resuming saved auto-tune profile");
            }
        }

        // Decide (holding the tuner lock only for the decision).
        let (action, locked_best) = {
            let mut t = tuner.lock().unwrap_or_else(|e| e.into_inner());
            let action = t.evaluate(&metrics);
            let locked = (t.phase == TunePhase::Locked)
                .then(|| t.best.map(|(setpoint, ..)| setpoint))
                .flatten();
            (action, locked)
        };

        // Persist a freshly-locked best-known-good point (once).
        if let (Some(best), Some(serial)) = (locked_best, serial.as_ref())
            && saved_best != Some(best)
        {
            saved_best = Some(best);
            let profile = tuner.lock().unwrap_or_else(|e| e.into_inner()).profile;
            save_profile(
                serial,
                SavedProfile {
                    enabled: true,
                    profile,
                    setpoint: best,
                },
            );
            info!(board = %name, ?best, "Auto-tune converged; profile saved");
        }

        // Persist enable/disable transitions so a reboot resumes only what the
        // user left running (a board turned back to manual is not retuned).
        let (enabled_now, profile_now, best_now) = {
            let t = tuner.lock().unwrap_or_else(|e| e.into_inner());
            (t.enabled, t.profile, t.best.map(|(setpoint, ..)| setpoint))
        };
        if let Some(serial) = serial.as_ref()
            && last_enabled != Some(enabled_now)
        {
            last_enabled = Some(enabled_now);
            let setpoint = best_now.unwrap_or(TuneSetpoint {
                frequency_mhz: metrics.frequency_mhz,
                core_voltage_mv: metrics.core_voltage_mv,
            });
            save_profile(
                serial,
                SavedProfile {
                    enabled: enabled_now,
                    profile: profile_now,
                    setpoint,
                },
            );
        }

        // Apply the action through the board command channel.
        if let Some(action) = action {
            let (cmd, desc): (BoardCommand, String) = match action {
                TuneAction::SetFrequency(mhz) => {
                    let (tx, _rx) = oneshot::channel();
                    (
                        BoardCommand::SetFrequency { mhz, reply: tx },
                        format!("{mhz:.0} MHz"),
                    )
                }
                TuneAction::SetVoltage(mv) => {
                    let (tx, _rx) = oneshot::channel();
                    (
                        BoardCommand::SetCoreVoltage {
                            millivolts: mv,
                            reply: tx,
                        },
                        format!("{mv} mV"),
                    )
                }
                TuneAction::SetFanFull => {
                    let (tx, _rx) = oneshot::channel();
                    (
                        BoardCommand::SetFanControl {
                            update: FanControlUpdate {
                                auto: false,
                                percent: Some(100),
                                ..Default::default()
                            },
                            reply: tx,
                        },
                        "fan 100%".to_string(),
                    )
                }
                TuneAction::RestoreFanAuto => {
                    let (tx, _rx) = oneshot::channel();
                    (
                        BoardCommand::SetFanControl {
                            // Auto with no overrides: the board resolves the
                            // documented default target/minimum. Phase-1
                            // limitation: this restores the *default* curve,
                            // not a custom one the operator may have set.
                            update: FanControlUpdate {
                                auto: true,
                                ..Default::default()
                            },
                            reply: tx,
                        },
                        "fan auto".to_string(),
                    )
                }
            };
            // Best-effort: a full command buffer just means we retry next cycle.
            if let Err(e) = sender.try_send(cmd) {
                debug!(board = %name, error = %e, "Auto-tune command dropped (busy); will retry");
            } else {
                debug!(board = %name, action = %desc, "Auto-tune applied");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(temp: f32, power: f32, hash_ths: f32, freq: f32, volt: u16) -> Metrics {
        Metrics {
            asic_temp_c: temp,
            power_w: power,
            hashrate_ths: hash_ths,
            frequency_mhz: freq,
            core_voltage_mv: volt,
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
        t.enable(TuneProfile::MaxHash);
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
        t.enable(TuneProfile::Balanced); // temp cap 62
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
        t.enable(TuneProfile::Quiet); // power cap 12
        // Power breaches skip the fan (airflow doesn't cut watts) and drop
        // the clock directly.
        let action = safety_step(&mut t, &m(50.0, 15.0, 1.2, 525.0, 1150));
        assert_eq!(action, Some(TuneAction::SetFrequency(500.0)));
        assert!(!t.fan_forced_full);
    }

    #[test]
    fn restores_fan_when_temp_clears() {
        let mut t = AutoTuner::default();
        t.enable(TuneProfile::MaxHash); // temp cap 68
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
        t.enable(TuneProfile::Balanced); // temp cap 62
        assert_eq!(
            t.evaluate(&m(70.0, 14.0, 1.3, 525.0, 1150)),
            Some(TuneAction::SetFanFull)
        );
        assert!(t.fan_forced_full);

        // Operator switches profile while the fan is still forced full.
        t.enable(TuneProfile::Efficient); // temp cap 60
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
        t.enable(TuneProfile::MaxHash); // temp cap 68
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
        t.enable(TuneProfile::Quiet); // power cap 12
        // Already at the clock floor and over the power cap: shed voltage.
        let action = safety_step(&mut t, &m(50.0, 15.0, 1.0, MIN_FREQ_MHZ, 1150));
        assert_eq!(action, Some(TuneAction::SetVoltage(1140)));
    }

    #[test]
    fn maxhash_climbs_with_headroom() {
        let mut t = AutoTuner::default();
        t.enable(TuneProfile::MaxHash);
        // Cool, low power, first solid point -> raise clock.
        let action = step_to_action(&mut t, &m(55.0, 13.0, 1.25, 550.0, 1150));
        assert_eq!(action, Some(TuneAction::SetFrequency(575.0)));
    }

    #[test]
    fn stable_point_with_headroom_climbs_the_clock() {
        // Judged on the measured hashrate directly (no nameplate table): a
        // solid point with thermal/power headroom pushes the clock up.
        let mut t = AutoTuner::default();
        t.enable(TuneProfile::MaxHash);
        let action = step_to_action(&mut t, &m(55.0, 11.5, 1.0, 525.0, 1150));
        assert_eq!(action, Some(TuneAction::SetFrequency(550.0)));
    }

    #[test]
    fn reverts_to_best_when_higher_clock_does_not_improve() {
        let mut t = AutoTuner::default();
        t.enable(TuneProfile::MaxHash);
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
        t.enable(TuneProfile::MaxHash);
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
        t.enable(TuneProfile::MaxHash);
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
        t.enable(TuneProfile::MaxHash);
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
        t.enable(TuneProfile::MaxHash); // temp cap 68
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
        t.enable(TuneProfile::MaxHash);
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
        t.enable(TuneProfile::MaxHash);
        step_to_action(&mut t, &m(55.0, 13.0, 1.0, 525.0, 1150));
        let action = step_to_action(&mut t, &m(55.0, 13.0, 0.1, 575.0, 1150));
        assert_eq!(action, Some(TuneAction::SetFrequency(525.0)));
    }

    #[test]
    fn efficient_trims_voltage_when_stable() {
        let mut t = AutoTuner::default();
        t.enable(TuneProfile::Efficient);
        let action = step_to_action(&mut t, &m(55.0, 12.0, 1.2, 525.0, 1150));
        assert_eq!(action, Some(TuneAction::SetVoltage(1140)));
    }

    #[test]
    fn efficient_reverts_and_locks_when_undervolt_hurts() {
        // The efficiency walk must not oscillate: an undervolt that worsens
        // efficiency is reverted and its voltage marked as the probe floor.
        let mut t = AutoTuner::default();
        t.enable(TuneProfile::Efficient); // temp cap 60, power cap 13
        // 1150 solid -> trim to 1140.
        let a1 = step_to_action(&mut t, &m(55.0, 12.0, 1.2, 525.0, 1150));
        assert_eq!(a1, Some(TuneAction::SetVoltage(1140)));
        // 1140 is worse (same hashrate, but we assert efficiency didn't beat
        // best: lower power at same hashrate WOULD be better, so make power
        // higher to model an undervolt that cost hashrate). Model the
        // undervolt hurting: hashrate drops so efficiency worsens.
        let a2 = step_to_action(&mut t, &m(55.0, 12.0, 1.0, 525.0, 1140));
        assert_eq!(a2, Some(TuneAction::SetVoltage(1150)));
        assert_eq!(t.probe_floor_mv, 1140);
        // Back at best; the floor blocks re-trimming to 1140, so it locks.
        let a3 = step_to_action(&mut t, &m(55.0, 12.0, 1.2, 525.0, 1150));
        assert_eq!(a3, None);
        assert_eq!(t.phase, TunePhase::Locked);
    }

    #[test]
    fn quiet_locks_when_stable_and_no_headroom_sought() {
        let mut t = AutoTuner::default();
        t.enable(TuneProfile::Quiet); // not seek_hash, not efficiency
        let action = step_to_action(&mut t, &m(50.0, 11.0, 1.2, 525.0, 1150));
        assert_eq!(action, None);
        assert_eq!(t.phase, TunePhase::Locked);
    }
}
