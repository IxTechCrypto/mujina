//! Power & cooling presets mirroring Avalon Nano 3S semantics.

/// 15 A stock fuse hard cap (W) and default.
pub const STOCK_DEFAULT_W: f32 = 144.0;
pub const STOCK_MAX_W: f32 = 180.0;
pub const UPGRADED_DEFAULT_W: f32 = 192.0;
pub const UPGRADED_MAX_W: f32 = 240.0;
pub const AIO_DEFAULT_W: f32 = 240.0;
pub const AIO_MAX_W: f32 = 240.0;
/// Stock-fuse dynamic cap: watts per input volt (15 A).
pub const STOCK_W_PER_VIN: f32 = 15.0;
/// M3 pump/fan (+12V/GND only) is not metered by the TPS53647.
pub const PUMP_HEADROOM_MIN_W: f32 = 10.0;
pub const PUMP_HEADROOM_MAX_W: f32 = 15.0;
pub const MIN_CEILING_W: f32 = 60.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preset {
    /// #1A Stock air, 15 A fuse.
    StockAir,
    /// #1B Upgraded air, >=20 A fuse.
    UpgradedAir,
    /// #2 AiO liquid, >=20 A fuse.
    AiO,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PowerCoolingConfig {
    pub preset: Preset,
    pub fuse_amps: u8,
    pub fuse_upgrade_confirmed: bool,
    pub power_ceiling_w: f32,
    /// Estimated un-metered M3 pump/fan load (W).
    pub pump_w: f32,
}

impl PowerCoolingConfig {
    pub fn for_preset(preset: Preset, fuse_amps: u8, confirmed: bool) -> Self {
        let (ceiling, pump) = match preset {
            Preset::StockAir => (STOCK_DEFAULT_W, 0.0),
            Preset::UpgradedAir => (UPGRADED_DEFAULT_W, 0.0),
            Preset::AiO => (AIO_DEFAULT_W, PUMP_HEADROOM_MAX_W),
        };
        Self { preset, fuse_amps, fuse_upgrade_confirmed: confirmed, power_ceiling_w: ceiling, pump_w: pump }
    }

    pub fn default_stock() -> Self {
        Self::for_preset(Preset::StockAir, 15, false)
    }

    pub fn preset_default_w(&self) -> f32 {
        match self.preset {
            Preset::StockAir => STOCK_DEFAULT_W,
            Preset::UpgradedAir => UPGRADED_DEFAULT_W,
            Preset::AiO => AIO_DEFAULT_W,
        }
    }

    /// True when the >180 W tiers are legitimately unlocked.
    pub fn unlocked(&self) -> bool {
        self.preset != Preset::StockAir && self.fuse_amps >= 20 && self.fuse_upgrade_confirmed
    }

    /// Max configurable ceiling; falls back to 180 W unless unlocked.
    pub fn max_ceiling_w(&self) -> f32 {
        if !self.unlocked() {
            return STOCK_MAX_W;
        }
        match self.preset {
            Preset::UpgradedAir => UPGRADED_MAX_W,
            _ => AIO_MAX_W,
        }
    }

    /// Ceiling actually enforced: user value clamped to [60, max] and to the wall budget.
    pub fn effective_ceiling_w(&self, vin_v: f32) -> f32 {
        // Fail closed on a bad input-voltage reading.
        if !vin_v.is_finite() || vin_v <= 0.0 {
            return MIN_CEILING_W;
        }
        let mut c = self.power_ceiling_w;
        if !(c >= MIN_CEILING_W) {
            c = MIN_CEILING_W;
        }
        let max = self.max_ceiling_w();
        if c > max {
            c = max;
        }
        // Wall budget: 15*Vin on stock fuse; fuse*Vin minus un-metered pump otherwise.
        let dyn_cap = if self.unlocked() {
            self.fuse_amps as f32 * vin_v - self.pump_w
        } else {
            STOCK_W_PER_VIN * vin_v
        };
        if dyn_cap < c {
            c = dyn_cap;
        }
        if c < MIN_CEILING_W {
            c = MIN_CEILING_W;
        }
        c
    }
    /// Extra headroom (W) the wall budget (`fuse*vin`) leaves beyond board input
    /// power and the un-metered pump. Negative means overdrawn.
    pub fn pump_headroom_w(&self, pin_w: f32, vin_v: f32) -> f32 {
        self.fuse_amps as f32 * vin_v - pin_w - self.pump_w
    }

    /// True while the wall budget leaves room for the pump (at least 10 W, or
    /// the configured pump estimate if larger).
    pub fn pump_guard_ok(&self, pin_w: f32, vin_v: f32) -> bool {
        if self.pump_w <= 0.0 {
            return true;
        }
        let need = if self.pump_w > PUMP_HEADROOM_MIN_W { self.pump_w } else { PUMP_HEADROOM_MIN_W };
        self.fuse_amps as f32 * vin_v - pin_w >= need
    }
}
impl Default for PowerCoolingConfig {
    fn default() -> Self {
        Self::default_stock()
    }
}


