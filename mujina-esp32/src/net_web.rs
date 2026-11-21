//! ESP-IDF Wi-Fi, embedded HTTP API, and Stratum V1 client for NerdQX (ESP32-S3).

use std::fmt::Write as FmtWrite;
use std::io::{self, Read as IoRead, Write as IoWrite};
use std::net::{TcpStream, ToSocketAddrs};
#[cfg(feature = "esp32s3")]
use std::net::{TcpListener, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub use crate::captive_dns::build_captive_dns_reply;
#[cfg(feature = "esp32s3")]
use crate::captive_dns::{parse_http_head, ParsedHttpHead};
use crate::lcd_hud::UiTheme;

#[cfg(feature = "esp32s3")]
use esp_idf_svc::eventloop::EspSystemEventLoop;
#[cfg(feature = "esp32s3")]
use esp_idf_svc::hal::modem::Modem;
#[cfg(feature = "esp32s3")]
use esp_idf_svc::ipv4;
#[cfg(feature = "esp32s3")]
use esp_idf_svc::netif::{EspNetif, NetifConfiguration, NetifStack};
#[cfg(feature = "esp32s3")]
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};
#[cfg(feature = "esp32s3")]
use esp_idf_svc::sys::EspError;
#[cfg(feature = "esp32s3")]
use esp_idf_svc::wifi::{
    AccessPointConfiguration, AuthMethod, BlockingWifi, ClientConfiguration, Configuration,
    EspWifi, WifiDriver,
};

#[cfg(feature = "esp32s3")]
static DASHBOARD_HTML_GZ: &[u8] = include_bytes!("../assets/dashboard.html.gz");
/// Raccoon head mark shown next to the MUJINA wordmark in the dashboard header.
static HEAD_MARK_SVG_GZ: &[u8] = include_bytes!("../assets/mujina-head-mark.svg.gz");

#[cfg(feature = "esp32s3")]
static AUTH_JS: &[u8] = b"/* embedded */\n";

#[cfg(feature = "esp32s3")]

#[cfg(feature = "esp32s3")]
static LED_HTML: &str = r##"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>MUJINA // DISPLAY</title>
<style>
:root{--cyan:#00f0ff;--bg:#05060c;--panel:#0a0f1c;--line:#14243a;--ink:#d6f7ff;--dim:#4a6a86;--green:#1bff9c}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--ink);font-family:monospace}
.wrap{max-width:760px;margin:0 auto;padding:24px 18px}.top{display:flex;justify-content:space-between;align-items:baseline;border-bottom:1px solid var(--line);padding-bottom:12px;margin-bottom:14px}
h1{margin:0;font-size:20px;letter-spacing:.14em}.menubar{display:flex;gap:8px;flex-wrap:wrap;margin-bottom:18px}
.menubtn{background:#0a1120;border:1px solid var(--line);color:var(--cyan);text-decoration:none;padding:8px 13px;border-radius:4px;font-size:12px;letter-spacing:.1em;text-transform:uppercase}
.menubtn.on{border-color:var(--cyan);background:rgba(0,240,255,.1)}
.card{background:var(--panel);border:1px solid var(--line);border-radius:8px;padding:18px}
.row{display:flex;justify-content:space-between;align-items:center;gap:12px;margin-top:12px}
select,.btn{background:#0d1424;color:var(--ink);border:1px solid var(--line);border-radius:6px;padding:8px 12px;font:inherit}
.btn{cursor:pointer;color:var(--cyan);border-color:var(--cyan)}
.msg{margin-top:10px;font-size:12px;color:var(--green)}
</style></head><body><div class="wrap">
<div class="top"><h1>// MUJINA DISPLAY</h1><span style="color:var(--dim)">NerdQX ST7789 LCD</span></div>
<div class="menubar"><a class="menubtn" href="/dashboard">Dashboard</a><a class="menubtn on" href="/led">LED / Display</a><a class="menubtn" href="/info">Info</a></div>
<div class="card">
<p style="margin-top:0;color:var(--dim);font-size:13px">NerdQX drives a 320&times;170 LilyGO ST7789 front LCD instead of external WS2812 LEDs. Theme changes sync both the LCD and web dashboard.</p>
<div class="row"><label for="t">LCD &amp; Dashboard Theme</label><select id="t"><option value="cyberpunk">Cyberpunk Neon</option><option value="minimal">Minimal</option><option value="parasite">Parasite Retro B&amp;W</option></select></div>
<div class="row" style="justify-content:flex-end"><button class="btn" id="save">Apply Theme</button></div>
<div class="msg" id="m"></div>
</div></div><script>
const t=document.getElementById('t'),m=document.getElementById('m');
fetch('/api/v0/display').then(r=>r.json()).then(d=>{if(d&&d.theme)t.value=d.theme;}).catch(()=>{});
document.getElementById('save').onclick=async()=>{
  try{localStorage.setItem('mujina-theme',t.value);}catch(e){}
  const r=await fetch('/api/v0/display',{method:'PATCH',headers:{'Content-Type':'application/json'},body:JSON.stringify({theme:t.value})});
  m.textContent=r.ok?'Applied theme: '+t.value:'Failed ('+r.status+')';
};
</script></body></html>"##;

#[cfg(feature = "esp32s3")]
static INFO_HTML: &str = r##"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>MUJINA // INFO</title>
<style>
:root{--cyan:#00f0ff;--amber:#ffb000;--green:#1bff9c;--red:#ff3860;--bg:#05060c;--panel:#0a0f1c;--line:#14243a;--dim:#4a6a86;--ink:#d6f7ff}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--ink);font-family:monospace;font-variant-numeric:tabular-nums}
.wrap{max-width:1080px;margin:0 auto;padding:24px 18px 50px}.top{display:flex;justify-content:space-between;align-items:baseline;border-bottom:1px solid var(--line);padding-bottom:12px;margin-bottom:14px;gap:10px;flex-wrap:wrap}
h1{margin:0;font-size:20px;letter-spacing:.14em}.menubar{display:flex;gap:8px;flex-wrap:wrap;margin-bottom:16px}
.menubtn{background:#0a1120;border:1px solid var(--line);color:var(--cyan);text-decoration:none;padding:8px 13px;border-radius:4px;font-size:12px;letter-spacing:.1em;text-transform:uppercase}
.menubtn.on{border-color:var(--cyan);background:rgba(0,240,255,.1)}
.sec{font-size:11px;letter-spacing:.14em;text-transform:uppercase;color:var(--dim);margin:18px 0 8px}
.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(160px,1fr));gap:10px}
.stat{background:var(--panel);border:1px solid var(--line);border-radius:6px;padding:10px 12px}
.stat .lbl{font-size:10px;color:var(--dim);text-transform:uppercase;letter-spacing:.1em}.stat .val{font-size:18px;font-weight:700;margin-top:4px}
.val.cyan{color:var(--cyan)}.val.amber{color:var(--amber)}.val.green{color:var(--green)}.val.red{color:var(--red)}
.tw{overflow-x:auto;border:1px solid var(--line);border-radius:6px;background:var(--panel)}
table{width:100%;border-collapse:collapse;font-size:13px}th,td{padding:8px 10px;border-bottom:1px solid var(--line);text-align:right;white-space:nowrap}
th:first-child,td:first-child{text-align:left}th{color:var(--dim);font-size:11px;text-transform:uppercase}
</style></head><body><div class="wrap">
<div class="top"><h1>// MUJINA INFO</h1><span id="conn" style="color:var(--dim)">connecting...</span></div>
<div class="menubar"><a class="menubtn" href="/dashboard">Dashboard</a><a class="menubtn" href="/led">LED / Display</a><a class="menubtn on" href="/info">Info</a></div>
<div id="body">loading...</div></div><script>
const f1=v=>v==null?'--':(+v).toFixed(1),f2=v=>v==null?'--':(+v).toFixed(2);
const stat=(l,v,u,c)=>`<div class="stat"><div class="lbl">${l}</div><div class="val ${c||''}">${v}${u?' <small>'+u+'</small>':''}</div></div>`;
function render(d){
  let h='<div class="sec">Chain Summary</div><div class="grid">';
  h+=stat('Hashrate',f1(d.ghsspd),'GH/s','cyan');
  h+=stat('Fail / Err Rate',f2(d.spd_dh),'%',d.spd_dh>2?'amber':'green');
  h+=stat('Temp Avg / Max',f1(d.temp_avg)+' / '+f1(d.temp_max),'C',d.temp_max>70?'red':'');
  h+=stat('Chips Online',d.asics_total,'of 4','');
  h+=stat('Chain State',d.paused?'PAUSED':'MINING','',d.paused?'amber':'green');
  h+=stat('Base Freq',((d.pll_freq_commanded||[])[0]||'--'),'MHz','');
  h+=stat('Core Voltage',d.voltage_mv_commanded,'mV','amber');
  h+='</div><div class="sec">TPS53647 Power &amp; Stratum</div><div class="grid">';
  h+=stat('Input Voltage',f2(d.ina_bus_v),'V','');
  h+=stat('Input Current',f2(d.ina_current_a),'A','');
  h+=stat('Input Power',f1(d.ina_power_w),'W','amber');
  h+=stat('Pool Diff',f2(d.difficulty),'','');
  h+=stat('Shares Accepted',d.shares_accepted,'','green');
  h+=stat('HW Errors',d.err_crc,'',d.err_crc>0?'amber':'');
  h+='</div><div class="sec">Per-Chip BM1370 Telemetry</div><div class="tw"><table><thead><tr><th>Chip</th><th>Temp C</th><th>Freq MHz</th><th>Offset MHz</th><th>Hashrate</th><th>Error %</th><th>Hits</th></tr></thead><tbody>';
  const offs=d.chip_offset_mhz||[];
  (d.chips||[]).forEach((c,i)=>{
    const off=offs[i]!=null?offs[i]:0;
    h+=`<tr><td>chip${c.chip}</td><td>${f1(c.temp_c)}</td><td>${f1((c.pll_freq||[])[0])}</td><td>${(off>=0?'+':'')+f2(off)}</td><td>${f1(c.ghsspd)} GH/s</td><td>${f2(c.spd_dh)}%</td><td>${c.nonce_data}</td></tr>`;
  });
  h+='</tbody></table></div>';
  document.getElementById('body').innerHTML=h;
}
async function tick(){
  try{const r=await fetch('/nano3s-detail');if(!r.ok)throw new Error(r.status);
    const d=await r.json();document.getElementById('conn').textContent='live';render(d);
  }catch(e){document.getElementById('conn').textContent='error: '+e;}
}
tick();setInterval(tick,2000);
</script></body></html>"##;

/// Default fallback SoftAP SSID when no Wi-Fi credentials are configured.
pub const FALLBACK_AP_SSID: &str = "Mujina-NerdQX";

#[cfg(feature = "esp32s3")]
const NVS_NAMESPACE: &str = "mujina";
const DEFAULT_POOL_HOST: &str = "parasite.wtf";
const DEFAULT_POOL_PORT: u16 = 42069;
const DEFAULT_POOL_PASS: &str = "x";
const DEFAULT_UI_THEME: &str = "cyberpunk";
const AP_MODE_IP: &str = "192.168.71.1";

/// Map an ESP-IDF `esp_reset_reason_t` numeric code (`0..=15`) to a stable telemetry label.
pub fn reset_reason_name(code: u32) -> &'static str {
    match code {
        0 => "unknown",
        1 => "poweron",
        2 => "ext",
        3 => "sw",
        4 => "panic",
        5 => "int_wdt",
        6 => "task_wdt",
        7 => "wdt",
        8 => "deepsleep",
        9 => "brownout",
        10 => "sdio",
        11 => "usb",
        12 => "jtag",
        13 => "efuse",
        14 => "pwr_glitch",
        15 => "cpu_lockup",
        _ => "unknown",
    }
}

/// Persistent Wi-Fi, Stratum pool, and UI theme configuration stored in ESP-IDF NVS.
#[derive(Clone, Debug)]
pub struct DeviceConfig {
    pub wifi_ssid: String,
    pub wifi_pass: String,
    pub pool_host: String,
    pub pool_port: u16,
    pub pool_user: String,
    pub pool_pass: String,
    pub ui_theme: String,
    /// Last selected tuner profile (`quiet`, `efficient`, `balanced`, `max_hash`); picks the boot point.
    pub tuner_profile: String,
}

impl Default for DeviceConfig {
    fn default() -> Self {
        Self {
            wifi_ssid: String::new(),
            wifi_pass: String::new(),
            pool_host: String::from(DEFAULT_POOL_HOST),
            pool_port: DEFAULT_POOL_PORT,
            pool_user: String::new(),
            pool_pass: String::from(DEFAULT_POOL_PASS),
            ui_theme: String::from(DEFAULT_UI_THEME),
            tuner_profile: String::from("efficient"),
        }
    }
}

#[cfg(feature = "esp32s3")]
fn nvs_get_string(nvs: &EspNvs<NvsDefault>, key: &str) -> Option<String> {
    let mut buf = [0u8; 256];
    nvs.get_str(key, &mut buf).ok().flatten().map(str::to_owned)
}

/// Load [`DeviceConfig`] from the `"mujina"` NVS namespace, applying defaults
/// for unset pool fields (`parasite.wtf:42069`, password `"x"`) and UI theme (`"cyberpunk"`).
#[cfg(feature = "esp32s3")]
pub fn load_config_nvs(nvs: &EspDefaultNvsPartition) -> DeviceConfig {
    let default_cfg = DeviceConfig::default();

    let Ok(store) = EspNvs::<NvsDefault>::new(nvs.clone(), NVS_NAMESPACE, false) else {
        return default_cfg;
    };

    let wifi_ssid = nvs_get_string(&store, "wifi_ssid").unwrap_or_default();
    let wifi_pass = nvs_get_string(&store, "wifi_pass").unwrap_or_default();
    let pool_host = nvs_get_string(&store, "pool_host")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| String::from(DEFAULT_POOL_HOST));
    let pool_port = store
        .get_u16("pool_port")
        .ok()
        .flatten()
        .filter(|&p| p != 0)
        .unwrap_or(DEFAULT_POOL_PORT);
    let pool_user = nvs_get_string(&store, "pool_user").unwrap_or_default();
    let pool_pass = nvs_get_string(&store, "pool_pass")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| String::from(DEFAULT_POOL_PASS));
    let ui_theme = nvs_get_string(&store, "ui_theme")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| String::from(DEFAULT_UI_THEME));
    let tuner_profile = nvs_get_string(&store, "tuner_profile")
        .filter(|s| matches!(s.as_str(), "quiet" | "efficient" | "balanced" | "max_hash"))
        .unwrap_or_else(|| String::from("efficient"));

    DeviceConfig {
        wifi_ssid,
        wifi_pass,
        pool_host,
        pool_port,
        pool_user,
        pool_pass,
        ui_theme,
        tuner_profile,
    }
}

/// Persist [`DeviceConfig`] into the `"mujina"` NVS namespace.
#[cfg(feature = "esp32s3")]
pub fn save_config_nvs(nvs: &EspDefaultNvsPartition, cfg: &DeviceConfig) -> Result<(), EspError> {
    let mut store = EspNvs::<NvsDefault>::new(nvs.clone(), NVS_NAMESPACE, true)?;
    store.set_str("wifi_ssid", &cfg.wifi_ssid)?;
    store.set_str("wifi_pass", &cfg.wifi_pass)?;
    store.set_str("pool_host", &cfg.pool_host)?;
    store.set_u16("pool_port", cfg.pool_port)?;
    store.set_str("pool_user", &cfg.pool_user)?;
    store.set_str("pool_pass", &cfg.pool_pass)?;
    let theme = if cfg.ui_theme.is_empty() {
        DEFAULT_UI_THEME
    } else {
        cfg.ui_theme.as_str()
    };
    store.set_str("ui_theme", theme)?;
    store.set_str("tuner_profile", &cfg.tuner_profile)?;
    Ok(())
}

/// Activity log item exposed via `/api/v0/boards/nerdqx/autotune`.
#[derive(Debug, Clone)]
pub struct TunerLogEntry {
    pub created_uptime_s: u64,
    pub frequency_mhz: f32,
    pub core_voltage_mv: u16,
    pub power_w: f32,
    pub hashrate_ths: f32,
    pub efficiency_j_th: f32,
    pub asic_temp_c: f32,
    pub message: String,
}

/// Embedded auto-tuner control & telemetry snapshot shared with the dashboard UI.
#[derive(Debug, Clone)]
pub struct EmbeddedTunerState {
    pub enabled: bool,
    pub profile: String,
    pub target_axis: Option<String>,
    pub target_value: Option<f32>,
    pub continuous: bool,
    pub phase: String,
    pub fan_target_c: f32,
    pub best_freq_mhz: Option<f32>,
    pub best_vcore_mv: Option<u16>,
    pub best_j_th: Option<f32>,
    pub avg_ticks: u32,
    pub avg_needed: u32,
    pub per_chip_phase: String,
    pub per_chip_last_event: Option<String>,
    pub events: Vec<TunerLogEntry>,
    pub dirty: bool,
}

impl Default for EmbeddedTunerState {
    fn default() -> Self {
        Self {
            enabled: true,
            profile: String::from("efficient"),
            target_axis: None,
            target_value: None,
            continuous: true,
            phase: String::from("warmup"),
            fan_target_c: 60.0,
            best_freq_mhz: None,
            best_vcore_mv: None,
            best_j_th: None,
            avg_ticks: 0,
            avg_needed: 3,
            per_chip_phase: String::from("waiting"),
            per_chip_last_event: None,
            events: Vec::new(),
            dirty: false,
        }
    }
}

/// Live miner telemetry shared between the mining loop, HUD, and HTTP API.
#[derive(Debug, Clone)]
pub struct LiveTelemetry {
    pub hashrate_ghs: f32,
    pub chip_hashrates_ghs: [f32; 4],
    pub chip_shares: [u32; 4],
    pub chip_offsets_mhz: [f32; 4],
    pub chip_error_pct: [f32; 4],
    pub fail_rate_pct: f32,
    pub hw_errors: u32,
    pub chip_temps_c: [Option<f32>; 4],
    pub asic_temp_c: f32,
    pub vr_temp_c: f32,
    pub vin_v: f32,
    pub iin_a: f32,
    pub iout_a: f32,
    pub pin_w: f32,
    pub vcore_mv: u16,
    pub target_vcore_mv: u16,
    pub pending_manual_tune: Option<(f32, u16)>,
    pub chip_count: u8,
    pub fault_reason: Option<String>,
    pub freq_mhz: f32,
    pub fan_rpm: [Option<u16>; 2],
    pub fan_pct: u8,
    /// VR fan (EMC2302 ch1) duty; `fan_pct` is the ASIC fan (ch2).
    pub fan_vr_pct: u8,
    /// Bitcoin tip height / USD price from mempool.space and CoinGecko (LCD clock screen).
    pub btc_height: Option<u32>,
    pub btc_price_usd: Option<f64>,
    /// LCD screen index requested over HTTP (`PATCH /api/v0/display/screen`), consumed by the display loop.
    pub screen_req: Option<u8>,
    pub fan_auto: bool,
    pub fan_manual_pct: u8,
    pub fan_curve: Vec<(f32, u8)>,
    pub psu_preset: &'static str,
    pub psu_budget_w: f32,
    pub ip_addr: String,
    pub wifi_ssid: String,
    pub is_ap_mode: bool,
    pub paused: bool,
    pub pause_dirty: Option<bool>,
    pub shares_accepted: u64,
    pub shares_rejected: u64,
    pub best_diff: f64,
    pub pool_diff: f64,
    pub uptime_s: u64,
    pub pool_url: String,
    pub pool_user: String,
    pub pool_connected: bool,
    pub pool_dirty: bool,
    pub series_ghs: Vec<f32>,
    pub ui_theme: UiTheme,
    pub reset_reason: &'static str,
    pub free_internal_kb: u32,
    pub min_internal_kb: u32,
    pub free_psram_kb: u32,
    pub tuner: EmbeddedTunerState,
}

impl Default for LiveTelemetry {
    fn default() -> Self {
        Self {
            hashrate_ghs: 0.0,
            chip_hashrates_ghs: [0.0; 4],
            chip_shares: [0; 4],
            chip_offsets_mhz: [0.0; 4],
            chip_error_pct: [0.0; 4],
            fail_rate_pct: 0.0,
            hw_errors: 0,
            chip_temps_c: [None; 4],
            asic_temp_c: 0.0,
            vr_temp_c: 0.0,
            vin_v: 0.0,
            iin_a: 0.0,
            iout_a: 0.0,
            pin_w: 0.0,
            vcore_mv: 1250,
            target_vcore_mv: 1250,
            pending_manual_tune: None,
            chip_count: 0,
            fault_reason: None,
            freq_mhz: 777.0,
            fan_rpm: [None; 2],
            fan_pct: 50,
            fan_vr_pct: 60,
            btc_height: None,
            btc_price_usd: None,
            screen_req: None,
            fan_auto: true,
            fan_manual_pct: 25,
            fan_curve: vec![(50.0, 30), (58.0, 50), (60.0, 80), (62.0, 100)],
            psu_preset: "stock",
            psu_budget_w: 144.0,
            ip_addr: String::from(AP_MODE_IP),
            wifi_ssid: String::from(FALLBACK_AP_SSID),
            is_ap_mode: true,
            paused: false,
            pause_dirty: None,
            shares_accepted: 0,
            shares_rejected: 0,
            best_diff: 0.0,
            pool_diff: 512.0,
            uptime_s: 0,
            pool_url: format!("stratum+tcp://{DEFAULT_POOL_HOST}:{DEFAULT_POOL_PORT}"),
            pool_user: String::new(),
            pool_connected: false,
            pool_dirty: false,
            series_ghs: Vec::new(),
            ui_theme: UiTheme::Cyberpunk,
            reset_reason: "unknown",
            free_internal_kb: 0,
            min_internal_kb: 0,
            free_psram_kb: 0,
            tuner: EmbeddedTunerState::default(),
        }
    }
}

/// Thread-safe handle to [`LiveTelemetry`].
pub type SharedTelemetry = Arc<Mutex<LiveTelemetry>>;

/// Create a new [`SharedTelemetry`] initialized with defaults.
pub fn new_shared_telemetry() -> SharedTelemetry {
    Arc::new(Mutex::new(LiveTelemetry::default()))
}

/// Connect to Wi-Fi in STA mode using `ssid`/`password`, or fall back to SoftAP
/// (`Mujina-NerdQX`) if `ssid` is empty or STA connection fails.
///
/// The SoftAP netif explicitly advertises `192.168.71.1` as its DHCP DNS server
/// (overriding `esp-idf-svc`'s `8.8.8.8` default) so iOS/Android automatically pop
/// up the captive portal when joining.
#[cfg(feature = "esp32s3")]
pub fn connect_wifi(
    modem: Modem,
    sysloop: EspSystemEventLoop,
    nvs: EspDefaultNvsPartition,
    ssid: &str,
    password: &str,
) -> Result<(BlockingWifi<EspWifi<'static>>, String, bool, String), EspError> {
    let mut ap_conf = NetifConfiguration::wifi_default_router();
    ap_conf.ip_configuration = Some(ipv4::Configuration::Router(ipv4::RouterConfiguration {
        subnet: ipv4::Subnet {
            gateway: ipv4::Ipv4Addr::new(192, 168, 71, 1),
            mask: ipv4::Mask(24),
        },
        dhcp_enabled: true,
        dns: Some(ipv4::Ipv4Addr::new(192, 168, 71, 1)),
        secondary_dns: Some(ipv4::Ipv4Addr::new(192, 168, 71, 1)),
    }));
    let sta_netif = EspNetif::new(NetifStack::Sta)?;
    let ap_netif = EspNetif::new_with_conf(&ap_conf)?;
    let driver = WifiDriver::new(modem, sysloop.clone(), Some(nvs))?;
    let esp_wifi = EspWifi::wrap_all(driver, sta_netif, ap_netif)?;
    let mut wifi = BlockingWifi::wrap(esp_wifi, sysloop)?;

    if !ssid.is_empty() {
        let auth_method = if password.is_empty() {
            AuthMethod::None
        } else {
            AuthMethod::WPA2Personal
        };
        let sta_cfg = ClientConfiguration {
            ssid: ssid.try_into().unwrap_or_default(),
            password: password.try_into().unwrap_or_default(),
            auth_method,
            ..Default::default()
        };
        wifi.set_configuration(&Configuration::Client(sta_cfg))?;
        wifi.start()?;
        match wifi.connect().and_then(|()| wifi.wait_netif_up()) {
            Ok(()) => {
                unsafe {
                    let _ = esp_idf_svc::sys::esp_wifi_set_ps(
                        esp_idf_svc::sys::wifi_ps_type_t_WIFI_PS_NONE,
                    );
                }
                let ip = wifi.wifi().sta_netif().get_ip_info()?.ip.to_string();
                log::info!("Wi-Fi STA connected ({ssid}): {ip}");
                return Ok((wifi, ip, false, String::from(ssid)));
            }
            Err(e) => {
                log::warn!("Wi-Fi STA failed ({e:?}), falling back to AP {FALLBACK_AP_SSID}");
                let _ = wifi.stop();
            }
        }
    }

    let ap_cfg = AccessPointConfiguration {
        ssid: FALLBACK_AP_SSID.try_into().unwrap_or_default(),
        ssid_hidden: false,
        channel: 1,
        max_connections: 4,
        auth_method: AuthMethod::None,
        ..Default::default()
    };
    wifi.set_configuration(&Configuration::AccessPoint(ap_cfg))?;
    wifi.start()?;
    wifi.wait_netif_up()?;
    let ap_info = wifi.wifi().ap_netif().get_ip_info()?;
    let ip = ap_info.ip.to_string();
    log::info!(
        "Wi-Fi SoftAP active ({FALLBACK_AP_SSID}): ip={ip}, dns={:?}",
        ap_info.dns
    );
    Ok((wifi, ip, true, String::from(FALLBACK_AP_SSID)))
}

/// Start a lightweight UDP port 53 DNS responder that resolves every `A`/`ANY` query
/// to `192.168.71.1` so phones trigger captive-portal popup automatically in SoftAP mode.
#[cfg(feature = "esp32s3")]
pub fn start_captive_dns_server() {
    let sock = match UdpSocket::bind("0.0.0.0:53") {
        Ok(s) => s,
        Err(e) => {
            log::warn!("Captive portal DNS failed to bind 0.0.0.0:53: {e:?}");
            return;
        }
    };
    log::info!("Captive portal DNS server listening on 0.0.0.0:53 -> {AP_MODE_IP}");

    let _ = std::thread::Builder::new()
        .name(String::from("captive_dns"))
        .stack_size(8192)
        .spawn(move || {
            let mut buf = [0u8; 512];
            loop {
                let Ok((n, src)) = sock.recv_from(&mut buf) else {
                    continue;
                };
                if let Some(reply) = build_captive_dns_reply(&buf[..n], [192, 168, 71, 1]) {
                    let _ = sock.send_to(&reply, src);
                }
            }
        });
}

/// Guard handle returned by [`start_http_server`].
pub struct HttpServerHandle;

#[cfg(feature = "esp32s3")]
const MAX_HTTP_HEADER_BYTES: usize = 8192;
#[cfg(feature = "esp32s3")]
const MAX_HTTP_BODY_BYTES: usize = 4096;

#[cfg(feature = "esp32s3")]
fn read_http_request(stream: &mut TcpStream) -> Option<(ParsedHttpHead, Vec<u8>)> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(15)));

    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 512];
    let head = loop {
        if let Some(parsed) = parse_http_head(&buf) {
            break parsed;
        }
        if buf.len() >= MAX_HTTP_HEADER_BYTES {
            return None;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };

    let want_body = head.content_length.min(MAX_HTTP_BODY_BYTES);
    let mut body = buf[head.body_offset..].to_vec();
    while body.len() < want_body {
        let need = want_body - body.len();
        let to_read = need.min(chunk.len());
        match stream.read(&mut chunk[..to_read]) {
            Ok(0) | Err(_) => break,
            Ok(n) => body.extend_from_slice(&chunk[..n]),
        }
    }
    if body.len() > want_body {
        body.truncate(want_body);
    }

    Some((head, body))
}

#[cfg(feature = "esp32s3")]
fn write_http_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    extra_headers: &[(&str, &str)],
    body: &[u8],
) {
    let mut hdr = String::with_capacity(256);
    let _ = write!(
        hdr,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in extra_headers {
        let _ = write!(hdr, "{k}: {v}\r\n");
    }
    hdr.push_str("\r\n");
    let _ = stream.write_all(hdr.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

#[cfg(feature = "esp32s3")]
fn is_ap_local_host(host: &str) -> bool {
    let bare = host.split(':').next().unwrap_or("");
    bare.is_empty() || bare == AP_MODE_IP || bare.eq_ignore_ascii_case("localhost")
}

#[cfg(feature = "esp32s3")]
fn is_known_ap_path(path: &str) -> bool {
    matches!(
        path,
        "/" | "/setup" | "/api/v0/setup" | "/api/v0/config" | "/save"
    )
}

/// Map a PSU preset identifier to `(hud_label, api_code, power_budget_w, fuse_max_a)`,
/// aligned to `power_cooling` safety limits (15 A stock fuse max 180 W; 20 A tiers up to 240 W).
pub fn psu_info(preset: &str) -> Option<(&'static str, &'static str, f32, f32)> {
    match preset {
        "stock" | "1A" => Some(("stock", "1A", 144.0, 15.0)),
        "2A" | "12v15a" => Some(("2A", "2A", 180.0, 15.0)),
        "1B" | "upgraded_air" => Some(("1B", "1B", 192.0, 20.0)),
        "2" | "4A" | "12v20a" | "aio" => Some(("4A", "4A", 240.0, 20.0)),
        _ => None,
    }
}

/// Parse `PATCH /api/v0/boards/nerdqx/autotune` JSON body into updated `EmbeddedTunerState`.
pub fn parse_autotune_patch(
    body: &str,
    current: &EmbeddedTunerState,
) -> Result<EmbeddedTunerState, &'static str> {
    let enabled_opt = extract_json_bool_field(body, "\"enabled\"");
    let target_slice = extract_json_object_slice(body, "\"target\"");
    let target_null = target_slice.is_none() && extract_json_null_field(body, "\"target\"");
    let profile_opt = extract_json_string_field(body, "\"profile\"");
    let cont_opt = extract_json_bool_field(body, "\"continuous\"");

    let parsed_target = if let Some(ts) = target_slice {
        let axis = extract_json_string_field(ts, "\"axis\"").unwrap_or_else(|| String::from("power"));
        if axis != "power" && axis != "hashrate" {
            return Err("invalid target axis");
        }
        let val = extract_json_f64_field(ts, "\"value\"").ok_or("missing target value")? as f32;
        if !val.is_finite() {
            return Err("invalid target value");
        }
        let (min_v, max_v) = if axis == "hashrate" { (0.5, 12.0) } else { (20.0, 240.0) };
        if val < min_v || val > max_v {
            return Err("target value out of range");
        }
        Some((axis, val))
    } else {
        None
    };

    if let Some(ref p) = profile_opt {
        if !matches!(
            p.as_str(),
            "off" | "quiet" | "efficient" | "balanced" | "max_hash" | "target"
        ) {
            return Err("unknown tuner profile");
        }
    }

    let next_enabled = if let Some(e) = enabled_opt {
        e
    } else if parsed_target.is_some() {
        true
    } else if let Some(ref p) = profile_opt {
        p != "off"
    } else {
        current.enabled
    };

    let mut next = current.clone();
    if let Some(c) = cont_opt {
        next.continuous = c;
    }

    if !next_enabled {
        next.enabled = false;
        next.phase = String::from("off");
        next.per_chip_phase = String::from("off");
        next.target_axis = None;
        next.target_value = None;
        next.avg_ticks = 0;
        next.dirty = true;
        return Ok(next);
    }

    if let Some((axis, val)) = parsed_target {
        let changed = !current.enabled
            || current.target_axis.as_deref() != Some(axis.as_str())
            || current.target_value != Some(val);
        next.enabled = true;
        next.profile = String::from("target");
        next.fan_target_c = 60.0;
        next.target_axis = Some(axis);
        next.target_value = Some(val);
        if changed {
            next.phase = String::from("warmup");
            next.per_chip_phase = String::from("waiting");
            next.avg_ticks = 0;
            next.dirty = true;
        }
        return Ok(next);
    }

    if let Some(prof) = profile_opt {
        if prof != "off" && prof != "target" {
            let changed =
                !current.enabled || current.target_axis.is_some() || current.profile != prof;
            next.enabled = true;
            next.fan_target_c = match prof.as_str() {
                "quiet" => 61.0,
                "efficient" => 60.0,
                "max_hash" => 63.0,
                _ => 60.0,
            };
            next.profile = prof;
            next.target_axis = None;
            next.target_value = None;
            if changed {
                next.phase = String::from("warmup");
                next.per_chip_phase = String::from("waiting");
                next.avg_ticks = 0;
                next.dirty = true;
            }
            return Ok(next);
        }
    }

    next.enabled = true;
    if target_null {
        next.target_axis = None;
        next.target_value = None;
    }
    if !current.enabled {
        next.phase = String::from("warmup");
        next.per_chip_phase = String::from("waiting");
        next.avg_ticks = 0;
        next.dirty = true;
    }
    Ok(next)
}

/// Parse `PATCH /api/v0/boards/nerdqx/tuning` JSON body -> `(freq_mhz, core_voltage_mv)`.
pub fn parse_manual_tuning_patch(
    body: &str,
    cur_freq: f32,
    cur_mv: u16,
) -> Result<(f32, u16), &'static str> {
    let freq_opt = extract_json_f64_field(body, "\"frequency_mhz\"")
        .or_else(|| extract_json_f64_field(body, "\"freq_mhz\""));
    let mv_opt = extract_json_f64_field(body, "\"core_voltage_mv\"")
        .or_else(|| extract_json_f64_field(body, "\"voltage_mv\""));
    if freq_opt.is_none() && mv_opt.is_none() {
        return Err("missing frequency_mhz or core_voltage_mv");
    }
    let next_freq = if let Some(f) = freq_opt {
        if !f.is_finite() || !(50.0..=1200.0).contains(&f) {
            return Err("frequency_mhz out of range");
        }
        (f as f32).clamp(crate::chip_tuner::MIN_BASE_FREQ_MHZ, crate::chip_tuner::MAX_BASE_FREQ_MHZ)
    } else {
        cur_freq
    };
    let next_mv = if let Some(v) = mv_opt {
        if !v.is_finite() || !(500.0..=1600.0).contains(&v) {
            return Err("core_voltage_mv out of range");
        }
        (v.round() as i32).clamp(crate::chip_tuner::MIN_VCORE_MV as i32, crate::chip_tuner::MAX_VCORE_MV as i32) as u16
    } else {
        cur_mv
    };
    Ok((next_freq, next_mv))
}

/// Parse `PATCH /api/v0/boards/nerdqx/fan` (`{auto, percent}` or `{mode, manual_duty_percent}`).
pub fn parse_fan_patch(body: &str) -> Result<(bool, Option<u8>), &'static str> {
    let auto = if let Some(a) = extract_json_bool_field(body, "\"auto\"") {
        a
    } else if let Some(mode) = extract_json_string_field(body, "\"mode\"") {
        match mode.as_str() {
            "auto" => true,
            "manual" => false,
            _ => return Err("invalid fan mode"),
        }
    } else {
        return Err("missing auto or mode field");
    };
    let pct_opt = extract_json_f64_field(body, "\"percent\"")
        .or_else(|| extract_json_f64_field(body, "\"manual_duty_percent\""));
    let pct = if let Some(p) = pct_opt {
        if !p.is_finite() || !(0.0..=100.0).contains(&p) {
            return Err("fan percent out of range");
        }
        Some((p.round() as u8).clamp(25, 100))
    } else {
        None
    };
    if !auto && pct.is_none() {
        return Err("manual fan mode requires percent");
    }
    Ok((auto, pct))
}

/// Parse `PATCH /api/v0/boards/nerdqx/fan-curve` (`{"points":[{"temp_c":...,"duty_pct":...},...]}`).
pub fn parse_fan_curve_patch(body: &str) -> Result<Vec<(f32, u8)>, &'static str> {
    let slice = extract_bracket_slice(body, "\"points\"").ok_or("missing points array")?;
    let mut pts = Vec::new();
    let bytes = slice.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            let start = i;
            let mut depth = 0usize;
            while i < bytes.len() {
                if bytes[i] == b'{' {
                    depth += 1;
                } else if bytes[i] == b'}' {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        let obj = &slice[start..=i];
                        let t = extract_json_f64_field(obj, "\"temp_c\"")
                            .ok_or("missing point temp_c")?;
                        let d = extract_json_f64_field(obj, "\"duty_pct\"")
                            .ok_or("missing point duty_pct")?;
                        if !t.is_finite() || !(0.0..=150.0).contains(&t) {
                            return Err("point temp_c out of range");
                        }
                        if !d.is_finite() || !(0.0..=100.0).contains(&d) {
                            return Err("point duty_pct out of range");
                        }
                        pts.push((t as f32, (d.round() as u8).clamp(25, 100)));
                        break;
                    }
                }
                i += 1;
            }
        }
        i += 1;
    }
    if !(2..=8).contains(&pts.len()) {
        return Err("fan curve requires 2..=8 points");
    }
    pts.sort_by(|a, b| a.0.total_cmp(&b.0));
    Ok(pts)
}

/// Piecewise linear fan curve evaluation across sorted `(temp_c, duty_pct)` points (25..=100% floor).
pub fn eval_fan_curve_pct(points: &[(f32, u8)], temp_c: f32) -> u8 {
    if points.is_empty() || !temp_c.is_finite() {
        return 100;
    }
    if temp_c <= points[0].0 {
        return points[0].1.clamp(25, 100);
    }
    for win in points.windows(2) {
        let (t0, d0) = win[0];
        let (t1, d1) = win[1];
        if temp_c <= t1 {
            let span = t1 - t0;
            if span <= 0.01 {
                return d1.clamp(25, 100);
            }
            let frac = ((temp_c - t0) / span).clamp(0.0, 1.0);
            let duty = d0 as f32 + frac * (d1 as f32 - d0 as f32);
            return (duty.round() as i32).clamp(25, 100) as u8;
        }
    }
    points[points.len() - 1].1.clamp(25, 100)
}

/// Validate a `PATCH /api/v0/boards/nerdqx/psu` JSON body, enforcing `"confirm":true`
/// for any 20 A / >180 W tier and rejecting unsupported presets (`3A`, `5A`, etc.).
pub fn validate_psu_patch(body: &str) -> Result<(&'static str, &'static str, f32, f32), &'static str> {
    let preset = extract_json_string_field(body, "\"preset\"").ok_or("missing preset field")?;
    let info = psu_info(preset.trim()).ok_or("unknown or unsupported preset")?;
    if (info.3 >= 20.0 || info.2 > 180.0)
        && extract_json_bool_field(body, "\"confirm\"") != Some(true)
    {
        return Err("confirmation required for 20A / >180W preset");
    }
    Ok(info)
}

fn parse_stratum_url(url: &str, default_port: u16) -> Option<(String, u16)> {
    let trimmed = url.trim().trim_end_matches('/');
    let rest = if let Some(s) = trimmed.strip_prefix("stratum+tcp://") {
        s
    } else if let Some(s) = trimmed.strip_prefix("stratum+ssl://") {
        s
    } else if let Some(s) = trimmed.strip_prefix("tcp://") {
        s
    } else {
        return None;
    };
    let rest = rest.split('/').next().unwrap_or("").trim();
    if rest.is_empty() {
        return None;
    }
    match rest.rsplit_once(':') {
        Some((h, p)) if !h.trim().is_empty() => {
            let port = p.trim().parse::<u16>().ok().filter(|&x| x != 0)?;
            Some((h.trim().to_owned(), port))
        }
        None => Some((rest.to_owned(), default_port)),
        _ => None,
    }
}

#[cfg(feature = "esp32s3")]
fn handle_http_client(
    mut stream: TcpStream,
    telemetry: &SharedTelemetry,
    nvs: &EspDefaultNvsPartition,
) {
    let Some((head, body)) = read_http_request(&mut stream) else {
        return;
    };

    let snap = telemetry.lock().map(|g| g.clone()).unwrap_or_default();
    let in_ap = snap.is_ap_mode || snap.ip_addr == AP_MODE_IP;

    // Captive portal behavior in SoftAP mode: redirect any probe host or unknown
    // path with 302 Found -> http://192.168.71.1/ so iOS/Android auto-pop the portal.
    if in_ap && (!is_ap_local_host(&head.host) || !is_known_ap_path(&head.path)) {
        write_http_response(
            &mut stream,
            "302 Found",
            "text/plain; charset=utf-8",
            &[
                ("Location", "http://192.168.71.1/"),
                ("Cache-Control", "no-cache, no-store, must-revalidate"),
            ],
            b"Redirect to the captive portal",
        );
        return;
    }

    match (head.method.as_str(), head.path.as_str()) {
        ("GET", "/") | ("GET", "/dashboard") => {
            if in_ap {
                let cfg = load_config_nvs(nvs);
                let html = render_setup_html(&cfg);
                write_http_response(
                    &mut stream,
                    "200 OK",
                    "text/html; charset=utf-8",
                    &[("Cache-Control", "no-cache, no-store, must-revalidate")],
                    html.as_bytes(),
                );
            } else {
                write_http_response(
                    &mut stream,
                    "200 OK",
                    "text/html; charset=utf-8",
                    &[
                        ("Content-Encoding", "gzip"),
                        ("Cache-Control", "no-cache"),
                    ],
                    DASHBOARD_HTML_GZ,
                );
            }
        }
        ("GET", "/mujina-head-mark.svg") => {
            write_http_response(
                &mut stream,
                "200 OK",
                "image/svg+xml",
                &[
                    ("Content-Encoding", "gzip"),
                    ("Cache-Control", "public, max-age=86400"),
                ],
                HEAD_MARK_SVG_GZ,
            );
        }
        ("GET", "/pool") => {
            write_http_response(
                &mut stream,
                "302 Found",
                "text/plain; charset=utf-8",
                &[("Location", "/dashboard#pool")],
                b"Redirect to /dashboard#pool",
            );
        }
        ("GET", "/tuning") => {
            write_http_response(
                &mut stream,
                "302 Found",
                "text/plain; charset=utf-8",
                &[("Location", "/dashboard#tuning")],
                b"Redirect to /dashboard#tuning",
            );
        }
        ("GET", "/auth.js") => {
            write_http_response(
                &mut stream,
                "200 OK",
                "application/javascript",
                &[("Cache-Control", "no-store")],
                AUTH_JS,
            );
        }
        ("GET", "/info") => {
            write_http_response(
                &mut stream,
                "200 OK",
                "text/html; charset=utf-8",
                &[("Cache-Control", "no-cache")],
                INFO_HTML.as_bytes(),
            );
        }
        ("GET", "/led") => {
            write_http_response(
                &mut stream,
                "200 OK",
                "text/html; charset=utf-8",
                &[("Cache-Control", "no-cache")],
                LED_HTML.as_bytes(),
            );
        }
        ("GET", "/nano3s-detail") => {
            let json = format_detail_json(&snap);
            write_http_response(&mut stream, "200 OK", "application/json", &[], json.as_bytes());
        }
        ("GET", "/setup") => {
            let cfg = load_config_nvs(nvs);
            let html = render_setup_html(&cfg);
            write_http_response(
                &mut stream,
                "200 OK",
                "text/html; charset=utf-8",
                &[("Cache-Control", "no-cache, no-store, must-revalidate")],
                html.as_bytes(),
            );
        }
        ("POST", "/api/v0/setup")
        | ("GET", "/api/v0/setup")
        | ("POST", "/api/v0/config")
        | ("POST", "/save") => {
            let form_str = if head.method == "POST" && !body.is_empty() {
                String::from_utf8_lossy(&body).into_owned()
            } else {
                head.query.clone()
            };
            let cfg = parse_setup_form(&form_str, load_config_nvs(nvs));
            if let Err(e) = save_config_nvs(nvs, &cfg) {
                let err_html = format!("<h2>Save failed: {e:?}</h2>");
                write_http_response(
                    &mut stream,
                    "500 Internal Server Error",
                    "text/html; charset=utf-8",
                    &[],
                    err_html.as_bytes(),
                );
                return;
            }
            let ok_html = format!(
                "<!doctype html><html><head><meta charset=\"utf-8\">\
                 <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
                 <title>Mujina NerdQX</title></head>\
                 <body style=\"font-family:sans-serif;background:#0a0e14;color:#e2e8f0;padding:24px\">\
                 <h2 style=\"color:#38bdf8\">Wi-Fi Saved!</h2>\
                 <p>Connecting NerdQX to <strong>{}</strong> and rebooting...</p>\
                 <p style=\"color:#94a3b8;font-size:14px\">Once connected, check the front LCD screen for your NerdQX IP address to configure your mining pool.</p>\
                 </body></html>",
                html_escape(&cfg.wifi_ssid)
            );
            write_http_response(
                &mut stream,
                "200 OK",
                "text/html; charset=utf-8",
                &[("Cache-Control", "no-store")],
                ok_html.as_bytes(),
            );
            drop(stream);
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_millis(800));
                esp_idf_svc::hal::reset::restart();
            });
        }
        ("GET", "/data") => {
            let json = format_dashboard_data_json(&snap);
            write_http_response(&mut stream, "200 OK", "application/json", &[], json.as_bytes());
        }
        ("GET", "/api/v0/system") => {
            let json = format_system_json(&snap);
            write_http_response(&mut stream, "200 OK", "application/json", &[], json.as_bytes());
        }
        ("GET", "/api/v0/pool") => {
            let cfg = load_config_nvs(nvs);
            let url = format!("stratum+tcp://{}:{}", cfg.pool_host, cfg.pool_port);
            let json = format!(
                "{{\"url\":\"{}\",\"user\":\"{}\",\"password_set\":{}}}",
                json_escape(&url),
                json_escape(&cfg.pool_user),
                !cfg.pool_pass.is_empty(),
            );
            write_http_response(&mut stream, "200 OK", "application/json", &[], json.as_bytes());
        }
        ("PATCH", "/api/v0/pool") | ("POST", "/api/v0/pool") => {
            let body_str = String::from_utf8_lossy(&body);
            let mut cfg = load_config_nvs(nvs);
            if let Some(url) = extract_json_string_field(&body_str, "\"url\"") {
                let Some((host, port)) = parse_stratum_url(&url, DEFAULT_POOL_PORT) else {
                    write_http_response(
                        &mut stream,
                        "400 Bad Request",
                        "application/json",
                        &[],
                        b"{\"error\":\"invalid pool url\"}",
                    );
                    return;
                };
                cfg.pool_host = host;
                cfg.pool_port = port;
            }
            if let Some(user) = extract_json_string_field(&body_str, "\"user\"") {
                cfg.pool_user = user.trim().to_owned();
            }
            if let Some(pass) = extract_json_string_field(&body_str, "\"password\"") {
                if !pass.is_empty() {
                    cfg.pool_pass = pass;
                }
            }
            if let Err(e) = save_config_nvs(nvs, &cfg) {
                let err = format!("{{\"error\":\"save failed: {e:?}\"}}");
                write_http_response(
                    &mut stream,
                    "500 Internal Server Error",
                    "application/json",
                    &[],
                    err.as_bytes(),
                );
                return;
            }
            let full_url = format!("stratum+tcp://{}:{}", cfg.pool_host, cfg.pool_port);
            if let Ok(mut guard) = telemetry.lock() {
                guard.pool_url = full_url;
                guard.pool_user = cfg.pool_user.clone();
                guard.pool_dirty = true;
            }
            write_http_response(
                &mut stream,
                "200 OK",
                "application/json",
                &[],
                b"{\"ok\":true,\"applied\":\"live\"}",
            );
        }
        ("GET", "/api/v0/miner") => {
            let json = format_miner_json(&snap);
            write_http_response(&mut stream, "200 OK", "application/json", &[], json.as_bytes());
        }
        ("GET", "/api/v0/boards/nerdqx/psu") => {
            let (_, code, budget_w, fuse_max_a) =
                psu_info(snap.psu_preset).unwrap_or(("stock", "1A", snap.psu_budget_w, 15.0));
            let json = format!(
                "{{\"board\":\"nerdqx\",\"preset\":\"{code}\",\"power_budget_w\":{budget_w:.1},\"fuse_max_a\":{fuse_max_a:.1},\"vin_v\":{:.2},\"pin_w\":{:.1}}}",
                snap.vin_v, snap.pin_w
            );
            write_http_response(&mut stream, "200 OK", "application/json", &[], json.as_bytes());
        }
        ("PATCH", "/api/v0/boards/nerdqx/psu") | ("POST", "/api/v0/boards/nerdqx/psu") => {
            let body_str = String::from_utf8_lossy(&body);
            let (hud_name, code, budget_w, fuse_max_a) = match validate_psu_patch(&body_str) {
                Ok(info) => info,
                Err(msg) => {
                    let err = format!("{{\"error\":\"{msg}\"}}");
                    write_http_response(
                        &mut stream,
                        "400 Bad Request",
                        "application/json",
                        &[],
                        err.as_bytes(),
                    );
                    return;
                }
            };
            if let Ok(mut guard) = telemetry.lock() {
                guard.psu_preset = hud_name;
                guard.psu_budget_w = budget_w;
            }
            let json = format!(
                "{{\"ok\":true,\"preset\":\"{code}\",\"power_budget_w\":{budget_w:.1},\"fuse_max_a\":{fuse_max_a:.1}}}"
            );
            write_http_response(&mut stream, "200 OK", "application/json", &[], json.as_bytes());
        }
        ("GET", "/api/v0/boards/nerdqx/autotune") | ("GET", "/api/v0/boards/nerdqx/tuner") => {
            let json = format_autotune_json(&snap);
            write_http_response(&mut stream, "200 OK", "application/json", &[], json.as_bytes());
        }
        ("PATCH", "/api/v0/boards/nerdqx/autotune")
        | ("POST", "/api/v0/boards/nerdqx/autotune")
        | ("PATCH", "/api/v0/boards/nerdqx/tuner")
        | ("POST", "/api/v0/boards/nerdqx/tuner") => {
            let body_str = String::from_utf8_lossy(&body);
            let next_tuner = match parse_autotune_patch(&body_str, &snap.tuner) {
                Ok(t) => t,
                Err(msg) => {
                    let err = format!("{{\"error\":\"{msg}\"}}");
                    write_http_response(&mut stream, "400 Bad Request", "application/json", &[], err.as_bytes());
                    return;
                }
            };
            // Remember the selected profile so the next boot starts at the matching operating point.
            let new_profile = next_tuner.profile.clone();
            let profile_changed = new_profile != snap.tuner.profile
                && matches!(new_profile.as_str(), "quiet" | "efficient" | "balanced" | "max_hash");
            let updated_snap = if let Ok(mut guard) = telemetry.lock() {
                guard.tuner = next_tuner;
                guard.clone()
            } else {
                snap
            };
            if profile_changed {
                let mut cfg = load_config_nvs(nvs);
                cfg.tuner_profile = new_profile;
                let _ = save_config_nvs(nvs, &cfg);
            }
            let json = format_autotune_json(&updated_snap);
            write_http_response(&mut stream, "200 OK", "application/json", &[], json.as_bytes());
        }
        ("PATCH", "/api/v0/boards/nerdqx/tuning") | ("POST", "/api/v0/boards/nerdqx/tuning") => {
            let body_str = String::from_utf8_lossy(&body);
            let (freq_mhz, mv) =
                match parse_manual_tuning_patch(&body_str, snap.freq_mhz, snap.target_vcore_mv) {
                    Ok(v) => v,
                    Err(msg) => {
                        let err = format!("{{\"error\":\"{msg}\"}}");
                        write_http_response(&mut stream, "400 Bad Request", "application/json", &[], err.as_bytes());
                        return;
                    }
                };
            if let Ok(mut guard) = telemetry.lock() {
                guard.pending_manual_tune = Some((freq_mhz, mv));
                guard.target_vcore_mv = mv;
                guard.tuner.enabled = false;
                guard.tuner.phase = String::from("off");
                guard.tuner.per_chip_phase = String::from("off");
                guard.tuner.dirty = true;
            }
            let json = format!("{{\"ok\":true,\"frequency_mhz\":{freq_mhz:.1},\"core_voltage_mv\":{mv}}}");
            write_http_response(&mut stream, "200 OK", "application/json", &[], json.as_bytes());
        }
        ("PATCH", "/api/v0/boards/nerdqx/fan") | ("POST", "/api/v0/boards/nerdqx/fan") => {
            let body_str = String::from_utf8_lossy(&body);
            let (auto, pct_opt) = match parse_fan_patch(&body_str) {
                Ok(v) => v,
                Err(msg) => {
                    let err = format!("{{\"error\":\"{msg}\"}}");
                    write_http_response(&mut stream, "400 Bad Request", "application/json", &[], err.as_bytes());
                    return;
                }
            };
            let eff_pct = if let Ok(mut guard) = telemetry.lock() {
                guard.fan_auto = auto;
                if let Some(p) = pct_opt {
                    guard.fan_manual_pct = p;
                    if !auto {
                        guard.fan_pct = p;
                    }
                }
                guard.fan_pct
            } else {
                snap.fan_pct
            };
            let json = format!("{{\"ok\":true,\"auto\":{auto},\"percent\":{eff_pct}}}");
            write_http_response(&mut stream, "200 OK", "application/json", &[], json.as_bytes());
        }
        ("GET", "/api/v0/boards/nerdqx/fan-curve") => {
            let json = format_fan_curve_json(&snap.fan_curve);
            write_http_response(&mut stream, "200 OK", "application/json", &[], json.as_bytes());
        }
        ("PATCH", "/api/v0/boards/nerdqx/fan-curve")
        | ("POST", "/api/v0/boards/nerdqx/fan-curve") => {
            let body_str = String::from_utf8_lossy(&body);
            let pts = match parse_fan_curve_patch(&body_str) {
                Ok(p) => p,
                Err(msg) => {
                    let err = format!("{{\"error\":\"{msg}\"}}");
                    write_http_response(&mut stream, "400 Bad Request", "application/json", &[], err.as_bytes());
                    return;
                }
            };
            let json = format_fan_curve_json(&pts);
            if let Ok(mut guard) = telemetry.lock() {
                guard.fan_curve = pts;
            }
            write_http_response(&mut stream, "200 OK", "application/json", &[], json.as_bytes());
        }
        ("PATCH", "/api/v0/boards/nerdqx/pause")
        | ("POST", "/api/v0/boards/nerdqx/pause")
        | ("PATCH", "/api/v0/miner")
        | ("POST", "/api/v0/miner") => {
            let body_str = String::from_utf8_lossy(&body);
            let Some(paused) = extract_json_bool_field(&body_str, "\"paused\"") else {
                write_http_response(
                    &mut stream,
                    "400 Bad Request",
                    "application/json",
                    &[],
                    b"{\"error\":\"missing paused boolean\"}",
                );
                return;
            };
            if let Ok(mut guard) = telemetry.lock() {
                guard.pause_dirty = Some(paused);
                guard.paused = paused;
            }
            let json = format!("{{\"ok\":true,\"paused\":{paused}}}");
            write_http_response(&mut stream, "200 OK", "application/json", &[], json.as_bytes());
        }
        ("GET", "/api/v0/display") => {
            use std::sync::atomic::Ordering::Relaxed;
            let json = format!(
                "{{\"theme\":\"{}\",\"fps\":{:.1},\"render_ms\":{:.1},\"draw_ms\":{:.1},\"loop_max_ms\":{:.1},\"hud_stages_ms\":[{:.1},{:.1},{:.1},{:.1},{:.1},{:.1}]}}",
                snap.ui_theme.as_str(),
                crate::hud_data::DISPLAY_FPS_X10.load(Relaxed) as f32 / 10.0,
                crate::hud_data::DISPLAY_RENDER_US.load(Relaxed) as f32 / 1000.0,
                crate::hud_data::DISPLAY_DRAW_US.load(Relaxed) as f32 / 1000.0,
                crate::hud_data::DISPLAY_LOOP_MAX_US.load(Relaxed) as f32 / 1000.0,
                crate::hud_data::DISPLAY_STAGE_US[0].load(Relaxed) as f32 / 1000.0,
                crate::hud_data::DISPLAY_STAGE_US[1].load(Relaxed) as f32 / 1000.0,
                crate::hud_data::DISPLAY_STAGE_US[2].load(Relaxed) as f32 / 1000.0,
                crate::hud_data::DISPLAY_STAGE_US[3].load(Relaxed) as f32 / 1000.0,
                crate::hud_data::DISPLAY_STAGE_US[4].load(Relaxed) as f32 / 1000.0,
                crate::hud_data::DISPLAY_STAGE_US[5].load(Relaxed) as f32 / 1000.0,
            );
            write_http_response(&mut stream, "200 OK", "application/json", &[], json.as_bytes());
        }
        ("PATCH", "/api/v0/display") => {
            let body_str = String::from_utf8_lossy(&body);
            let Some(val) = extract_json_string_field(&body_str, "\"theme\"") else {
                write_http_response(
                    &mut stream,
                    "400 Bad Request",
                    "application/json",
                    &[],
                    b"{\"error\":\"missing theme field\"}",
                );
                return;
            };
            let theme = UiTheme::parse(&val);
            if let Ok(mut guard) = telemetry.lock() {
                guard.ui_theme = theme;
            }
            let mut cfg = load_config_nvs(nvs);
            cfg.ui_theme = String::from(theme.as_str());
            let _ = save_config_nvs(nvs, &cfg);
            let json = format!("{{\"theme\":\"{}\"}}", theme.as_str());
            write_http_response(&mut stream, "200 OK", "application/json", &[], json.as_bytes());
        }
        ("PATCH", "/api/v0/display/screen") => {
            let body_str = String::from_utf8_lossy(&body);
            let n = extract_json_f64_field(&body_str, "\"screen\"")
                .filter(|v| v.is_finite() && (0.0..6.0).contains(v))
                .map(|v| v as u8);
            let Some(n) = n else {
                write_http_response(
                    &mut stream,
                    "400 Bad Request",
                    "application/json",
                    &[],
                    b"{\"error\":\"screen must be 0..5\"}",
                );
                return;
            };
            if let Ok(mut guard) = telemetry.lock() {
                guard.screen_req = Some(n);
            }
            let json = format!("{{\"screen\":{n}}}");
            write_http_response(&mut stream, "200 OK", "application/json", &[], json.as_bytes());
        }
        ("GET", "/api/v0/display/screenshot") => {
            use crate::hud_data::{frame_to_bmp, FRAME_SHOT, SHOT_REQ};
            use std::sync::atomic::Ordering;
            if let Ok(mut f) = FRAME_SHOT.lock() {
                f.clear();
            }
            SHOT_REQ.store(true, Ordering::SeqCst);
            let mut frame: Vec<u16> = Vec::new();
            for _ in 0..30 {
                std::thread::sleep(Duration::from_millis(100));
                if let Ok(f) = FRAME_SHOT.lock() {
                    if !f.is_empty() {
                        frame = f.clone();
                        break;
                    }
                }
            }
            if frame.is_empty() {
                write_http_response(&mut stream, "503 Service Unavailable", "text/plain", &[], b"no frame");
            } else {
                let bmp = frame_to_bmp(&frame, crate::hw_pins::LCD_WIDTH as usize, crate::hw_pins::LCD_HEIGHT as usize);
                write_http_response(&mut stream, "200 OK", "image/bmp", &[("Cache-Control", "no-store")], &bmp);
            }
        }
        ("POST", "/api/v0/reboot") | ("POST", "/api/v0/restart") => {
            write_http_response(&mut stream, "200 OK", "application/json", &[], b"{\"ok\":true}");
            drop(stream);
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_millis(500));
                esp_idf_svc::hal::reset::restart();
            });
        }
        _ => {
            write_http_response(&mut stream, "404 Not Found", "text/plain", &[], b"Not Found");
        }
    }
}

/// Start the pure-Rust port-80 HTTP server (supports 8 KiB headers, ESP-Miner-style
/// `302 Found` captive portal redirects, and gzip-compressed `dashboard.html`).
#[cfg(feature = "esp32s3")]
pub fn start_http_server(
    telemetry: SharedTelemetry,
    nvs: EspDefaultNvsPartition,
) -> Result<HttpServerHandle, EspError> {
    let listener = match TcpListener::bind("0.0.0.0:80") {
        Ok(l) => l,
        Err(e) => {
            log::error!("HTTP server failed to bind 0.0.0.0:80: {e:?}");
            return Err(EspError::from_infallible::<{ esp_idf_svc::sys::ESP_FAIL }>());
        }
    };
    log::info!("Pure-Rust HTTP server listening on 0.0.0.0:80 (8 KiB header limit)");

    let _ = std::thread::Builder::new()
        .name(String::from("http_srv"))
        .stack_size(24 * 1024)
        .spawn(move || {
            for incoming in listener.incoming() {
                if let Ok(stream) = incoming {
                    handle_http_client(stream, &telemetry, &nvs);
                }
            }
        });

    Ok(HttpServerHandle)
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// Simplified Wi-Fi-only captive portal setup page rendered on `192.168.71.1`.
pub fn render_setup_html(cfg: &DeviceConfig) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>Mujina NerdQX Wi-Fi Setup</title>\
         <style>\
         body{{font-family:-apple-system,BlinkMacSystemFont,sans-serif;background:#0a0e14;color:#e2e8f0;margin:0;padding:18px}}\
         .card{{max-width:420px;margin:0 auto;background:#111827;border:1px solid #1f2937;border-radius:10px;padding:18px}}\
         h2{{margin:0 0 8px;color:#00f0ff;font-size:20px}}\
         p{{margin:0 0 14px;font-size:13px;color:#94a3b8;line-height:1.45}}\
         label{{display:block;margin:10px 0 4px;font-size:13px;color:#cbd5e1}}\
         input[type=text],input[type=password]{{width:100%;box-sizing:border-box;padding:10px;background:#0a0e14;color:#f8fafc;border:1px solid #334155;border-radius:6px;font-size:15px}}\
         .row{{display:flex;align-items:center;gap:8px;margin-top:8px;font-size:13px;color:#94a3b8}}\
         button{{margin-top:16px;width:100%;padding:12px;background:#0284c7;color:#fff;border:0;border-radius:6px;font-size:16px;font-weight:bold;cursor:pointer}}\
         </style></head><body><div class=\"card\">\
         <h2>Mujina NerdQX Wi-Fi</h2>\
         <p>Enter your Wi-Fi network credentials below. After connecting, open the IP shown on the NerdQX screen to configure your pool.</p>\
         <form method=\"POST\" action=\"/api/v0/setup\" enctype=\"application/x-www-form-urlencoded\">\
         <label for=\"wifi_ssid\">Wi-Fi Network (SSID)</label>\
         <input id=\"wifi_ssid\" name=\"wifi_ssid\" type=\"text\" value=\"{}\" autocomplete=\"off\" required>\
         <label for=\"wifi_pass\">Wi-Fi Password</label>\
         <input id=\"wifi_pass\" name=\"wifi_pass\" type=\"password\" value=\"{}\" autocomplete=\"off\">\
         <label class=\"row\"><input type=\"checkbox\" onclick=\"var p=document.getElementById('wifi_pass');p.type=this.checked?'text':'password'\"> Show password</label>\
         <button type=\"submit\">Connect Wi-Fi</button>\
         </form></div></body></html>",
        html_escape(&cfg.wifi_ssid),
        html_escape(&cfg.wifi_pass),
    )
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                    out.push((hi << 4) | lo);
                    i += 3;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn parse_setup_form(body: &str, mut cfg: DeviceConfig) -> DeviceConfig {
    for pair in body.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(pair), String::new()),
        };
        match k.as_str() {
            "wifi_ssid" => cfg.wifi_ssid = v,
            "wifi_pass" => cfg.wifi_pass = v,
            "pool_host" => {
                let trimmed = v.trim();
                cfg.pool_host = if trimmed.is_empty() {
                    String::from(DEFAULT_POOL_HOST)
                } else {
                    trimmed.to_owned()
                };
            }
            "pool_port" => {
                cfg.pool_port = v
                    .trim()
                    .parse::<u16>()
                    .ok()
                    .filter(|&p| p != 0)
                    .unwrap_or(DEFAULT_POOL_PORT);
            }
            "pool_user" => cfg.pool_user = v.trim().to_owned(),
            "pool_pass" => {
                cfg.pool_pass = if v.is_empty() {
                    String::from(DEFAULT_POOL_PASS)
                } else {
                    v
                };
            }
            _ => {}
        }
    }
    cfg
}

fn fmt_opt_f32(v: Option<f32>) -> String {
    match v {
        Some(x) if x.is_finite() => format!("{x:.1}"),
        _ => String::from("null"),
    }
}

fn fmt_opt_u16(v: Option<u16>) -> String {
    match v {
        Some(x) => x.to_string(),
        None => String::from("null"),
    }
}

fn fmt_diff(d: f64) -> String {
    if !d.is_finite() || d <= 0.0 {
        return String::from("0");
    }
    if d >= 1e9 {
        format!("{:.2}G", d / 1e9)
    } else if d >= 1e6 {
        format!("{:.2}M", d / 1e6)
    } else if d >= 1e3 {
        format!("{:.1}K", d / 1e3)
    } else if (d - d.round()).abs() < 0.05 {
        format!("{:.0}", d)
    } else {
        format!("{d:.1}")
    }
}

fn fmt_uptime(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    format!("{h}h {m:02}m {s:02}s")
}

pub fn format_system_json(t: &LiveTelemetry) -> String {
    format!(
        "{{\"api\":1,\"mode\":\"embedded\",\"platform\":\"ESP32-S3\",\
         \"pool_connected\":{},\"chip_count\":{},\
         \"reset_reason\":\"{}\",\"free_internal_kb\":{},\"min_internal_kb\":{},\"free_psram_kb\":{},\
         \"device\":{{\"model\":\"NerdQX\",\"can_reboot\":true,\"can_restart_miner\":true}},\
         \"firmware\":{{\"name\":\"mujina-esp32\",\"version\":\"5.1.0\"}},\
         \"pool\":{{\"endpoint\":\"/api/v0/pool\",\"apply\":\"live\"}},\
         \"power\":{{\"shared_rail\":true,\"ceiling_w\":null,\"psu_contract_w\":{:.1}}},\
         \"boards\":[{{\"name\":\"nerdqx\",\"model\":\"NerdQX\",\"family\":\"nerdqx\",\"chips\":{},\"chip_count\":{},\
         \"limits\":{{\"freq_mhz\":[{},{}],\"core_mv\":[{},{}],\"die_target_c\":[58,63],\
         \"fan_min_pct\":{{\"vr\":{},\"asic\":{}}}}},\"profiles\":{},\
         \"caps\":{{\"chip_grid\":\"per_chip\",\"temps\":[\"asic\",\"vr\"],\
         \"fan\":\"board\",\"fan_curve\":true,\
         \"tuner\":\"/api/v0/boards/nerdqx/autotune\",\
         \"manual_tuning\":\"/api/v0/boards/nerdqx/tuning\",\
         \"psu\":\"/api/v0/boards/nerdqx/psu\"}}}}]}}",
        t.pool_connected,
        t.chip_count,
        t.reset_reason,
        t.free_internal_kb,
        t.min_internal_kb,
        t.free_psram_kb,
        t.psu_budget_w,
        t.chip_count,
        t.chip_count,
        crate::chip_tuner::MIN_BASE_FREQ_MHZ as u32,
        crate::chip_tuner::MAX_BASE_FREQ_MHZ as u32,
        crate::chip_tuner::MIN_VCORE_MV,
        crate::chip_tuner::MAX_VCORE_MV,
        crate::chip_tuner::VR_FAN_MIN_PCT,
        crate::chip_tuner::ASIC_FAN_MIN_PCT,
        nerdqx_profiles_json()
    )
}

/// JSON array of the NerdQX tuner profiles (id, label, hint) for the dashboard.
fn nerdqx_profiles_json() -> String {
    let items: Vec<String> = crate::chip_tuner::NERDQX_PROFILES
        .iter()
        .map(|(id, label, hint)| {
            format!("{{\"id\":\"{id}\",\"label\":\"{label}\",\"hint\":\"{hint}\"}}")
        })
        .collect();
    format!("[{}]", items.join(","))
}

fn fmt_opt_json_str(opt: Option<&str>) -> String {
    match opt {
        Some(v) => format!("\"{}\"", json_escape(v)),
        None => String::from("null"),
    }
}

pub fn format_dashboard_data_json(t: &LiveTelemetry) -> String {
    let hr_ghs = t.hashrate_ghs.max(0.0);
    let hr_ths = hr_ghs / 1000.0;
    let core_v = (t.vcore_mv as f32) / 1000.0;
    let eff = if hr_ths > 0.001 && t.pin_w > 0.1 {
        Some(t.pin_w / hr_ths)
    } else {
        None
    };
    let asic_c = if t.asic_temp_c.is_finite() && t.asic_temp_c > 0.0 {
        Some(t.asic_temp_c)
    } else {
        let mut sum = 0.0f32;
        let mut n = 0usize;
        for v in t.chip_temps_c.into_iter().flatten() {
            if v.is_finite() && v > 0.0 {
                sum += v;
                n += 1;
            }
        }
        (n > 0).then(|| sum / n as f32)
    };
    let vr_c = (t.vr_temp_c.is_finite() && t.vr_temp_c > 0.0).then_some(t.vr_temp_c);
    let core_a = if t.iout_a.is_finite() && t.iout_a > 0.0 {
        t.iout_a
    } else if core_v > 0.1 {
        t.pin_w / core_v
    } else {
        0.0
    };

    let mut series_s = String::from("[");
    for (i, &val) in t.series_ghs.iter().enumerate() {
        if i > 0 {
            series_s.push(',');
        }
        let _ = write!(series_s, "{val:.2}");
    }
    series_s.push(']');

    let mut chips_s = String::from("[");
    for i in 0..4 {
        if i > 0 {
            chips_s.push(',');
        }
        let _ = write!(
            chips_s,
            "{{\"index\":{i},\"ghs\":{:.2},\"temp_c\":{},\"offset_mhz\":{:.2},\"spd_dh\":{:.2}}}",
            t.chip_hashrates_ghs[i],
            fmt_opt_f32(t.chip_temps_c[i]),
            t.chip_offsets_mhz[i],
            t.chip_error_pct[i],
        );
    }
    chips_s.push(']');

    let mut fans_s = String::from("[");
    let mut fan_first = true;
    for (i, rpm) in t.fan_rpm.iter().enumerate() {
        if let Some(r) = rpm {
            if !fan_first {
                fans_s.push(',');
            }
            fan_first = false;
            let _ = write!(
                fans_s,
                "{{\"name\":\"fan{}\",\"rpm\":{},\"percent\":{}}}",
                i + 1,
                r,
                if i == 0 { t.fan_vr_pct } else { t.fan_pct }
            );
        }
    }
    if fan_first {
        let _ = write!(fans_s, "{{\"name\":\"fan1\",\"rpm\":null,\"percent\":{}}}", t.fan_pct);
    }
    fans_s.push(']');

    let primary_rpm = t.fan_rpm[0].or(t.fan_rpm[1]);
    let pool_name = t
        .pool_url
        .strip_prefix("stratum+tcp://")
        .or_else(|| t.pool_url.strip_prefix("stratum+ssl://"))
        .unwrap_or(&t.pool_url);
    let fault_json = fmt_opt_json_str(t.fault_reason.as_deref());

    let mut s = String::with_capacity(1400);
    let _ = write!(
        s,
        "{{\"ok\":true,\"miner_name\":\"nerdqx\",\
         \"pool_name\":\"{}\",\"pool_url\":\"{}\",\"pool_connected\":{},\
         \"chip_count\":{},\"fault\":{fault_json},\
         \"shares\":{},\"difficulty\":\"{}\",\"best_diff\":\"{}\",\
         \"uptime\":\"{}\",\"paused\":{},\
         \"fleet\":{{\"hashrate_ghs\":{hr_ghs:.2},\"hashrate_ths\":{hr_ths:.3},\
         \"boards_online\":1,\"avg_freq_mhz\":{:.0},\"avg_core_v\":{core_v:.3},\
         \"avg_asic_c\":{},\"avg_vr_c\":{},\"avg_fail_rate_pct\":{:.2},\"total_power_w\":{:.1},\
         \"efficiency_jth\":{},\"avg_fan_rpm\":{},\"series\":{series_s}}},\
         \"boards\":[{{\"board_name\":\"nerdqx\",\"model\":\"NerdQX\",\"serial\":\"ESP32-S3R8\",\"chip_count\":{},\
         \"hashrate_ghs\":{hr_ghs:.2},\"hashrate_ths\":{hr_ths:.3},\
         \"series\":{series_s},\"freq_mhz\":{:.0},\"core_v\":{core_v:.3},\
         \"core_w\":{:.1},\"core_a\":{core_a:.2},\"input_v\":{:.2},\"input_a\":{:.2},\"input_w\":{:.1},\
         \"efficiency_jth\":{},\"asic_c\":{},\"vr_c\":{},\"fail_rate_pct\":{:.2},\
         \"chips\":{chips_s},\"fans\":{fans_s},\
         \"fan_rpm\":{},\"fan_pct\":{}}}]}}",
        json_escape(pool_name),
        json_escape(&t.pool_url),
        t.pool_connected,
        t.chip_count,
        t.shares_accepted,
        fmt_diff(t.pool_diff),
        fmt_diff(t.best_diff),
        fmt_uptime(t.uptime_s),
        t.paused,
        t.freq_mhz,
        fmt_opt_f32(asic_c),
        fmt_opt_f32(vr_c),
        t.fail_rate_pct,
        t.pin_w,
        fmt_opt_f32(eff),
        fmt_opt_u16(primary_rpm),
        t.chip_count,
        t.freq_mhz,
        t.pin_w,
        t.vin_v,
        t.iin_a,
        t.pin_w,
        fmt_opt_f32(eff),
        fmt_opt_f32(asic_c),
        fmt_opt_f32(vr_c),
        t.fail_rate_pct,
        fmt_opt_u16(primary_rpm),
        t.fan_pct,
    );
    s
}

/// Serialize `EmbeddedTunerState` + live metrics to `/api/v0/boards/nerdqx/autotune`.
pub fn format_autotune_json(t: &LiveTelemetry) -> String {
    let tu = &t.tuner;
    let target_json = match (&tu.target_axis, tu.target_value) {
        (Some(axis), Some(val)) => format!("{{\"axis\":\"{}\",\"value\":{val:.2}}}", json_escape(axis)),
        _ => String::from("null"),
    };
    let best_json = match (tu.best_freq_mhz, tu.best_vcore_mv) {
        (Some(f), Some(v)) => {
            let j_str = fmt_opt_f32(tu.best_j_th);
            format!("{{\"frequency_mhz\":{f:.1},\"core_voltage_mv\":{v},\"efficiency_j_th\":{j_str}}}")
        }
        _ => String::from("null"),
    };
    let last_ev_json = fmt_opt_json_str(tu.per_chip_last_event.as_deref());
    let mut events_s = String::from("[");
    for (i, ev) in tu.events.iter().enumerate() {
        if i > 0 {
            events_s.push(',');
        }
        let ago = t.uptime_s.saturating_sub(ev.created_uptime_s);
        let _ = write!(
            events_s,
            "{{\"t_secs_ago\":{ago},\"frequency_mhz\":{:.1},\"core_voltage_mv\":{},\"power_w\":{:.1},\
             \"hashrate_ths\":{:.2},\"efficiency_j_th\":{:.1},\"asic_temp_c\":{:.1},\"message\":\"{}\"}}",
            ev.frequency_mhz,
            ev.core_voltage_mv,
            ev.power_w,
            ev.hashrate_ths,
            ev.efficiency_j_th,
            ev.asic_temp_c,
            json_escape(&ev.message),
        );
    }
    events_s.push(']');

    format!(
        "{{\"enabled\":{},\"profile\":\"{}\",\"target\":{target_json},\"continuous\":{},\
         \"phase\":\"{}\",\"fan_duty_percent\":{},\"fan_target_c\":{:.1},\
         \"continuous_ceiling_w\":{:.1},\"frequency_mhz\":{:.1},\"core_voltage_mv\":{},\
         \"setpoint\":{{\"frequency_mhz\":{:.1},\"core_voltage_mv\":{}}},\
         \"averaging_progress\":{{\"ticks\":{},\"needed\":{}}},\
         \"per_chip\":{{\"phase\":\"{}\",\"last_event\":{last_ev_json}}},\
         \"best\":{best_json},\"events\":{events_s}}}",
        tu.enabled,
        json_escape(&tu.profile),
        tu.continuous,
        json_escape(&tu.phase),
        t.fan_pct,
        tu.fan_target_c,
        t.psu_budget_w,
        t.freq_mhz,
        t.target_vcore_mv,
        t.freq_mhz,
        t.target_vcore_mv,
        tu.avg_ticks,
        tu.avg_needed,
        json_escape(&tu.per_chip_phase),
    )
}

/// Serialize the fan curve points array for `GET/PATCH /api/v0/boards/nerdqx/fan-curve`.
pub fn format_fan_curve_json(points: &[(f32, u8)]) -> String {
    let mut pts_s = String::from("{\"points\":[");
    for (i, &(temp_c, duty_pct)) in points.iter().enumerate() {
        if i > 0 {
            pts_s.push(',');
        }
        let _ = write!(pts_s, "{{\"temp_c\":{temp_c:.1},\"duty_pct\":{duty_pct}}}");
    }
    pts_s.push_str("]}");
    pts_s
}

/// Serialize per-chip & hardware detail for `GET /nano3s-detail` and `/info`.
pub fn format_detail_json(t: &LiveTelemetry) -> String {
    let mut temp_max: Option<f32> = None;
    for v in t.chip_temps_c.into_iter().flatten() {
        if v.is_finite() {
            temp_max = Some(temp_max.map_or(v, |m| m.max(v)));
        }
    }
    let mut chips_s = String::from("[");
    let mut offs_s = String::from("[");
    for i in 0..4 {
        if i > 0 {
            chips_s.push(',');
            offs_s.push(',');
        }
        let chip_freq = (t.freq_mhz + t.chip_offsets_mhz[i]).max(56.25);
        let _ = write!(offs_s, "{:.2}", t.chip_offsets_mhz[i]);
        let _ = write!(
            chips_s,
            "{{\"chip\":{i},\"temp_c\":{},\"volt_mv\":{},\"pll_cnt\":[1,0,0,0],\
             \"pll_freq\":[{chip_freq:.1}],\"ghsspd\":{:.2},\"spd_dh\":{:.2},\
             \"nonce_timeout\":0,\"nonce_heartbeat\":0,\"nonce_data\":{}}}",
            fmt_opt_f32(t.chip_temps_c[i]),
            t.vcore_mv,
            t.chip_hashrates_ghs[i],
            t.chip_error_pct[i],
            t.chip_shares[i],
        );
    }
    chips_s.push(']');
    offs_s.push(']');

    format!(
        "{{\"paused\":{},\"ipc_connected\":true,\"asics_total\":{},\"ghsspd\":{:.2},\
         \"ghsmm\":4800.0,\"spd_dh\":{:.2},\"temp_avg\":{:.1},\"temp_max\":{},\
         \"pll_freq_commanded\":[{:.1}],\"voltage_mv_commanded\":{},\
         \"err_crc\":{},\"nonce_read_err\":0,\"nonce_bad_len\":0,\"nonce_overflow\":0,\"regread_err\":0,\
         \"ina_bus_v\":{:.2},\"ina_current_a\":{:.2},\"ina_power_w\":{:.1},\
         \"job_id\":0,\"difficulty\":{:.1},\"shares_found\":{},\"shares_accepted\":{},\
         \"shares_rejected\":{},\"chip_offset_mhz\":{offs_s},\"chips\":{chips_s}}}",
        t.paused,
        t.chip_count,
        t.hashrate_ghs,
        t.fail_rate_pct,
        t.asic_temp_c,
        fmt_opt_f32(temp_max),
        t.freq_mhz,
        t.vcore_mv,
        t.hw_errors,
        t.vin_v,
        t.iin_a,
        t.pin_w,
        t.pool_diff,
        t.chip_shares.iter().sum::<u32>(),
        t.shares_accepted,
        t.shares_rejected,
    )
}

pub fn format_miner_json(t: &LiveTelemetry) -> String {
    let fault_json = fmt_opt_json_str(t.fault_reason.as_deref());
    let mut s = String::with_capacity(480);
    let _ = write!(
        s,
        "{{\"board\":\"nerdqx\",\"chip_count\":{},\"pool_connected\":{},\"fault\":{fault_json},\
         \"reset_reason\":\"{}\",\"free_internal_kb\":{},\"min_internal_kb\":{},\"free_psram_kb\":{},\
         \"hashrate_ghs\":{:.2},\
         \"chip_hashrates_ghs\":[{:.2},{:.2},{:.2},{:.2}],\
         \"chip_temps_c\":[{},{},{},{}],\
         \"vr_temp_c\":{:.1},\"vin_v\":{:.2},\"iin_a\":{:.2},\"iout_a\":{:.2},\"pin_w\":{:.1},\"vcore_mv\":{},\
         \"fan_rpm\":[{},{}],\
         \"psu_preset\":\"{}\",\"ip_addr\":\"{}\",\"shares_accepted\":{}}}",
        t.chip_count,
        t.pool_connected,
        t.reset_reason,
        t.free_internal_kb,
        t.min_internal_kb,
        t.free_psram_kb,
        t.hashrate_ghs,
        t.chip_hashrates_ghs[0],
        t.chip_hashrates_ghs[1],
        t.chip_hashrates_ghs[2],
        t.chip_hashrates_ghs[3],
        fmt_opt_f32(t.chip_temps_c[0]),
        fmt_opt_f32(t.chip_temps_c[1]),
        fmt_opt_f32(t.chip_temps_c[2]),
        fmt_opt_f32(t.chip_temps_c[3]),
        t.vr_temp_c,
        t.vin_v,
        t.iin_a,
        t.iout_a,
        t.pin_w,
        t.vcore_mv,
        fmt_opt_u16(t.fan_rpm[0]),
        fmt_opt_u16(t.fan_rpm[1]),
        t.psu_preset,
        t.ip_addr,
        t.shares_accepted,
    );
    s
}

// ---------------------------------------------------------------------------
// Pure-Rust SHA-256 / SHA-256d and Bitcoin / Stratum merkle math
// ---------------------------------------------------------------------------

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// Compute a single SHA-256 digest in pure Rust.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
        0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];

    let bit_len = (data.len() as u64) * 8;
    let mut padded = Vec::with_capacity(data.len() + 72);
    padded.extend_from_slice(data);
    padded.push(0x80);
    while (padded.len() % 64) != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    let mut w = [0u32; 64];
    for chunk in padded.chunks_exact(64) {
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// Compute double SHA-256 (`SHA256(SHA256(data))`).
pub fn sha256d(data: &[u8]) -> [u8; 32] {
    sha256(&sha256(data))
}

fn push_hex_bytes(hex: &str, out: &mut Vec<u8>) {
    let bytes = hex.trim().as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if let (Some(hi), Some(lo)) = (hex_val(bytes[i]), hex_val(bytes[i + 1])) {
            out.push((hi << 4) | lo);
        }
        i += 2;
    }
}

/// Reverse the eight 4-byte words of a 32-byte internal Bitcoin hash for the BM13xx wire packet.
pub fn hash_to_wire_words(internal: &[u8; 32]) -> [u8; 32] {
    let mut wire = [0u8; 32];
    for i in 0..8 {
        wire[(7 - i) * 4..(8 - i) * 4].copy_from_slice(&internal[i * 4..(i + 1) * 4]);
    }
    wire
}

/// Convert a Stratum `prevhash` hex string to 32-byte internal Bitcoin block-hash order.
pub fn stratum_prevhash_to_internal(prevhash_hex: &str) -> [u8; 32] {
    let mut raw = Vec::with_capacity(32);
    push_hex_bytes(prevhash_hex, &mut raw);
    let mut out = [0u8; 32];
    if raw.len() == 32 {
        for i in 0..8 {
            let s = i * 4;
            out[s] = raw[s + 3];
            out[s + 1] = raw[s + 2];
            out[s + 2] = raw[s + 1];
            out[s + 3] = raw[s];
        }
    }
    out
}

/// Convert a Stratum `prevhash` hex string directly to the 32-byte BM13xx wire format.
///
/// Stratum sends 8 big-endian-printed little-endian 4-byte words; swapping each
/// 4-byte word yields Bitcoin internal order, and reversing the 8 words for the
/// BM13xx packet equals reversing all 32 decoded bytes.
pub fn stratum_prevhash_to_wire(prevhash_hex: &str) -> [u8; 32] {
    hash_to_wire_words(&stratum_prevhash_to_internal(prevhash_hex))
}

/// Compute the Bitcoin pool share difficulty of an 80-byte block header.
pub fn header_difficulty(
    version: u32,
    prev_internal: &[u8; 32],
    merkle_internal: &[u8; 32],
    ntime: u32,
    nbits: u32,
    nonce: u32,
) -> f64 {
    let mut hdr = [0u8; 80];
    hdr[0..4].copy_from_slice(&version.to_le_bytes());
    hdr[4..36].copy_from_slice(prev_internal);
    hdr[36..68].copy_from_slice(merkle_internal);
    hdr[68..72].copy_from_slice(&ntime.to_le_bytes());
    hdr[72..76].copy_from_slice(&nbits.to_le_bytes());
    hdr[76..80].copy_from_slice(&nonce.to_le_bytes());

    let hash = sha256d(&hdr);
    // Interpret 256-bit hash (little-endian bytes, big-endian uint256 value) as f64.
    let mut val = 0.0f64;
    for &b in hash.iter().rev() {
        val = val * 256.0 + (b as f64);
    }
    if val <= 0.0 {
        return f64::INFINITY;
    }
    // Bitcoin diff-1 target = 0x00000000_FFFF0000_... = 65535 * 2^208
    const DIFF1_TARGET: f64 = 2.695953529101131e67;
    DIFF1_TARGET / val
}

/// Compute the 32-byte internal Bitcoin merkle root from Stratum coinbase pieces and branches.
pub fn compute_merkle_root(
    coinb1_hex: &str,
    extranonce1_hex: &str,
    extranonce2_hex: &str,
    coinb2_hex: &str,
    merkle_branch: &[String],
) -> [u8; 32] {
    let mut coinbase = Vec::with_capacity(
        (coinb1_hex.len() + extranonce1_hex.len() + extranonce2_hex.len() + coinb2_hex.len()) / 2,
    );
    push_hex_bytes(coinb1_hex, &mut coinbase);
    push_hex_bytes(extranonce1_hex, &mut coinbase);
    push_hex_bytes(extranonce2_hex, &mut coinbase);
    push_hex_bytes(coinb2_hex, &mut coinbase);

    let mut cur = sha256d(&coinbase);
    let mut branch_bytes = Vec::with_capacity(32);
    let mut pair = [0u8; 64];
    for branch_hex in merkle_branch {
        branch_bytes.clear();
        push_hex_bytes(branch_hex, &mut branch_bytes);
        if branch_bytes.len() == 32 {
            pair[..32].copy_from_slice(&cur);
            pair[32..].copy_from_slice(&branch_bytes);
            cur = sha256d(&pair);
        }
    }
    cur
}

// ---------------------------------------------------------------------------
// Minimal Stratum V1 client
// ---------------------------------------------------------------------------

/// Parsed Stratum V1 `mining.notify` job parameters.
#[derive(Debug, Clone)]
pub struct StratumJob {
    pub job_id: String,
    pub prevhash: String,
    pub coinb1: String,
    pub coinb2: String,
    pub merkle_branch: Vec<String>,
    pub version: String,
    pub nbits: String,
    pub ntime: String,
    pub clean_jobs: bool,
}

/// Incoming Stratum V1 message decoded from a JSON-RPC line.
#[derive(Debug, Clone)]
pub enum StratumEvent {
    Notify(StratumJob),
    SetDifficulty(f64),
    Subscribed {
        id: u64,
        extranonce1: String,
        extranonce2_size: usize,
    },
    Response {
        id: u64,
        accepted: bool,
    },
    Raw(String),
}

/// Extract the next newline-delimited JSON line from a persistent socket buffer,
/// preserving any trailing partial segment in `rx_buf`.
pub fn drain_next_line(rx_buf: &mut Vec<u8>) -> Option<String> {
    let nl_pos = rx_buf.iter().position(|&b| b == b'\n')?;
    let line_bytes: Vec<u8> = rx_buf.drain(..=nl_pos).collect();
    let trimmed_end = if line_bytes.len() >= 2 && line_bytes[line_bytes.len() - 2] == b'\r' {
        line_bytes.len() - 2
    } else {
        line_bytes.len() - 1
    };
    Some(String::from_utf8_lossy(&line_bytes[..trimmed_end]).into_owned())
}

/// Minimal Stratum V1 TCP JSON-RPC line client.
pub struct StratumClient {
    stream: TcpStream,
    rx_buf: Vec<u8>,
    next_id: u64,
    pub extranonce1: String,
    pub extranonce2_size: usize,
    pub version_rolling: bool,
}

impl StratumClient {
    /// Connect to a Stratum V1 pool (`host:port`) with connect and read timeouts.
    pub fn connect(addr: &str, timeout: Duration) -> io::Result<Self> {
        let sock = addr
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no socket address"))?;
        let stream = TcpStream::connect_timeout(&sock, timeout)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        Ok(Self {
            stream,
            rx_buf: Vec::with_capacity(2048),
            next_id: 1,
            extranonce1: String::new(),
            extranonce2_size: 4,
            version_rolling: false,
        })
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }

    pub fn set_read_timeout(&self, timeout: Duration) -> io::Result<()> {
        self.stream.set_read_timeout(Some(timeout))
    }

    pub fn set_write_timeout(&self, timeout: Duration) -> io::Result<()> {
        self.stream.set_write_timeout(Some(timeout))
    }

    fn send_line(&mut self, line: &str) -> io::Result<()> {
        self.stream.write_all(line.as_bytes())?;
        self.stream.write_all(b"\n")?;
        self.stream.flush()
    }

    /// Request BIP320 version-rolling extension (`1fffe000`) and return the request ID.
    pub fn configure_version_rolling(&mut self) -> io::Result<u64> {
        let id = self.alloc_id();
        let msg = format!(
            "{{\"id\":{id},\"method\":\"mining.configure\",\"params\":[[\"version-rolling\"],{{\"version-rolling.mask\":\"1fffe000\"}}]}}"
        );
        self.send_line(&msg)?;
        Ok(id)
    }

    /// Send `mining.subscribe` and return the JSON-RPC request ID.
    pub fn subscribe(&mut self, user_agent: &str) -> io::Result<u64> {
        let id = self.alloc_id();
        let msg = format!(
            "{{\"id\":{id},\"method\":\"mining.subscribe\",\"params\":[\"{user_agent}\"]}}"
        );
        self.send_line(&msg)?;
        Ok(id)
    }

    /// Send `mining.authorize` and return the JSON-RPC request ID.
    pub fn authorize(&mut self, worker: &str, password: &str) -> io::Result<u64> {
        let id = self.alloc_id();
        let msg = format!(
            "{{\"id\":{id},\"method\":\"mining.authorize\",\"params\":[\"{}\",\"{}\"]}}",
            json_escape(worker),
            json_escape(password)
        );
        self.send_line(&msg)?;
        Ok(id)
    }

    /// Send `mining.submit` (with optional BIP320 `version_bits` hex) and return the request ID.
    pub fn submit(
        &mut self,
        worker: &str,
        job_id: &str,
        extranonce2: &str,
        ntime: &str,
        nonce: &str,
        version_bits: Option<&str>,
    ) -> io::Result<u64> {
        let id = self.alloc_id();
        let msg = if let Some(vb) = version_bits {
            format!(
                "{{\"id\":{id},\"method\":\"mining.submit\",\"params\":[\"{}\",\"{}\",\"{}\",\"{}\",\"{}\",\"{}\"]}}",
                json_escape(worker),
                json_escape(job_id),
                json_escape(extranonce2),
                json_escape(ntime),
                json_escape(nonce),
                json_escape(vb),
            )
        } else {
            format!(
                "{{\"id\":{id},\"method\":\"mining.submit\",\"params\":[\"{}\",\"{}\",\"{}\",\"{}\",\"{}\"]}}",
                json_escape(worker),
                json_escape(job_id),
                json_escape(extranonce2),
                json_escape(ntime),
                json_escape(nonce),
            )
        };
        self.send_line(&msg)?;
        Ok(id)
    }

    fn decode_and_track(&mut self, line: &str) -> StratumEvent {
        let ev = parse_stratum_line(line.trim());
        match &ev {
            StratumEvent::Subscribed {
                extranonce1,
                extranonce2_size,
                ..
            } => {
                self.extranonce1 = extranonce1.clone();
                self.extranonce2_size = (*extranonce2_size).clamp(1, 16);
            }
            StratumEvent::Raw(raw)
                if raw.contains("\"version-rolling\":true")
                    || raw.contains("\"version-rolling\": true") =>
            {
                self.version_rolling = true;
            }
            _ => {}
        }
        ev
    }

    /// Read and decode one newline-delimited JSON-RPC event while preserving any
    /// partial trailing TCP segment across `WouldBlock` / `TimedOut` polls.
    pub fn poll_event(&mut self) -> io::Result<Option<StratumEvent>> {
        if let Some(line) = drain_next_line(&mut self.rx_buf) {
            return Ok(Some(self.decode_and_track(&line)));
        }

        let mut chunk = [0u8; 512];
        loop {
            match self.stream.read(&mut chunk) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "stratum connection closed",
                    ))
                }
                Ok(n) => {
                    if self.rx_buf.len() + n > 16 * 1024 {
                        self.rx_buf.clear();
                    }
                    self.rx_buf.extend_from_slice(&chunk[..n]);
                    if let Some(line) = drain_next_line(&mut self.rx_buf) {
                        return Ok(Some(self.decode_and_track(&line)));
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(None);
                }
                Err(e) => return Err(e),
            }
        }
    }
}

pub fn parse_stratum_line(line: &str) -> StratumEvent {
    let is_method = line.contains("\"method\"");
    if is_method && line.contains("\"mining.notify\"") {
        if let Some(job) = parse_notify_job(line) {
            return StratumEvent::Notify(job);
        }
    } else if is_method && line.contains("\"mining.set_difficulty\"") {
        if let Some(params) = extract_bracket_slice(line, "\"params\"") {
            if let Ok(diff) = params.trim().parse::<f64>() {
                return StratumEvent::SetDifficulty(diff);
            }
        }
    } else if let Some(id) = extract_u64_field(line, "\"id\"") {
        if let Some((en1, en2_size)) = parse_subscribe_result(line) {
            return StratumEvent::Subscribed {
                id,
                extranonce1: en1,
                extranonce2_size: en2_size,
            };
        }
        if line.contains("\"version-rolling\"") {
            return StratumEvent::Raw(line.to_string());
        }
        let accepted = line.contains("\"result\":true") || line.contains("\"result\": true");
        return StratumEvent::Response { id, accepted };
    }
    StratumEvent::Raw(line.to_string())
}

fn parse_subscribe_result(line: &str) -> Option<(String, usize)> {
    let result = extract_bracket_slice(line, "\"result\"")?;
    let mut depth = 0usize;
    let mut in_str = false;
    let mut cur = String::new();
    let mut top_strings = Vec::new();
    let mut num_buf = String::new();

    for ch in result.chars() {
        if in_str {
            if ch == '"' {
                in_str = false;
                if depth == 0 {
                    top_strings.push(core::mem::take(&mut cur));
                } else {
                    cur.clear();
                }
            } else {
                cur.push(ch);
            }
            continue;
        }
        match ch {
            '"' => {
                in_str = true;
                cur.clear();
            }
            '[' => depth += 1,
            ']' => depth = depth.saturating_sub(1),
            c if depth == 0 && c.is_ascii_digit() => num_buf.push(c),
            _ => {}
        }
    }

    let en1 = top_strings.into_iter().next()?;
    let en2_size = num_buf.parse::<usize>().ok().filter(|&n| n > 0)?;
    Some((en1, en2_size))
}

fn extract_json_string_field(body: &str, key: &str) -> Option<String> {
    let idx = body.find(key)? + key.len();
    let rest = body[idx..].trim_start().strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_owned())
}

fn extract_json_bool_field(body: &str, key: &str) -> Option<bool> {
    let idx = body.find(key)? + key.len();
    let rest = body[idx..].trim_start().strip_prefix(':')?.trim_start();
    if rest.starts_with("true") {
        Some(true)
    } else if rest.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

fn extract_json_null_field(body: &str, key: &str) -> bool {
    let Some(pos) = body.find(key) else {
        return false;
    };
    let rest = body[pos + key.len()..].trim_start();
    let Some(after_colon) = rest.strip_prefix(':') else {
        return false;
    };
    after_colon.trim_start().starts_with("null")
}

fn extract_json_f64_field(body: &str, key: &str) -> Option<f64> {
    let idx = body.find(key)? + key.len();
    let rest = body[idx..].trim_start().strip_prefix(':')?.trim_start();
    let mut end = 0usize;
    let bytes = rest.as_bytes();
    while end < bytes.len() {
        let b = bytes[end];
        if b.is_ascii_digit() || b == b'.' || (end == 0 && b == b'-') {
            end += 1;
        } else {
            break;
        }
    }
    if end == 0 {
        return None;
    }
    rest[..end].parse::<f64>().ok().filter(|v| v.is_finite())
}

fn extract_json_object_slice<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    let idx = body.find(key)? + key.len();
    let rest = body[idx..].trim_start().strip_prefix(':')?.trim_start();
    if !rest.starts_with('{') {
        return None;
    }
    let bytes = rest.as_bytes();
    let mut depth = 0usize;
    let mut in_str = false;
    let mut esc = false;
    for (i, &b) in bytes.iter().enumerate() {
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(&rest[..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

fn extract_u64_field(line: &str, key: &str) -> Option<u64> {
    let idx = line.find(key)? + key.len();
    let rest = line[idx..].trim_start().strip_prefix(':')?.trim_start();
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

fn extract_bracket_slice<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let idx = line.find(key)? + key.len();
    let rest = line[idx..].trim_start().strip_prefix(':')?.trim_start();
    if !rest.starts_with('[') {
        return None;
    }
    let bytes = rest.as_bytes();
    let mut depth = 0usize;
    let mut in_str = false;
    let mut esc = false;
    for (i, &b) in bytes.iter().enumerate() {
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'[' => depth += 1,
            b']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(&rest[1..i]);
                }
            }
            _ => {}
        }
    }
    None
}

fn parse_notify_job(line: &str) -> Option<StratumJob> {
    let params = extract_bracket_slice(line, "\"params\"")?;
    let mut strings = Vec::with_capacity(8);
    let mut merkle_branch = Vec::new();
    let mut in_str = false;
    let mut depth = 0usize;
    let mut cur = String::new();

    for ch in params.chars() {
        if in_str {
            if ch == '"' {
                in_str = false;
                if depth == 0 {
                    strings.push(core::mem::take(&mut cur));
                } else if depth == 1 {
                    merkle_branch.push(core::mem::take(&mut cur));
                } else {
                    cur.clear();
                }
            } else {
                cur.push(ch);
            }
            continue;
        }
        match ch {
            '[' => depth += 1,
            ']' => depth = depth.saturating_sub(1),
            '"' => {
                in_str = true;
                cur.clear();
            }
            _ => {}
        }
    }

    if strings.len() < 7 {
        return None;
    }

    let clean_jobs = params.trim_end().ends_with("true");
    Some(StratumJob {
        job_id: strings[0].clone(),
        prevhash: strings[1].clone(),
        coinb1: strings[2].clone(),
        coinb2: strings[3].clone(),
        merkle_branch,
        version: strings[4].clone(),
        nbits: strings[5].clone(),
        ntime: strings[6].clone(),
        clean_jobs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to_hex(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            let _ = write!(s, "{b:02x}");
        }
        s
    }

    #[test]
    fn sha256_abc_vector() {
        // FIPS 180-4 test vector for "abc"
        assert_eq!(
            to_hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn stratum_subscribe_notify_and_merkle_rosetta() {
        // Real BM1370 capture from mujina-miner/src/asic/bm13xx/test_data.rs
        let sub_line = r#"{"id":1,"result":[[["mining.set_difficulty","d1"],["mining.notify","n1"]],"4128064f",4],"error":null}"#;
        match parse_stratum_line(sub_line) {
            StratumEvent::Subscribed {
                id,
                extranonce1,
                extranonce2_size,
            } => {
                assert_eq!(id, 1);
                assert_eq!(extranonce1, "4128064f");
                assert_eq!(extranonce2_size, 4);
            }
            other => panic!("expected Subscribed, got {other:?}"),
        }

        let notify = r#"{
          "id": null,
          "method": "mining.notify",
          "params": [
            "875b4b7",
            "6b6455fd6db962c101f2d4fc0d67f4a3bc96391d000152960000000000000000",
            "02000000010000000000000000000000000000000000000000000000000000000000000000ffffffff170330c30d5075626c69632d506f6f6c",
            "ffffffff02e5b5c61200000000220020984a77c289084ff2d434c316bdada021c6c183d507c8a20d3b159b09ac02fe280000000000000000266a24aa21a9edb98ee50410ed4abd48401ed484fc874409d086a3faf0816136a8ad6168314c5800000000",
            [
              "21af451ddb51e887ff1feb5592b87290098565035eb8500031aedcc776d4e72a",
              "c5af269519c809a9546d5a58ca6445d3dbb80cb7045448ecc48309af034da8f8",
              "fb9f8f9959f6bb0ceb63fa53aed1d5a615c6b6d3f50a468ea89a45a1234bda74",
              "a4f4fee8e5fc19ca8d93e67b9236c37ddb864982010434745c0abfe9b914980c",
              "33092206642744fbe5499c3e621cd5c6b52733e54fbebd869f070082b807f740",
              "3b857e32c5cff4864efab967b9a456ca03b2167ab96bd9076ce294c8a67a7fe2",
              "881a07cd881d0c3e590b4b090ea8d58e1439dc56c63686f7de23c47045441e30",
              "315e4dbcc8e7b1c9d594a73978268791880dddb2c26eec8e75768668dad99d80",
              "69952b77c632be16b1ac7ac7048f13d4e962b2e215d79a343f01e6e281d7c304",
              "fc63eb4392c4d6c6d689788875fca35143fdcd4f4a82e8698e0e441751a70b4a",
              "09e419bbe20aa3a7640f1b91f50599ceddff899e90d3f18951ad5418c4850a6b",
              "004978aa346b4f1880bcadb3ca3792d771ee6aeca427f61e74baba44b75cfb88"
            ],
            "20000000",
            "17023a04",
            "685468d7",
            false
          ]
        }"#;

        let StratumEvent::Notify(job) = parse_stratum_line(notify) else {
            panic!("failed to parse mining.notify");
        };
        assert_eq!(job.merkle_branch.len(), 12);
        assert_eq!(job.version, "20000000");

        let merkle_internal = compute_merkle_root(
            &job.coinb1,
            "4128064f",
            "17000000",
            &job.coinb2,
            &job.merkle_branch,
        );
        let merkle_wire = hash_to_wire_words(&merkle_internal);
        assert_eq!(
            to_hex(&merkle_wire),
            "5519a7cb044f88726355919e61a98bcf71a0c28795ea54db8c36414b06ddf5f0"
        );

        let prev_wire = stratum_prevhash_to_wire(&job.prevhash);
        assert_eq!(
            to_hex(&prev_wire),
            "0000000000000000965201001d3996bca3f4670dfcd4f201c162b96dfd55646b"
        );

        let prev_int = stratum_prevhash_to_internal(&job.prevhash);
        let diff = header_difficulty(
            0x2000_0000 | 0x00b4_4000,
            &prev_int,
            &merkle_internal,
            0x6854_68d7,
            0x1702_3a04,
            0x7552_034c,
        );
        assert!(
            (diff - 29588.0).abs() < 2.0,
            "expected ~29588 diff, got {diff}"
        );
    }

    #[test]
    fn setup_form_and_pool_url_parser() {
        let cfg = DeviceConfig {
            pool_host: String::from("solo.ckpool.org"),
            pool_port: 3333,
            pool_user: String::from("bc1qtest"),
            pool_pass: String::from("pw"),
            ..DeviceConfig::default()
        };
        let updated = parse_setup_form("wifi_ssid=Home+Net&wifi_pass=secret%21", cfg);
        assert_eq!(updated.wifi_ssid, "Home Net");
        assert_eq!(updated.wifi_pass, "secret!");
        assert_eq!(updated.pool_host, "solo.ckpool.org");
        assert_eq!(updated.pool_port, 3333);

        assert_eq!(
            parse_stratum_url("stratum+tcp://parasite.wtf:42069", 3333),
            Some((String::from("parasite.wtf"), 42069))
        );
        assert_eq!(
            parse_stratum_url("stratum+ssl://pool.example.com", 42069),
            Some((String::from("pool.example.com"), 42069))
        );
        assert_eq!(parse_stratum_url("http://invalid", 42069), None);
    }

    #[test]
    fn dashboard_data_and_system_json_shape() {
        let mut t = LiveTelemetry::default();
        t.is_ap_mode = false;
        t.ip_addr = String::from("192.168.4.23");
        t.hashrate_ghs = 4820.5;
        t.pin_w = 88.2;
        t.vin_v = 12.66;
        t.iin_a = 7.05;
        t.iout_a = 72.5;
        t.chip_count = 4;
        t.pool_connected = true;
        let data = format_dashboard_data_json(&t);
        assert!(data.contains("\"board_name\":\"nerdqx\""));
        assert!(data.contains("\"chip_count\":4"));
        assert!(data.contains("\"pool_connected\":true"));
        assert!(data.contains("\"core_a\":72.50"));
        assert!(data.contains("\"input_a\":7.05"));
        assert!(data.contains("\"shares\":0"));
        assert!(data.contains("\"pool_url\":\"stratum+tcp://parasite.wtf:42069\""));
        let sys = format_system_json(&t);
        assert!(sys.contains("\"apply\":\"live\""));
        assert!(sys.contains("\"chip_count\":4"));
        assert!(sys.contains("\"pool_connected\":true"));
        assert!(sys.contains("/api/v0/boards/nerdqx/psu"));
        let miner = format_miner_json(&t);
        assert!(miner.contains("\"iout_a\":72.50"));
        assert!(miner.contains("\"chip_count\":4"));
    }

    #[test]
    fn stratum_partial_line_reassembly_across_reads() {
        let mut buf = Vec::new();
        buf.extend_from_slice(br#"{"id":null,"method":"mining.set_"#);
        assert_eq!(drain_next_line(&mut buf), None);
        buf.extend_from_slice(b"difficulty\",\"params\":[512]}\r\n{\"id\":1");
        assert_eq!(
            drain_next_line(&mut buf).as_deref(),
            Some(r#"{"id":null,"method":"mining.set_difficulty","params":[512]}"#)
        );
        assert_eq!(drain_next_line(&mut buf), None);
        buf.extend_from_slice(b",\"result\":true}\n");
        assert_eq!(
            drain_next_line(&mut buf).as_deref(),
            Some(r#"{"id":1,"result":true}"#)
        );
        assert!(buf.is_empty());
    }

    #[test]
    fn psu_patch_rejects_unconfirmed_20a_and_360w_tiers() {
        assert_eq!(
            validate_psu_patch(r#"{"preset":"1A"}"#),
            Ok(("stock", "1A", 144.0, 15.0))
        );
        assert_eq!(
            validate_psu_patch(r#"{"preset":"2A"}"#),
            Ok(("2A", "2A", 180.0, 15.0))
        );
        // 20A (>180W) tiers must reject without confirm:true
        assert!(validate_psu_patch(r#"{"preset":"1B"}"#).is_err());
        assert!(validate_psu_patch(r#"{"preset":"4A","confirm":false}"#).is_err());
        assert_eq!(
            validate_psu_patch(r#"{"preset":"1B","confirm":true}"#),
            Ok(("1B", "1B", 192.0, 20.0))
        );
        assert_eq!(
            validate_psu_patch(r#"{"preset":"4A","confirm":true}"#),
            Ok(("4A", "4A", 240.0, 20.0))
        );
        // Dropped 3A (216W) / 5A (360W) must always be rejected
        assert!(validate_psu_patch(r#"{"preset":"3A","confirm":true}"#).is_err());
        assert!(validate_psu_patch(r#"{"preset":"5A","confirm":true}"#).is_err());
    }

    #[test]
    fn system_capabilities_and_detail_json_parity() {
        let mut t = LiveTelemetry::default();
        t.chip_count = 4;
        t.pool_connected = true;
        t.psu_budget_w = 192.0;
        t.fail_rate_pct = 1.25;
        t.chip_offsets_mhz = [6.25, 0.0, -6.25, 12.5];
        t.chip_error_pct = [0.5, 0.0, 1.5, 0.0];
        t.tuner.events.push(TunerLogEntry {
            created_uptime_s: 10,
            frequency_mhz: 500.0,
            core_voltage_mv: 1150,
            power_w: 88.0,
            hashrate_ths: 4.9,
            efficiency_j_th: 18.0,
            asic_temp_c: 54.0,
            message: String::from("test step"),
        });
        t.uptime_s = 25;

        let sys = format_system_json(&t);
        assert!(sys.contains("\"tuner\":\"/api/v0/boards/nerdqx/autotune\""));
        assert!(sys.contains("\"manual_tuning\":\"/api/v0/boards/nerdqx/tuning\""));
        assert!(sys.contains("\"fan_curve\":true"));
        assert!(sys.contains("\"psu_contract_w\":192.0"));
        assert!(sys.contains("\"ceiling_w\":null"));
        assert!(sys.contains("\"family\":\"nerdqx\""));
        assert!(sys.contains("\"freq_mhz\":[100,825]"));
        assert!(sys.contains("\"core_mv\":[950,1280]"));
        assert!(sys.contains("\"fan_min_pct\":{\"vr\":40,\"asic\":25}"));
        assert!(sys.contains("\"id\":\"max_hash\",\"label\":\"Max hash\""));

        let data = format_dashboard_data_json(&t);
        assert!(data.contains("\"avg_fail_rate_pct\":1.25"));
        assert!(data.contains("\"fail_rate_pct\":1.25"));
        assert!(data.contains("\"offset_mhz\":6.25"));
        assert!(data.contains("\"spd_dh\":1.50"));

        let auto = format_autotune_json(&t);
        assert!(auto.contains("\"profile\":\"efficient\""));
        assert!(auto.contains("\"t_secs_ago\":15"));
        assert!(auto.contains("\"message\":\"test step\""));

        let detail = format_detail_json(&t);
        assert!(detail.contains("\"paused\":false"));
        assert!(detail.contains("\"chip_offset_mhz\":[6.25,0.00,-6.25,12.50]"));
        assert!(detail.contains("\"asics_total\":4"));
    }

    #[test]
    fn autotune_fan_curve_and_manual_parsers() {
        let cur = EmbeddedTunerState::default();
        let next = parse_autotune_patch(
            r#"{"board_name":"nerdqx","continuous":false,"target":{"axis":"power","value":165}}"#,
            &cur,
        )
        .unwrap();
        assert!(next.enabled);
        assert!(!next.continuous);
        assert_eq!(next.target_axis.as_deref(), Some("power"));
        assert_eq!(next.target_value, Some(165.0));

        let off = parse_autotune_patch(r#"{"enabled":false,"profile":null}"#, &next).unwrap();
        assert!(!off.enabled);
        assert_eq!(off.phase, "off");

        assert_eq!(
            parse_manual_tuning_patch(r#"{"frequency_mhz":515,"core_voltage_mv":1165}"#, 495.0, 1150),
            Ok((515.0, 1165))
        );
        assert!(parse_manual_tuning_patch(r#"{}"#, 495.0, 1150).is_err());

        assert_eq!(parse_fan_patch(r#"{"auto":false,"percent":72}"#), Ok((false, Some(72))));
        assert_eq!(parse_fan_patch(r#"{"auto":false,"percent":0}"#), Ok((false, Some(25))));
        assert_eq!(parse_fan_patch(r#"{"mode":"auto"}"#), Ok((true, None)));
        assert!(parse_fan_patch(r#"{"auto":false}"#).is_err());

        let pts = parse_fan_curve_patch(
            r#"{"points":[{"temp_c":60,"duty_pct":80},{"temp_c":40,"duty_pct":30}]}"#,
        )
        .unwrap();
        assert_eq!(pts, vec![(40.0, 30), (60.0, 80)]);
        assert_eq!(eval_fan_curve_pct(&pts, 50.0), 55);
        assert_eq!(eval_fan_curve_pct(&pts, 30.0), 30);
        assert_eq!(eval_fan_curve_pct(&pts, 75.0), 80);
        assert_eq!(eval_fan_curve_pct(&[(40.0, 0), (60.0, 80)], 30.0), 25);

        let def_t = LiveTelemetry::default();
        assert_eq!(eval_fan_curve_pct(&def_t.fan_curve, 50.0), 30);
        assert_eq!(eval_fan_curve_pct(&def_t.fan_curve, 58.0), 50);
        assert_eq!(eval_fan_curve_pct(&def_t.fan_curve, 60.0), 80);
        assert_eq!(eval_fan_curve_pct(&def_t.fan_curve, 62.0), 100);
        assert_eq!(eval_fan_curve_pct(&def_t.fan_curve, 72.0), 100);
        assert!((58.0..=63.0).contains(&def_t.tuner.fan_target_c));
        for prof in ["quiet", "efficient", "balanced", "max_hash"] {
            let p_state = parse_autotune_patch(&format!("{{\"profile\":\"{prof}\"}}"), &cur).unwrap();
            assert!((58.0..=63.0).contains(&p_state.fan_target_c));
        }
        assert!((58.0..=63.0).contains(&next.fan_target_c));
    }

    #[test]
    fn sliding_window_hashrate_estimator_math() {
        let mut win = crate::bm1370::ChainHashrateWindow::new();
        for _ in 0..10 {
            for chip in 0..4 {
                win.record_valid(chip);
            }
            win.commit_second();
        }
        // 1 valid hit/s at diff 256 -> ~1099.51 GH/s per chip, ~4398 GH/s total, 0% errors.
        for chip in 0..4 {
            assert!((win.chip_ghs[chip] - 1099.51).abs() < 1.0);
            assert_eq!(win.chip_err_pct[chip], 0.0);
        }
        assert!((win.total_ghs() - 4398.05).abs() < 3.0);
        // Record 1 error + 1 valid on chip 0 over 1s -> overall chip 0 error rate = 1 / 12 = 8.33%.
        win.record_valid(0);
        win.record_error(0);
        win.commit_second();
        assert!((win.chip_err_pct[0] - (100.0 / 12.0)).abs() < 0.1);
    }

    #[test]
    fn reset_reason_mapping_and_heap_telemetry_json() {
        assert_eq!(reset_reason_name(0), "unknown");
        assert_eq!(reset_reason_name(1), "poweron");
        assert_eq!(reset_reason_name(4), "panic");
        assert_eq!(reset_reason_name(6), "task_wdt");
        assert_eq!(reset_reason_name(9), "brownout");
        assert_eq!(reset_reason_name(15), "cpu_lockup");
        assert_eq!(reset_reason_name(99), "unknown");

        let mut t = LiveTelemetry::default();
        t.reset_reason = reset_reason_name(6);
        t.free_internal_kb = 180;
        t.min_internal_kb = 140;
        t.free_psram_kb = 7900;

        let sys = format_system_json(&t);
        assert!(sys.contains("\"reset_reason\":\"task_wdt\""));
        assert!(sys.contains("\"free_internal_kb\":180"));
        assert!(sys.contains("\"min_internal_kb\":140"));
        assert!(sys.contains("\"free_psram_kb\":7900"));

        let miner = format_miner_json(&t);
        assert!(miner.contains("\"reset_reason\":\"task_wdt\""));
        assert!(miner.contains("\"free_internal_kb\":180"));
        assert!(miner.contains("\"min_internal_kb\":140"));
        assert!(miner.contains("\"free_psram_kb\":7900"));
    }
}
