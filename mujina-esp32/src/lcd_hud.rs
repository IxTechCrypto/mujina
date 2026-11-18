//! 6-screen 320x170 ST7789 HUD state machine driven by BTN1/BTN2.

pub const LONG_PRESS_MS: u32 = 1500;
pub const DUAL_PRESS_MS: u32 = 5000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    MainHud,
    ChipDieGrid,
    TunerPsu,
    PoolShares,
    ClockBtc,
    SystemQr,
}

impl Screen {
    pub const ALL: [Screen; 6] = [
        Screen::MainHud,
        Screen::ChipDieGrid,
        Screen::TunerPsu,
        Screen::PoolShares,
        Screen::ClockBtc,
        Screen::SystemQr,
    ];

    fn index(self) -> usize {
        Self::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }
    pub fn next(self) -> Screen {
        Self::ALL[(self.index() + 1) % 6]
    }
    pub fn prev(self) -> Screen {
        Self::ALL[(self.index() + 5) % 6]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Btn1,
    Btn2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Press,
    Release,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HudAction {
    ScreenChanged(Screen),
    BacklightOn,
    BacklightOff,
    Retune,
    SafePowerDown,
}

#[derive(Clone, Copy, Debug)]
pub struct Hud {
    pub screen: Screen,
    pub backlight_on: bool,
    down: [Option<u32>; 2],
    /// A long/dual action already fired for the current gesture.
    fired: bool,
    /// Both buttons were held at some point in this gesture.
    dual: bool,
}

fn idx(b: Button) -> usize {
    match b {
        Button::Btn1 => 0,
        Button::Btn2 => 1,
    }
}

impl Default for Hud {
    fn default() -> Self {
        Self::new()
    }
}

impl Hud {
    pub fn new() -> Self {
        Self { screen: Screen::MainHud, backlight_on: true, down: [None, None], fired: false, dual: false }
    }

    pub fn on_button(&mut self, b: Button, edge: Edge, now_ms: u32) -> Option<HudAction> {
        let i = idx(b);
        match edge {
            Edge::Press => {
                if self.down[i].is_none() {
                    self.down[i] = Some(now_ms);
                }
                if self.down[0].is_some() && self.down[1].is_some() {
                    self.dual = true;
                }
                None
            }
            Edge::Release => {
                let was_down = self.down[i].take().is_some();
                let result = if was_down && !self.fired && !self.dual {
                    self.short_press(b)
                } else {
                    None
                };
                if self.down[0].is_none() && self.down[1].is_none() {
                    self.fired = false;
                    self.dual = false;
                }
                result
            }
        }
    }

    fn short_press(&mut self, b: Button) -> Option<HudAction> {
        if !self.backlight_on {
            self.backlight_on = true;
            return Some(HudAction::BacklightOn); // wake only, no navigation
        }
        self.screen = if b == Button::Btn1 { self.screen.next() } else { self.screen.prev() };
        Some(HudAction::ScreenChanged(self.screen))
    }

    /// Call periodically (e.g. every 50 ms) to detect long presses.
    pub fn tick(&mut self, now_ms: u32) -> Option<HudAction> {
        if self.fired {
            return None;
        }
        if let (Some(a), Some(b)) = (self.down[0], self.down[1]) {
            let since = if a > b { a } else { b };
            if now_ms.wrapping_sub(since) >= DUAL_PRESS_MS {
                self.fired = true;
                return Some(HudAction::SafePowerDown);
            }
            return None;
        }
        if self.dual {
            return None;
        }
        for (b, i) in [(Button::Btn1, 0usize), (Button::Btn2, 1usize)] {
            if let Some(t) = self.down[i] {
                if now_ms.wrapping_sub(t) >= LONG_PRESS_MS {
                    self.fired = true;
                    return Some(match b {
                        Button::Btn1 => {
                            self.backlight_on = !self.backlight_on;
                            if self.backlight_on { HudAction::BacklightOn } else { HudAction::BacklightOff }
                        }
                        Button::Btn2 => HudAction::Retune,
                    });
                }
            }
        }
        None
    }
}
