//! Plain telemetry snapshot and text formatters for the LCD data screens.

/// Telemetry snapshot drawn by the HUD screens (plain data so it can be unit-tested on the host).
#[derive(Clone, Debug, Default)]
pub struct HudData {
    pub hashrate_ghs: f32,
    pub chip_hashrates_ghs: [f32; 4],
    pub chip_temps_c: [Option<f32>; 4],
    pub chip_offsets_mhz: [f32; 4],
    pub chip_error_pct: [f32; 4],
    pub vr_temp_c: Option<f32>,
    pub fan_asic_pct: u8,
    pub fan_vr_pct: u8,
    pub vin_v: f32,
    pub iin_a: f32,
    pub pin_w: f32,
    pub budget_w: f32,
    pub psu_tier: String,
    pub vcore_mv: u16,
    pub freq_mhz: f32,
    pub profile: String,
    pub phase: String,
    pub eff_jth: Option<f32>,
    pub pool_url: String,
    pub pool_user: String,
    pub pool_connected: bool,
    pub shares_accepted: u64,
    pub shares_rejected: u64,
    pub best_diff: f64,
    pub pool_diff: f64,
    pub hw_errors: u32,
    pub uptime_s: u64,
    pub net_nbits: u32,
    pub btc_height: Option<u32>,
    pub btc_price_usd: Option<f64>,
    /// Wall-clock seconds since the Unix epoch once SNTP has set the clock.
    pub utc_s: Option<u64>,
}

/// Network difficulty from a compact `nbits` target (`0` when invalid).
pub fn network_difficulty(nbits: u32) -> f64 {
    let exp = (nbits >> 24) as i32;
    let mant = (nbits & 0x00FF_FFFF) as f64;
    if mant == 0.0 || exp == 0 {
        return 0.0;
    }
    (65535.0 / mant) * 256f64.powi(0x1d - exp)
}

/// Compact difficulty text: `512`, `282.5K`, `1.31M`, `102T`.
pub fn fmt_diff(d: f64) -> String {
    if !d.is_finite() || d <= 0.0 {
        return String::from("---");
    }
    const UNITS: [(&str, f64); 5] = [("P", 1e15), ("T", 1e12), ("G", 1e9), ("M", 1e6), ("K", 1e3)];
    for (suffix, scale) in UNITS {
        if d >= scale {
            let v = d / scale;
            return if v >= 100.0 {
                format!("{v:.0}{suffix}")
            } else if v >= 10.0 {
                format!("{v:.1}{suffix}")
            } else {
                format!("{v:.2}{suffix}")
            };
        }
    }
    format!("{d:.0}")
}

/// `HH:MM:SS` (hours keep counting past 99).
pub fn fmt_hms(secs: u64) -> String {
    format!("{:02}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60)
}

/// `HH:MM` and `SS` of a UTC Unix timestamp.
pub fn fmt_utc_clock(utc_s: u64) -> (String, String) {
    let day = utc_s % 86_400;
    (format!("{:02}:{:02}", day / 3600, (day / 60) % 60), format!("{:02}", day % 60))
}

/// `$97,412` for a USD price (`---` when unknown).
pub fn fmt_usd(price: Option<f64>) -> String {
    price.map_or_else(|| String::from("---"), |p| format!("${}", crate::market::fmt_thousands(p.round() as u64)))
}

/// Latest rendered LCD frame (RGB565, byte-swapped) captured on request for screenshots.
pub static FRAME_SHOT: std::sync::Mutex<Vec<u16>> = std::sync::Mutex::new(Vec::new());
/// Set by the HTTP server; the display loop copies the next frame into [`FRAME_SHOT`].
pub static SHOT_REQ: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Encode a byte-swapped RGB565 frame as a 24-bit top-down BMP.
pub fn frame_to_bmp(frame: &[u16], w: usize, h: usize) -> Vec<u8> {
    let row = (w * 3 + 3) & !3;
    let size = 54 + row * h;
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(size as u32).to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(-(h as i32)).to_le_bytes()); // negative height = top-down rows
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&[0; 24]);
    for y in 0..h {
        for x in 0..w {
            let p = frame.get(y * w + x).copied().unwrap_or(0).swap_bytes();
            let r = ((p >> 11) & 0x1F) as u8;
            let g = ((p >> 5) & 0x3F) as u8;
            let b = (p & 0x1F) as u8;
            out.extend_from_slice(&[(b << 3) | (b >> 2), (g << 2) | (g >> 4), (r << 3) | (r >> 2)]);
        }
        out.resize(54 + row * (y + 1), 0);
    }
    out
}

/// Pool `host:port` without the `stratum+tcp://` style scheme.
pub fn pool_host(url: &str) -> &str {
    url.split_once("://").map_or(url, |(_, rest)| rest)
}

/// Worker name: the text after the last `.` of `address.worker` (or the whole user).
pub fn worker_name(user: &str) -> &str {
    user.rsplit_once('.').map_or(user, |(_, w)| w)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_price_and_bmp() {
        assert_eq!(fmt_utc_clock(86_400 * 20_000 + 3_725), (String::from("01:02"), String::from("05")));
        assert_eq!(fmt_usd(Some(97_412.4)), "$97,412");
        assert_eq!(fmt_usd(None), "---");
        // One white pixel then black, byte-swapped RGB565.
        let bmp = frame_to_bmp(&[0xFFFF, 0x0000], 2, 1);
        assert_eq!(&bmp[0..2], b"BM");
        assert_eq!(bmp.len(), 54 + 8);
        assert_eq!(&bmp[54..57], &[255, 255, 255]);
        assert_eq!(&bmp[57..60], &[0, 0, 0]);
        assert_eq!(i32::from_le_bytes(bmp[22..26].try_into().unwrap()), -1);
    }

    #[test]
    fn hud_formatters() {
        assert_eq!(fmt_diff(0.0), "---");
        assert_eq!(fmt_diff(512.0), "512");
        assert_eq!(fmt_diff(283_400.0), "283K");
        assert_eq!(fmt_diff(28_300.0), "28.3K");
        assert_eq!(fmt_diff(1_313_000.0), "1.31M");
        assert_eq!(fmt_diff(101_600_000_000_000.0), "102T");
        assert_eq!(fmt_hms(3_725), "01:02:05");
        assert_eq!(pool_host("stratum+tcp://pool.example.com:3333"), "pool.example.com:3333");
        assert_eq!(worker_name("bc1qabc.nerdqx1"), "nerdqx1");
        assert_eq!(worker_name("plainuser"), "plainuser");
        assert!((network_difficulty(0x1d00ffff) - 1.0).abs() < 1e-9);
        assert!(network_difficulty(0x17034219) > 1.0e13);
        assert_eq!(network_difficulty(0), 0.0);
    }
}

/// Display loop statistics, refreshed every ~10 s by the main loop and served by `GET /api/v0/display`.
pub static DISPLAY_FPS_X10: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// Average time to render one frame into the framebuffer, microseconds.
pub static DISPLAY_RENDER_US: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// Average time of the panel transfer (`draw_full`), microseconds.
pub static DISPLAY_DRAW_US: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// Longest main-loop iteration in the window, microseconds.
pub static DISPLAY_LOOP_MAX_US: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Per-stage time of the last main-HUD frame, microseconds: background copy, header, dial arcs,
/// trace pulses, hero number, callout cards. Diagnostic only.
pub static DISPLAY_STAGE_US: [std::sync::atomic::AtomicU32; 6] = [
    std::sync::atomic::AtomicU32::new(0),
    std::sync::atomic::AtomicU32::new(0),
    std::sync::atomic::AtomicU32::new(0),
    std::sync::atomic::AtomicU32::new(0),
    std::sync::atomic::AtomicU32::new(0),
    std::sync::atomic::AtomicU32::new(0),
];
