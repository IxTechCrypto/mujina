//! In-process pool supervisor.
//!
//! Adding, removing, or reconfiguring a pool happens here, inside the
//! running miner. Restarting the process leaves the Core 1 IPC handle
//! stale, so this task never asks for a restart. A pool whose url, user
//! and password are unchanged keeps its stratum session; only its weight
//! is updated. Anything else is stopped and started.

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::api::commands::SchedulerCommand;
use crate::job_source::{
    SourceCommand, SourceEvent,
    dummy::DummySource,
    forced_rate::{ForcedRateConfig, ForcedRateSource},
    stratum_v1::StratumV1Source,
};
use crate::pool_config::PoolEntry;
use crate::scheduler::SourceRegistration;
use crate::stratum_v1::{PoolConfig as StratumPoolConfig, TcpConnector};
use crate::tracing::prelude::*;

/// Identity of a running stratum session. Weight is not part of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolKey {
    pub url: String,
    pub user: String,
    pub pass: String,
}

impl PoolKey {
    fn from_entry(entry: &PoolEntry) -> Self {
        Self {
            url: entry.url.clone(),
            user: entry.user.clone(),
            pass: entry.pass.clone(),
        }
    }
}

struct LivePool {
    key: PoolKey,
    name: String,
    cancel: CancellationToken,
}

/// What one reload does to the sessions that are already running.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// Same url, user and password. Keep the session and set this weight.
    Keep { name: String, weight: u16 },
    /// This session is no longer in the config.
    Stop { name: String },
    /// A pool that was not already running.
    Start { index: usize },
}

/// Diff `live` against `next`. Matching is by url+user+password, one to
/// one, so two identical entries still pair with two sessions.
pub fn plan(live: &[(String, PoolKey)], next: &[PoolEntry]) -> Vec<Step> {
    let mut used = vec![false; next.len()];
    let mut steps = Vec::new();
    for (name, key) in live {
        if let Some(i) = next
            .iter()
            .enumerate()
            .position(|(i, entry)| !used[i] && PoolKey::from_entry(entry) == *key)
        {
            used[i] = true;
            steps.push(Step::Keep {
                name: name.clone(),
                weight: next[i].weight,
            });
        } else {
            steps.push(Step::Stop { name: name.clone() });
        }
    }
    for (index, taken) in used.into_iter().enumerate() {
        if !taken {
            steps.push(Step::Start { index });
        }
    }
    steps
}

/// Runs until `shutdown`. `initial` is applied immediately; later vectors
/// arrive on `reload_rx` (from `PUT /api/v0/pools` and `PATCH /api/v0/pool`).
pub async fn run(
    shutdown: CancellationToken,
    mut reload_rx: mpsc::Receiver<Vec<PoolEntry>>,
    source_reg_tx: mpsc::Sender<SourceRegistration>,
    scheduler_cmd_tx: mpsc::Sender<SchedulerCommand>,
    initial: Vec<PoolEntry>,
) {
    let mut live: Vec<LivePool> = Vec::new();
    let mut dummy: Option<CancellationToken> = None;

    apply(
        &shutdown,
        &source_reg_tx,
        &scheduler_cmd_tx,
        &mut live,
        &mut dummy,
        &initial,
    )
    .await;

    loop {
        tokio::select! {
            Some(next) = reload_rx.recv() => {
                info!(pools = next.len(), "Applying pool configuration");
                apply(
                    &shutdown,
                    &source_reg_tx,
                    &scheduler_cmd_tx,
                    &mut live,
                    &mut dummy,
                    &next,
                )
                .await;
            }
            _ = shutdown.cancelled() => {
                for pool in live.drain(..) {
                    pool.cancel.cancel();
                }
                if let Some(token) = dummy.take() {
                    token.cancel();
                }
                break;
            }
        }
    }
}

async fn apply(
    shutdown: &CancellationToken,
    source_reg_tx: &mpsc::Sender<SourceRegistration>,
    scheduler_cmd_tx: &mpsc::Sender<SchedulerCommand>,
    live: &mut Vec<LivePool>,
    dummy: &mut Option<CancellationToken>,
    next: &[PoolEntry],
) {
    if next.is_empty() {
        for pool in live.drain(..) {
            info!(pool = %pool.name, "Stopping pool");
            pool.cancel.cancel();
        }
        if dummy.is_none() {
            *dummy = Some(spawn_dummy(shutdown, source_reg_tx).await);
        }
        return;
    }
    if let Some(token) = dummy.take() {
        token.cancel();
    }

    let snapshot: Vec<(String, PoolKey)> = live
        .iter()
        .map(|p| (p.name.clone(), p.key.clone()))
        .collect();
    let steps = plan(&snapshot, next);
    for step in steps {
        match step {
            Step::Keep { name, weight } => {
                let _ = scheduler_cmd_tx
                    .send(SchedulerCommand::SetWeight { name, weight })
                    .await;
            }
            Step::Stop { name } => {
                if let Some(i) = live.iter().position(|p| p.name == name) {
                    info!(pool = %name, "Stopping pool");
                    live.swap_remove(i).cancel.cancel();
                }
            }
            Step::Start { index } => {
                let entry = &next[index];
                let name = format!("pool-{index}");
                info!(pool = %name, url = %entry.url, weight = entry.weight, "Starting pool");
                let cancel = spawn_stratum(shutdown, source_reg_tx, entry, &name, index).await;
                live.push(LivePool {
                    key: PoolKey::from_entry(entry),
                    name,
                    cancel,
                });
            }
        }
    }
}

async fn spawn_stratum(
    shutdown: &CancellationToken,
    source_reg_tx: &mpsc::Sender<SourceRegistration>,
    entry: &PoolEntry,
    name: &str,
    index: usize,
) -> CancellationToken {
    let cancel = shutdown.child_token();
    let (event_tx, event_rx) = mpsc::channel::<SourceEvent>(100);
    let (cmd_tx, cmd_rx) = mpsc::channel::<SourceCommand>(10);
    let pool_url = entry.url.clone();
    let stratum_config = StratumPoolConfig {
        url: pool_url.clone(),
        username: entry.user.clone(),
        password: entry.pass.clone(),
        user_agent: "mujina-miner/0.1.0-alpha".to_string(),
    };

    // The forced-rate wrapper is a single-pool test hook. It stays on
    // pool 0 only, so a multi-pool config is not silently rewritten.
    if index == 0
        && let Some(forced) = ForcedRateConfig::from_env()
    {
        let (inner_event_tx, inner_event_rx) = mpsc::channel::<SourceEvent>(100);
        let (inner_cmd_tx, inner_cmd_rx) = mpsc::channel::<SourceCommand>(10);
        let stratum = StratumV1Source::new(
            stratum_config,
            inner_cmd_rx,
            inner_event_tx,
            cancel.clone(),
            Box::new(TcpConnector::new(pool_url.clone())),
        );
        let cancel_stratum = cancel.clone();
        tokio::spawn(async move {
            let _ = cancel_stratum;
            if let Err(e) = stratum.run().await {
                error!(error = %e, "Stratum v1 source error");
            }
        });
        let wrapper = ForcedRateSource::new(
            forced,
            inner_event_rx,
            event_tx,
            inner_cmd_tx,
            cmd_rx,
            cancel.clone(),
        );
        let cancel_wrapper = cancel.clone();
        tokio::spawn(async move {
            let _ = cancel_wrapper;
            if let Err(e) = wrapper.run().await {
                error!(error = %e, "Forced rate wrapper error");
            }
        });
    } else {
        let stratum = StratumV1Source::new(
            stratum_config,
            cmd_rx,
            event_tx,
            cancel.clone(),
            Box::new(TcpConnector::new(pool_url.clone())),
        );
        let cancel_stratum = cancel.clone();
        tokio::spawn(async move {
            let _ = cancel_stratum;
            if let Err(e) = stratum.run().await {
                error!(error = %e, "Stratum v1 source error");
            }
        });
    }

    if source_reg_tx
        .send(SourceRegistration {
            name: name.to_string(),
            url: Some(pool_url),
            event_rx,
            command_tx: cmd_tx,
            weight: entry.weight,
        })
        .await
        .is_err()
    {
        warn!(pool = %name, "Scheduler gone; pool session will exit");
        cancel.cancel();
    }
    cancel
}

async fn spawn_dummy(
    shutdown: &CancellationToken,
    source_reg_tx: &mpsc::Sender<SourceRegistration>,
) -> CancellationToken {
    info!("No pool configured; using the dummy job source");
    let cancel = shutdown.child_token();
    let (event_tx, event_rx) = mpsc::channel::<SourceEvent>(100);
    let (cmd_tx, cmd_rx) = mpsc::channel::<SourceCommand>(10);
    match DummySource::new(
        cmd_rx,
        event_tx,
        cancel.clone(),
        tokio::time::Duration::from_secs(30),
    ) {
        Ok(dummy) => {
            tokio::spawn(async move {
                if let Err(e) = dummy.run().await {
                    error!(error = %e, "DummySource error");
                }
            });
            let _ = source_reg_tx
                .send(SourceRegistration {
                    name: "dummy".into(),
                    url: None,
                    event_rx,
                    command_tx: cmd_tx,
                    weight: 100,
                })
                .await;
        }
        Err(e) => {
            error!(error = %e, "Failed to create dummy source");
            cancel.cancel();
        }
    }
    cancel
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(url: &str, weight: u16) -> PoolEntry {
        PoolEntry {
            url: url.into(),
            user: "worker".into(),
            pass: "x".into(),
            weight,
        }
    }

    #[test]
    fn plan_keeps_a_pool_whose_only_change_is_weight() {
        let live = vec![(
            "pool-0".into(),
            PoolKey {
                url: "stratum+tcp://a.example:3333".into(),
                user: "worker".into(),
                pass: "x".into(),
            },
        )];
        let next = vec![
            entry("stratum+tcp://a.example:3333", 70),
            entry("stratum+tcp://b.example:3333", 30),
        ];
        assert_eq!(
            plan(&live, &next),
            vec![
                Step::Keep {
                    name: "pool-0".into(),
                    weight: 70
                },
                Step::Start { index: 1 },
            ]
        );
    }

    #[test]
    fn plan_stops_a_removed_pool_and_starts_its_replacement() {
        let live = vec![(
            "pool-0".into(),
            PoolKey {
                url: "stratum+tcp://a.example:3333".into(),
                user: "worker".into(),
                pass: "x".into(),
            },
        )];
        let next = vec![entry("stratum+tcp://b.example:3333", 100)];
        assert_eq!(
            plan(&live, &next),
            vec![
                Step::Stop {
                    name: "pool-0".into()
                },
                Step::Start { index: 0 }
            ]
        );
    }

    #[tokio::test]
    async fn add_and_remove_a_pool_without_leaving_the_director_task() {
        let shutdown = CancellationToken::new();
        let (reload_tx, reload_rx) = mpsc::channel(4);
        let (reg_tx, mut reg_rx) = mpsc::channel(8);
        let (cmd_tx, mut cmd_rx) = mpsc::channel(8);
        let initial = vec![entry("stratum+tcp://a.example:3333", 100)];

        let director = shutdown.clone();
        let task = tokio::spawn(async move {
            run(director, reload_rx, reg_tx, cmd_tx, initial).await;
        });

        let mut first = reg_rx.recv().await.expect("first pool registered");
        assert_eq!(first.name, "pool-0");
        assert_eq!(first.weight, 100);
        // The session is the same task: a weight change does not register again.
        reload_tx
            .send(vec![
                entry("stratum+tcp://a.example:3333", 60),
                entry("stratum+tcp://b.example:3333", 40),
            ])
            .await
            .unwrap();
        let mut second = reg_rx.recv().await.expect("added pool registered");
        assert_eq!(second.name, "pool-1");
        assert_eq!(second.weight, 40);
        let reweight = cmd_rx.recv().await.expect("weight update");
        match reweight {
            SchedulerCommand::SetWeight { name, weight } => {
                assert_eq!(name, "pool-0");
                assert_eq!(weight, 60);
            }
            other => panic!("expected SetWeight, got {other:?}"),
        }

        reload_tx
            .send(vec![entry("stratum+tcp://a.example:3333", 100)])
            .await
            .unwrap();
        // pool-1's event channel closes because its token was cancelled.
        // No new registration is produced for pool-0.
        let closed = second.event_rx.recv().await;
        assert!(closed.is_none(), "removed pool must drop its event channel");

        shutdown.cancel();
        task.await.unwrap();
        // The director task itself ended because of the token, not because
        // the process restarted. pool-0 was still the original session.
        assert!(first.event_rx.recv().await.is_none());
    }
}
