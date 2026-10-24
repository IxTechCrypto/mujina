//! `GET /api/v0/system`: the capability document the unified dashboard reads
//! (see docs/unified-dashboard-spec.md). The builder is pure so it tests on
//! the host without the `nano3s` feature.

use serde_json::{Value, json};

/// Per-board capabilities.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BoardCaps {
    pub chip_grid: &'static str,
    pub temps: Vec<&'static str>,
    pub fan: &'static str,
    pub fan_curve: bool,
    pub tuner: Option<String>,
    pub manual_tuning: Option<String>,
}

impl BoardCaps {
    /// Avalon Nano 3S: per-chip grid, asic + outlet NTC, harness-driven fan
    /// with a curve, owner tuner and manual tuning.
    pub(crate) fn nano3s(name: &str) -> Self {
        Self {
            chip_grid: "per_chip",
            temps: vec!["asic", "outlet"],
            fan: "harness",
            fan_curve: true,
            tuner: Some(format!("/api/v0/boards/{name}/tuner")),
            manual_tuning: Some(format!("/api/v0/boards/{name}/tuning")),
        }
    }

    /// Generic board with no extra controls wired in this build.
    pub(crate) fn generic() -> Self {
        Self { chip_grid: "none", temps: vec!["asic"], fan: "none", fan_curve: false, tuner: None, manual_tuning: None }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SystemBoard {
    pub name: String,
    pub model: Option<String>,
    pub chips: u32,
    pub caps: BoardCaps,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SystemInfo {
    pub mode: &'static str,
    pub platform: String,
    pub version: &'static str,
    pub build: Option<String>,
    pub update: &'static str,
    pub device_model: Option<String>,
    pub can_reboot: bool,
    pub can_restart_miner: bool,
    pub pool_apply: &'static str,
    pub shared_rail: bool,
    pub ceiling_w: Option<f64>,
    pub psu_contract_w: Option<f64>,
    pub boards: Vec<SystemBoard>,
}

/// Git short sha baked in by build.rs, if the build had git.
pub(crate) fn build_sha() -> Option<String> {
    option_env!("MUJINA_GIT_SHA").filter(|s| !s.is_empty()).map(str::to_owned)
}

/// Continuous power ceiling shown to the user: only known when a PD contract
/// was detected (`None` otherwise). `owner_ceiling` is the owner tuner's
/// formula applied to the detected values.
pub(crate) fn ceiling_if_known(psu_contract_w: Option<f64>, owner_ceiling: f32) -> Option<f64> {
    psu_contract_w.filter(|w| w.is_finite()).map(|_| owner_ceiling as f64)
}

pub(crate) fn build_system(i: &SystemInfo) -> Value {
    json!({
        "api": 1,
        "mode": i.mode,
        "platform": i.platform,
        "firmware": {
            "name": "mujina-minerd",
            "version": i.version,
            "build": i.build,
            "update": i.update,
        },
        "device": {
            "model": i.device_model,
            "can_reboot": i.can_reboot,
            "can_restart_miner": i.can_restart_miner,
        },
        "pool": { "endpoint": "/api/v0/pool", "apply": i.pool_apply },
        "power": {
            "shared_rail": i.shared_rail,
            "ceiling_w": i.ceiling_w,
            "psu_contract_w": i.psu_contract_w,
        },
        "boards": i.boards.iter().map(|b| json!({
            "name": b.name,
            "model": b.model,
            "chips": b.chips,
            "caps": {
                "chip_grid": b.caps.chip_grid,
                "temps": b.caps.temps,
                "fan": b.caps.fan,
                "fan_curve": b.caps.fan_curve,
                "tuner": b.caps.tuner,
                "manual_tuning": b.caps.manual_tuning,
            },
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nano() -> SystemInfo {
        SystemInfo {
            mode: "embedded",
            platform: "nano3s-k230".into(),
            version: "0.1.0",
            build: Some("abc1234".into()),
            update: "bundle",
            device_model: Some("Avalon Nano 3S".into()),
            can_reboot: true,
            can_restart_miner: false,
            pool_apply: "reboot",
            shared_rail: true,
            ceiling_w: Some(112.0),
            psu_contract_w: Some(140.0),
            boards: vec![SystemBoard {
                name: "nano3s-0".into(),
                model: Some("Avalon Nano 3S".into()),
                chips: 12,
                caps: BoardCaps::nano3s("nano3s-0"),
            }],
        }
    }

    #[test]
    fn embedded_document_matches_the_spec() {
        let v = build_system(&nano());
        println!("{}", serde_json::to_string(&v).unwrap());
        assert_eq!(v["api"], 1);
        assert_eq!(v["mode"], "embedded");
        assert_eq!(v["platform"], "nano3s-k230");
        assert_eq!(v["firmware"]["update"], "bundle");
        assert_eq!(v["firmware"]["build"], "abc1234");
        assert_eq!(v["device"]["model"], "Avalon Nano 3S");
        assert_eq!(v["device"]["can_reboot"], true);
        assert_eq!(v["device"]["can_restart_miner"], false);
        assert_eq!(v["pool"], json!({ "endpoint": "/api/v0/pool", "apply": "reboot" }));
        assert_eq!(v["power"], json!({ "shared_rail": true, "ceiling_w": 112.0, "psu_contract_w": 140.0 }));
        let b = &v["boards"][0];
        assert_eq!(b["name"], "nano3s-0");
        assert_eq!(b["caps"]["chip_grid"], "per_chip");
        assert_eq!(b["caps"]["temps"], json!(["asic", "outlet"]));
        assert_eq!(b["caps"]["fan"], "harness");
        assert_eq!(b["caps"]["fan_curve"], true);
        assert_eq!(b["caps"]["tuner"], "/api/v0/boards/nano3s-0/tuner");
        assert_eq!(b["caps"]["manual_tuning"], "/api/v0/boards/nano3s-0/tuning");
    }

    #[test]
    fn unknown_values_serialize_as_null() {
        let mut i = nano();
        i.build = None;
        i.ceiling_w = None;
        i.psu_contract_w = None;
        i.boards[0].caps = BoardCaps::generic();
        let v = build_system(&i);
        assert!(v["firmware"]["build"].is_null());
        assert!(v["power"]["ceiling_w"].is_null());
        assert!(v["power"]["psu_contract_w"].is_null());
        assert!(v["boards"][0]["caps"]["tuner"].is_null());
        assert_eq!(v["boards"][0]["caps"]["chip_grid"], "none");
    }

    #[test]
    fn ceiling_needs_a_detected_contract() {
        assert_eq!(ceiling_if_known(Some(140.0), 112.0), Some(112.0));
        assert_eq!(ceiling_if_known(None, 134.0), None);
        assert_eq!(ceiling_if_known(Some(f64::INFINITY), 134.0), None);
    }
}
