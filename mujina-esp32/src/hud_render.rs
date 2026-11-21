#![cfg(feature = "esp32s3")]
//! Nano 3S-styled 320x170 RGB565 HUD renderer using 4x-supersampled templates,
//! 8-bit anti-aliased TrueType bitmap fonts, and a Version 3-L QR renderer.

use crate::hw_pins::{LCD_HEIGHT, LCD_WIDTH};
use crate::lcd_driver::{fill_rect, rgb565};
use crate::hud_data::{
    fmt_diff, fmt_hms, fmt_usd, fmt_utc_clock, pool_host, worker_name, HudData,
};
use crate::market::{blocks_to_halving, fmt_thousands};
use crate::lcd_hud::{
    CutoffDial, Rect, Screen, ScreenLayout, UiTheme, BOOT_SPLASH_LAYOUT, FOOTER_HEIGHT,
    PAGE_HEIGHT, PAGE_Y,
};
use crate::qr_code::QrMatrix;
use crate::ui_assets::{
    BG_HUD_CYBER, BG_HUD_MINIMAL, BG_HUD_PARASITE, BG_QR_CYBER, BG_QR_MINIMAL, BG_QR_PARASITE,
    BG_SPLASH_CYBER, BG_SPLASH_MINIMAL, BG_SPLASH_PARASITE,
};
use crate::ui_fonts::{
    UiFont, UiGlyph, FONT_BAHN_12, FONT_BAHN_24, FONT_COURIER_11, FONT_COURIER_22, FONT_COURIER_8,
    FONT_BAHN_16, FONT_COURIER_15, FONT_ORBITRON_11, FONT_ORBITRON_15, FONT_ORBITRON_22,
    FONT_SEGOE_8, FONT_SPACE_8,
};

const FRAME_LEN: usize = (LCD_WIDTH as usize) * (LCD_HEIGHT as usize);

/// QR code payload mode shown on the Front Wi-Fi AP & QR screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QrMode {
    /// Standard Wi-Fi join payload: `WIFI:T:nopass;S:Mujina-NerdQX;;`
    WifiJoin,
    /// Direct browser setup / dashboard URL: `http://<ip>`
    DashboardUrl,
}

impl QrMode {
    pub const fn toggle(self) -> Self {
        match self {
            Self::WifiJoin => Self::DashboardUrl,
            Self::DashboardUrl => Self::WifiJoin,
        }
    }
}

struct ThemeStyle {
    splash_bg: &'static [u16; FRAME_LEN],
    hud_bg: &'static [u16; FRAME_LEN],
    qr_bg: &'static [u16; FRAME_LEN],
    font_hero: &'static UiFont,
    font_val: &'static UiFont,
    #[allow(dead_code)]
    font_sm: &'static UiFont,
    /// Header IP address font (between `font_val` and `font_hero`).
    font_ip: &'static UiFont,
    /// Cycling accent colours (card neon, dial sweeps, trace pulses).
    accents: [u16; 4],
    text_primary: u16,
    text_muted: u16,
    primary: u16,
    secondary: u16,
    ok: u16,
    warn: u16,
    pill_bg: u16,
    /// Flat page background, card fill and card outline for the data screens.
    page_bg: u16,
    card_bg: u16,
    card_line: u16,
}

fn theme_style(theme: UiTheme) -> ThemeStyle {
    match theme {
        UiTheme::Cyberpunk => ThemeStyle {
            splash_bg: &BG_SPLASH_CYBER,
            hud_bg: &BG_HUD_CYBER,
            qr_bg: &BG_QR_CYBER,
            font_hero: &FONT_ORBITRON_22,
            font_val: &FONT_ORBITRON_11,
            font_sm: &FONT_SPACE_8,
            font_ip: &FONT_ORBITRON_15,
            accents: [rgb565(255, 176, 0), rgb565(0, 240, 255), rgb565(27, 255, 156), rgb565(255, 46, 151)],
            text_primary: rgb565(255, 255, 255),
            text_muted: rgb565(132, 162, 192),
            primary: rgb565(255, 176, 0),
            secondary: rgb565(0, 240, 255),
            ok: rgb565(27, 255, 156),
            warn: rgb565(255, 78, 78),
            pill_bg: rgb565(12, 24, 42),
            page_bg: rgb565(4, 8, 16),
            card_bg: rgb565(12, 24, 42),
            card_line: rgb565(0, 96, 110),
        },
        UiTheme::Minimal => ThemeStyle {
            splash_bg: &BG_SPLASH_MINIMAL,
            hud_bg: &BG_HUD_MINIMAL,
            qr_bg: &BG_QR_MINIMAL,
            font_hero: &FONT_BAHN_24,
            font_val: &FONT_BAHN_12,
            font_sm: &FONT_SEGOE_8,
            font_ip: &FONT_BAHN_16,
            accents: [rgb565(0, 102, 204), rgb565(16, 185, 129), rgb565(124, 58, 237), rgb565(14, 165, 233)],
            text_primary: rgb565(30, 41, 59),
            text_muted: rgb565(92, 111, 132),
            primary: rgb565(0, 102, 204),
            secondary: rgb565(16, 185, 129),
            ok: rgb565(16, 185, 129),
            warn: rgb565(239, 68, 68),
            pill_bg: rgb565(255, 255, 255),
            page_bg: rgb565(238, 242, 248),
            card_bg: rgb565(255, 255, 255),
            card_line: rgb565(203, 213, 225),
        },
        UiTheme::Parasite => ThemeStyle {
            splash_bg: &BG_SPLASH_PARASITE,
            hud_bg: &BG_HUD_PARASITE,
            qr_bg: &BG_QR_PARASITE,
            font_hero: &FONT_COURIER_22,
            font_val: &FONT_COURIER_11,
            font_sm: &FONT_COURIER_8,
            font_ip: &FONT_COURIER_15,
            accents: [rgb565(255, 51, 51), rgb565(255, 255, 255), rgb565(165, 165, 165), rgb565(255, 51, 51)],
            text_primary: rgb565(255, 255, 255),
            text_muted: rgb565(165, 165, 165),
            primary: rgb565(255, 255, 255),
            secondary: rgb565(255, 51, 51),
            ok: rgb565(255, 255, 255),
            warn: rgb565(255, 51, 51),
            pill_bg: rgb565(255, 255, 255),
            page_bg: rgb565(0, 0, 0),
            card_bg: rgb565(0, 0, 0),
            card_line: rgb565(120, 120, 120),
        },
    }
}

#[inline]
fn blend_pixel(bg_be: u16, fg_be: u16, alpha: u8) -> u16 {
    if alpha == 0 {
        return bg_be;
    }
    if alpha == 255 {
        return fg_be;
    }
    let a = alpha as u32;
    let inv = 255 - a;
    let bg = bg_be.swap_bytes();
    let fg = fg_be.swap_bytes();

    let br = ((bg >> 11) & 0x1F) as u32;
    let bg_g = ((bg >> 5) & 0x3F) as u32;
    let bb = (bg & 0x1F) as u32;

    let fr = ((fg >> 11) & 0x1F) as u32;
    let fg_g = ((fg >> 5) & 0x3F) as u32;
    let fb_b = (fg & 0x1F) as u32;

    let r = (fr * a + br * inv + 127) / 255;
    let g = (fg_g * a + bg_g * inv + 127) / 255;
    let b = (fb_b * a + bb * inv + 127) / 255;

    let native = ((r as u16) << 11) | ((g as u16) << 5) | (b as u16);
    native.swap_bytes()
}

fn find_glyph(font: &UiFont, cp: u16) -> Option<&'static UiGlyph> {
    if let Ok(idx) = font.glyphs.binary_search_by_key(&cp, |g| g.codepoint) {
        return Some(&font.glyphs[idx]);
    }
    font.glyphs
        .iter()
        .find(|g| g.codepoint == cp)
        .or_else(|| font.glyphs.iter().find(|g| g.codepoint == b'?' as u16))
}

/// Measure pixel width of `text` rendered with `font`.
pub fn measure_aa_text(text: &str, font: &UiFont) -> i32 {
    let mut w = 0i32;
    for ch in text.chars() {
        if let Some(g) = find_glyph(font, ch as u16) {
            w += g.advance as i32;
        }
    }
    w
}

/// Draw anti-aliased 8-bit alpha text at top-left `(x, y)` over existing framebuffer pixels.
pub fn draw_aa_text(fb: &mut [u16], x: i32, y: i32, text: &str, font: &UiFont, color_be: u16) {
    let mut pen_x = x;
    for ch in text.chars() {
        let Some(g) = find_glyph(font, ch as u16) else {
            continue;
        };
        let gw = g.width as i32;
        let gh = g.height as i32;
        let gx0 = pen_x + g.x_off as i32;
        let gy0 = y + g.y_off as i32;
        let base_off = g.data_offset as usize;

        for row in 0..gh {
            let py = gy0 + row;
            if py < 0 || py >= LCD_HEIGHT as i32 {
                continue;
            }
            let fb_row = (py as usize) * (LCD_WIDTH as usize);
            let src_row = base_off + (row as usize) * (gw as usize);
            for col in 0..gw {
                let px = gx0 + col;
                if px < 0 || px >= LCD_WIDTH as i32 {
                    continue;
                }
                let a = font.bitmap[src_row + col as usize];
                if a > 4 {
                    let idx = fb_row + px as usize;
                    fb[idx] = blend_pixel(fb[idx], color_be, a);
                }
            }
        }
        pen_x += g.advance as i32;
    }
}

/// Like `draw_aa_text` but every font pixel becomes a `scale` x `scale` block (big hero numbers).
pub fn draw_aa_text_scaled(
    fb: &mut [u16],
    x: i32,
    y: i32,
    text: &str,
    font: &UiFont,
    color_be: u16,
    scale: i32,
) {
    let mut pen_x = x;
    for ch in text.chars() {
        let Some(g) = find_glyph(font, ch as u16) else {
            continue;
        };
        let gw = g.width as i32;
        let gh = g.height as i32;
        let base_off = g.data_offset as usize;
        for row in 0..gh {
            for col in 0..gw {
                let a = font.bitmap[base_off + (row as usize) * (gw as usize) + col as usize];
                if a <= 4 {
                    continue;
                }
                let px0 = pen_x + (g.x_off as i32 + col) * scale;
                let py0 = y + (g.y_off as i32 + row) * scale;
                for sy in 0..scale {
                    let py = py0 + sy;
                    if py < 0 || py >= LCD_HEIGHT as i32 {
                        continue;
                    }
                    for sx in 0..scale {
                        let px = px0 + sx;
                        if px < 0 || px >= LCD_WIDTH as i32 {
                            continue;
                        }
                        let idx = (py as usize) * (LCD_WIDTH as usize) + px as usize;
                        fb[idx] = blend_pixel(fb[idx], color_be, a);
                    }
                }
            }
        }
        pen_x += g.advance as i32 * scale;
    }
}

/// Draw `text` so its right edge sits at `right`.
fn draw_text_right(fb: &mut [u16], right: i32, y: i32, text: &str, font: &UiFont, color: u16) {
    let w = measure_aa_text(text, font);
    draw_aa_text(fb, right - w, y, text, font, color);
}

/// Big number in the hero font followed by a small unit label on the same baseline.
fn draw_value_unit(
    fb: &mut [u16],
    st: &ThemeStyle,
    x: i32,
    y: i32,
    num: &str,
    unit: &str,
    color: u16,
) {
    draw_aa_text(fb, x, y, num, st.font_hero, color);
    let w = measure_aa_text(num, st.font_hero);
    draw_aa_text(fb, x + w + 4, y + 11, unit, st.font_val, st.text_muted);
}

/// Fill a rectangle with its four corners cut at 45 degrees by `c` pixels.
fn fill_chamfer(fb: &mut [u16], x: i32, y: i32, w: i32, h: i32, c: i32, color: u16) {
    for row in 0..h {
        let inset = (c - row).max(c - (h - 1 - row)).max(0);
        fill_rect(fb, x + inset, y + row, w - 2 * inset, 1, color);
    }
}

/// Cyberpunk card: chamfered corners, 1 px outline and a neon strip on the left edge.
fn draw_card_chamfer(fb: &mut [u16], st: &ThemeStyle, r: Rect, accent: u16) {
    let (x, y, w, h, c) = (r.x as i32, r.y as i32, r.w as i32, r.h as i32, 7);
    fill_chamfer(fb, x, y, w, h, c, st.card_line);
    fill_chamfer(fb, x + 1, y + 1, w - 2, h - 2, c - 1, st.card_bg);
    fill_rect(fb, x, y + c, 3, h - 2 * c, accent);
}

fn draw_rect_outline(fb: &mut [u16], r: Rect, color: u16) {
    let x = r.x as i32;
    let y = r.y as i32;
    let w = r.w as i32;
    let h = r.h as i32;
    fill_rect(fb, x, y, w, 1, color);
    fill_rect(fb, x, y + h - 1, w, 1, color);
    fill_rect(fb, x, y, 1, h, color);
    fill_rect(fb, x + w - 1, y, 1, h, color);
}

/// Fill an annular arc: points between `r_inner` and `r_outer` of `(cx, cy)` whose angle (clockwise from +x,
/// screen coordinates) lies within `sweep_deg` of `start_deg`. Only the ring's own pixels are visited and the
/// angle test uses two cross products against the arc's end vectors, so there is no per-pixel trigonometry.
fn draw_arc_segment(
    fb: &mut [u16],
    cx: i32,
    cy: i32,
    r_inner: i32,
    r_outer: i32,
    start_deg: f32,
    sweep_deg: f32,
    color: u16,
    y_min: i32,
) {
    let r_in2 = r_inner * r_inner;
    let r_out2 = r_outer * r_outer;
    let y0 = (cy - r_outer).max(y_min);
    let y1 = (cy + r_outer).min(LCD_HEIGHT as i32 - 1);
    let sweep = sweep_deg.clamp(0.0, 360.0);
    let full = sweep >= 359.5;
    // A sweep over 180 degrees is drawn as everything except the complementary (shorter) arc.
    let (a0, span, invert) = if sweep > 180.0 {
        ((start_deg + sweep).rem_euclid(360.0), 360.0 - sweep, true)
    } else {
        (start_deg.rem_euclid(360.0), sweep, false)
    };
    let (s0, c0) = a0.to_radians().sin_cos();
    let (s1, c1) = (a0 + span).to_radians().sin_cos();
    let w = LCD_WIDTH as i32;
    for y in y0..=y1 {
        let dy = y - cy;
        let dy2 = dy * dy;
        if dy2 > r_out2 {
            continue;
        }
        let xo = ((r_out2 - dy2) as f32).sqrt() as i32;
        let xi = if dy2 < r_in2 { ((r_in2 - dy2) as f32).sqrt() as i32 } else { 0 };
        let base = (y as usize) * (LCD_WIDTH as usize);
        // Left and right spans of the ring on this row (a single span when the row misses the hole).
        for (lo, hi) in [(cx - xo, cx - xi), (cx + xi, cx + xo)] {
            if xi == 0 && lo > cx - xo {
                continue; // single span already drawn by the left pass
            }
            let (lo, hi) = if xi == 0 { (cx - xo, cx + xo) } else { (lo, hi) };
            for x in lo.max(0)..=hi.min(w - 1) {
                let dx = x - cx;
                let d2 = dx * dx + dy2;
                if d2 < r_in2 || d2 > r_out2 {
                    continue;
                }
                let inside = if full {
                    true
                } else {
                    let in_arc = c0 * dy as f32 - s0 * dx as f32 >= 0.0 && dx as f32 * s1 - dy as f32 * c1 >= 0.0;
                    in_arc != invert
                };
                if inside {
                    fb[base + x as usize] = color;
                }
            }
            if xi == 0 {
                break;
            }
        }
    }
}

/// Render the Nano 3S-styled 320x170 animated boot splash screen.
pub fn draw_boot_splash(fb: &mut [u16], frame: u32, total_frames: u32, theme: UiTheme) {
    let st = theme_style(theme);
    fb[..FRAME_LEN].copy_from_slice(st.splash_bg);

    let pb = BOOT_SPLASH_LAYOUT.progress_bar;
    let bar_y = (pb.y + 8).min(LCD_HEIGHT - 12);
    fill_rect(fb, pb.x as i32, bar_y as i32, pb.w as i32, pb.h as i32, st.text_muted);

    let denom = total_frames.max(1);
    let clamped = frame.min(denom);
    let fill_w = ((pb.w as u32) * clamped / denom) as i32;
    if fill_w > 0 {
        let fill_col = if theme == UiTheme::Parasite {
            st.secondary
        } else {
            st.primary
        };
        fill_rect(fb, pb.x as i32, bar_y as i32, fill_w, pb.h as i32, fill_col);
    }
}

/// Blend `color` over one framebuffer pixel with `alpha` (0..=255), ignoring off-screen points.
fn blend_at(fb: &mut [u16], x: i32, y: i32, color: u16, alpha: u32) {
    if x < 0 || y < 0 || x >= LCD_WIDTH as i32 || y >= LCD_HEIGHT as i32 || alpha == 0 {
        return;
    }
    let idx = (y as usize) * (LCD_WIDTH as usize) + x as usize;
    fb[idx] = blend_pixel(fb[idx], color, alpha.min(255) as u8);
}

/// Bolts of energy that run from the hashrate dial along each circuit trace to its metric card:
/// a bright core with a soft glow and a long fading comet tail in the card's accent colour.
/// The geometry mirrors `TRACE_STARTS` / `HUD_CARDS` in `tools/generate_lilygo_ui_assets.py`.
fn draw_trace_pulses(fb: &mut [u16], st: &ThemeStyle, cx: i32, cy: i32, frame: u32) {
    const DYS: [i32; 4] = [-65, -40, -12, 16];
    const DXS: [i32; 4] = [76, 92, 99, 99];
    const CARD_X: i32 = 186;
    const CARD_YS: [i32; 4] = [40, 73, 106, 139];
    const TAIL: i32 = 44;
    for i in 0..4 {
        let (sx, sy) = (cx + DXS[i], cy + DYS[i]);
        let ey = CARD_YS[i];
        let kx = (sx + (ey - sy).abs()).min(CARD_X - 8);
        let (dx, dy) = (kx - sx, ey - sy);
        let l1 = dx.abs().max(dy.abs()).max(1);
        let total = l1 + (CARD_X - kx);
        let period = total + TAIL + 6;
        let head = ((frame as i32 * 3) + i as i32 * (period / 4 + 7)) % period;
        let accent = st.accents[i];
        let point = |s: i32| -> (i32, i32) {
            if s < l1 {
                (sx + dx * s / l1, sy + dy * s / l1)
            } else {
                (kx + (s - l1), ey)
            }
        };
        for t in (0..=TAIL).rev() {
            let s = head - t;
            if s < 0 || s > total {
                continue;
            }
            let (px, py) = point(s);
            // Intensity falls off quadratically from the head to the end of the tail.
            let k = (TAIL - t) as u32 * 255 / TAIL as u32;
            let a = (k * (k + 255) / 2 / 255).min(255);
            // Soft glow around the streak.
            for gy in -1..=1 {
                for gx in -1..=1 {
                    if gx != 0 || gy != 0 {
                        blend_at(fb, px + gx, py + gy, accent, a / 2);
                    }
                }
            }
            // Bright core: accent that whitens toward the head.
            for (ox, oy) in [(0, 0), (1, 0), (0, 1)] {
                blend_at(fb, px + ox, py + oy, accent, a);
                if t < 16 {
                    blend_at(fb, px + ox, py + oy, st.text_primary, a * (16 - t as u32) / 16);
                }
            }
        }
        // Head flare.
        if head <= total {
            let (px, py) = point(head);
            for gy in -2..=2 {
                for gx in -2..=2 {
                    blend_at(fb, px + gx, py + gy, accent, 120);
                }
            }
            fill_rect(fb, px - 1, py - 1, 3, 3, st.text_primary);
        }
    }
}

/// Record the time since the previous lap in `DISPLAY_STAGE_US[i]` and restart the lap timer.
fn lap(i: usize, t: &mut std::time::Instant) {
    let now = std::time::Instant::now();
    crate::hud_data::DISPLAY_STAGE_US[i].store((now - *t).as_micros() as u32, std::sync::atomic::Ordering::Relaxed);
    *t = now;
}

/// Draw `MainHud` using the 4x-supersampled template background, animated dial arcs,
/// Wi-Fi AP/STA network name & IP in the header, and anti-aliased card values.
#[allow(clippy::too_many_arguments)]
pub fn draw_main_hud(
    fb: &mut [u16],
    hashrate_ghs: f32,
    chip_temp_c: Option<f32>,
    vr_temp_c: Option<f32>,
    eff_jth: Option<f32>,
    fan_rpm: Option<u16>,
    wifi_ssid: &str,
    is_ap_mode: bool,
    ip: &str,
    preset: &str,
    frame: u32,
    theme: UiTheme,
) {
    let st = theme_style(theme);
    let mut lap_t = std::time::Instant::now();
    fb[..FRAME_LEN].copy_from_slice(st.hud_bg);
    lap(0, &mut lap_t);

    // Header right: IP only (the SSID is on the QR screen), in the readable font.
    let hdr_info = if is_ap_mode { format!("AP {ip}") } else { ip.to_string() };
    let _ = wifi_ssid;
    let iw = measure_aa_text(&hdr_info, st.font_ip);
    let ix = (LCD_WIDTH as i32 - iw - 8).max(120);
    let hdr_col = if is_ap_mode {
        if theme == UiTheme::Parasite {
            st.secondary
        } else {
            st.primary
        }
    } else {
        st.text_primary
    };
    draw_aa_text(fb, ix, 2, &hdr_info, st.font_ip, hdr_col);
    let _ = preset;

    lap(1, &mut lap_t);
    // Animated sweeping arcs inside bottom-left CutoffDial::MAIN_HUD
    let dial = CutoffDial::MAIN_HUD;
    let cx = dial.cx as i32;
    let cy = dial.cy as i32;
    // Several short arcs in different accent colours, rotating at different speeds.
    let ac = st.accents;
    let outer = ((frame * 2) % 360) as f32;
    for (k, col) in [ac[0], ac[1], ac[3]].iter().enumerate() {
        draw_arc_segment(fb, cx, cy, 94, 100, outer + k as f32 * 120.0, 22.0, *col, 23);
    }
    let mid = (360.0 - ((frame * 3 / 2) % 360) as f32).rem_euclid(360.0);
    draw_arc_segment(fb, cx, cy, 82, 86, mid, 16.0, ac[2], 23);
    draw_arc_segment(fb, cx, cy, 82, 86, mid + 180.0, 16.0, ac[0], 23);
    let inner = ((frame * 3) % 360) as f32;
    draw_arc_segment(fb, cx, cy, 71, 75, inner, 14.0, ac[3], 23);
    draw_arc_segment(fb, cx, cy, 71, 75, inner + 180.0, 14.0, ac[1], 23);

    lap(2, &mut lap_t);
    draw_trace_pulses(fb, &st, cx, cy, frame);
    lap(3, &mut lap_t);

    // Hero hashrate inside cutoff circle
    let (hr_val, hr_unit) = if hashrate_ghs >= 1000.0 {
        (format!("{:.2}", hashrate_ghs / 1000.0), "TH/S")
    } else {
        (format!("{:.0}", hashrate_ghs.max(0.0)), "GH/S")
    };
    let vw = measure_aa_text(&hr_val, st.font_hero) * 2;
    let vx = (cx - vw / 2).max(4);
    let vy = cy - 34;
    draw_aa_text_scaled(fb, vx, vy, &hr_val, st.font_hero, st.text_primary, 2);
    let uw = measure_aa_text(hr_unit, st.font_val);
    draw_aa_text(fb, (cx - uw / 2).max(6), cy + 20, hr_unit, st.font_val, st.primary);

    lap(4, &mut lap_t);
    // 4 Telemetry callout card values (cards + labels already drawn in BG_HUD_*)
    let cards = match Screen::MainHud.layout() {
        ScreenLayout::MainHud { callout_cards, .. } => callout_cards,
        _ => unreachable!(),
    };

    let chip_str = chip_temp_c
        .map(|t| format!("{t:.1}°C"))
        .unwrap_or_else(|| "---".to_string());
    let chip_col = match chip_temp_c {
        Some(t) if t >= 75.0 => st.warn,
        Some(t) if t >= 65.0 => st.primary,
        _ => st.text_primary,
    };

    let vr_str = vr_temp_c
        .map(|t| format!("{t:.1}°C"))
        .unwrap_or_else(|| "---".to_string());
    let vr_col = match vr_temp_c {
        Some(t) if t >= 95.0 => st.warn,
        _ => st.text_primary,
    };

    let eff_str = eff_jth
        .map(|e| format!("{e:.1}"))
        .unwrap_or_else(|| "---".to_string());

    let fan_str = fan_rpm
        .map(|r| format!("{r}"))
        .unwrap_or_else(|| "---".to_string());

    let chip_str = chip_str.replace("°C", "°");
    let vr_str = vr_str.replace("°C", "°");
    let entries = [
        (cards[0], "CHIP", chip_str, chip_col),
        (cards[1], "VR", vr_str, vr_col),
        (cards[2], "J/TH", eff_str, st.text_primary),
        (cards[3], "RPM", fan_str, st.text_primary),
    ];

    for (card, label, val, col) in entries {
        // Paint over the small label baked into the template with the card's own fill colour.
        let sample = st.hud_bg[(card.y as usize + card.h as usize / 2) * (LCD_WIDTH as usize) + card.x as usize + 70];
        fill_rect(fb, card.x as i32 + 4, card.y as i32 + 2, card.w as i32 - 6, card.h as i32 - 4, sample);
        draw_aa_text(fb, card.x as i32 + 9, card.y as i32 + (card.h as i32 - 13) / 2, label, st.font_val, st.text_muted);
        let w = measure_aa_text(&val, st.font_hero);
        draw_aa_text(fb, card.right() as i32 - w - 8, card.y as i32 + (card.h as i32 - 26) / 2, &val, st.font_hero, col);
    }
    lap(5, &mut lap_t);
}

/// Render the Front Wi-Fi AP & Scannable QR Code screen styled in the active Nano 3S theme.
#[allow(clippy::too_many_arguments)]
pub fn draw_ap_qr_screen(
    fb: &mut [u16],
    qr: &QrMatrix,
    qr_mode: QrMode,
    wifi_ssid: &str,
    is_ap_mode: bool,
    ip: &str,
    chip_temp_c: Option<f32>,
    vr_temp_c: Option<f32>,
    theme: UiTheme,
) {
    let st = theme_style(theme);
    fb[..FRAME_LEN].copy_from_slice(st.qr_bg);

    // Header badge on top-right
    let hdr_status = if is_ap_mode {
        format!("AP BROADCASTING • {ip}")
    } else {
        format!("WLAN ONLINE • {ip}")
    };
    let hw = measure_aa_text(&hdr_status, st.font_val);
    let hx = (LCD_WIDTH as i32 - hw - 8).max(88);
    draw_aa_text(
        fb,
        hx,
        4,
        &hdr_status,
        st.font_val,
        if is_ap_mode { st.ok } else { st.text_muted },
    );

    // Left 29x29 QR code inside white quiet-zone box (x=13..137, y=32..156 -> 124x124)
    // 29 * 4 = 116 px centered with 4 px quiet margin on every side.
    let qr_x0 = 17i32;
    let qr_y0 = 36i32;
    let module_px = 4i32;
    let black = rgb565(0, 0, 0);
    for r in 0..qr.size() {
        for c in 0..qr.size() {
            if qr.get(r, c) {
                fill_rect(
                    fb,
                    qr_x0 + (c as i32) * module_px,
                    qr_y0 + (r as i32) * module_px,
                    module_px,
                    module_px,
                    black,
                );
            }
        }
    }

    // Right Info Panel (x=148..312, y=27..161), readable fonts
    let ix = 156i32;
    let iw = 150i32;
    let net_label = if is_ap_mode { "WIFI (AP)" } else { "WIFI" };
    draw_aa_text(fb, ix, 36, net_label, st.font_val, st.text_muted);
    draw_aa_text(fb, ix, 52, &fit_text(wifi_ssid, st.font_val, iw), st.font_val, st.primary);

    draw_aa_text(fb, ix, 82, "DASHBOARD", st.font_val, st.text_muted);
    let url = format!("http://{ip}");
    draw_aa_text(fb, ix, 98, &fit_text(&url, st.font_val, iw), st.font_val, st.text_primary);

    // Mode pill: SCAN TO JOIN WIFI vs SCAN FOR DASHBOARD
    let mode_txt = match qr_mode {
        QrMode::WifiJoin => "SCAN: JOIN WIFI",
        QrMode::DashboardUrl => "SCAN: DASHBOARD",
    };
    fill_rect(fb, ix, 126, 148, 24, st.pill_bg);
    draw_rect_outline(fb, Rect::new(ix as u16, 126, 148, 24), st.secondary);
    let m_fg = if theme == UiTheme::Parasite {
        rgb565(0, 0, 0)
    } else {
        st.secondary
    };
    draw_aa_text(fb, ix + 8, 131, mode_txt, st.font_val, m_fg);
    let _ = (chip_temp_c, vr_temp_c);
}

fn fit_text(text: &str, font: &UiFont, max_w: i32) -> String {
    let mut out = String::from(text);
    while measure_aa_text(&out, font) > max_w && out.pop().is_some() {}
    out
}

fn draw_card(fb: &mut [u16], st: &ThemeStyle, r: Rect) {
    fill_rect(fb, r.x as i32, r.y as i32, r.w as i32, r.h as i32, st.card_bg);
    draw_rect_outline(fb, r, st.card_line);
}

/// Horizontal progress bar: outlined track plus `frac` (0..=1) fill.
fn draw_bar(fb: &mut [u16], st: &ThemeStyle, r: Rect, frac: f32, color: u16, frame: u32) {
    fill_rect(fb, r.x as i32, r.y as i32, r.w as i32, r.h as i32, st.card_line);
    let f = if frac.is_finite() { frac.clamp(0.0, 1.0) } else { 0.0 };
    let w = (r.w as f32 * f) as i32;
    if w > 0 {
        fill_rect(fb, r.x as i32, r.y as i32, w, r.h as i32, color);
        // Bright glint sliding along the filled part, like the Nano 3S dial sweeps.
        let glint = 6;
        let gx = ((frame as i32 * 3 / 2) % (w + glint)) - glint;
        let x0 = gx.max(0);
        let x1 = (gx + glint).min(w);
        if x1 > x0 {
            fill_rect(fb, r.x as i32 + x0, r.y as i32, x1 - x0, r.h as i32, st.text_primary);
        }
    }
}

fn temp_color(st: &ThemeStyle, t: Option<f32>) -> u16 {
    match t {
        Some(t) if t >= 75.0 => st.warn,
        Some(t) if t >= 65.0 => st.primary,
        _ => st.text_primary,
    }
}

fn temp_text(t: Option<f32>) -> String {
    t.map_or_else(|| String::from("---"), |t| format!("{t:.1}°C"))
}

/// Start a data screen: themed header from the HUD template, flat page body, screen title.
fn begin_page(fb: &mut [u16], st: &ThemeStyle, title: &str, frame: u32) {
    fb[..FRAME_LEN].copy_from_slice(st.hud_bg);
    fill_rect(fb, 0, PAGE_Y as i32, LCD_WIDTH as i32, PAGE_HEIGHT as i32, st.page_bg);
    // The screen title and the header activity ring are intentionally not drawn.
    let _ = (title, frame);
}

/// `ChipDieGrid`: 2x2 cards, one per BM1370, with a large die temperature and a load bar.
pub fn draw_chip_grid(fb: &mut [u16], d: &HudData, frame: u32, theme: UiTheme) {
    let st = theme_style(theme);
    begin_page(fb, &st, "CHIP DIES", frame);
    let (cards, bars) = match Screen::ChipDieGrid.layout() {
        ScreenLayout::ChipDieGrid { cards, load_bars } => (cards, load_bars),
        _ => unreachable!(),
    };
    let top = d.chip_hashrates_ghs.iter().cloned().fold(1.0f32, f32::max);
    for i in 0..4 {
        let c = cards[i];
        draw_card_chamfer(fb, &st, c, st.accents[i]);
        let x = c.x as i32 + 10;
        let y = c.y as i32;
        let right = c.right() as i32 - 10;
        draw_aa_text(fb, x, y + 4, &format!("CHIP {}", i + 1), st.font_val, st.text_muted);
        let hr = d.chip_hashrates_ghs[i];
        draw_text_right(fb, right, y + 4, &format!("{:.2} TH/S", hr / 1000.0), st.font_val, st.primary);
        let t = d.chip_temps_c[i];
        draw_aa_text(fb, x, y + 20, &temp_text(t), st.font_hero, temp_color(&st, t));
        let err = d.chip_error_pct[i];
        let err_col = if err > 2.0 { st.warn } else { st.ok };
        draw_text_right(fb, right, y + 19, "ERR", st.font_val, st.text_muted);
        draw_text_right(fb, right, y + 33, &format!("{err:.1}%"), st.font_val, err_col);
        draw_bar(fb, &st, bars[i], hr / top, st.secondary, frame.wrapping_add(i as u32 * 7));
    }
    draw_page_dots(fb, Screen::ChipDieGrid, theme);
}

/// `TunerPsu`: left card is wall power against the PSU budget, right card is the auto-tuner.
pub fn draw_tuner_psu(fb: &mut [u16], d: &HudData, frame: u32, theme: UiTheme) {
    let st = theme_style(theme);
    begin_page(fb, &st, "POWER / TUNER", frame);
    let (psu, bar, tuner) = match Screen::TunerPsu.layout() {
        ScreenLayout::TunerPsu { psu_card, power_bar, tuner_card } => (psu_card, power_bar, tuner_card),
        _ => unreachable!(),
    };
    draw_card(fb, &st, psu);
    let x = psu.x as i32 + 8;
    let y = psu.y as i32;
    draw_aa_text(fb, x, y + 5, "POWER", st.font_val, st.text_muted);
    draw_text_right(fb, psu.right() as i32 - 8, y + 5, &format!("TIER {}", d.psu_tier), st.font_val, st.primary);
    let watts = format!("{:.0}", d.pin_w);
    let scale = if measure_aa_text(&watts, st.font_hero) * 2 + 34 <= psu.w as i32 - 16 { 2 } else { 1 };
    draw_aa_text_scaled(fb, x, y + 22, &watts, st.font_hero, st.text_primary, scale);
    let ww = measure_aa_text(&watts, st.font_hero) * scale;
    draw_aa_text(fb, x + ww + 6, y + 22 + 26 * scale - 18, "W", st.font_val, st.text_muted);
    draw_aa_text(fb, x, y + 78, &format!("of {:.0} W budget", d.budget_w), st.font_val, st.text_muted);
    let frac = if d.budget_w > 0.0 { d.pin_w / d.budget_w } else { 0.0 };
    let bar_col = if frac >= 0.95 { st.warn } else { st.ok };
    draw_bar(fb, &st, bar, frac, bar_col, frame);
    draw_aa_text(fb, x, y + 116, &format!("{:.1} V   {:.1} A", d.vin_v, d.iin_a), st.font_val, st.text_primary);

    draw_card(fb, &st, tuner);
    let tx = tuner.x as i32 + 8;
    let ty = tuner.y as i32;
    let max_w = tuner.w as i32 - 16;
    draw_aa_text(fb, tx, ty + 5, "TUNER", st.font_val, st.text_muted);
    draw_text_right(fb, tuner.right() as i32 - 8, ty + 5, &d.phase.to_uppercase(), st.font_val, st.ok);
    let profile = d.profile.replace('_', " ").to_uppercase();
    let (pfont, py) = if measure_aa_text(&profile, st.font_hero) <= max_w {
        (st.font_hero, ty + 22)
    } else {
        (st.font_val, ty + 28)
    };
    draw_aa_text(fb, tx, py, &fit_text(&profile, pfont, max_w), pfont, st.primary);
    draw_value_unit(fb, &st, tx, ty + 54, &format!("{:.0}", d.freq_mhz), "MHz", st.text_primary);
    let eff = d.eff_jth.map_or_else(|| String::from("---"), |e| format!("{e:.1}"));
    draw_value_unit(fb, &st, tx, ty + 86, &eff, "J/TH", st.text_primary);
    draw_aa_text(
        fb,
        tx,
        ty + 118,
        &format!("VR {}  FAN {}%", temp_text(d.vr_temp_c), d.fan_asic_pct),
        st.font_val,
        st.text_muted,
    );
    draw_page_dots(fb, Screen::TunerPsu, theme);
}

/// `PoolShares`: accepted / rejected / best in large numbers, then pool and worker.
pub fn draw_pool_shares(fb: &mut [u16], d: &HudData, frame: u32, theme: UiTheme) {
    let st = theme_style(theme);
    begin_page(fb, &st, "POOL / SHARES", frame);
    let (summary, rows) = match Screen::PoolShares.layout() {
        ScreenLayout::PoolShares { summary_bar, rows } => (summary_bar, rows),
        _ => unreachable!(),
    };
    draw_card(fb, &st, summary);
    let rej_col = if d.shares_rejected > 0 { st.warn } else { st.text_primary };
    let cols = [
        ("ACCEPTED", format!("{}", d.shares_accepted), st.ok),
        ("REJECTED", format!("{}", d.shares_rejected), rej_col),
        ("BEST", fmt_diff(d.best_diff), st.primary),
    ];
    let col_w = summary.w as i32 / 3;
    for (i, (label, value, color)) in cols.iter().enumerate() {
        let cx = summary.x as i32 + i as i32 * col_w + 10;
        draw_aa_text(fb, cx, summary.y as i32 + 6, label, st.font_val, st.text_muted);
        draw_aa_text(fb, cx, summary.y as i32 + 24, &fit_text(value, st.font_hero, col_w - 14), st.font_hero, *color);
    }

    let status_col = if d.pool_connected { st.ok } else { st.warn };
    let items = [
        ("POOL", pool_host(&d.pool_url).split(':').next().unwrap_or("").to_string()),
        ("WORKER", worker_name(&d.pool_user).to_string()),
    ];
    for (i, (label, value)) in items.iter().enumerate() {
        let r = rows[i];
        draw_card(fb, &st, r);
        draw_aa_text(fb, r.x as i32 + 8, r.y as i32 + (r.h as i32 - 13) / 2, label, st.font_val, st.text_muted);
        let vx = r.x as i32 + 80;
        let max_w = r.w as i32 - 80 - if i == 0 { 28 } else { 10 };
        draw_aa_text(fb, vx, r.y as i32 + (r.h as i32 - 26) / 2, &fit_text(value, st.font_hero, max_w), st.font_hero, st.text_primary);
        if i == 0 {
            // Pool link status dot.
            fill_rect(fb, r.right() as i32 - 20, r.y as i32 + (r.h as i32 - 10) / 2, 10, 10, status_col);
        }
    }
    draw_page_dots(fb, Screen::PoolShares, theme);
}

/// `ClockBtc`: radar dial with a large UTC clock (uptime until SNTP sets the time),
/// Bitcoin block height with halving countdown, and BTC/USD price.
pub fn draw_clock(fb: &mut [u16], d: &HudData, frame: u32, theme: UiTheme) {
    let st = theme_style(theme);
    begin_page(fb, &st, "CLOCK / BTC", frame);
    let (clock, sec, left, right) = match Screen::ClockBtc.layout() {
        ScreenLayout::ClockBtc { clock_box, sec_bar, left_card, right_card } => {
            (clock_box, sec_bar, left_card, right_card)
        }
        _ => unreachable!(),
    };
    draw_card(fb, &st, clock);

    // Radar dial (Nano 3S style concentric sweeps) on the left of the clock box.
    let (cx, cy) = (clock.x as i32 + 38, clock.y as i32 + clock.h as i32 / 2);
    let clip = clock.y as i32 + 1;
    let secs = d.utc_s.map_or(d.uptime_s % 60, |u| u % 60);
    draw_arc_segment(fb, cx, cy, 26, 29, 0.0, 360.0, st.card_line, clip);
    draw_arc_segment(fb, cx, cy, 26, 29, 270.0, (secs as f32 + 1.0) * 6.0, st.ok, clip);
    draw_arc_segment(fb, cx, cy, 20, 23, ((frame * 5 / 2) % 360) as f32, 80.0, st.primary, clip);
    draw_arc_segment(fb, cx, cy, 14, 17, (360 - (frame * 4) % 360) as f32, 60.0, st.secondary, clip);
    let bw = measure_aa_text("BTC", st.font_val);
    draw_aa_text(fb, cx - bw / 2 + 1, cy - 6, "BTC", st.font_val, st.primary);

    let text_x = clock.x as i32 + 80;
    let (hm, ss, label) = match d.utc_s {
        Some(u) => {
            let (hm, ss) = fmt_utc_clock(u);
            (hm, ss, "UTC")
        }
        None => {
            let t = fmt_hms(d.uptime_s);
            (t[..5].to_string(), t[6..].to_string(), "UPTIME")
        }
    };
    draw_aa_text_scaled(fb, text_x, clock.y as i32 + 5, &hm, st.font_hero, st.text_primary, 2);
    let hw = measure_aa_text(&hm, st.font_hero) * 2;
    draw_aa_text(fb, text_x + hw + 4, clock.y as i32 + 36, &format!(":{ss}"), st.font_val, st.primary);
    draw_text_right(fb, clock.right() as i32 - 8, clock.y as i32 + 5, label, st.font_val, st.text_muted);

    draw_bar(fb, &st, sec, secs as f32 / 59.0, st.primary, frame);

    // Block height + halving countdown.
    draw_card(fb, &st, left);
    let inner_w = left.w as i32 - 16;
    draw_aa_text(fb, left.x as i32 + 8, left.y as i32 + 5, "BLOCK HEIGHT", st.font_val, st.text_muted);
    let (height, sub) = match d.btc_height {
        Some(h) => (
            fmt_thousands(h as u64),
            format!("HALVING {}", fmt_thousands(blocks_to_halving(h) as u64)),
        ),
        None => (String::from("---"), String::from("NO NETWORK")),
    };
    draw_aa_text(fb, left.x as i32 + 8, left.y as i32 + 20, &fit_text(&height, st.font_hero, inner_w), st.font_hero, st.primary);
    draw_aa_text(fb, left.x as i32 + 8, left.y as i32 + 46, &fit_text(&sub, st.font_val, inner_w), st.font_val, st.text_muted);

    // BTC / USD price.
    draw_card(fb, &st, right);
    draw_aa_text(fb, right.x as i32 + 8, right.y as i32 + 5, "BTC / USD", st.font_val, st.text_muted);
    let price = fmt_usd(d.btc_price_usd);
    draw_aa_text(fb, right.x as i32 + 8, right.y as i32 + 26, &fit_text(&price, st.font_hero, inner_w), st.font_hero, st.ok);
    if d.btc_price_usd.is_none() {
        draw_aa_text(fb, right.x as i32 + 8, right.y as i32 + 46, "NO NETWORK", st.font_val, st.text_muted);
    }
    draw_page_dots(fb, Screen::ClockBtc, theme);
}

/// Page position dots in the footer strip: one per screen, current one filled.
pub fn draw_page_dots(fb: &mut [u16], current: Screen, theme: UiTheme) {
    let st = theme_style(theme);
    let n = Screen::ALL.len() as i32;
    let gap = 10;
    let x0 = (LCD_WIDTH as i32 - (n - 1) * gap) / 2;
    let y = (LCD_HEIGHT - FOOTER_HEIGHT) as i32 + 3;
    for (i, s) in Screen::ALL.iter().enumerate() {
        let (size, col) = if *s == current { (4, st.primary) } else { (2, st.text_muted) };
        fill_rect(fb, x0 + i as i32 * gap - size / 2, y + (4 - size) / 2, size, size, col);
    }
}
