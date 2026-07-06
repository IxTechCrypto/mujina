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

use super::commands::BoardCommand;
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
/// Fraction of the expected (baseline-scaled) hashrate below which the chip
/// is treated as unstable (invalid shares / hung cores).
const STABLE_FRACTION: f32 = 0.85;
/// Below this hashrate the board is still warming up / not usefully hashing;
/// don't calibrate or judge stability yet.
const MIN_HASHRATE_THS: f32 = 0.3;

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
    /// Lowest clock observed to be unstable; the tuner stays below it.
    unstable_ceiling_mhz: f32,
    /// Best-seen hashrate per MHz, calibrated to this chip. Expected
    /// hashrate at any clock is `hash_per_mhz * clock`, so stability is
    /// judged relative to what the chip has actually delivered rather than
    /// an absolute nameplate that may not match a given board or pool.
    hash_per_mhz: Option<f32>,
    best: Option<TuneSetpoint>,
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
            unstable_ceiling_mhz: MAX_FREQ_MHZ + FREQ_STEP_MHZ,
            hash_per_mhz: None,
            best: None,
            last_efficiency: None,
            log: VecDeque::with_capacity(LOG_CAPACITY),
        }
    }
}

impl AutoTuner {
    /// Enable tuning with a profile, resetting the search.
    pub fn enable(&mut self, profile: TuneProfile) {
        self.enabled = true;
        self.profile = profile;
        self.phase = TunePhase::Warmup;
        self.cycles_since_change = 0;
        self.unstable_ceiling_mhz = MAX_FREQ_MHZ + FREQ_STEP_MHZ;
        self.hash_per_mhz = None;
        // Fresh search: drop any best/log from a previous profile so we never
        // persist a stale point under the newly-selected profile.
        self.best = None;
        self.log.clear();
    }

    /// Disable tuning. The board keeps whatever setpoint it is at.
    pub fn disable(&mut self) {
        self.enabled = false;
        self.phase = TunePhase::Disabled;
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
            best: self.best,
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
        //    instantaneous). Lower the clock a step; if already at the floor,
        //    trim voltage to shed power/heat.
        if m.asic_temp_c > caps.temp_c || m.power_w > caps.power_w {
            self.phase = TunePhase::BackedOff;
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
                self.unstable_ceiling_mhz = self.unstable_ceiling_mhz.min(m.frequency_mhz);
                self.log_event(format!("{why}: lowering to {target:.0} MHz"), m);
                return Some(TuneAction::SetFrequency(target));
            }
            if m.core_voltage_mv > MIN_VOLT_MV {
                let v = m
                    .core_voltage_mv
                    .saturating_sub(VOLT_STEP_MV)
                    .max(MIN_VOLT_MV);
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

        // 2. Tuning steps are slow: wait a full settle so the ~300 s windowed
        //    hashrate reflects the last change before it is judged.
        if self.cycles_since_change < SETTLE_CYCLES {
            return None;
        }
        self.cycles_since_change = 0;

        // Not usefully hashing yet: hold in warmup, don't calibrate on noise.
        if m.hashrate_ths < MIN_HASHRATE_THS {
            self.phase = TunePhase::Warmup;
            return None;
        }

        // 3. Instability: measured hashrate well under what this chip has shown
        //    it can deliver at this clock. Because we only reach here after a
        //    full settle, the reading is current, so max() calibrates the
        //    baseline rather than ratcheting on stale post-change data.
        let ratio = m.hashrate_ths / m.frequency_mhz.max(1.0);
        let hash_per_mhz = self.hash_per_mhz.map_or(ratio, |b| b.max(ratio));
        self.hash_per_mhz = Some(hash_per_mhz);
        let expected = hash_per_mhz * m.frequency_mhz;
        if m.hashrate_ths < STABLE_FRACTION * expected {
            // Hash-seeking profiles: try more voltage to hold this clock before
            // giving it up. Do NOT lower the ceiling here — if the extra
            // voltage stabilizes the clock we must still be allowed to keep it.
            if caps.seek_hash
                && m.core_voltage_mv + VOLT_STEP_MV <= AUTO_VOLT_CEIL_MV
                && m.power_w < caps.power_w - 1.0
            {
                let v = m.core_voltage_mv + VOLT_STEP_MV;
                self.phase = TunePhase::Seeking;
                self.log_event(format!("unstable: raising voltage to {v} mV"), m);
                return Some(TuneAction::SetVoltage(v));
            }
            // Voltage exhausted (or not a hash profile): this clock is the
            // unstable ceiling; drop below it.
            self.unstable_ceiling_mhz = self.unstable_ceiling_mhz.min(m.frequency_mhz);
            let target = (m.frequency_mhz - FREQ_STEP_MHZ).max(MIN_FREQ_MHZ);
            self.phase = TunePhase::Seeking;
            self.log_event(format!("unstable: lowering to {target:.0} MHz"), m);
            return (target < m.frequency_mhz).then_some(TuneAction::SetFrequency(target));
        }

        // Stable and within caps: a known-good point.
        self.best = Some(TuneSetpoint {
            frequency_mhz: m.frequency_mhz,
            core_voltage_mv: m.core_voltage_mv,
        });

        // 4a. Efficiency profiles: trim voltage while stable.
        if caps.seek_efficiency && m.core_voltage_mv.saturating_sub(VOLT_STEP_MV) >= MIN_VOLT_MV {
            let v = m.core_voltage_mv - VOLT_STEP_MV;
            self.phase = TunePhase::Seeking;
            self.log_event(format!("trimming voltage to {v} mV for efficiency"), m);
            return Some(TuneAction::SetVoltage(v));
        }

        // 4b. Hash profiles: climb the clock while there is headroom.
        let next = m.frequency_mhz + FREQ_STEP_MHZ;
        let has_headroom = m.asic_temp_c < caps.temp_c - 3.0 && m.power_w < caps.power_w - 1.0;
        if caps.seek_hash
            && next < self.unstable_ceiling_mhz
            && next <= MAX_FREQ_MHZ
            && has_headroom
        {
            self.phase = TunePhase::Seeking;
            self.log_event(format!("headroom: raising to {next:.0} MHz"), m);
            return Some(TuneAction::SetFrequency(next));
        }

        // 5. Nothing better to try: lock in the best-known-good point.
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
            let locked = (t.phase == TunePhase::Locked).then(|| t.best).flatten();
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
            (t.enabled, t.profile, t.best)
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
    fn backs_off_over_temp_cap() {
        let mut t = AutoTuner::default();
        t.enable(TuneProfile::Balanced); // temp cap 62
        let action = safety_step(&mut t, &m(70.0, 14.0, 1.3, 550.0, 1150));
        assert_eq!(action, Some(TuneAction::SetFrequency(525.0)));
        assert_eq!(t.phase, TunePhase::BackedOff);
    }

    #[test]
    fn backs_off_over_power_cap() {
        let mut t = AutoTuner::default();
        t.enable(TuneProfile::Quiet); // power cap 12
        let action = safety_step(&mut t, &m(50.0, 15.0, 1.2, 525.0, 1150));
        assert_eq!(action, Some(TuneAction::SetFrequency(500.0)));
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
        // Cool, low power, stable -> raise clock.
        let action = step_to_action(&mut t, &m(55.0, 13.0, 1.25, 550.0, 1150));
        assert_eq!(action, Some(TuneAction::SetFrequency(575.0)));
    }

    #[test]
    fn unstable_lowers_clock_when_voltage_maxed() {
        let mut t = AutoTuner::default();
        t.enable(TuneProfile::MaxHash);
        // Establish a healthy baseline (~1.2 TH/s at 525 MHz).
        step_to_action(&mut t, &m(55.0, 14.0, 1.2, 525.0, AUTO_VOLT_CEIL_MV));
        // Now a much higher clock delivering far too little hashrate is
        // unstable; voltage is already at the ceiling, so drop the clock.
        let action = step_to_action(&mut t, &m(60.0, 15.0, 0.5, 600.0, AUTO_VOLT_CEIL_MV));
        assert_eq!(action, Some(TuneAction::SetFrequency(575.0)));
    }

    #[test]
    fn baseline_calibration_avoids_false_instability() {
        // A chip delivering ~1.0 TH/s at 525 MHz (below any 1.2 nameplate)
        // must read as stable — the baseline calibrates to the chip, so a
        // hash-seeking profile does not chase phantom instability by
        // over-raising voltage.
        let mut t = AutoTuner::default();
        t.enable(TuneProfile::MaxHash);
        let action = step_to_action(&mut t, &m(55.0, 11.5, 1.0, 525.0, 1150));
        // Stable + headroom -> climb the clock, not raise voltage.
        assert_eq!(action, Some(TuneAction::SetFrequency(550.0)));
    }

    #[test]
    fn efficient_trims_voltage_when_stable() {
        let mut t = AutoTuner::default();
        t.enable(TuneProfile::Efficient);
        let action = step_to_action(&mut t, &m(55.0, 12.0, 1.2, 525.0, 1150));
        assert_eq!(action, Some(TuneAction::SetVoltage(1140)));
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
