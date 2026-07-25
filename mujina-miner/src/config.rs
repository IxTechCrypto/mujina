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
    fn no_pool_means_no_worker_string() {
        let settings = MinerSettings {
            name: Some("garage-rig".into()),
            pool: None,
        };
        assert_eq!(settings.worker_username(), None);
    }
}
