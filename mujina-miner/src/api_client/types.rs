//! API data transfer objects.
//!
//! These types define the API contract shared between the server and
//! clients (CLI, TUI). See `docs/api.md` (at the repository root)
//! for the full API contract documentation, including conventions
//! for null values and units.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::types::Temperature;

/// Full miner telemetry snapshot.
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct MinerTelemetry {
    pub uptime_secs: u64,
    /// Aggregate hashrate in hashes per second.
    pub hashrate: u64,
    pub shares_submitted: u64,
    pub paused: bool,
    pub boards: Vec<BoardTelemetry>,
    pub sources: Vec<SourceTelemetry>,
}

/// Board telemetry snapshot.
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct BoardTelemetry {
    /// URL-friendly identifier (e.g. "bitaxe-e2f56f9b").
    pub name: String,
    pub model: String,
    pub serial: Option<String>,
    /// ASIC hash clock in MHz, or null if the board does not report one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_mhz: Option<f32>,
    pub fans: Vec<Fan>,
    pub temperatures: Vec<TemperatureSensor>,
    pub powers: Vec<PowerMeasurement>,
    pub threads: Vec<ThreadTelemetry>,
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
    /// `max_hash`. Ignored when disabling; defaults to `balanced`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
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
