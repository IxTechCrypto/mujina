//! API data transfer objects.
//!
//! These types define the API contract shared between the server and
//! clients (CLI, TUI). See `docs/api.md` (at the repository root)
//! for the full API contract documentation, including conventions
//! for null values and units.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::api::autotune::TuneTarget;
use crate::types::Temperature;

/// Full miner telemetry snapshot.
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct MinerTelemetry {
    /// User-chosen friendly name for this miner, or null if unnamed.
    ///
    /// Config rather than measurement, so the API layer attaches it to each
    /// snapshot instead of the scheduler carrying it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub uptime_secs: u64,
    /// Aggregate hashrate in hashes per second.
    pub hashrate: u64,
    pub shares_submitted: u64,
    pub paused: bool,
    pub boards: Vec<BoardTelemetry>,
    pub sources: Vec<SourceTelemetry>,
    /// Per-thread measurements, carried from the scheduler so the API can
    /// attach each one to the board that owns it.
    ///
    /// Not serialized: the public contract is the per-board `threads`
    /// array, and repeating the same numbers at the top level would be
    /// two things to keep in agreement.
    #[serde(skip)]
    pub threads: Vec<ThreadTelemetry>,
}

/// Board telemetry snapshot.
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct BoardTelemetry {
    /// URL-friendly identifier (e.g. "bitaxe-e2f56f9b").
    pub name: String,
    pub model: String,
    pub serial: Option<String>,
    /// ASIC chip model on this board (e.g. "BM1370"), or null if unknown.
    /// Looked up against `bm13xx::chip_profile` to get the safe tuning
    /// envelope and hashrate-target UI bounds for this board's silicon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chip_model: Option<String>,
    /// Number of ASIC chips on this board.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chip_count: Option<u32>,
    /// ASIC hash clock in MHz, or null if the board does not report one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_mhz: Option<f32>,
    pub fans: Vec<Fan>,
    pub temperatures: Vec<TemperatureSensor>,
    pub powers: Vec<PowerMeasurement>,
    pub threads: Vec<ThreadTelemetry>,
    /// Number of hash threads the board handed to the backplane.
    ///
    /// Zero means the board is present but contributes nothing to the
    /// aggregate hashrate, either because its hash threads are not
    /// implemented yet or because it has none by design. Unlike `threads`,
    /// which stays empty until per-thread hashrate accounting exists, this
    /// is a fact every board already knows at construction, so clients can
    /// use it to attribute the miner-wide hashrate when exactly one board
    /// is hashing. Always serialized: an absent field means an older
    /// daemon, which a client cannot distinguish from a genuine zero.
    #[serde(default)]
    pub thread_count: u32,
}

/// Fan status.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Fan {
    pub name: String,
    /// Measured RPM, or null if the tachometer read failed.
    pub rpm: Option<u32>,
    /// Measured duty cycle, or null if the read failed.
    pub percent: Option<u8>,
    /// Commanded duty cycle the controller is currently driving toward
    /// (the manual setpoint, or the value the automatic curve chose), or
    /// null if no command has been issued yet.
    pub target_percent: Option<u8>,
    /// Whether automatic temperature-tracking control is active, or null
    /// if the board does not expose a controllable fan policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto: Option<bool>,
    /// Automatic-mode target temperature in Celsius, when in auto mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_c: Option<f32>,
    /// Automatic-mode minimum duty cycle, when in auto mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_percent: Option<u8>,
}

/// Temperature sensor reading.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct TemperatureSensor {
    pub name: String,
    #[serde(rename = "temperature_c")]
    #[schema(value_type = Option<f32>)]
    pub temperature: Option<Temperature>,
}

/// Voltage, current, and power from a single measurement point.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct PowerMeasurement {
    pub name: String,
    pub voltage_v: Option<f32>,
    pub current_a: Option<f32>,
    pub power_w: Option<f32>,
}

/// Per-thread telemetry.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct ThreadTelemetry {
    pub name: String,
    /// Hashrate in hashes per second.
    pub hashrate: u64,
    pub is_active: bool,
    /// Per-ASIC breakdown, for chains whose silicon identifies which chip
    /// found each nonce. Empty on single-chip boards and on any chain that
    /// does not report it, so an empty list means "not available" rather
    /// than "no chips".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chips: Vec<ChipTelemetry>,
}

/// Per-ASIC telemetry within a chain.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct ChipTelemetry {
    /// Position along the chain, counting from the host.
    pub index: u8,
    /// Hashrate in hashes per second, measured from the shares this chip
    /// produced.
    pub hashrate: u64,
}

/// Writable fields for `PATCH /api/v0/miner`.
///
/// All fields are optional; only those present in the request body are
/// applied. Read-only fields like `uptime_secs` and `hashrate` are not
/// included and cannot be set.
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct MinerPatchRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paused: Option<bool>,
}

/// Pool settings as returned by the API.
///
/// The password is never sent back -- the API is unauthenticated, and a
/// client that only needs to know whether one is configured can read
/// `password_set`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct PoolSettingsView {
    pub url: String,
    /// Account the pool authorizes, without the worker suffix.
    pub user: String,
    /// Whether a password is stored. Its value is not disclosed.
    pub password_set: bool,
}

/// Response body for `GET`/`PATCH /settings`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct SettingsResponse {
    /// Friendly miner name, or null if unnamed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Configured pool, or null when the miner runs the dummy job source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pool: Option<PoolSettingsView>,
    /// The full worker string that will be sent to the pool: the pool user
    /// with the miner name appended. Null when no pool is configured.
    ///
    /// Returned so a client can show the effect of naming a miner rather
    /// than making the user infer the concatenation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_username: Option<String>,
    /// True when the saved settings differ from the ones this process is
    /// running with. Pool and name are read once at startup, so a change
    /// only takes effect after a restart and the client must say so.
    pub restart_required: bool,
}

/// Pool fields in a `PATCH /settings` body.
///
/// `url` and `user` are required when `pool` is present; omitting
/// `password` keeps the stored one, which is how a client updates the URL
/// without ever having seen the password.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct PoolSettingsPatch {
    pub url: String,
    pub user: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

/// Request body for `PATCH /settings`.
///
/// Fields left out are unchanged. Sending `name` as an empty (or
/// whitespace-only) string clears it, rather than needing a `null` that a
/// plain `Option` cannot distinguish from "absent".
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct SettingsPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<PoolSettingsPatch>,
}

/// Request body for `PATCH /boards/{name}/fan`.
///
/// `auto` selects the mode; the remaining fields refine it and any left
/// unset keep their current value.
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct FanControlRequest {
    /// `true` = automatic temperature-tracking curve, `false` = fixed
    /// manual duty cycle.
    pub auto: bool,
    /// Automatic-mode target ASIC temperature in Celsius.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_c: Option<f32>,
    /// Automatic-mode minimum duty cycle (0--100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_percent: Option<u8>,
    /// Manual-mode fixed duty cycle (0--100), used when `auto` is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent: Option<u8>,
}

/// Request body for `PATCH /boards/{name}/tuning`.
///
/// Only the fields present are applied; the board clamps each to a safe
/// range. Setting these is a manual override; an auto-tuner (future) will
/// drive the same knobs.
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct TuningRequest {
    /// Target ASIC hash clock, in MHz.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frequency_mhz: Option<f32>,
    /// Target ASIC core voltage, in millivolts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub core_voltage_mv: Option<u16>,
}

/// Request body for `PATCH /boards/{name}/autotune`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct AutoTuneRequest {
    /// Turn the auto-tuner on or off.
    pub enabled: bool,
    /// Profile to tune toward: `quiet`, `efficient`, `balanced`, or
    /// `max_hash`. Ignored when disabling, or when `target` is set;
    /// defaults to `balanced`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// A power/hashrate setpoint to converge to and hold, instead of a
    /// cap-based profile. When set, takes priority over `profile`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<TuneTarget>,
}

/// Job source telemetry.
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct SourceTelemetry {
    pub name: String,
    /// Connection URL (e.g. "stratum+tcp://pool:3333"), if applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Current share difficulty set by the source.
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_opt_f64_as_integer_when_whole"
    )]
    pub difficulty: Option<f64>,
}

/// Serialize an `Option<f64>` so that whole numbers appear without a
/// fractional part (e.g. `2328` instead of `2328.0`).
fn serialize_opt_f64_as_integer_when_whole<S: serde::Serializer>(
    value: &Option<f64>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match value {
        None => serializer.serialize_none(),
        Some(v) if v.fract() == 0.0 && v.is_finite() => serializer.serialize_i64(*v as i64),
        Some(v) => serializer.serialize_f64(*v),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_difficulty_serializes_as_integer() {
        let source = SourceTelemetry {
            difficulty: Some(2048.0),
            ..Default::default()
        };
        let json: serde_json::Value = serde_json::to_value(&source).unwrap();
        assert!(
            json["difficulty"].is_u64(),
            "expected integer, got {}",
            json["difficulty"]
        );
    }

    #[test]
    fn fractional_difficulty_serializes_as_float() {
        let source = SourceTelemetry {
            difficulty: Some(2048.5),
            ..Default::default()
        };
        let json: serde_json::Value = serde_json::to_value(&source).unwrap();
        assert!(
            json["difficulty"].is_f64(),
            "expected float, got {}",
            json["difficulty"]
        );
    }
}
