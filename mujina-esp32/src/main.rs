//! NerdQX standalone embedded firmware entrypoint (ESP32-S3 / LilyGO T-Display-S3).

use std::thread;
use std::time::{Duration, Instant};

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::gpio::{PinDriver, Pull};
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::log::EspLogger;
use esp_idf_svc::nvs::EspDefaultNvsPartition;

use mujina_esp32::bm1370::{Bm1370Chain, ChainHashrateWindow};
use mujina_esp32::chip_tuner::{
    boot_point_for_profile, nerdqx_profile_goal, tmux_safe_clamp, vcore_for_freq_mv, vr_fan_duty_pct, ChipTuneEngine, ASIC_FAN_MIN_PCT, MAX_BASE_FREQ_MHZ,
    MAX_VCORE_MV, MIN_BASE_FREQ_MHZ, MIN_VCORE_MV, TuneInput, TEMP_HOT_C, TEMP_TARGET_C, VR_TEMP_LIMIT_C,
};
use mujina_esp32::hud_render::{
    draw_ap_qr_screen, draw_boot_splash, draw_chip_grid, draw_clock, draw_main_hud, draw_page_dots,
    draw_pool_shares, draw_tuner_psu, QrMode,
};
use mujina_esp32::hw_pins::{LCD_HEIGHT, LCD_WIDTH};
use mujina_esp32::i2c_bus::NerdqxI2c;
use mujina_esp32::lcd_driver::{rgb565, DmaFramebuffer, St7789Panel};
use mujina_esp32::hud_data::HudData;
use mujina_esp32::lcd_hud::{Screen, UiTheme};
use mujina_esp32::net_web::{
    compute_merkle_root, connect_wifi, eval_fan_curve_pct, hash_to_wire_words, header_difficulty,
    load_config_nvs, new_shared_telemetry, reset_reason_name,
    start_captive_dns_server, start_http_server, stratum_prevhash_to_internal, DeviceConfig,
    LiveTelemetry, StratumClient, StratumEvent, StratumJob, TunerLogEntry, FALLBACK_AP_SSID,
};
use mujina_esp32::qr_code::QrMatrix;
use mujina_esp32::power_cooling::{
    evaluate_safety_cutoff, PowerCoolingConfig, Preset,
};

const APP_STACK_BYTES: usize = 64 * 1024;
const BOOT_SPLASH_FRAMES: u32 = 30;
const BOOT_SPLASH_FRAME_MS: u64 = 40;
const LOOP_TICK_MS: u64 = 50;
const SENSOR_POLL_TICKS: u32 = 5; // 250 ms (~4 Hz)
const RENDER_TICKS: u32 = 1; // every 50 ms tick: 20 FPS target for smooth dial sweeps
const DEFAULT_FAN_DUTY_PCT: u8 = 50;
const STRATUM_CONNECT_TIMEOUT_MS: u64 = 3000;
const STRATUM_POLL_TIMEOUT_MS: u64 = 20;
const STRATUM_WRITE_TIMEOUT_MS: u64 = 500;
const STRATUM_RECONNECT_INTERVAL_S: u64 = 10;
const STRATUM_IDLE_TIMEOUT_S: u64 = 180;
const WIFI_RECONNECT_INTERVAL_S: u64 = 5;
const MAX_NONCES_PER_TICK: usize = 64;
const NTIME_ROLL_INTERVAL_S: u64 = 1;
const WORK_ROTATE_INTERVAL_S: u64 = 20;
const AUTOTUNE_INTERVAL_S: u64 = 15;
const MAX_TUNER_EVENTS: usize = 16;
const STRATUM_USER_AGENT: &str = "mujina-esp32/0.1";

fn read_heap_kb() -> (u32, u32, u32) {
    unsafe {
        let int_caps = esp_idf_svc::sys::MALLOC_CAP_INTERNAL | esp_idf_svc::sys::MALLOC_CAP_8BIT;
        let free_int = (esp_idf_svc::sys::heap_caps_get_free_size(int_caps) / 1024) as u32;
        let min_int = (esp_idf_svc::sys::heap_caps_get_minimum_free_size(int_caps) / 1024) as u32;
        let free_psram =
            (esp_idf_svc::sys::heap_caps_get_free_size(esp_idf_svc::sys::MALLOC_CAP_SPIRAM) / 1024)
                as u32;
        (free_int, min_int, free_psram)
    }
}

#[derive(Clone)]
struct ActiveAsicJob {
    job: StratumJob,
    en2_hex: String,
    prev_internal: [u8; 32],
    merkle_internal: [u8; 32],
    version: u32,
    nbits: u32,
    ntime: u32,
}

/// Snapshot of the live telemetry for the LCD data screens.
fn hud_snapshot(t: &LiveTelemetry, vr_temp: Option<f32>, net_nbits: u32) -> HudData {
    let hr_ths = t.hashrate_ghs / 1000.0;
    HudData {
        hashrate_ghs: t.hashrate_ghs,
        chip_hashrates_ghs: t.chip_hashrates_ghs,
        chip_temps_c: t.chip_temps_c,
        chip_offsets_mhz: t.chip_offsets_mhz,
        chip_error_pct: t.chip_error_pct,
        vr_temp_c: vr_temp,
        fan_asic_pct: t.fan_pct,
        fan_vr_pct: t.fan_vr_pct,
        vin_v: t.vin_v,
        iin_a: t.iin_a,
        pin_w: t.pin_w,
        budget_w: t.psu_budget_w,
        psu_tier: String::from(t.psu_preset),
        vcore_mv: t.vcore_mv,
        freq_mhz: t.freq_mhz,
        profile: t.tuner.profile.clone(),
        phase: t.tuner.phase.clone(),
        eff_jth: (hr_ths > 0.1 && t.pin_w > 1.0).then(|| t.pin_w / hr_ths),
        pool_url: t.pool_url.clone(),
        pool_user: t.pool_user.clone(),
        pool_connected: t.pool_connected,
        shares_accepted: t.shares_accepted,
        shares_rejected: t.shares_rejected,
        best_diff: t.best_diff,
        pool_diff: t.pool_diff,
        hw_errors: t.hw_errors,
        uptime_s: t.uptime_s,
        net_nbits,
        btc_height: t.btc_height,
        btc_price_usd: t.btc_price_usd,
        utc_s: wall_clock_utc_s(),
    }
}

/// Seconds since the Unix epoch once SNTP has set the clock (`None` before that).
fn wall_clock_utc_s() -> Option<u64> {
    const MIN_VALID_UNIX_S: u64 = 1_704_067_200; // 2024-01-01
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
        .filter(|s| *s >= MIN_VALID_UNIX_S)
}

fn parse_hex_u32(s: &str) -> u32 {
    u32::from_str_radix(s.trim(), 16).unwrap_or(0)
}

fn format_en2_hex(counter: u64, en2_bytes: usize) -> String {
    let width = en2_bytes.clamp(1, 16) * 2;
    let mut full = format!("{counter:0width$x}");
    if full.len() > width {
        let start = full.len() - width;
        full = full[start..].to_owned();
    }
    full
}

fn connect_stratum_client(cfg: &DeviceConfig) -> Option<(StratumClient, u64)> {
    if cfg.pool_host.is_empty() || cfg.pool_user.is_empty() {
        return None;
    }
    let pool_addr = format!("{}:{}", cfg.pool_host, cfg.pool_port);
    match StratumClient::connect(&pool_addr, Duration::from_millis(STRATUM_CONNECT_TIMEOUT_MS)) {
        Ok(mut client) => {
            log::info!("Stratum connected to {pool_addr}");
            let _ = client.set_read_timeout(Duration::from_millis(STRATUM_POLL_TIMEOUT_MS));
            let _ = client.set_write_timeout(Duration::from_millis(STRATUM_WRITE_TIMEOUT_MS));
            if let Err(e) = client.configure_version_rolling() {
                log::warn!("Stratum configure failed: {e:?}");
            }
            if let Err(e) = client.subscribe(STRATUM_USER_AGENT) {
                log::warn!("Stratum subscribe failed: {e:?}");
            }
            let auth_id = match client.authorize(&cfg.pool_user, &cfg.pool_pass) {
                Ok(id) => {
                    log::info!("Stratum authorize sent for {}", cfg.pool_user);
                    id
                }
                Err(e) => {
                    log::warn!("Stratum authorize failed: {e:?}");
                    0
                }
            };
            Some((client, auth_id))
        }
        Err(e) => {
            log::warn!("Stratum connect to {pool_addr} failed ({e:?}); retrying later");
            None
        }
    }
}

fn power_config_for_preset(preset: &str) -> PowerCoolingConfig {
    match preset {
        "2A" | "12v15a" => {
            let mut c = PowerCoolingConfig::for_preset(Preset::StockAir, 15, false);
            c.power_ceiling_w = 180.0;
            c
        }
        "1B" | "upgraded_air" => PowerCoolingConfig::for_preset(Preset::UpgradedAir, 20, true),
        "2" | "4A" | "12v20a" | "aio" => PowerCoolingConfig::for_preset(Preset::AiO, 20, true),
        _ => PowerCoolingConfig::default_stock(),
    }
}

#[allow(clippy::too_many_arguments)]
fn dispatch_work(
    job: &StratumJob,
    client: &StratumClient,
    en2_counter: &mut u64,
    slots: &mut [Option<ActiveAsicJob>; 16],
    next_slot: &mut u8,
    latest_slot: &mut usize,
    asic: Option<&mut Bm1370Chain<'_>>,
) {
    *en2_counter = en2_counter.wrapping_add(1);
    let en2_hex = format_en2_hex(*en2_counter, client.extranonce2_size);
    let prev_internal = stratum_prevhash_to_internal(&job.prevhash);
    let merkle_internal = compute_merkle_root(
        &job.coinb1,
        &client.extranonce1,
        &en2_hex,
        &job.coinb2,
        &job.merkle_branch,
    );
    let version = parse_hex_u32(&job.version);
    let nbits = parse_hex_u32(&job.nbits);
    let ntime = parse_hex_u32(&job.ntime);

    let slot = (*next_slot & 0x0F) as usize;
    *next_slot = next_slot.wrapping_add(1) & 0x0F;
    *latest_slot = slot;

    slots[slot] = Some(ActiveAsicJob {
        job: job.clone(),
        en2_hex,
        prev_internal,
        merkle_internal,
        version,
        nbits,
        ntime,
    });

    if let Some(chain) = asic {
        let prev_wire = hash_to_wire_words(&prev_internal);
        let merkle_wire = hash_to_wire_words(&merkle_internal);
        let asic_job_byte = ((slot as u8) & 0x0F) << 3;
        if let Err(e) = chain.send_work(
            asic_job_byte,
            version,
            &prev_wire,
            &merkle_wire,
            ntime,
            nbits,
        ) {
            log::warn!("BM1370 send_work failed: {e:?}");
        }
    }
}

/// Fast 1-second `ntime` increment on the current Stratum work without rebuilding coinbase/merkle.
fn roll_ntime_work(
    slots: &mut [Option<ActiveAsicJob>; 16],
    next_slot: &mut u8,
    latest_slot: &mut usize,
    asic: Option<&mut Bm1370Chain<'_>>,
) -> bool {
    let Some(prev) = slots[*latest_slot].as_ref().cloned() else {
        return false;
    };
    let new_ntime = prev.ntime.wrapping_add(1);
    let mut job = prev.job.clone();
    job.ntime = format!("{new_ntime:08x}");

    let slot = (*next_slot & 0x0F) as usize;
    *next_slot = next_slot.wrapping_add(1) & 0x0F;
    *latest_slot = slot;

    let next_active = ActiveAsicJob {
        job,
        en2_hex: prev.en2_hex,
        prev_internal: prev.prev_internal,
        merkle_internal: prev.merkle_internal,
        version: prev.version,
        nbits: prev.nbits,
        ntime: new_ntime,
    };
    let prev_wire = hash_to_wire_words(&next_active.prev_internal);
    let merkle_wire = hash_to_wire_words(&next_active.merkle_internal);
    let version = next_active.version;
    let nbits = next_active.nbits;
    slots[slot] = Some(next_active);

    if let Some(chain) = asic {
        let asic_job_byte = ((slot as u8) & 0x0F) << 3;
        if let Err(e) = chain.send_work(
            asic_job_byte,
            version,
            &prev_wire,
            &merkle_wire,
            new_ntime,
            nbits,
        ) {
            log::warn!("BM1370 ntime-roll send_work failed: {e:?}");
        }
    }
    true
}

/// Safely transition core frequency and core voltage (voltage-first when raising clock).
fn apply_freq_and_vcore(
    asic: &mut Option<Bm1370Chain<'_>>,
    i2c: &mut Option<NerdqxI2c<'_>>,
    cur_freq: &mut f32,
    cur_mv: &mut u16,
    req_freq: f32,
    req_mv: u16,
    offsets: &[f32; 4],
) {
    let tmux_ok = i2c.as_ref().is_some_and(|b| b.is_using_tmp451());
    let (safe_f, safe_mv) = tmux_safe_clamp(
        tmux_ok,
        req_freq.clamp(MIN_BASE_FREQ_MHZ, MAX_BASE_FREQ_MHZ),
        req_mv.clamp(MIN_VCORE_MV, MAX_VCORE_MV) as u32,
    );
    let target_mv = safe_mv as u16;
    let raising = safe_f > *cur_freq;

    if raising && target_mv != *cur_mv {
        let Some(bus) = i2c.as_mut() else {
            log::warn!("Skipping clock raise to {safe_f:.1} MHz: I2C VR unavailable");
            return;
        };
        match bus.set_vcore_mv_clamped(target_mv) {
            Ok(applied) => *cur_mv = applied,
            Err(e) => {
                log::warn!("Skipping clock raise to {safe_f:.1} MHz: set_vcore_mv({target_mv}) failed ({e:?})");
                return;
            }
        }
    }
    if (safe_f - *cur_freq).abs() >= 0.5 {
        if let Some(chain) = asic.as_mut() {
            if chain.set_chain_frequency(*cur_freq, safe_f).is_ok() {
                *cur_freq = safe_f;
                for (idx, &off) in offsets.iter().enumerate() {
                    if off.abs() >= 0.5 {
                        let _ = chain.set_chip_frequency(idx as u8, safe_f + off);
                    }
                }
            }
        } else {
            *cur_freq = safe_f;
        }
    }
    if !raising && target_mv != *cur_mv {
        if let Some(bus) = i2c.as_mut() {
            if let Ok(applied) = bus.set_vcore_mv_clamped(target_mv) {
                *cur_mv = applied;
            }
        }
    }
}

fn main() {
    esp_idf_svc::sys::link_patches();
    EspLogger::initialize_default();

    // Run on a 64 KiB thread stack so Wi-Fi/net80211/NVS never overflows ESP-IDF's 3.5 KiB main_task.
    thread::Builder::new()
        .name(String::from("mujina_app"))
        .stack_size(APP_STACK_BYTES)
        .spawn(app_main)
        .expect("failed to spawn mujina_app thread")
        .join()
        .expect("mujina_app thread panicked");
}

fn app_main() {
    let reset_reason = reset_reason_name(unsafe { esp_idf_svc::sys::esp_reset_reason() } as u32);
    let (init_int_kb, init_min_kb, init_psram_kb) = read_heap_kb();
    log::info!(
        "mujina-esp32 starting on NerdQX (ESP32-S3) reset_reason={reset_reason} \
         heap_internal={init_int_kb}KiB (min={init_min_kb}KiB) psram={init_psram_kb}KiB"
    );

    let peripherals = Peripherals::take().expect("failed to take ESP32-S3 peripherals");
    let sysloop = EspSystemEventLoop::take().expect("failed to take EspSystemEventLoop");
    let nvs = EspDefaultNvsPartition::take().expect("failed to take EspDefaultNvsPartition");

    let mut cfg = load_config_nvs(&nvs);
    let initial_theme = UiTheme::parse(&cfg.ui_theme);

    // 1. Initialize ST7789 320x170 i80 display and play 30-frame Nano 3S-styled boot splash.
    let mut panel = match St7789Panel::init() {
        Ok(p) => {
            log::info!("ST7789 320x170 i80 panel initialized");
            Some(p)
        }
        Err(e) => {
            log::warn!("ST7789 panel init failed ({e:?}); continuing headless");
            None
        }
    };

    let fb_len = (LCD_WIDTH as usize) * (LCD_HEIGHT as usize);
    let bg = rgb565(5, 6, 12);
    let mut fbs = [
        DmaFramebuffer::new(fb_len, bg),
        DmaFramebuffer::new(fb_len, bg),
    ];
    for frame in 0..BOOT_SPLASH_FRAMES {
        let fb = fbs[(frame as usize) & 1].as_mut_slice();
        draw_boot_splash(fb, frame, BOOT_SPLASH_FRAMES, initial_theme);
        if let Some(p) = panel.as_mut() {
            let _ = p.draw_full(fb);
        }
        thread::sleep(Duration::from_millis(BOOT_SPLASH_FRAME_MS));
    }

    // 2. Front buttons: BTN1 (GPIO14) and BTN2 (GPIO0), active-low with pull-up.
    let mut btn1 = PinDriver::input(peripherals.pins.gpio14).ok();
    if let Some(b) = btn1.as_mut() {
        let _ = b.set_pull(Pull::Up);
    }
    let mut btn2 = PinDriver::input(peripherals.pins.gpio0).ok();
    if let Some(b) = btn2.as_mut() {
        let _ = b.set_pull(Pull::Up);
    }
    let mut prev_btn1_low = false;
    let mut prev_btn2_low = false;

    // 3. Initialize NerdQX I2C0 bus, fans, TPS53647 configuration, LDO_EN -> PWR_EN -> VR_RDY,
    //    then re-apply VOUT_COMMAND and settle 50 ms before bringing up the BM1370 chain.
    let mut fault_latched = false;
    let mut fault_reason: Option<String> = None;
    let mut rail_ready = false;

    // Max-hash boots at 777 MHz / 1250 mV; every other profile at the efficient 450 MHz / 1120 mV.
    let (start_freq_mhz, start_mv) = boot_point_for_profile(&cfg.tuner_profile);
    let mut boot_freq_mhz: u16 = start_freq_mhz;
    let mut boot_mv: u16 = start_mv;

    let mut i2c = match NerdqxI2c::new(
        peripherals.i2c0,
        peripherals.pins.gpio44,
        peripherals.pins.gpio43,
        peripherals.pins.gpio2,
        peripherals.pins.gpio3,
        peripherals.pins.gpio10,
        peripherals.pins.gpio13,
        peripherals.pins.gpio11,
    ) {
        Ok(mut bus) => {
            log::info!("NerdQX I2C0 bus initialized");
            if let Err(e) = bus.set_fan_duties(vr_fan_duty_pct(None).min(60), DEFAULT_FAN_DUTY_PCT) {
                log::warn!("EMC2302 set_fan_duties failed: {e:?}");
            }
            // Without a working TMP451 die-temp path, fall back to the conservative point.
            let (bf, bmv) = tmux_safe_clamp(
                bus.is_using_tmp451(),
                start_freq_mhz as f32,
                start_mv as u32,
            );
            boot_freq_mhz = bf as u16;
            boot_mv = bmv as u16;
            log::info!("Boot operating point: {boot_freq_mhz} MHz @ {boot_mv} mV");
            match bus.init_tps53647_and_enable(boot_mv) {
                Ok(()) => {
                    rail_ready = true;
                }
                Err(reason) => {
                    log::error!("Core rail power-up aborted: {reason}");
                    bus.emergency_shutdown();
                    fault_latched = true;
                    fault_reason = Some(String::from(reason));
                }
            }
            Some(bus)
        }
        Err(e) => {
            log::warn!("NerdQX I2C0 init failed ({e:?}); continuing fail-closed");
            fault_latched = true;
            fault_reason = Some(String::from("i2c0_init_failed"));
            None
        }
    };

    // 4. Initialize 4-chip BM1370 ASIC chain ONLY after VR_RDY + post-enable VOUT verify,
    //    and require all 4 chips to answer broadcast ChipId at 1 Mbps.
    let mut detected_chips: u8 = 0;
    let mut asic = if rail_ready {
        match Bm1370Chain::new(
            peripherals.uart1,
            peripherals.pins.gpio17.into(),
            peripherals.pins.gpio18.into(),
            peripherals.pins.gpio1.into(),
        ) {
            Ok(mut chain) => {
                match chain.reset_and_init(boot_freq_mhz) {
                    Ok(count) if count == 4 => {
                        detected_chips = count;
                        log::info!(
                            "BM1370 4/4 chips initialized and verified at {boot_freq_mhz} MHz"
                        );
                    }
                    Ok(count) => {
                        detected_chips = count;
                        log::error!(
                            "BM1370 chain enumeration mismatch: detected {count}/4 chips"
                        );
                    }
                    Err(e) => {
                        log::error!("BM1370 chain reset_and_init failed: {e:?}");
                    }
                }
                Some(chain)
            }
            Err(e) => {
                log::warn!("BM1370 UART1 init failed ({e:?}); continuing without ASIC chain");
                None
            }
        }
    } else {
        log::error!("Skipping BM1370 chain init because core rail did not reach VR_RDY");
        None
    };

    // 5. Connect Wi-Fi (STA or SoftAP fallback), and start HTTP API.
    let telemetry = new_shared_telemetry();

    let (mut wifi, mut ip_addr, wifi_ok, is_ap_mode, active_ssid) = match connect_wifi(
        peripherals.modem,
        sysloop,
        nvs.clone(),
        cfg.wifi_ssid.as_str(),
        cfg.wifi_pass.as_str(),
    ) {
        Ok((wifi, ip, ap_mode, ssid)) => {
            log::info!("Network ready at {ip} (ssid={ssid}, ap_mode={ap_mode})");
            (Some(wifi), ip, true, ap_mode, ssid)
        }
        Err(e) => {
            log::warn!("Wi-Fi init failed ({e:?}); continuing offline");
            (
                None,
                String::from("192.168.71.1"),
                false,
                true,
                String::from(FALLBACK_AP_SSID),
            )
        }
    };

    let want_initial_pool =
        wifi_ok && !is_ap_mode && !cfg.pool_host.is_empty() && !cfg.pool_user.is_empty();
    if let Ok(mut guard) = telemetry.lock() {
        guard.ip_addr = ip_addr.clone();
        guard.wifi_ssid = active_ssid.clone();
        guard.is_ap_mode = is_ap_mode;
        guard.ui_theme = initial_theme;
        guard.tuner.profile = cfg.tuner_profile.clone();
        guard.tuner.fan_target_c = if cfg.tuner_profile == "max_hash" { 63.0 } else { 60.0 };
        guard.pool_url = format!("stratum+tcp://{}:{}", cfg.pool_host, cfg.pool_port);
        guard.pool_user = cfg.pool_user.clone();
        guard.pool_dirty = want_initial_pool;
        guard.reset_reason = reset_reason;
        guard.free_internal_kb = init_int_kb;
        guard.min_internal_kb = init_min_kb;
        guard.free_psram_kb = init_psram_kb;
    }

    let _http_server = match start_http_server(telemetry.clone(), nvs.clone()) {
        Ok(srv) => {
            log::info!("Embedded HTTP server started on http://{ip_addr}/");
            Some(srv)
        }
        Err(e) => {
            log::warn!("HTTP server failed to start: {e:?}");
            None
        }
    };

    if wifi_ok && is_ap_mode {
        start_captive_dns_server();
    }

    // 6. Precompute QR matrices for fast front-screen rendering.
    let wifi_qr_payload = format!("WIFI:T:nopass;S:{active_ssid};;");
    let url_qr_payload = format!("http://{ip_addr}");
    let qr_wifi = QrMatrix::encode(wifi_qr_payload.as_bytes());
    let mut qr_url = QrMatrix::encode(url_qr_payload.as_bytes());

    let mut active_screen = if is_ap_mode {
        Screen::SystemQr
    } else {
        Screen::MainHud
    };
    let qr_base_mode = if is_ap_mode {
        QrMode::WifiJoin
    } else {
        QrMode::DashboardUrl
    };

    // 7. Runtime state for Stratum + BM1370 hashing + telemetry.
    let mut stratum: Option<StratumClient> = None;
    let mut auth_req_id: u64 = 0;
    let mut stratum_subscribed = false;
    let mut latest_notify: Option<StratumJob> = None;
    let mut last_stratum_attempt = Instant::now() - Duration::from_secs(STRATUM_RECONNECT_INTERVAL_S);
    let mut last_work_dispatch = Instant::now();
    let mut last_ntime_roll = Instant::now();
    let mut last_autotune_tick = Instant::now();

    let mut job_slots: [Option<ActiveAsicJob>; 16] = Default::default();
    let mut next_slot: u8 = 0;
    let mut latest_slot: usize = 0;
    let mut extranonce2_counter: u64 = 0;

    let start_time = Instant::now();
    let mut tick: u32 = 0;
    let mut frame: u32 = 0;
    // Display timing statistics (logged and served by GET /api/v0/display every ~10 s).
    let mut st_window = Instant::now();
    let mut st_frames: u32 = 0;
    let mut st_render_us: u64 = 0;
    let mut st_draw_us: u64 = 0;
    let mut st_loop_max_us: u64 = 0;
    let mut shares_accepted: u64 = 0;
    let mut shares_rejected: u64 = 0;
    let mut best_diff: f64 = 0.0;
    let mut current_diff: f64 = 512.0;

    let mut base_freq_mhz: f32 = boot_freq_mhz as f32;
    let mut target_vcore_mv: u16 = boot_mv;
    let mut paused: bool = false;
    let mut hw_errors: u32 = 0;

    let mut chip_shares: [u32; 4] = [0; 4];
    let mut hr_window = ChainHashrateWindow::new();
    let mut chip_tuner = ChipTuneEngine::new();
    let mut window_start = Instant::now();
    let mut last_series_push = Instant::now();

    let mut vr_temp_opt: Option<f32> = None;
    let mut vin_v: f32 = 0.0;
    let mut iin_a: f32 = 0.0;
    let mut iout_a: f32 = 0.0;
    let mut pin_w: f32 = 0.0;
    let mut vcore_mv: u16 = boot_mv;
    let mut fan_pct: u8 = if fault_latched { 100 } else { DEFAULT_FAN_DUTY_PCT };
    let mut tps_comm_failures: u8 = 0;
    let mut chip_temps: [Option<f32>; 4] = [None; 4];
    let mut vr_fan_pct: u8 = 60;
    let mut fan_rpms: [Option<u16>; 2] = [None; 2];

    let mut wifi_connected = wifi_ok && !is_ap_mode;
    let mut last_wifi_check = Instant::now();
    let mut last_stratum_rx = Instant::now();

    // Wall clock (needed for TLS certificate checks and the LCD clock) and BTC market data.
    let _sntp = if wifi_connected {
        esp_idf_svc::sntp::EspSntp::new_default().ok()
    } else {
        None
    };
    if !is_ap_mode {
        mujina_esp32::market::spawn(telemetry.clone());
    }

    unsafe {
        let _ = esp_idf_svc::sys::esp_task_wdt_add(core::ptr::null_mut());
    }

    loop {
        let loop_t0 = Instant::now();
        unsafe {
            let _ = esp_idf_svc::sys::esp_task_wdt_reset();
        }
        // Handle front buttons with rising-edge press detection (active-low).
        let b1_low = btn1.as_ref().is_some_and(|b| b.is_low());
        let b2_low = btn2.as_ref().is_some_and(|b| b.is_low());

        // BTN1 = next screen, BTN2 = previous screen. The UI theme only changes from the dashboard.
        if b1_low && !prev_btn1_low {
            active_screen = active_screen.next();
        }
        if b2_low && !prev_btn2_low {
            active_screen = active_screen.prev();
        }
        if let Some(n) = telemetry.lock().ok().and_then(|mut g| g.screen_req.take()) {
            active_screen = Screen::ALL[(n as usize) % Screen::ALL.len()];
        }
        let btn_edge = (b1_low && !prev_btn1_low) || (b2_low && !prev_btn2_low);
        prev_btn1_low = b1_low;
        prev_btn2_low = b2_low;

        // Consume dashboard control flags (pool update, pause/resume, manual tuning, autotuner reset).
        let (pool_dirty, pause_req, manual_req, tuner_dirty, preset, ui_theme, fan_auto, fan_manual_pct, fan_curve) =
            if let Ok(mut g) = telemetry.lock() {
                let p_dirty = g.pool_dirty;
                g.pool_dirty = false;
                let p_req = g.pause_dirty.take();
                let m_req = g.pending_manual_tune.take();
                let t_dirty = g.tuner.dirty;
                g.tuner.dirty = false;
                (
                    p_dirty,
                    p_req,
                    m_req,
                    t_dirty,
                    g.psu_preset,
                    g.ui_theme,
                    g.fan_auto,
                    g.fan_manual_pct,
                    g.fan_curve.clone(),
                )
            } else {
                (
                    false,
                    None,
                    None,
                    false,
                    "stock",
                    initial_theme,
                    true,
                    DEFAULT_FAN_DUTY_PCT,
                    vec![(50.0, 30), (58.0, 50), (60.0, 80), (62.0, 100)],
                )
            };

        if let Some(want_pause) = pause_req {
            if want_pause && !paused {
                paused = true;
                if let Some(chain) = asic.as_mut() {
                    chain.hold_reset_low();
                }
                hr_window.reset();
                job_slots = Default::default();
                log::info!("Miner paused via API");
            } else if !want_pause && paused && !fault_latched {
                paused = false;
                hr_window.reset();
                if let Some(chain) = asic.as_mut() {
                    match chain.reset_and_init(base_freq_mhz.round() as u16) {
                        Ok(cnt) if cnt == 4 => {
                            detected_chips = cnt;
                            log::info!("BM1370 4/4 chips re-initialized on resume");
                        }
                        Ok(cnt) => {
                            detected_chips = cnt;
                            log::error!("BM1370 resume enumeration mismatch: detected {cnt}/4 chips");
                        }
                        Err(e) => {
                            detected_chips = 0;
                            log::error!("BM1370 resume reset_and_init failed: {e:?}");
                        }
                    }
                    unsafe {
                        let _ = esp_idf_svc::sys::esp_task_wdt_reset();
                    }
                    let offsets = chip_tuner.offsets_mhz();
                    for (idx, &off) in offsets.iter().enumerate() {
                        if off.abs() >= 0.5 {
                            let _ = chain.set_chip_frequency(idx as u8, base_freq_mhz + off);
                        }
                    }
                }
                if let (Some(client), Some(job)) = (stratum.as_mut(), latest_notify.as_ref()) {
                    dispatch_work(
                        job,
                        client,
                        &mut extranonce2_counter,
                        &mut job_slots,
                        &mut next_slot,
                        &mut latest_slot,
                        asic.as_mut(),
                    );
                    last_work_dispatch = Instant::now();
                    last_ntime_roll = Instant::now();
                }
                log::info!("Miner resumed via API");
            }
        }

        if let Some((req_freq, req_mv)) = manual_req {
            if !fault_latched && !paused {
                let prev_f = base_freq_mhz;
                let prev_mv = target_vcore_mv;
                chip_tuner.reset();
                apply_freq_and_vcore(
                    &mut asic,
                    &mut i2c,
                    &mut base_freq_mhz,
                    &mut target_vcore_mv,
                    req_freq,
                    req_mv,
                    &[0.0; 4],
                );
                hr_window.reset();
                log::info!(
                    "Manual tuning applied: {prev_f:.1}->{base_freq_mhz:.1} MHz @ {prev_mv}->{target_vcore_mv} mV"
                );
            }
        }

        if tuner_dirty {
            chip_tuner.reset();
            last_autotune_tick = Instant::now();
        }

        if pool_dirty {
            cfg = load_config_nvs(&nvs);
            stratum = None;
            stratum_subscribed = false;
            latest_notify = None;
            job_slots = Default::default();
            last_stratum_attempt = Instant::now() - Duration::from_secs(STRATUM_RECONNECT_INTERVAL_S);
        }

        if wifi_ok
            && !is_ap_mode
            && last_wifi_check.elapsed() >= Duration::from_secs(WIFI_RECONNECT_INTERVAL_S)
        {
            last_wifi_check = Instant::now();
            if let Some(w) = wifi.as_mut() {
                let connected = w.is_connected().unwrap_or(false);
                if !connected {
                    if wifi_connected {
                        log::warn!(
                            "Wi-Fi STA link lost; dropping Stratum and triggering reconnect"
                        );
                    }
                    wifi_connected = false;
                    stratum = None;
                    stratum_subscribed = false;
                    let err = unsafe { esp_idf_svc::sys::esp_wifi_connect() };
                    if err != 0 {
                        log::debug!("esp_wifi_connect() returned {err}");
                    }
                } else if !wifi_connected {
                    if let Ok(ip_info) = w.wifi().sta_netif().get_ip_info() {
                        let new_ip = ip_info.ip.to_string();
                        if new_ip != "0.0.0.0" {
                            wifi_connected = true;
                            if new_ip != ip_addr {
                                ip_addr = new_ip;
                                let next_url_qr = format!("http://{ip_addr}");
                                qr_url = QrMatrix::encode(next_url_qr.as_bytes());
                                if let Ok(mut guard) = telemetry.lock() {
                                    guard.ip_addr = ip_addr.clone();
                                }
                            }
                            log::info!("Wi-Fi STA reconnected ({active_ssid}): {ip_addr}");
                        }
                    }
                }
            }
        }

        if stratum.is_none()
            && wifi_connected
            && !is_ap_mode
            && !cfg.pool_host.is_empty()
            && !cfg.pool_user.is_empty()
            && last_stratum_attempt.elapsed() >= Duration::from_secs(STRATUM_RECONNECT_INTERVAL_S)
        {
            last_stratum_attempt = Instant::now();
            unsafe {
                let _ = esp_idf_svc::sys::esp_task_wdt_reset();
            }
            let res = connect_stratum_client(&cfg);
            unsafe {
                let _ = esp_idf_svc::sys::esp_task_wdt_reset();
            }
            if let Some((client, aid)) = res {
                stratum = Some(client);
                auth_req_id = aid;
                stratum_subscribed = false;
                latest_notify = None;
                last_stratum_rx = Instant::now();
            }
        }

        // Step non-blocking TMP451 mux state machine (one 50 ms step per loop tick) and poll I2C at ~4 Hz.
        if let Some(bus) = i2c.as_mut() {
            bus.step_temp_mux();
            if tick % SENSOR_POLL_TICKS == 0 {
                match bus.read_tps53647() {
                    Ok(tps) => {
                        tps_comm_failures = 0;
                        vin_v = tps.vin_v;
                        iin_a = tps.iin_a;
                        iout_a = tps.iout_a;
                        pin_w = tps.pin_w;
                        if tps.vout_v.is_finite() && tps.vout_v > 0.2 {
                            vcore_mv = (tps.vout_v * 1000.0).round() as u16;
                        }
                        vr_temp_opt = if tps.vr_temp_c.is_finite() {
                            Some(tps.vr_temp_c)
                        } else {
                            None
                        };
                    }
                    Err(e) => {
                        tps_comm_failures = tps_comm_failures.saturating_add(1);
                        // Keep the previous vr_temp_opt so a single transient I2C error does not
                        // trip vr_overtemp_or_invalid before the tps_comm_failures >= 3 streak limit.
                        log::warn!("TPS53647 read error ({e:?}) [streak={tps_comm_failures}]");
                    }
                }
                chip_temps = bus.asic_temps();
                fan_rpms = bus.read_fan_rpms();
            }
        }

        // Poll Stratum JSON-RPC events and feed work to BM1370 chain (gated by !fault_latched && !paused).
        if let Some(client) = stratum.as_mut() {
            let mut drop_client = false;
            for _ in 0..8 {
                match client.poll_event() {
                    Ok(Some(ev)) => {
                        last_stratum_rx = Instant::now();
                        match ev {
                            StratumEvent::Subscribed { .. } => {
                                stratum_subscribed = true;
                                log::info!(
                                    "Stratum subscribed: en1={} en2_size={}",
                                    client.extranonce1,
                                    client.extranonce2_size
                                );
                                if !fault_latched && !paused {
                                    if let Some(job) = latest_notify.as_ref() {
                                        dispatch_work(
                                            job,
                                            client,
                                            &mut extranonce2_counter,
                                            &mut job_slots,
                                            &mut next_slot,
                                            &mut latest_slot,
                                            asic.as_mut(),
                                        );
                                        last_work_dispatch = Instant::now();
                                        last_ntime_roll = Instant::now();
                                    }
                                }
                            }
                            StratumEvent::Notify(job) => {
                                if job.clean_jobs {
                                    job_slots = Default::default();
                                }
                                if !fault_latched
                                    && !paused
                                    && (stratum_subscribed || !client.extranonce1.is_empty())
                                {
                                    dispatch_work(
                                        &job,
                                        client,
                                        &mut extranonce2_counter,
                                        &mut job_slots,
                                        &mut next_slot,
                                        &mut latest_slot,
                                        asic.as_mut(),
                                    );
                                    last_work_dispatch = Instant::now();
                                    last_ntime_roll = Instant::now();
                                }
                                latest_notify = Some(job);
                            }
                            StratumEvent::SetDifficulty(diff) => {
                                if diff.is_finite() && diff > 0.0 {
                                    current_diff = diff;
                                    log::info!("Stratum difficulty set to {diff}");
                                }
                            }
                            StratumEvent::Response { id, accepted } => {
                                if accepted && id > auth_req_id {
                                    shares_accepted = shares_accepted.saturating_add(1);
                                } else if !accepted && id > auth_req_id {
                                    shares_rejected = shares_rejected.saturating_add(1);
                                    log::warn!("Stratum rejected share id={id}");
                                }
                            }
                            StratumEvent::Raw(_) => {}
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        log::warn!("Stratum poll error ({e:?}); reconnecting");
                        drop_client = true;
                        break;
                    }
                }
            }

            if !drop_client
                && last_stratum_rx.elapsed() >= Duration::from_secs(STRATUM_IDLE_TIMEOUT_S)
            {
                log::warn!(
                    "Stratum idle for {}s without messages; reconnecting",
                    STRATUM_IDLE_TIMEOUT_S
                );
                drop_client = true;
            }

            if !drop_client
                && !fault_latched
                && !paused
                && (stratum_subscribed || !client.extranonce1.is_empty())
            {
                if last_work_dispatch.elapsed() >= Duration::from_secs(WORK_ROTATE_INTERVAL_S) {
                    if let Some(job) = latest_notify.as_ref() {
                        dispatch_work(
                            job,
                            client,
                            &mut extranonce2_counter,
                            &mut job_slots,
                            &mut next_slot,
                            &mut latest_slot,
                            asic.as_mut(),
                        );
                        last_work_dispatch = Instant::now();
                        last_ntime_roll = Instant::now();
                    }
                } else if last_ntime_roll.elapsed() >= Duration::from_secs(NTIME_ROLL_INTERVAL_S) {
                    if roll_ntime_work(
                        &mut job_slots,
                        &mut next_slot,
                        &mut latest_slot,
                        asic.as_mut(),
                    ) {
                        last_ntime_roll = Instant::now();
                    }
                }
            }

            if drop_client {
                stratum = None;
                stratum_subscribed = false;
            }
        }

        // Poll BM1370 nonces, verify share difficulty, update 120s sliding window, and submit shares.
        if !paused && !fault_latched {
            if let Some(chain) = asic.as_mut() {
                for _ in 0..MAX_NONCES_PER_TICK {
                    let Some(hit) = chain.poll_nonce() else {
                        break;
                    };
                    let chip_idx = (hit.chip_index as usize) & 3;
                    let slot_idx = ((hit.job_id >> 4) & 0x0F) as usize;
                    let active_opt = job_slots[slot_idx]
                        .as_ref()
                        .or_else(|| job_slots[latest_slot].as_ref());

                    if let Some(active) = active_opt {
                        let full_version = active.version | ((hit.version_bits as u32) << 13);
                        let share_diff = header_difficulty(
                            full_version,
                            &active.prev_internal,
                            &active.merkle_internal,
                            active.ntime,
                            active.nbits,
                            hit.nonce,
                        );
                        if share_diff.is_finite() && share_diff >= 128.0 {
                            hr_window.record_valid(chip_idx);
                            chip_shares[chip_idx] = chip_shares[chip_idx].saturating_add(1);
                            if share_diff > best_diff {
                                best_diff = share_diff;
                            }
                            if share_diff >= current_diff {
                                let submit_err = if let Some(client) = stratum.as_mut() {
                                    let nonce_hex = format!("{:08x}", hit.nonce);
                                    let vb_hex = if client.version_rolling || hit.version_bits != 0 {
                                        Some(format!("{:08x}", (hit.version_bits as u32) << 13))
                                    } else {
                                        None
                                    };
                                    client
                                        .submit(
                                            &cfg.pool_user,
                                            &active.job.job_id,
                                            &active.en2_hex,
                                            &active.job.ntime,
                                            &nonce_hex,
                                            vb_hex.as_deref(),
                                        )
                                        .err()
                                } else {
                                    None
                                };
                                if let Some(e) = submit_err {
                                    log::warn!("Stratum submit failed ({e:?}); dropping connection");
                                    stratum = None;
                                    stratum_subscribed = false;
                                }
                            }
                        } else {
                            hr_window.record_error(chip_idx);
                            hw_errors = hw_errors.saturating_add(1);
                        }
                    } else {
                        hr_window.record_error(chip_idx);
                        hw_errors = hw_errors.saturating_add(1);
                    }
                }
            }
        }

        // Commit 1-second slice to 120-second sliding-window estimator.
        if window_start.elapsed() >= Duration::from_secs(1) {
            if !paused {
                hr_window.commit_second();
            }
            window_start = Instant::now();
        }
        let chip_hashrates_ghs = hr_window.chip_ghs;
        let hashrate_ghs: f32 = hr_window.total_ghs();

        let max_chip_temp_c = chip_temps
            .iter()
            .copied()
            .flatten()
            .filter(|t| t.is_finite())
            .reduce(f32::max);
        let avg_chip_temp_c = {
            let mut sum = 0.0f32;
            let mut n = 0usize;
            for v in chip_temps.into_iter().flatten() {
                if v.is_finite() {
                    sum += v;
                    n += 1;
                }
            }
            if n > 0 {
                sum / n as f32
            } else {
                0.0
            }
        };
        let fan_rpm = fan_rpms[0].or(fan_rpms[1]);
        let push_series = last_series_push.elapsed() >= Duration::from_secs(2);
        if push_series {
            last_series_push = Instant::now();
        }

        // Fail-closed thermal / power cutoff & dynamic fan curve (~4 Hz).
        let eff_ceiling_w = power_config_for_preset(preset).effective_ceiling_w(vin_v);
        if tick % SENSOR_POLL_TICKS == 0 {
            let warmup_done = start_time.elapsed().as_secs() >= 5;
            if !fault_latched {
                if let Some(reason) = evaluate_safety_cutoff(
                    warmup_done,
                    vin_v,
                    pin_w,
                    eff_ceiling_w,
                    vr_temp_opt,
                    &chip_temps,
                    tps_comm_failures,
                ) {
                    log::error!(
                        "SAFETY CUTOFF TRIGGERED ({reason}): pin={pin_w:.1}W ceiling={eff_ceiling_w:.1}W vin={vin_v:.2}V vr={vr_temp_opt:?}"
                    );
                    if let Some(chain) = asic.as_mut() {
                        chain.hold_reset_low();
                    }
                    if let Some(bus) = i2c.as_mut() {
                        bus.emergency_shutdown();
                    }
                    fault_latched = true;
                    fault_reason = Some(String::from(reason));
                }
            }
            let thermal_emergency = fault_latched
                || max_chip_temp_c.is_none_or(|t| !t.is_finite() || t >= 72.0)
                || vr_temp_opt.is_none_or(|v| !v.is_finite() || v >= 80.0);
            // EMC2302 ch1 = VR fan (>= 40%, follows VRM temp); ch2 = ASIC fan (die-temp curve).
            let (target_vr_fan, target_fan) = if thermal_emergency {
                (100, 100)
            } else {
                let asic = if !fan_auto {
                    fan_manual_pct.min(100)
                } else {
                    eval_fan_curve_pct(&fan_curve, max_chip_temp_c.unwrap_or(80.0))
                };
                (vr_fan_duty_pct(vr_temp_opt), asic.max(ASIC_FAN_MIN_PCT))
            };
            if target_fan != fan_pct || target_vr_fan != vr_fan_pct {
                if let Some(bus) = i2c.as_mut() {
                    if bus.set_fan_duties(target_vr_fan, target_fan).is_ok() {
                        fan_pct = target_fan;
                        vr_fan_pct = target_vr_fan;
                    }
                } else {
                    fan_pct = target_fan;
                    vr_fan_pct = target_vr_fan;
                }
            }
        }

        // Embedded auto-tuner cycle (every 15 seconds when enabled and actively mining).
        let uptime_s = start_time.elapsed().as_secs();
        if last_autotune_tick.elapsed() >= Duration::from_secs(AUTOTUNE_INTERVAL_S) {
            last_autotune_tick = Instant::now();
            if !fault_latched && !paused {
                let mut tuner_snap = telemetry.lock().ok().map(|g| g.tuner.clone());
                if let Some(mut tu) = tuner_snap.take() {
                    if tu.enabled {
                        let hr_ths = hashrate_ghs / 1000.0;
                        let eff_jth_now = if hr_ths > 0.1 && pin_w > 1.0 {
                            pin_w / hr_ths
                        } else {
                            999.0
                        };
                        let max_t = max_chip_temp_c.unwrap_or(avg_chip_temp_c);
                        let vr_t = vr_temp_opt.unwrap_or(0.0);
                        let mut event_msg: Option<String> = None;

                        if tu.avg_ticks < tu.avg_needed {
                            tu.avg_ticks += 1;
                            tu.phase = String::from("warmup");
                            tu.per_chip_phase = String::from("waiting");
                            if tu.avg_ticks == 1 {
                                event_msg = Some(format!(
                                    "Warmup sampling ({}/{})",
                                    tu.avg_ticks, tu.avg_needed
                                ));
                            }
                        } else {
                            if hr_ths >= 0.3 && pin_w > 1.0 {
                                let better_eff = tu
                                    .best_j_th
                                    .is_none_or(|b| eff_jth_now < b && (tu.profile != "max_hash"));
                                let better_hash = tu.profile == "max_hash"
                                    && tu.best_j_th.is_none_or(|b| eff_jth_now <= b * 1.05);
                                if better_eff || better_hash {
                                    tu.best_freq_mhz = Some(base_freq_mhz);
                                    tu.best_vcore_mv = Some(target_vcore_mv);
                                    tu.best_j_th = Some(eff_jth_now);
                                }
                            }

                            let (goal_f, goal_mv, prof_cap_w) =
                                nerdqx_profile_goal(tu.profile.as_str(), eff_ceiling_w);
                            let cap_w = prof_cap_w.min(eff_ceiling_w * 0.96);
                            // Max-hash walks the voltage curve with the clock; other profiles hold a fixed mV.
                            let mv_for = |f: f32| {
                                if tu.profile == "max_hash" { vcore_for_freq_mv(f) } else { goal_mv }
                            };
                            let climb_temp_c = tu.fan_target_c.clamp(58.0, TEMP_TARGET_C);
                            let safe_cool = max_t < TEMP_HOT_C && vr_t < VR_TEMP_LIMIT_C - 15.0;
                            let low_err = hr_window.board_err_pct <= 2.0;

                            let mut next_f = base_freq_mhz;
                            let mut next_mv = target_vcore_mv;
                            let mut action_desc = "holding";

                            if pin_w > cap_w || max_t >= TEMP_HOT_C || vr_t >= VR_TEMP_LIMIT_C - 15.0 {
                                next_f = (base_freq_mhz - 12.5).max(300.0);
                                next_mv = target_vcore_mv;
                                tu.phase = String::from("throttling");
                                action_desc = "thermal/power backoff";
                            } else if !low_err {
                                next_f = (base_freq_mhz - 12.5).max(300.0);
                                next_mv = target_vcore_mv.max(1150);
                                tu.phase = String::from("throttling");
                                action_desc = "hw-error backoff";
                            } else if let (Some(axis), Some(val)) =
                                (tu.target_axis.as_deref(), tu.target_value)
                            {
                                let (cur_v, tol) = if axis == "hashrate" {
                                    (hr_ths, (val * 0.02).max(0.08))
                                } else {
                                    (pin_w, (val * 0.02).max(2.0))
                                };
                                let diff = cur_v - val;
                                if diff > tol {
                                    next_f = (base_freq_mhz - 12.5).max(300.0);
                                    tu.phase = String::from("seeking");
                                    action_desc = "trimming clock to target";
                                } else if diff < -tol && pin_w < cap_w * 0.96 && safe_cool && max_t < climb_temp_c {
                                    next_f = (base_freq_mhz + 12.5).min(MAX_BASE_FREQ_MHZ);
                                    next_mv = vcore_for_freq_mv(next_f);
                                    tu.phase = String::from("climbing");
                                    action_desc = "climbing toward target";
                                } else {
                                    tu.phase = String::from(if tu.continuous { "tracking" } else { "holding" });
                                    if hr_window.board_err_pct < 0.5 && target_vcore_mv > 1090 {
                                        next_mv = target_vcore_mv - 10;
                                        action_desc = "undervolt trim (-10 mV)";
                                    }
                                }
                            } else if (tu.continuous || tu.phase != "holding") && safe_cool {
                                if base_freq_mhz + 6.0 < goal_f && pin_w < cap_w * 0.93 && max_t < climb_temp_c {
                                    next_f = (base_freq_mhz + 12.5).min(goal_f);
                                    next_mv = mv_for(next_f);
                                    tu.phase = String::from("climbing");
                                    action_desc = "climbing (+12.5 MHz)";
                                } else if base_freq_mhz - 6.0 > goal_f {
                                    next_f = (base_freq_mhz - 12.5).max(goal_f);
                                    next_mv = mv_for(next_f);
                                    tu.phase = String::from("seeking");
                                    action_desc = "stepping down (-12.5 MHz)";
                                } else {
                                    next_mv = mv_for(next_f);
                                    tu.phase = String::from(if tu.continuous { "tracking" } else { "holding" });
                                    if tu.phase != "holding" {
                                        action_desc = "settled in profile band";
                                    }
                                }
                            }

                            let prev_f = base_freq_mhz;
                            let prev_mv = target_vcore_mv;
                            let cur_offsets = chip_tuner.offsets_mhz();
                            if (next_f - prev_f).abs() >= 0.5 || next_mv != prev_mv {
                                apply_freq_and_vcore(
                                    &mut asic,
                                    &mut i2c,
                                    &mut base_freq_mhz,
                                    &mut target_vcore_mv,
                                    next_f,
                                    next_mv,
                                    &cur_offsets,
                                );
                                hr_window.reset();
                                tu.avg_ticks = 0;
                                event_msg = Some(format!(
                                    "{}: {:.0} MHz @ {} mV ({action_desc})",
                                    tu.profile, base_freq_mhz, target_vcore_mv
                                ));
                            }

                            // Stage-2 per-chip PLL optimization when all 4 die temps are valid.
                            if let [Some(t0), Some(t1), Some(t2), Some(t3)] = chip_temps {
                                let inp = TuneInput {
                                    chip_temps_c: [t0, t1, t2, t3],
                                    chip_hashrates_ghs,
                                    chip_error_pct: hr_window.chip_err_pct,
                                    pin_w,
                                    effective_ceiling_w: cap_w,
                                    vr_temp_c: vr_t,
                                };
                                let old_offsets = chip_tuner.offsets_mhz();
                                let out = chip_tuner.step(&inp);
                                tu.per_chip_phase = String::from("optimizing");
                                let mut changed = Vec::new();
                                for idx in 0..4 {
                                    if (out.offsets_mhz[idx] - old_offsets[idx]).abs() >= 0.5 {
                                        if let Some(chain) = asic.as_mut() {
                                            let _ = chain.set_chip_frequency(
                                                idx as u8,
                                                base_freq_mhz + out.offsets_mhz[idx],
                                            );
                                        }
                                        changed.push(format!("c{idx}:{:+.1}", out.offsets_mhz[idx]));
                                    }
                                }
                                if !changed.is_empty() {
                                    let desc = changed.join(" ");
                                    tu.per_chip_last_event = Some(desc.clone());
                                    if event_msg.is_none() {
                                        event_msg = Some(format!("Per-chip PLL: {desc}"));
                                    }
                                }
                            }
                        }

                        if let Some(msg) = event_msg {
                            tu.events.push(TunerLogEntry {
                                created_uptime_s: uptime_s,
                                frequency_mhz: base_freq_mhz,
                                core_voltage_mv: target_vcore_mv,
                                power_w: pin_w,
                                hashrate_ths: hr_ths,
                                efficiency_j_th: eff_jth_now,
                                asic_temp_c: max_t,
                                message: msg,
                            });
                            if tu.events.len() > MAX_TUNER_EVENTS {
                                let excess = tu.events.len() - MAX_TUNER_EVENTS;
                                tu.events.drain(0..excess);
                            }
                        }

                        if let Ok(mut guard) = telemetry.lock() {
                            if !guard.tuner.dirty {
                                guard.tuner = tu;
                            }
                        }
                    }
                }
            }
        }

        if let Ok(mut guard) = telemetry.lock() {
            guard.chip_hashrates_ghs = chip_hashrates_ghs;
            guard.chip_shares = chip_shares;
            guard.chip_offsets_mhz = chip_tuner.offsets_mhz();
            guard.chip_error_pct = hr_window.chip_err_pct;
            guard.fail_rate_pct = hr_window.board_err_pct;
            guard.hw_errors = hw_errors;
            guard.chip_temps_c = chip_temps;
            guard.asic_temp_c = avg_chip_temp_c;
            guard.vr_temp_c = vr_temp_opt.unwrap_or(0.0);
            guard.vin_v = vin_v;
            guard.iin_a = iin_a;
            guard.iout_a = iout_a;
            guard.pin_w = pin_w;
            guard.vcore_mv = vcore_mv;
            guard.target_vcore_mv = target_vcore_mv;
            guard.chip_count = detected_chips;
            guard.fault_reason = fault_reason.clone();
            guard.freq_mhz = base_freq_mhz;
            guard.fan_rpm = fan_rpms;
            guard.fan_pct = fan_pct;
            guard.fan_vr_pct = vr_fan_pct;
            guard.paused = paused;
            guard.shares_accepted = shares_accepted;
            guard.shares_rejected = shares_rejected;
            guard.best_diff = best_diff;
            guard.pool_diff = current_diff;
            guard.uptime_s = uptime_s;
            guard.pool_connected = stratum.is_some();
            guard.hashrate_ghs = hashrate_ghs;
            if push_series {
                let (free_int_kb, min_int_kb, free_psram_kb) = read_heap_kb();
                guard.free_internal_kb = free_int_kb;
                guard.min_internal_kb = min_int_kb;
                guard.free_psram_kb = free_psram_kb;
                guard.series_ghs.push(hashrate_ghs);
                if guard.series_ghs.len() > 60 {
                    let excess = guard.series_ghs.len() - 60;
                    guard.series_ghs.drain(0..excess);
                }
            }
        }

        // Render screen on every sensor poll or button state change (~4 Hz animation)
        if tick % RENDER_TICKS == 0 || btn_edge {
            let render_t0 = Instant::now();
            let eff_jth = if hashrate_ghs > 0.1 && pin_w > 0.1 {
                Some(pin_w / (hashrate_ghs / 1000.0))
            } else {
                None
            };
            let fb = fbs[(frame as usize) & 1].as_mut_slice();

            let hud = if matches!(
                active_screen,
                Screen::ChipDieGrid | Screen::TunerPsu | Screen::PoolShares | Screen::ClockBtc
            ) {
                let nbits = latest_notify.as_ref().map_or(0, |j| parse_hex_u32(&j.nbits));
                telemetry.lock().ok().map(|g| hud_snapshot(&g, vr_temp_opt, nbits))
            } else {
                None
            };
            match (active_screen, hud.as_ref()) {
                (Screen::ChipDieGrid, Some(d)) => draw_chip_grid(fb, d, frame, ui_theme),
                (Screen::TunerPsu, Some(d)) => draw_tuner_psu(fb, d, frame, ui_theme),
                (Screen::PoolShares, Some(d)) => draw_pool_shares(fb, d, frame, ui_theme),
                (Screen::ClockBtc, Some(d)) => draw_clock(fb, d, frame, ui_theme),
                (Screen::SystemQr, _) => {
                    // Alternate Wi-Fi join / dashboard QR every ~6 s (60 frames at ~10 FPS).
                    let qr_mode = if (frame / 60) % 2 == 0 { qr_base_mode } else { qr_base_mode.toggle() };
                    let qr = match qr_mode {
                        QrMode::WifiJoin => &qr_wifi,
                        QrMode::DashboardUrl => &qr_url,
                    };
                    draw_ap_qr_screen(
                        fb,
                        qr,
                        qr_mode,
                        &active_ssid,
                        is_ap_mode,
                        &ip_addr,
                        max_chip_temp_c,
                        vr_temp_opt,
                        ui_theme,
                    );
                    draw_page_dots(fb, Screen::SystemQr, ui_theme);
                }
                _ => {
                    draw_main_hud(
                        fb,
                        hashrate_ghs,
                        max_chip_temp_c,
                        vr_temp_opt,
                        eff_jth,
                        fan_rpm,
                        &active_ssid,
                        is_ap_mode,
                        &ip_addr,
                        preset,
                        frame,
                        ui_theme,
                    );
                    draw_page_dots(fb, Screen::MainHud, ui_theme);
                }
            }

            if mujina_esp32::hud_data::SHOT_REQ.swap(false, std::sync::atomic::Ordering::SeqCst) {
                if let Ok(mut shot) = mujina_esp32::hud_data::FRAME_SHOT.lock() {
                    shot.clear();
                    shot.extend_from_slice(&fb[..]);
                }
            }
            let draw_t0 = Instant::now();
            st_render_us += (draw_t0 - render_t0).as_micros() as u64;
            if let Some(p) = panel.as_mut() {
                let _ = p.draw_full(fb);
            }
            st_draw_us += draw_t0.elapsed().as_micros() as u64;
            st_frames += 1;
            frame = frame.wrapping_add(1);
        }
        st_loop_max_us = st_loop_max_us.max(loop_t0.elapsed().as_micros() as u64);
        if st_window.elapsed() >= Duration::from_secs(10) && st_frames > 0 {
            use std::sync::atomic::Ordering::Relaxed;
            let secs = st_window.elapsed().as_secs_f32();
            let fps_x10 = (st_frames as f32 / secs * 10.0) as u32;
            let render_us = (st_render_us / st_frames as u64) as u32;
            let draw_us = (st_draw_us / st_frames as u64) as u32;
            mujina_esp32::hud_data::DISPLAY_FPS_X10.store(fps_x10, Relaxed);
            mujina_esp32::hud_data::DISPLAY_RENDER_US.store(render_us, Relaxed);
            mujina_esp32::hud_data::DISPLAY_DRAW_US.store(draw_us, Relaxed);
            mujina_esp32::hud_data::DISPLAY_LOOP_MAX_US.store(st_loop_max_us as u32, Relaxed);
            log::info!(
                "display: {:.1} fps, render {:.1} ms, panel transfer {:.1} ms, longest loop {:.1} ms",
                fps_x10 as f32 / 10.0,
                render_us as f32 / 1000.0,
                draw_us as f32 / 1000.0,
                st_loop_max_us as f32 / 1000.0
            );
            st_window = Instant::now();
            st_frames = 0;
            st_render_us = 0;
            st_draw_us = 0;
            st_loop_max_us = 0;
        }

        tick = tick.wrapping_add(1);
        // Pace to a fixed period: sleep only for what is left of the tick, so the
        // render time does not stretch the frame interval.
        let spent = loop_t0.elapsed();
        let budget = Duration::from_millis(LOOP_TICK_MS);
        thread::sleep(if spent < budget { budget - spent } else { Duration::from_millis(1) });
    }
}
