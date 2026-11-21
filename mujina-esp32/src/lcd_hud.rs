//! 6-screen 320x170 rectangular ST7789 HUD (LilyGO T-Display-S3) driven by BTN1/BTN2.
//!
//! Unlike the Avalon Nano 3S's 240x240 circular display, the NerdQX T-Display-S3
//! is a 320x170 landscape rectangle (`gap_y = 35`, `swap_xy = true`) with full
//! usable corners, a 22 px header strip, 140 px page body, and an 8 px footer.

use crate::hw_pins::{LCD_HEIGHT, LCD_WIDTH};

pub const HEADER_HEIGHT: u16 = 22;
pub const FOOTER_HEIGHT: u16 = 8;
pub const PAGE_Y: u16 = HEADER_HEIGHT;
pub const PAGE_HEIGHT: u16 = LCD_HEIGHT - HEADER_HEIGHT - FOOTER_HEIGHT; // 140 px

pub const LONG_PRESS_MS: u32 = 1500;
pub const DUAL_PRESS_MS: u32 = 5000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayShape {
    Rectangular,
    Circular,
}

/// Active LCD HUD visual theme (Nano 3S Direction A 3-theme parity).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum UiTheme {
    #[default]
    Cyberpunk,
    Minimal,
    Parasite,
}

impl UiTheme {
    /// Parse a theme name (case-insensitive). Defaults to [`UiTheme::Cyberpunk`].
    pub fn parse(s: &str) -> Self {
        let trimmed = s.trim();
        if trimmed.eq_ignore_ascii_case("minimal") || trimmed.eq_ignore_ascii_case("apple") {
            Self::Minimal
        } else if trimmed.eq_ignore_ascii_case("parasite") {
            Self::Parasite
        } else {
            Self::Cyberpunk
        }
    }

    /// Canonical lowercase identifier for API/NVS persistence.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cyberpunk => "cyberpunk",
            Self::Minimal => "minimal",
            Self::Parasite => "parasite",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayGeometry {
    pub width: u16,
    pub height: u16,
    pub shape: DisplayShape,
    pub header_h: u16,
    pub footer_h: u16,
    pub gap_y: u16,
    pub swap_xy: bool,
}

impl DisplayGeometry {
    /// LilyGO T-Display-S3 ST7789 8-bit i80 panel on NerdQX.
    pub const NERDQX_TDISPLAY_S3: Self = Self {
        width: LCD_WIDTH,
        height: LCD_HEIGHT,
        shape: DisplayShape::Rectangular,
        header_h: HEADER_HEIGHT,
        footer_h: FOOTER_HEIGHT,
        gap_y: 35,
        swap_xy: true,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    pub const fn new(x: u16, y: u16, w: u16, h: u16) -> Self {
        Self { x, y, w, h }
    }

    pub const fn right(self) -> u16 {
        self.x + self.w
    }

    pub const fn bottom(self) -> u16 {
        self.y + self.h
    }

    pub const fn fits_in(self, bounds: Rect) -> bool {
        self.w > 0
            && self.h > 0
            && self.x >= bounds.x
            && self.y >= bounds.y
            && self.right() <= bounds.right()
            && self.bottom() <= bounds.bottom()
    }

    pub const fn overlaps(self, other: Rect) -> bool {
        self.x < other.right()
            && other.x < self.right()
            && self.y < other.bottom()
            && other.y < self.bottom()
    }
}

pub const HEADER_RECT: Rect = Rect::new(0, 0, LCD_WIDTH, HEADER_HEIGHT);
pub const PAGE_BOUNDS: Rect = Rect::new(0, PAGE_Y, LCD_WIDTH, PAGE_HEIGHT);
pub const FOOTER_RECT: Rect = Rect::new(0, PAGE_Y + PAGE_HEIGHT, LCD_WIDTH, FOOTER_HEIGHT);
pub const SCREEN_BOUNDS: Rect = Rect::new(0, 0, LCD_WIDTH, LCD_HEIGHT);

/// Bottom-left cutoff circle geometry inspired by Nano 3S concentric dials.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CutoffDial {
    pub cx: i16,
    pub cy: i16,
    pub r_outer: u16,
    pub r_mid: u16,
    pub r_inner: u16,
    pub ring_width: u16,
}

impl CutoffDial {
    pub const MAIN_HUD: Self = Self {
        cx: 58,
        cy: 134,
        r_outer: 100,
        r_mid: 84,
        r_inner: 73,
        ring_width: 9,
    };

    pub const fn right_edge(self) -> i16 {
        self.cx + (self.r_outer as i16)
    }
}

/// Front-loading boot splash layout: Mujina raccoon mark + "MUJINA" + progress bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BootSplashLayout {
    pub raccoon_box: Rect,
    pub title_box: Rect,
    pub progress_bar: Rect,
}

pub const BOOT_SPLASH_LAYOUT: BootSplashLayout = BootSplashLayout {
    raccoon_box: Rect::new(112, 13, 96, 78),
    title_box: Rect::new(40, 96, 240, 24),
    progress_bar: Rect::new(70, 138, 180, 5),
};

/// Screen-specific rectangular layout primitives inside `320x170`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScreenLayout {
    MainHud {
        dial: CutoffDial,
        preset_badge: Rect,
        ip_badge: Rect,
        /// Callout cards connected via 45-deg PCB traces: Chip Temp, VR Temp, Efficiency, Fan.
        callout_cards: [Rect; 4],
    },
    ChipDieGrid {
        cards: [Rect; 4],
        load_bars: [Rect; 4],
    },
    TunerPsu {
        psu_card: Rect,
        power_bar: Rect,
        tuner_card: Rect,
    },
    PoolShares {
        summary_bar: Rect,
        rows: [Rect; 2],
    },
    ClockBtc {
        clock_box: Rect,
        sec_bar: Rect,
        left_card: Rect,
        right_card: Rect,
    },
    SystemQr {
        qr_box: Rect,
        info_card: Rect,
    },
}

impl Screen {
    /// Returns the widescreen 320x170 rectangular layout for this screen.
    pub const fn layout(self) -> ScreenLayout {
        match self {
            Screen::MainHud => ScreenLayout::MainHud {
                dial: CutoffDial::MAIN_HUD,
                preset_badge: Rect::new(178, 4, 46, 15),
                ip_badge: Rect::new(228, 4, 88, 15),
                callout_cards: [
                    Rect::new(186, 26, 128, 29),
                    Rect::new(186, 59, 128, 29),
                    Rect::new(186, 92, 128, 29),
                    Rect::new(186, 125, 128, 29),
                ],
            },
            Screen::ChipDieGrid => ScreenLayout::ChipDieGrid {
                // 2x2 cards so each die gets a large temperature readout.
                cards: [
                    Rect::new(4, PAGE_Y + 4, 154, 65),
                    Rect::new(162, PAGE_Y + 4, 154, 65),
                    Rect::new(4, PAGE_Y + 74, 154, 65),
                    Rect::new(162, PAGE_Y + 74, 154, 65),
                ],
                load_bars: [
                    Rect::new(12, PAGE_Y + 60, 138, 5),
                    Rect::new(170, PAGE_Y + 60, 138, 5),
                    Rect::new(12, PAGE_Y + 130, 138, 5),
                    Rect::new(170, PAGE_Y + 130, 138, 5),
                ],
            },
            Screen::TunerPsu => ScreenLayout::TunerPsu {
                psu_card: Rect::new(4, PAGE_Y + 4, 154, 134),
                power_bar: Rect::new(12, PAGE_Y + 104, 138, 8),
                tuner_card: Rect::new(162, PAGE_Y + 4, 154, 134),
            },
            Screen::PoolShares => ScreenLayout::PoolShares {
                summary_bar: Rect::new(4, PAGE_Y + 4, 312, 56),
                rows: [Rect::new(4, PAGE_Y + 64, 312, 36), Rect::new(4, PAGE_Y + 103, 312, 36)],
            },
            Screen::ClockBtc => ScreenLayout::ClockBtc {
                clock_box: Rect::new(4, PAGE_Y + 4, 312, 62),
                sec_bar: Rect::new(4, PAGE_Y + 68, 312, 3),
                left_card: Rect::new(4, PAGE_Y + 75, 154, 64),
                right_card: Rect::new(162, PAGE_Y + 75, 154, 64),
            },
            Screen::SystemQr => ScreenLayout::SystemQr {
                // Square QR code (124x124) fits cleanly inside 140 px vertical page height.
                qr_box: Rect::new(8, PAGE_Y + 8, 124, 124),
                info_card: Rect::new(140, PAGE_Y + 4, 176, 132),
            },
        }
    }
}

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

/// Footer button hint shown on the system/QR screen.
pub const BUTTON_HINT: &str = "BTN1:NEXT  BTN2:PREV";

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screens_cycle_both_ways_through_all_six() {
        let mut s = Screen::MainHud;
        let mut seen = vec![s];
        for _ in 0..5 {
            s = s.next();
            seen.push(s);
        }
        assert_eq!(seen, Screen::ALL.to_vec());
        assert_eq!(s.next(), Screen::MainHud);
        assert_eq!(Screen::MainHud.prev(), Screen::SystemQr);
    }

    #[test]
    fn ui_theme_parse_and_as_str() {
        assert_eq!(UiTheme::default(), UiTheme::Cyberpunk);
        assert_eq!(UiTheme::parse("cyberpunk"), UiTheme::Cyberpunk);
        assert_eq!(UiTheme::parse("minimal"), UiTheme::Minimal);
        assert_eq!(UiTheme::parse("apple"), UiTheme::Minimal);
        assert_eq!(UiTheme::parse("  APPLE "), UiTheme::Minimal);
        assert_eq!(UiTheme::parse("parasite"), UiTheme::Parasite);
        assert_eq!(UiTheme::parse("PARASITE"), UiTheme::Parasite);
        assert_eq!(UiTheme::parse("unknown"), UiTheme::Cyberpunk);
        assert_eq!(UiTheme::parse(""), UiTheme::Cyberpunk);
        assert_eq!(UiTheme::Cyberpunk.as_str(), "cyberpunk");
        assert_eq!(UiTheme::Minimal.as_str(), "minimal");
        assert_eq!(UiTheme::Parasite.as_str(), "parasite");
    }
}
