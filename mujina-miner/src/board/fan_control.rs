//! Fan control policy shared by every board that has a controllable fan.
//!
//! Two modes: a fixed duty the operator sets, or an automatic curve that
//! holds the ASIC die temperature near a target. The automatic curve is PI
//! control on an EMA-filtered temperature -- the proportional term does the
//! fast response, and a small integral term erases the steady-state offset
//! a proportional-only curve leaves once the fan settles at an equilibrium
//! duty.
//!
//! This lives here rather than in one board because the policy is a
//! property of "an ASIC with a fan and a thermometer", not of any
//! particular board's I2C layout. Boards differ in how many fan headers
//! they drive and which controller chip does it; they do not differ in what
//! duty a given temperature deserves.

use crate::api::FanControlUpdate;

/// Default automatic-mode target ASIC die temperature, in Celsius.
pub const DEFAULT_FAN_TARGET_C: f32 = 60.0;
/// Default automatic-mode minimum fan duty cycle, in percent.
pub const DEFAULT_FAN_MIN_PERCENT: u8 = 25;
/// Accepted range for an automatic-mode target temperature. The ceiling
/// stays clear of the 80 C emergency threshold so auto control actually
/// engages before an emergency shutdown would.
const MIN_FAN_TARGET_C: f32 = 40.0;
const MAX_FAN_TARGET_C: f32 = 75.0;
/// Never let the automatic minimum drop to zero: a running ASIC always
/// needs some airflow, and the emergency path is a backstop, not a plan.
const MIN_FAN_FLOOR_PERCENT: u8 = 10;
/// Temperature span above the target over which the automatic curve ramps
/// the fan from `min_percent` up to 100%.
const FAN_RAMP_SPAN_C: f32 = 15.0;
/// EMA smoothing weight applied to each new temperature reading before it
/// reaches the fan curve, damping sensor noise so the fan doesn't hunt
/// tick-to-tick. Lower is smoother but slower to react.
const FAN_TEMP_EMA_ALPHA: f32 = 0.3;
/// Proportional gain, in fan-percent per degree Celsius of error above
/// `target_c`. A single reading `FAN_RAMP_SPAN_C` above target drives duty
/// to full on its own, so the integral term only has to correct
/// steady-state droop, not carry the whole response.
const FAN_KP: f32 = (100 - DEFAULT_FAN_MIN_PERCENT as i32) as f32 / FAN_RAMP_SPAN_C;
/// Integral gain, in fan-percent per (degree-Celsius x second) of
/// accumulated error. Small: it exists to erase the residual offset a
/// proportional-only curve leaves, not to drive the fast response.
const FAN_KI: f32 = 0.05;
/// Anti-windup clamp on the integral accumulator, in degree-Celsius x
/// seconds. Bounds the integral term's maximum contribution to
/// `FAN_KI * FAN_INTEGRAL_MAX` percentage points, and limits how long a
/// past excursion keeps pushing the fan after temperature recovers.
const FAN_INTEGRAL_MAX: f32 = 400.0;

/// Fan control policy for a board.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FanControl {
    /// Hold the ASIC die temperature near `target_c`, driving the fan
    /// between `min_percent` and 100% as temperature rises.
    Auto { target_c: f32, min_percent: u8 },
    /// Fixed duty cycle set by the operator; no temperature feedback.
    Manual { percent: u8 },
}

impl Default for FanControl {
    fn default() -> Self {
        FanControl::Auto {
            target_c: DEFAULT_FAN_TARGET_C,
            min_percent: DEFAULT_FAN_MIN_PERCENT,
        }
    }
}

impl FanControl {
    /// Apply a partial update from the API, carrying forward any value the
    /// caller left unset from the current policy (or the defaults when
    /// switching into a mode that has no prior value to inherit).
    pub fn with_update(self, update: FanControlUpdate) -> Self {
        // Current auto parameters, falling back to defaults if we are
        // switching over from manual mode.
        let (cur_target, cur_min) = match self {
            FanControl::Auto {
                target_c,
                min_percent,
            } => (target_c, min_percent),
            FanControl::Manual { .. } => (DEFAULT_FAN_TARGET_C, DEFAULT_FAN_MIN_PERCENT),
        };
        if update.auto {
            FanControl::Auto {
                target_c: update
                    .target_c
                    .unwrap_or(cur_target)
                    .clamp(MIN_FAN_TARGET_C, MAX_FAN_TARGET_C),
                min_percent: update
                    .min_percent
                    .unwrap_or(cur_min)
                    .clamp(MIN_FAN_FLOOR_PERCENT, 100),
            }
        } else {
            let cur_manual = match self {
                FanControl::Manual { percent } => percent,
                // No prior manual value: fall back to full speed, the safe
                // default, rather than something arbitrary.
                FanControl::Auto { .. } => 100,
            };
            FanControl::Manual {
                percent: update.percent.unwrap_or(cur_manual),
            }
        }
    }

    /// Automatic-mode target temperature, or `None` in manual mode.
    pub fn target_c(self) -> Option<f32> {
        match self {
            FanControl::Auto { target_c, .. } => Some(target_c),
            FanControl::Manual { .. } => None,
        }
    }

    /// Automatic-mode minimum duty, or `None` in manual mode.
    pub fn min_percent(self) -> Option<u8> {
        match self {
            FanControl::Auto { min_percent, .. } => Some(min_percent),
            FanControl::Manual { .. } => None,
        }
    }

    pub fn is_auto(self) -> bool {
        matches!(self, FanControl::Auto { .. })
    }
}

/// Smooth a raw temperature reading with an exponential moving average,
/// seeding the filter with the first sample rather than an arbitrary
/// starting guess.
fn ema_filter(prev: Option<f32>, sample: f32, alpha: f32) -> f32 {
    match prev {
        Some(p) => p + alpha * (sample - p),
        None => sample,
    }
}

/// Advance the fan PI controller's integral term by one tick. Accumulates
/// `(temp_c - target_c) * dt_s`, so it unwinds again once the temperature
/// drops back under target rather than latching at its peak forever.
/// Clamped to `[0, FAN_INTEGRAL_MAX]`: it never goes negative (the
/// proportional floor already owns below-target duty) and never grows large
/// enough to keep the fan pinned long after a past excursion.
fn integrate_fan_error(integral: f32, temp_c: f32, target_c: f32, dt_s: f32) -> f32 {
    let error = temp_c - target_c;
    (integral + error * dt_s).clamp(0.0, FAN_INTEGRAL_MAX)
}

/// Automatic fan curve: PI control on the (EMA-filtered) ASIC die
/// temperature. `min_percent` and 100% remain hard floor and ceiling
/// regardless of how large the integral term gets.
fn auto_fan_duty(temp_c: f32, target_c: f32, min_percent: u8, integral: f32) -> u8 {
    let min_percent = (min_percent.min(100)) as f32;
    let error_above_target = (temp_c - target_c).max(0.0);
    let duty = min_percent + FAN_KP * error_above_target + FAN_KI * integral;
    duty.clamp(min_percent, 100.0).round() as u8
}

/// The policy plus the filter and integrator state it needs between ticks.
///
/// A board owns one of these and asks it for a duty each monitor cycle;
/// how that duty reaches the silicon is the board's business.
#[derive(Debug, Default)]
pub struct FanController {
    control: FanControl,
    /// EMA-filtered ASIC die temperature fed to the PI controller. `None`
    /// until the first usable reading arrives.
    filtered_temp: Option<f32>,
    /// Accumulated integral error, in degree-Celsius x seconds. Reset
    /// whenever control is not in automatic mode.
    integral: f32,
}

impl FanController {
    /// Start in a specific mode, for boards whose safe resting state is not
    /// the shared default (a board that parks its rail, say, and should sit
    /// at a known loud duty until something asks otherwise).
    pub fn new(control: FanControl) -> Self {
        Self {
            control,
            ..Default::default()
        }
    }

    pub fn control(&self) -> FanControl {
        self.control
    }

    /// Apply a partial policy update from the API.
    pub fn update(&mut self, update: FanControlUpdate) {
        self.control = self.control.with_update(update);
    }

    /// The duty to command this cycle, given the ASIC temperature reading
    /// (`None` when the sensor could not be read) and the time since the
    /// last call.
    ///
    /// Returns `None` in automatic mode with no reading: the fan is left
    /// where it is rather than guessed at, and the filter and integrator
    /// hold their values until a reading returns. Sustained sensor failure
    /// is the board's emergency path to handle, not this one's.
    pub fn tick(&mut self, asic_temp_c: Option<f32>, dt_s: f32) -> Option<u8> {
        match self.control {
            FanControl::Manual { percent } => {
                // Reset the PI state so a later switch back to automatic
                // starts clean rather than resuming a stale integral.
                self.filtered_temp = None;
                self.integral = 0.0;
                Some(percent)
            }
            FanControl::Auto {
                target_c,
                min_percent,
            } => asic_temp_c.map(|t| {
                let filtered = ema_filter(self.filtered_temp, t, FAN_TEMP_EMA_ALPHA);
                self.filtered_temp = Some(filtered);
                self.integral = integrate_fan_error(self.integral, filtered, target_c, dt_s);
                auto_fan_duty(filtered, target_c, min_percent, self.integral)
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // With a zero integral the PI curve collapses to a proportional-only
    // curve, so these first four cases (integral = 0.0) pin down that
    // behavior on its own.

    #[test]
    fn holds_minimum_at_or_below_target() {
        let min = DEFAULT_FAN_MIN_PERCENT;
        let target = DEFAULT_FAN_TARGET_C;
        assert_eq!(auto_fan_duty(target - 10.0, target, min, 0.0), min);
        assert_eq!(auto_fan_duty(target, target, min, 0.0), min);
    }

    #[test]
    fn reaches_full_at_top_of_ramp() {
        let min = DEFAULT_FAN_MIN_PERCENT;
        let target = DEFAULT_FAN_TARGET_C;
        assert_eq!(
            auto_fan_duty(target + FAN_RAMP_SPAN_C, target, min, 0.0),
            100
        );
        // Beyond the ramp span the fan stays clamped at 100%.
        assert_eq!(auto_fan_duty(target + 50.0, target, min, 0.0), 100);
    }

    #[test]
    fn ramps_linearly_between_target_and_full() {
        let min = 25;
        let target = 60.0;
        // Halfway up the 15 C span: 25% + 0.5 * (100 - 25) = 62.5 -> 63.
        assert_eq!(
            auto_fan_duty(target + FAN_RAMP_SPAN_C / 2.0, target, min, 0.0),
            63
        );
    }

    #[test]
    fn clamps_minimum_above_one_hundred() {
        // A nonsensical minimum never drives the fan past 100%.
        assert_eq!(auto_fan_duty(200.0, 60.0, 250, 0.0), 100);
    }

    #[test]
    fn integral_term_lifts_duty_above_proportional_floor() {
        // Same reading, but a wound-up integral from past error pushes
        // duty above what the proportional term alone would give.
        let target = 60.0;
        let min = 25;
        let proportional_only = auto_fan_duty(target, target, min, 0.0);
        let with_integral = auto_fan_duty(target, target, min, 100.0);
        assert!(with_integral > proportional_only);
        assert_eq!(with_integral, min + (FAN_KI * 100.0).round() as u8);
    }

    #[test]
    fn integral_never_pushes_past_ceiling() {
        assert_eq!(auto_fan_duty(75.0, 60.0, 25, FAN_INTEGRAL_MAX * 10.0), 100);
    }

    #[test]
    fn integral_accumulates_while_above_target() {
        assert_eq!(integrate_fan_error(0.0, 65.0, 60.0, 2.0), 10.0);
    }

    #[test]
    fn integral_unwinds_below_target_and_floors_at_zero() {
        // A prior wind-up of 10.0 fully unwinds (and stays non-negative)
        // once temperature drops 5 C under target for 2s.
        assert_eq!(integrate_fan_error(10.0, 55.0, 60.0, 2.0), 0.0);
    }

    #[test]
    fn integral_saturates_at_anti_windup_clamp() {
        assert_eq!(
            integrate_fan_error(FAN_INTEGRAL_MAX - 1.0, 100.0, 60.0, 2.0),
            FAN_INTEGRAL_MAX
        );
    }

    #[test]
    fn ema_filter_seeds_from_first_sample() {
        assert_eq!(ema_filter(None, 42.0, 0.3), 42.0);
    }

    #[test]
    fn ema_filter_smooths_toward_new_sample() {
        // 30% weight on the new sample: 60 -> 60 + 0.3*(70-60) = 63.
        let filtered = ema_filter(Some(60.0), 70.0, 0.3);
        assert_eq!(filtered, 63.0);
        // Smoothed, so it moves toward but does not jump to the raw
        // reading in one tick.
        assert!(filtered > 60.0 && filtered < 70.0);
    }

    #[test]
    fn update_changes_only_specified_auto_fields() {
        let start = FanControl::Auto {
            target_c: 60.0,
            min_percent: 25,
        };
        // Change only the target; minimum is carried forward.
        let updated = start.with_update(FanControlUpdate {
            auto: true,
            target_c: Some(55.0),
            min_percent: None,
            percent: None,
        });
        assert_eq!(
            updated,
            FanControl::Auto {
                target_c: 55.0,
                min_percent: 25
            }
        );
    }

    #[test]
    fn update_clamps_unsafe_auto_params() {
        // A target above the emergency threshold and a zero floor are both
        // pulled back into safe bounds so auto control still cools the chip.
        let updated = FanControl::default().with_update(FanControlUpdate {
            auto: true,
            target_c: Some(119.0),
            min_percent: Some(0),
            percent: None,
        });
        assert_eq!(
            updated,
            FanControl::Auto {
                target_c: MAX_FAN_TARGET_C,
                min_percent: MIN_FAN_FLOOR_PERCENT,
            }
        );
    }

    #[test]
    fn update_switches_auto_to_manual() {
        let start = FanControl::default();
        let updated = start.with_update(FanControlUpdate {
            auto: false,
            target_c: None,
            min_percent: None,
            percent: Some(70),
        });
        assert_eq!(updated, FanControl::Manual { percent: 70 });
    }

    #[test]
    fn update_switching_to_auto_from_manual_uses_defaults() {
        let start = FanControl::Manual { percent: 80 };
        let updated = start.with_update(FanControlUpdate {
            auto: true,
            target_c: None,
            min_percent: None,
            percent: None,
        });
        assert_eq!(
            updated,
            FanControl::Auto {
                target_c: DEFAULT_FAN_TARGET_C,
                min_percent: DEFAULT_FAN_MIN_PERCENT
            }
        );
    }

    #[test]
    fn manual_mode_commands_the_operator_duty_and_clears_pi_state() {
        let mut c = FanController::new(FanControl::Auto {
            target_c: 60.0,
            min_percent: 25,
        });
        // Wind the integrator up while hot.
        c.tick(Some(80.0), 2.0);
        assert!(c.integral > 0.0);

        c.update(FanControlUpdate {
            auto: false,
            percent: Some(42),
            ..Default::default()
        });
        assert_eq!(c.tick(Some(80.0), 2.0), Some(42));
        assert_eq!(c.integral, 0.0);
        assert_eq!(c.filtered_temp, None);
    }

    #[test]
    fn auto_mode_without_a_reading_leaves_the_fan_alone() {
        let mut c = FanController::default();
        assert_eq!(c.tick(None, 2.0), None);
        // The filter must not have been seeded with a fabricated sample.
        assert_eq!(c.filtered_temp, None);
    }

    #[test]
    fn auto_mode_raises_duty_as_temperature_climbs() {
        let mut c = FanController::default();
        let cool = c.tick(Some(50.0), 2.0).unwrap();
        // Repeated hot readings pull the EMA up and wind the integrator.
        let mut hot = cool;
        for _ in 0..20 {
            hot = c.tick(Some(75.0), 2.0).unwrap();
        }
        assert!(hot > cool, "duty {hot} should exceed {cool}");
    }
}
