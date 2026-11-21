//! Bitcoin block height (mempool.space) and USD price (CoinGecko) for the LCD clock screen.
//!
//! The parsers are plain functions so they can be unit-tested on the host. The HTTPS fetch
//! thread only exists on the ESP32 target and sends no device or user data (plain GETs).

/// `GET https://mempool.space/api/blocks/tip/height` returns the height as plain text.
pub fn parse_height(body: &str) -> Option<u32> {
    let h: u32 = body.trim().parse().ok()?;
    (h > 0).then_some(h)
}

/// `GET .../simple/price?ids=bitcoin&vs_currencies=usd` returns `{"bitcoin":{"usd":97412}}`.
pub fn parse_price_usd(body: &str) -> Option<f64> {
    let after = &body[body.find("\"usd\"")? + 5..];
    let after = after.trim_start().strip_prefix(':')?.trim_start();
    let end = after
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(after.len());
    let v: f64 = after[..end].parse().ok()?;
    (v.is_finite() && v > 0.0).then_some(v)
}

/// Blocks left until the next subsidy halving (every 210,000 blocks).
pub fn blocks_to_halving(height: u32) -> u32 {
    210_000 - (height % 210_000)
}

/// `874123` -> `874,123`.
pub fn fmt_thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(feature = "esp32s3")]
mod fetch {
    use super::{parse_height, parse_price_usd};
    use crate::net_web::SharedTelemetry;
    use embedded_svc::http::client::Client;
    use esp_idf_svc::http::client::{Configuration, EspHttpConnection};
    use std::thread;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    const HEIGHT_URL: &str = "https://mempool.space/api/blocks/tip/height";
    const PRICE_URL: &str =
        "https://api.coingecko.com/api/v3/simple/price?ids=bitcoin&vs_currencies=usd";
    const HEIGHT_EVERY_S: u64 = 60;
    const PRICE_EVERY_S: u64 = 120;
    /// Anything before 2024 means SNTP has not set the clock yet (TLS cert checks need it).
    const MIN_VALID_UNIX_S: u64 = 1_704_067_200;

    fn clock_is_set() -> bool {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .is_ok_and(|d| d.as_secs() >= MIN_VALID_UNIX_S)
    }

    fn http_get(url: &str) -> Option<String> {
        let conn = EspHttpConnection::new(&Configuration {
            crt_bundle_attach: Some(esp_idf_svc::sys::esp_crt_bundle_attach),
            timeout: Some(Duration::from_secs(8)),
            buffer_size: Some(2048),
            ..Default::default()
        })
        .ok()?;
        let mut client = Client::wrap(conn);
        let request = client
            .request(
                embedded_svc::http::Method::Get,
                url,
                &[("Accept", "application/json, text/plain"), ("User-Agent", "mujina-esp32")],
            )
            .ok()?;
        let mut response = request.submit().ok()?;
        if response.status() != 200 {
            log::warn!("market GET {url} -> HTTP {}", response.status());
            return None;
        }
        let mut out = Vec::new();
        let mut buf = [0u8; 256];
        loop {
            let n = response.read(&mut buf).ok()?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
            if out.len() > 4096 {
                return None;
            }
        }
        String::from_utf8(out).ok()
    }

    /// Poll block height and BTC price in the background while the device is on a LAN with internet.
    pub fn spawn(telemetry: SharedTelemetry) {
        let _ = thread::Builder::new()
            .name(String::from("market"))
            .stack_size(20 * 1024)
            .spawn(move || {
                // Start "due" so the first fetch happens as soon as the clock is set.
                let mut since_height: u64 = HEIGHT_EVERY_S;
                let mut since_price: u64 = PRICE_EVERY_S;
                loop {
                    thread::sleep(Duration::from_secs(5));
                    let online = telemetry.lock().is_ok_and(|t| !t.is_ap_mode) && clock_is_set();
                    if !online {
                        continue;
                    }
                    since_height += 5;
                    since_price += 5;
                    let height_due = since_height >= HEIGHT_EVERY_S;
                    let price_due = since_price >= PRICE_EVERY_S;
                    // Retry a failed fetch after ~15 s instead of waiting the full interval.
                    if height_due {
                        since_height = HEIGHT_EVERY_S - 15;
                    }
                    if price_due {
                        since_price = PRICE_EVERY_S - 15;
                    }
                    if height_due {
                        match http_get(HEIGHT_URL).as_deref().and_then(parse_height) {
                            Some(h) => {
                                since_height = 0;
                                if let Ok(mut t) = telemetry.lock() {
                                    t.btc_height = Some(h);
                                }
                            }
                            None => log::warn!("market: block height fetch failed"),
                        }
                    }
                    if price_due {
                        match http_get(PRICE_URL).as_deref().and_then(parse_price_usd) {
                            Some(p) => {
                                since_price = 0;
                                if let Ok(mut t) = telemetry.lock() {
                                    t.btc_price_usd = Some(p);
                                }
                            }
                            None => log::warn!("market: price fetch failed"),
                        }
                    }
                }
            });
    }
}

#[cfg(feature = "esp32s3")]
pub use fetch::spawn;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mempool_height() {
        assert_eq!(parse_height("874123"), Some(874_123));
        assert_eq!(parse_height(" 874123\n"), Some(874_123));
        assert_eq!(parse_height("<html>"), None);
        assert_eq!(parse_height("0"), None);
    }

    #[test]
    fn parses_coingecko_price() {
        assert_eq!(parse_price_usd(r#"{"bitcoin":{"usd":97412}}"#), Some(97_412.0));
        assert_eq!(parse_price_usd(r#"{"bitcoin": {"usd": 97412.35}}"#), Some(97_412.35));
        assert_eq!(parse_price_usd(r#"{"error":"rate limited"}"#), None);
        assert_eq!(parse_price_usd(r#"{"bitcoin":{"usd":null}}"#), None);
    }

    #[test]
    fn halving_and_thousands() {
        assert_eq!(blocks_to_halving(840_000), 210_000);
        assert_eq!(blocks_to_halving(1_049_999), 1);
        assert_eq!(fmt_thousands(7), "7");
        assert_eq!(fmt_thousands(97_412), "97,412");
        assert_eq!(fmt_thousands(1_050_000), "1,050,000");
    }
}
