//! Per-chip-model operating envelopes.
//!
//! Frequency/voltage ranges and nominal hashrate for each BM13xx chip
//! variant we support. Boards report which chip they carry (and how many)
//! in `BoardTelemetry`; the autotuner and manual tuning API look up this
//! table by that model string to compute safe search bounds and the
//! UI's Min/Max/Default display.
//!
//! `nominal_gh_per_mhz` is deliberately approximate (a datasheet/nameplate
//! figure, not a measurement) and is used ONLY for display and as a rough
//! initial guess when a target-mode setpoint is first chosen. The
//! autotuner's actual convergence and hill-climb decisions always judge
//! against live measured hashrate/power on the real chip in front of it,
//! never this table -- see `crate::api::autotune`.

/// A chip model's safe operating envelope.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChipProfile {
    pub min_freq_mhz: f32,
    pub max_freq_mhz: f32,
    pub default_freq_mhz: f32,
    pub min_voltage_mv: u16,
    pub max_voltage_mv: u16,
    pub default_voltage_mv: u16,
    /// Ceiling the autotuner will raise voltage to on its own. Kept below
    /// `max_voltage_mv` so automatic tuning never sits at the sustained-
    /// damage line, mirroring the existing `AUTO_VOLT_CEIL_MV` pattern.
    pub auto_voltage_ceiling_mv: u16,
    /// Approximate hashrate per chip per MHz of core clock, in GH/s/MHz.
    pub nominal_gh_per_mhz: f32,
}

/// BM1370 (Bitaxe Gamma, Antminer S21 Pro/XP). Ranges are conservative
/// picks within the datasheet's documented safe bounds (490-750+ MHz,
/// 1.15-1.35V): the frequency ceiling here (650 MHz) is the bottom of the
/// "aggressive maximum" band -- pushing toward 750+ MHz needs cooling
/// beyond what these boards have. `bm13xx::thread` and `board::bitaxe`
/// define their own constants in terms of this one so there is a single
/// source of truth.
pub const BM1370: ChipProfile = ChipProfile {
    min_freq_mhz: 490.0,
    max_freq_mhz: 650.0,
    default_freq_mhz: 525.0,
    min_voltage_mv: 1150,
    max_voltage_mv: 1300,
    default_voltage_mv: 1150,
    auto_voltage_ceiling_mv: 1250,
    // ~1000-1200 GH/s nameplate at 490-525 MHz nominal; paired with this
    // table's own default_freq_mhz -> ~2.29 GH/s/MHz.
    nominal_gh_per_mhz: 2.29,
};

/// BM1362 (Antminer S19 J Pro). Ranges are conservative picks within the
/// datasheet's documented safe bounds (150-450 MHz, 0.28-0.42V): the
/// outer edges of those ranges need extra cooling (liquid/immersion) or
/// carry higher degradation risk, so the tuner is kept inside a margin
/// by default.
pub const BM1362: ChipProfile = ChipProfile {
    min_freq_mhz: 200.0,
    max_freq_mhz: 400.0,
    default_freq_mhz: 325.0,
    min_voltage_mv: 280,
    max_voltage_mv: 400,
    default_voltage_mv: 340,
    auto_voltage_ceiling_mv: 400,
    // ~320-335 GH/s nominal at 300-350 MHz -> ~1.0 GH/s/MHz.
    nominal_gh_per_mhz: 1.0,
};

/// Look up the operating envelope for a chip model string as reported in
/// `BoardTelemetry::chip_model` (e.g. "BM1370", "BM1362"). Returns `None`
/// for an unrecognized or missing model; callers should fall back to a
/// conservative default rather than failing.
pub fn profile_for(chip_model: &str) -> Option<ChipProfile> {
    match chip_model {
        "BM1370" => Some(BM1370),
        "BM1362" => Some(BM1362),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_models_resolve() {
        assert_eq!(profile_for("BM1370"), Some(BM1370));
        assert_eq!(profile_for("BM1362"), Some(BM1362));
    }

    #[test]
    fn unknown_model_is_none() {
        assert_eq!(profile_for("BM9999"), None);
        assert_eq!(profile_for(""), None);
    }

    #[test]
    fn ranges_are_internally_consistent() {
        for profile in [BM1370, BM1362] {
            assert!(profile.min_freq_mhz < profile.max_freq_mhz);
            assert!(
                (profile.min_freq_mhz..=profile.max_freq_mhz).contains(&profile.default_freq_mhz)
            );
            assert!(profile.min_voltage_mv < profile.max_voltage_mv);
            assert!(
                (profile.min_voltage_mv..=profile.max_voltage_mv)
                    .contains(&profile.default_voltage_mv)
            );
            assert!(profile.auto_voltage_ceiling_mv <= profile.max_voltage_mv);
            assert!(profile.auto_voltage_ceiling_mv >= profile.min_voltage_mv);
        }
    }
}
