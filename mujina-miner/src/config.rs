//! Persisted miner settings: the miner's name and its pool credentials.
//!
//! These are the settings a user edits from the dashboard rather than from
//! the launch script, so they must outlive the process. They live in
//! `mujina-settings.json` beside the auto-tune state file, in the directory
//! named by `MUJINA_STATE_DIR`.
//!
//! Precedence is **file over environment**. The environment variables
//! (`MUJINA_POOL_URL` and friends) remain the way to configure a daemon that
//! has never been given settings through the API; once the file exists it is
//! authoritative, because otherwise a launch script exporting the old pool
//! would silently undo what the user just saved.
//!
//! Nothing here is applied live. The daemon reads its settings once at
//! startup, so a change takes effect on the next restart; the API says so in
//! its response rather than pretending otherwise.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::tracing::prelude::*;

/// Directory holding the daemon's persistent state files.
///
/// Next to the daemon by default; overridable for tests and deployments.
pub fn state_dir() -> PathBuf {
    std::env::var_os("MUJINA_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn settings_path() -> PathBuf {
    state_dir().join("mujina-settings.json")
}

/// Check that a pool URL is one the daemon will actually be able to dial.
///
/// Worth doing at the point of saving rather than at connect time: the
/// setting is only read at startup, so a URL that turns out to be
/// unparseable is discovered after a restart, with the miner already down
/// and no longer able to say why. Mirrors `stratum_v1::Connection::connect`
/// -- optional `stratum+tcp://` or `tcp://` scheme, then `host:port`.
pub fn validate_pool_url(url: &str) -> Result<(), &'static str> {
    let authority = url
        .strip_prefix("stratum+tcp://")
        .or_else(|| url.strip_prefix("tcp://"))
        .unwrap_or(url);
    // rsplit_once, not split_once: an IPv6 literal is full of colons and
    // only the last one separates the port.
    let Some((host, port)) = authority.rsplit_once(':') else {
        return Err("pool URL needs a port, e.g. stratum+tcp://pool.example.com:3333");
    };
    if host.is_empty() {
        return Err("pool URL has no host");
    }
    if port.parse::<u16>().is_err() {
        return Err("pool URL port must be a number from 0 to 65535");
    }
    Ok(())
}

/// Pool connection settings.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PoolSettings {
    /// Stratum v1 pool URL, e.g. `stratum+tcp://pool.example.com:3333`.
    pub url: String,

    /// Account or address the pool authorizes. The miner's name is appended
    /// to this to form the worker string actually sent -- see
    /// [`MinerSettings::worker_username`].
    pub user: String,

    /// Worker password. Most pools ignore it and take any value.
    pub password: String,
}

/// The full set of user-editable miner settings.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct MinerSettings {
    /// Friendly name for this miner. Shown in the dashboard and appended to
    /// the pool user as the worker name, so the pool's own dashboard
    /// distinguishes this rig from others on the same account.
    ///
    /// `None` means unnamed: the pool user is sent verbatim, which is what
    /// every existing deployment already does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Pool to mine to. `None` runs the built-in dummy job source, matching
    /// the behavior of leaving `MUJINA_POOL_URL` unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<PoolSettings>,
}

impl MinerSettings {
    /// Load settings, preferring the saved file and falling back to the
    /// environment for anything it does not define.
    ///
    /// A missing file is normal (first run). An unreadable or corrupt one is
    /// reported and then ignored in favor of the environment, rather than
    /// failing startup or being silently overwritten.
    pub fn load() -> Self {
        let path = settings_path();
        let mut settings = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Self>(&bytes) {
                Ok(settings) => settings,
                Err(e) => {
                    warn!(error = %e, path = %path.display(),
                          "Ignoring unreadable miner settings file");
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        };

        if settings.pool.is_none() {
            settings.pool = Self::pool_from_env();
        }
        settings
    }

    /// Pool settings from the environment, or `None` when no pool URL is set.
    fn pool_from_env() -> Option<PoolSettings> {
        let url = std::env::var("MUJINA_POOL_URL").ok()?;
        Some(PoolSettings {
            url,
            user: std::env::var("MUJINA_POOL_USER")
                .unwrap_or_else(|_| "mujina-testing".to_string()),
            password: std::env::var("MUJINA_POOL_PASS").unwrap_or_else(|_| "x".to_string()),
        })
    }

    /// Persist these settings, replacing whatever was saved before.
    ///
    /// Written to a temp file and renamed over the real one so a crash
    /// mid-write cannot leave a truncated file behind.
    pub fn save(&self) -> anyhow::Result<()> {
        let bytes = serde_json::to_vec_pretty(self)?;
        let path = settings_path();
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// The worker string to send to the pool: the pool user with the miner's
    /// name appended as a worker suffix.
    ///
    /// An unnamed miner sends the user verbatim. This is what makes naming a
    /// miner visible on the pool side rather than only in the dashboard.
    pub fn worker_username(&self) -> Option<String> {
        let user = &self.pool.as_ref()?.user;
        Some(match self.name.as_deref().map(str::trim) {
            Some(name) if !name.is_empty() => format!("{user}.{name}"),
            _ => user.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(user: &str) -> Option<PoolSettings> {
        Some(PoolSettings {
            url: "stratum+tcp://pool.example.com:3333".into(),
            user: user.into(),
            password: "x".into(),
        })
    }

    #[test]
    fn unnamed_miner_sends_the_pool_user_verbatim() {
        let settings = MinerSettings {
            name: None,
            pool: pool("bc1qexample"),
        };
        assert_eq!(settings.worker_username().unwrap(), "bc1qexample");
    }

    #[test]
    fn name_is_appended_as_the_worker_suffix() {
        let settings = MinerSettings {
            name: Some("garage-rig".into()),
            pool: pool("bc1qexample"),
        };
        assert_eq!(
            settings.worker_username().unwrap(),
            "bc1qexample.garage-rig"
        );
    }

    #[test]
    fn a_blank_name_is_not_a_worker_suffix() {
        // The dashboard sends "" for a cleared field; that must behave as
        // unnamed, not append a trailing dot the pool would read as a worker
        // called "".
        let settings = MinerSettings {
            name: Some("   ".into()),
            pool: pool("bc1qexample"),
        };
        assert_eq!(settings.worker_username().unwrap(), "bc1qexample");
    }

    #[test]
    fn accepts_pool_urls_the_daemon_can_dial() {
        assert!(validate_pool_url("stratum+tcp://pool.example.com:3333").is_ok());
        assert!(validate_pool_url("tcp://pool.example.com:3333").is_ok());
        // Scheme is optional, matching Connection::connect.
        assert!(validate_pool_url("pool.example.com:3333").is_ok());
        // An IPv6 literal is all colons; only the last one is the port.
        assert!(validate_pool_url("stratum+tcp://[::1]:3333").is_ok());
    }

    #[test]
    fn rejects_pool_urls_that_would_fail_at_startup() {
        assert!(validate_pool_url("stratum+tcp://pool.example.com").is_err());
        assert!(validate_pool_url("pool.example.com:notaport").is_err());
        assert!(validate_pool_url("stratum+tcp://:3333").is_err());
        assert!(validate_pool_url("").is_err());
        // Above u16.
        assert!(validate_pool_url("pool.example.com:99999").is_err());
    }

    #[test]
    fn no_pool_means_no_worker_string() {
        let settings = MinerSettings {
            name: Some("garage-rig".into()),
            pool: None,
        };
        assert_eq!(settings.worker_username(), None);
    }
}
