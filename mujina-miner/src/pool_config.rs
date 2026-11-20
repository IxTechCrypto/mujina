//! Persisted pool configuration, read at daemon startup and writable live
//! via `PATCH /api/v0/pool` without editing the boot script by hand.
//!
//! Stored at `/data/userconfig/pool.conf`, the same persistent partition
//! `bin/ble_setup.rs` uses for WiFi credentials (`wpa_supplicant.conf`).
//! Plain `key=value` lines -- `url`, `user`, `pass` -- to match that
//! file's own simplicity rather than pull in a TOML/JSON dependency for
//! three fields.
//!
//! Precedence at startup (see `daemon.rs`): a field set here overrides the
//! `MUJINA_POOL_*` env vars; any field left unset falls back to the env
//! var, then to the daemon's own default. `PUT /api/v0/pools` writes
//! `pools.conf` and applies the change in-process. `pool.conf` remains
//! pool 1, for `PATCH /api/v0/pool`.

use std::io;
use std::path::Path;

pub const POOL_CONFIG_PATH: &str = "/data/userconfig/pool.conf";
/// Up to four pools. Written atomically. An old `pool.conf` is read as
/// pool 1 at 100% when this file is absent.
pub const POOLS_CONFIG_PATH: &str = "/data/userconfig/pools.conf";
pub const MAX_POOLS: usize = 4;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PoolConfig {
    pub url: Option<String>,
    pub user: Option<String>,
    pub pass: Option<String>,
}

/// Directory that holds `pool.conf` and `pools.conf`. Tests point this at a
/// temp dir with `MUJINA_USERCONFIG`; the device uses `/data/userconfig`.
pub fn userconfig_dir() -> std::path::PathBuf {
    std::env::var("MUJINA_USERCONFIG")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/data/userconfig"))
}

fn pool_path() -> std::path::PathBuf {
    userconfig_dir().join("pool.conf")
}

fn pools_path() -> std::path::PathBuf {
    userconfig_dir().join("pools.conf")
}

/// Reads the persisted pool config file, if present. A missing file or an
/// unparseable/blank line is treated as "no override" for that field, not
/// an error -- callers fall back to env vars/defaults regardless.
pub fn load() -> PoolConfig {
    load_from(&pool_path())
}

/// Merges the given fields onto the currently persisted config (`None`
/// keeps the current persisted value for that field) and writes the
/// result, returning the merged config.
pub fn save_merged(
    url: Option<&str>,
    user: Option<&str>,
    pass: Option<&str>,
) -> io::Result<PoolConfig> {
    save_merged_to(&pool_path(), url, user, pass)
}

fn load_from(path: &Path) -> PoolConfig {
    let mut cfg = PoolConfig::default();
    let Ok(contents) = std::fs::read_to_string(path) else {
        return cfg;
    };
    for line in contents.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match key.trim() {
            "url" => cfg.url = Some(value.to_string()),
            "user" => cfg.user = Some(value.to_string()),
            "pass" => cfg.pass = Some(value.to_string()),
            _ => {}
        }
    }
    cfg
}

fn save_merged_to(
    path: &Path,
    url: Option<&str>,
    user: Option<&str>,
    pass: Option<&str>,
) -> io::Result<PoolConfig> {
    let mut cfg = load_from(path);
    if let Some(url) = url {
        cfg.url = Some(url.to_string());
    }
    if let Some(user) = user {
        cfg.user = Some(user.to_string());
    }
    if let Some(pass) = pass {
        cfg.pass = Some(pass.to_string());
    }

    let mut out = String::new();
    if let Some(u) = &cfg.url {
        out.push_str(&format!("url={u}\n"));
    }
    if let Some(u) = &cfg.user {
        out.push_str(&format!("user={u}\n"));
    }
    if let Some(p) = &cfg.pass {
        out.push_str(&format!("pass={p}\n"));
    }
    atomic_write(path, out.as_bytes())?;
    Ok(cfg)
}

/// One configured pool. `weight` is a percent of hash time. 0 means the
/// pool is a failover backup and is not in the normal rotation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolEntry {
    pub url: String,
    pub user: String,
    pub pass: String,
    pub weight: u16,
}

/// Rejects a set that is not 1..=4 pools, whose weights do not sum to 100,
/// or whose URL is not `stratum+tcp://` or `stratum+ssl://`.
pub fn validate(pools: &[PoolEntry]) -> Result<(), &'static str> {
    if pools.is_empty() || pools.len() > MAX_POOLS {
        return Err("need 1 to 4 pools");
    }
    let sum: u32 = pools.iter().map(|p| u32::from(p.weight)).sum();
    if sum != 100 {
        return Err("weights must sum to 100");
    }
    for pool in pools {
        if !stratum_url(&pool.url) {
            return Err("url must be stratum+tcp:// or stratum+ssl://");
        }
    }
    Ok(())
}

pub fn stratum_url(url: &str) -> bool {
    let rest = url
        .strip_prefix("stratum+tcp://")
        .or_else(|| url.strip_prefix("stratum+ssl://"));
    matches!(rest, Some(host) if !host.is_empty())
}

/// Loads `pools.conf`, or migrates a legacy `pool.conf` into one pool at
/// 100%. A missing file is an empty list; the daemon then uses
/// `MUJINA_POOL_*`.
pub fn load_pools() -> Vec<PoolEntry> {
    load_pools_from(&pools_path(), &pool_path())
}

/// What the daemon should connect to. A saved file wins. With no file, one
/// pool at 100% is built from `MUJINA_POOL_URL`, `MUJINA_POOL_USER` and
/// `MUJINA_POOL_PASS`. Nothing is baked into the binary.
pub fn effective_pools() -> Vec<PoolEntry> {
    let mut pools = load_pools();
    if pools.is_empty() {
        if let Ok(url) = std::env::var("MUJINA_POOL_URL")
            && stratum_url(&url)
        {
            pools.push(PoolEntry {
                url,
                user: std::env::var("MUJINA_POOL_USER")
                    .unwrap_or_else(|_| "mujina-testing".to_string()),
                pass: std::env::var("MUJINA_POOL_PASS").unwrap_or_else(|_| "x".to_string()),
                weight: 100,
            });
        }
        return pools;
    }
    if pools[0].user.is_empty()
        && let Ok(user) = std::env::var("MUJINA_POOL_USER")
    {
        pools[0].user = user;
    }
    if pools[0].pass.is_empty()
        && let Ok(pass) = std::env::var("MUJINA_POOL_PASS")
    {
        pools[0].pass = pass;
    }
    pools
}

pub fn load_pools_from(pools_path: &Path, legacy_path: &Path) -> Vec<PoolEntry> {
    if let Some(pools) = read_pools_file(pools_path) {
        return pools;
    }
    let legacy = load_from(legacy_path);
    match legacy.url {
        Some(url) => vec![PoolEntry {
            url,
            user: legacy.user.unwrap_or_default(),
            pass: legacy.pass.unwrap_or_default(),
            weight: 100,
        }],
        None => Vec::new(),
    }
}

/// Validates, writes `pools.conf` atomically, and mirrors pool 1 into
/// `pool.conf` so older readers still see it.
pub fn save_pools(pools: &[PoolEntry]) -> io::Result<()> {
    validate(pools).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    save_pools_to(&pools_path(), &pool_path(), pools)
}

pub fn save_pools_to(pools_path: &Path, legacy_path: &Path, pools: &[PoolEntry]) -> io::Result<()> {
    validate(pools).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    atomic_write(pools_path, &encode_pools(pools))?;
    let first = &pools[0];
    save_merged_to(
        legacy_path,
        Some(&first.url),
        Some(&first.user),
        Some(&first.pass),
    )?;
    Ok(())
}

fn encode_pools(pools: &[PoolEntry]) -> Vec<u8> {
    let mut out = String::new();
    for (i, pool) in pools.iter().enumerate() {
        out.push_str(&format!(
            "{i}.url={}\n{i}.user={}\n{i}.pass={}\n{i}.weight={}\n",
            pool.url, pool.user, pool.pass, pool.weight
        ));
    }
    out.into_bytes()
}

fn read_pools_file(path: &Path) -> Option<Vec<PoolEntry>> {
    let contents = std::fs::read_to_string(path).ok()?;
    let mut slots: Vec<Option<PoolEntry>> = Vec::new();
    for line in contents.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let Some((idx, field)) = key.trim().split_once('.') else {
            continue;
        };
        let Ok(idx) = idx.parse::<usize>() else {
            continue;
        };
        if idx >= MAX_POOLS {
            continue;
        }
        if slots.len() <= idx {
            slots.resize(idx + 1, None);
        }
        let entry = slots[idx].get_or_insert(PoolEntry {
            url: String::new(),
            user: String::new(),
            pass: String::new(),
            weight: 0,
        });
        match field {
            "url" => entry.url = value.trim().to_string(),
            "user" => entry.user = value.trim().to_string(),
            "pass" => entry.pass = value.trim().to_string(),
            "weight" => entry.weight = value.trim().parse().unwrap_or(0),
            _ => {}
        }
    }
    let pools: Vec<PoolEntry> = slots
        .into_iter()
        .flatten()
        .filter(|p| !p.url.is_empty())
        .collect();
    if pools.is_empty() { None } else { Some(pools) }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("conf.tmp");
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && let Ok(dir) = std::fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "mujina_pool_config_test_{name}_{:?}",
            std::thread::current().id()
        ))
    }

    #[test]
    fn missing_file_loads_as_default() {
        let path = temp_path("missing");
        let _ = std::fs::remove_file(&path);
        assert_eq!(load_from(&path), PoolConfig::default());
    }

    #[test]
    fn round_trips_all_fields() {
        let path = temp_path("roundtrip");
        let cfg = save_merged_to(
            &path,
            Some("stratum+tcp://pool:3333"),
            Some("wallet.worker"),
            Some("x"),
        )
        .unwrap();
        assert_eq!(cfg.url.as_deref(), Some("stratum+tcp://pool:3333"));
        assert_eq!(cfg.user.as_deref(), Some("wallet.worker"));
        assert_eq!(cfg.pass.as_deref(), Some("x"));
        assert_eq!(load_from(&path), cfg);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn unset_fields_keep_previous_value() {
        let path = temp_path("merge");
        save_merged_to(&path, Some("stratum+tcp://a:3333"), Some("user-a"), None).unwrap();
        let cfg = save_merged_to(&path, Some("stratum+tcp://b:3333"), None, None).unwrap();
        assert_eq!(cfg.url.as_deref(), Some("stratum+tcp://b:3333"));
        assert_eq!(cfg.user.as_deref(), Some("user-a"));
        assert_eq!(cfg.pass, None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn legacy_pool_conf_becomes_pool_one_at_100_percent() {
        let dir = temp_path("migrate");
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = dir.join("pool.conf");
        let pools = dir.join("pools.conf");
        std::fs::write(
            &legacy,
            "url=stratum+tcp://old.example:3333\nuser=legacy.worker\npass=secret\n",
        )
        .unwrap();
        let loaded = load_pools_from(&pools, &legacy);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].url, "stratum+tcp://old.example:3333");
        assert_eq!(loaded[0].user, "legacy.worker");
        assert_eq!(loaded[0].pass, "secret");
        assert_eq!(loaded[0].weight, 100);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pools_file_round_trips_and_rejects_a_bad_set() {
        let dir = temp_path("multi");
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = dir.join("pool.conf");
        let pools_path = dir.join("pools.conf");
        let pools = vec![
            PoolEntry {
                url: "stratum+tcp://a.example:3333".into(),
                user: "a.worker".into(),
                pass: "x".into(),
                weight: 60,
            },
            PoolEntry {
                url: "stratum+ssl://b.example:3333".into(),
                user: "b.worker".into(),
                pass: "y".into(),
                weight: 40,
            },
        ];
        save_pools_to(&pools_path, &legacy, &pools).unwrap();
        assert_eq!(load_pools_from(&pools_path, &legacy), pools);
        let text = std::fs::read_to_string(&legacy).unwrap();
        assert!(text.contains("url=stratum+tcp://a.example:3333"));
        assert!(
            validate(&[PoolEntry {
                url: "http://nope".into(),
                user: "a".into(),
                pass: "x".into(),
                weight: 100,
            }])
            .is_err()
        );
        assert!(validate(&[]).is_err());
        let mut five = pools.clone();
        five.push(pools[0].clone());
        five.push(pools[0].clone());
        five.push(pools[0].clone());
        assert!(validate(&five).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
