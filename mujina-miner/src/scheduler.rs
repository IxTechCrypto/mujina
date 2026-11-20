//! The scheduler module manages the distribution of mining jobs to hash boards
//! and ASIC chips.
//!
//! # Share Filtering (Three-Layer Architecture)
//!
//! Share filtering happens at three independent levels:
//!
//! **Layer 1 - Chip TicketMask (hardware pre-filter):**
//! - Configured by thread during initialization
//! - Chip only reports nonces meeting this threshold
//!
//! **Layer 2 - HashTask.share_target (scheduler target, per-thread):**
//! - Computed per thread from that thread's hashrate
//! - Clamps source difficulty between a measurement floor (1
//!   share/sec) and a flood ceiling (10 shares/sec)
//! - Feeds per-thread hashrate estimators with frequent samples
//!
//! **Layer 3 - JobTemplate.share_target (scheduler-to-source filter):**
//! - Set by pool via Stratum mining.set_difficulty
//! - Scheduler validates before forwarding to source
//! - Only pool-worthy shares submitted
//!
//! The scheduler receives shares meeting HashTask.share_target, uses them for
//! statistics and monitoring, then filters again before pool submission.

use slotmap::SlotMap;
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};

use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::{StreamExt, StreamMap};
use tokio_util::sync::CancellationToken;

use crate::api::commands::SchedulerCommand;
use crate::api_client::types::{MinerTelemetry, SourceTelemetry};
use crate::asic::hash_thread::{HashTask, HashThread, HashThreadEvent, Share};
use crate::job_source::{
    JobTemplate, MerkleRootKind, Share as SourceShare, SourceCommand, SourceEvent,
};
use crate::tracing::prelude::*;
use crate::types::{
    AlarmStatus, DebouncedAlarm, Difficulty, HashRate, HashrateEstimator, ShareRate, Target,
    expected_time_to_share_from_target,
};

/// A share is submitted only when its difficulty is at least the pool's.
///
/// `mining.set_difficulty 94` plus a share of difficulty 10 does not submit.
pub(crate) fn share_meets_pool_difficulty(pool: f64, share: f64) -> bool {
    pool.is_finite() && share.is_finite() && pool > 0.0 && share + 1e-9 >= pool
}

/// Unique identifier for a job source, assigned by the scheduler.
type SourceId = slotmap::DefaultKey;

/// Unique identifier for a hash thread, assigned by the scheduler.
type ThreadId = slotmap::DefaultKey;

/// Unique identifier for a task, assigned by the scheduler.
type TaskId = slotmap::DefaultKey;

// StreamMap type aliases used as locals in run() rather than struct fields.
type SourceEventStream = StreamMap<SourceId, ReceiverStream<SourceEvent>>;
type ThreadEventStream = StreamMap<ThreadId, ReceiverStream<HashThreadEvent>>;
type ShareStream = StreamMap<TaskId, ReceiverStream<Share>>;

/// Window duration for per-thread hashrate estimation.
const HASHRATE_WINDOW: Duration = Duration::from_secs(5 * 60);

/// Timeout for the startup gate, armed when the enumeration-complete signal
/// arrives. Forces the gate open if a thread never reports its hashrate.
const STARTUP_GATE_TIMEOUT: Duration = Duration::from_secs(10);

/// Per-thread measurement floor: minimum share rate for hashrate
/// estimation (1 share/sec). Overrides the source target when it would
/// produce a lower rate than this.
const MEASUREMENT_SHARE_RATE: ShareRate = ShareRate::from_interval(Duration::from_secs(1));

/// Per-thread flood ceiling: maximum share rate for hashrate estimation
/// (10 shares/sec). Overrides the source target when it would produce a
/// higher rate than this.
const FLOOD_CAP_RATE: ShareRate = ShareRate::from_interval(Duration::from_millis(100));

/// Scheduler-side bookkeeping for an active task.
///
/// Each HashTask sent to a thread has a corresponding TaskEntry in the
/// scheduler. When a share arrives on the task's channel, this provides
/// routing: which source to submit to and the job template for validation.
#[derive(Debug)]
struct TaskEntry {
    /// Source that provided this job
    source_id: SourceId,

    /// Job template (shared with the HashTask sent to thread)
    template: Arc<JobTemplate>,

    /// Thread this task was assigned to
    thread_id: ThreadId,
}

/// Registration message for adding a job source to the scheduler.
///
/// The daemon creates sources and sends this message to register them.
/// The scheduler inserts the source into its SlotMap and begins listening
/// for events.
pub struct SourceRegistration {
    /// Source name for logging
    pub name: String,

    /// Connection URL for this source (e.g. "stratum+tcp://pool:3333").
    pub url: Option<String>,

    /// Event receiver for this source (UpdateJob, ReplaceJob, ClearJobs)
    pub event_rx: mpsc::Receiver<SourceEvent>,

    /// Command sender for this source (SubmitShare, etc.)
    pub command_tx: mpsc::Sender<SourceCommand>,

    /// Percent of hash time. 0 is a failover-only backup.
    pub weight: u16,
}

/// Item the backplane sends to the scheduler on the thread-registration channel.
pub enum ThreadRegistration {
    /// A new hash thread to schedule.
    Thread(Box<dyn HashThread>),

    /// Initial enumeration across all transports is complete.
    ///
    /// Sent once, after every starting thread, so the scheduler knows its
    /// initial board set is fully registered.
    InitialEnumerationComplete,
}

/// Internal scheduler tracking for a registered source.
#[derive(Debug)]
struct SourceEntry {
    /// Source name for logging
    name: String,

    /// Connection URL for this source.
    url: Option<String>,

    /// Command channel for sending to this source
    command_tx: mpsc::Sender<SourceCommand>,

    /// Last job received from this source (for assigning to newly-arriving threads)
    last_job: Option<Arc<JobTemplate>>,

    /// Debounced alarm for high-difficulty warnings.
    difficulty_alarm: DebouncedAlarm,

    /// Percent of hash time. 0 is a failover-only backup.
    weight: u16,

    /// The stratum session is up. A pool starts up so it can be told a
    /// hashrate and connect. `ClearJobs` (disconnect) marks it down.
    up: bool,

    /// Weighted-rotation credit. See `pool_alloc`.
    credit: i64,

    accepted: u64,
    rejected: u64,
    /// Pool reject text and how many times it was returned. Capped.
    reject_reasons: Vec<(String, u64)>,

    /// Measured chain hashrate while this pool held the chain, else 0.
    /// One sample per 10 s telemetry tick, oldest first. 360 samples is 1 h.
    samples: VecDeque<u64>,

    /// Latest `mining.set_difficulty` for this connection. `None` until the
    /// pool sends one. Shares below this are counted and not submitted.
    pool_difficulty: Option<f64>,

    /// Shares the chip found that were under `pool_difficulty`.
    low_diff: u64,
}

/// Whether to update alongside existing work or replace it.
#[derive(Debug)]
enum AssignMode {
    /// Add new task alongside existing (UpdateJob behavior)
    Update,
    /// Invalidate old tasks, replace current work (ReplaceJob behavior)
    Replace,
}

/// Scheduler-side bookkeeping for a hash thread.
struct ThreadEntry {
    thread: Box<dyn HashThread>,
    hashrate: HashrateEstimator,

    /// Hashrate the thread declared via `ExpectedHashRate`, `None` until its
    /// first report.
    expected: Option<HashRate>,

    /// Most recent directly-measured hardware hashrate, from
    /// `HashThreadEvent::StatusUpdate`. `None` if this thread has never sent
    /// one; boards without direct telemetry fall back to `hashrate`, the
    /// shares-based estimator.
    hardware_hashrate: Option<HashRate>,
}

/// Core scheduler state. StreamMaps are kept as locals in `run()` rather
/// than fields here.
struct Scheduler {
    /// Source storage and command channels
    sources: SlotMap<SourceId, SourceEntry>,

    /// Thread storage
    threads: SlotMap<ThreadId, ThreadEntry>,

    /// Task bookkeeping (maps tasks to sources/threads)
    tasks: SlotMap<TaskId, TaskEntry>,

    /// Mining statistics
    stats: MiningStats,

    /// Track thread count for disconnect detection
    last_thread_count: usize,

    /// Holds the first source broadcast until startup enumeration completes
    startup_gate: StartupGate,

    /// Mining paused
    paused: bool,

    /// Source that currently owns the chain. `None` until the first job.
    mining: Option<SourceId>,
}

impl Scheduler {
    fn new() -> Self {
        Self {
            sources: SlotMap::new(),
            threads: SlotMap::new(),
            tasks: SlotMap::new(),
            stats: MiningStats::default(),
            last_thread_count: 0,
            startup_gate: StartupGate::new(),
            paused: false,
            mining: None,
        }
    }

    /// Aggregate measured hashrate from per-thread estimators.
    ///
    /// For each thread, uses the directly-measured `hardware_hashrate` if
    /// present, otherwise falls back to the shares-based
    /// `HashrateEstimator`, and sums across threads.
    fn measured_hashrate(&mut self) -> HashRate {
        self.threads
            .values_mut()
            .map(|entry| {
                entry
                    .hardware_hashrate
                    .unwrap_or_else(|| entry.hashrate.hashrate())
            })
            .sum()
    }

    /// Aggregate of the hashrates threads declared they expect to deliver.
    ///
    /// Feed-forward, summed across threads that have reported; a thread that
    /// has not yet reported contributes nothing. Drives source difficulty and
    /// the difficulty-too-high warning, where a zero at startup is unhelpful.
    fn expected_hashrate(&self) -> HashRate {
        self.threads
            .values()
            .filter_map(|entry| entry.expected)
            .sum()
    }

    /// Expected hashrate this source should report to its pool.
    ///
    /// The chain is one piece of hardware, so a source's share is its
    /// share of hash *time* after failover (`pool_alloc::time_shares`).
    /// A source whose share is currently zero (a weight-0 backup, or a
    /// pool that is down) is still told 1 H/s when the chain has a
    /// hashrate. That is below any real difficulty and only exists so the
    /// stratum client will connect and stay connected; it does not claim
    /// the chain.
    fn allocated_hashrate(&self, source_id: SourceId) -> HashRate {
        let total = self.expected_hashrate();
        let share = self.time_share(source_id);
        if share == 0 {
            if self.sources.contains_key(source_id) && !total.is_zero() {
                return HashRate(1);
            }
            return HashRate(0);
        }
        HashRate(total.0.saturating_mul(u64::from(share)) / 100)
    }

    fn time_share(&self, source_id: SourceId) -> u16 {
        let ids: Vec<SourceId> = self.sources.keys().collect();
        let views: Vec<crate::pool_alloc::PoolView> = ids
            .iter()
            .map(|id| {
                let source = &self.sources[*id];
                crate::pool_alloc::PoolView {
                    weight: source.weight,
                    up: source.up,
                }
            })
            .collect();
        let shares = crate::pool_alloc::time_shares(&views);
        ids.iter()
            .position(|id| *id == source_id)
            .and_then(|i| shares.get(i).copied())
            .unwrap_or(0)
    }

    /// One rotation step. Returns the source that owns the next slot.
    fn pick_slot(&mut self) -> Option<SourceId> {
        let ids: Vec<SourceId> = self.sources.keys().collect();
        let views: Vec<crate::pool_alloc::PoolView> = ids
            .iter()
            .map(|id| {
                let source = &self.sources[*id];
                crate::pool_alloc::PoolView {
                    weight: source.weight,
                    up: source.up,
                }
            })
            .collect();
        let shares = crate::pool_alloc::time_shares(&views);
        let mut credit: Vec<i64> = ids.iter().map(|id| self.sources[*id].credit).collect();
        let picked = crate::pool_alloc::pick_slot(&mut credit, &shares);
        for (id, c) in ids.iter().zip(credit) {
            if let Some(source) = self.sources.get_mut(*id) {
                source.credit = c;
            }
        }
        picked.map(|i| ids[i])
    }

    fn set_up(&mut self, source_id: SourceId, up: bool) -> bool {
        let Some(source) = self.sources.get_mut(source_id) else {
            return false;
        };
        if source.up == up {
            return false;
        }
        source.up = up;
        true
    }

    /// Threads eligible for work: those that have reported an expected hashrate.
    fn eligible_thread_ids(&self) -> impl Iterator<Item = ThreadId> + '_ {
        self.threads
            .iter()
            .filter(|(_, entry)| entry.expected.is_some())
            .map(|(id, _)| id)
    }

    /// Build a [`MinerTelemetry`] snapshot from current scheduler state.
    ///
    /// The scheduler contributes aggregate stats and source info. Board
    /// and thread details come from the backplane, not the scheduler, so
    /// `boards` is left empty here.
    fn compute_miner_telemetry(&mut self) -> MinerTelemetry {
        MinerTelemetry {
            uptime_secs: self.stats.start_time.elapsed().as_secs(),
            hashrate: u64::from(self.measured_hashrate()),
            shares_submitted: self.stats.shares_submitted,
            paused: self.paused,
            boards: vec![],
            sources: self
                .sources
                .values()
                .map(|s| SourceTelemetry {
                    name: s.name.clone(),
                    url: s.url.clone(),
                    difficulty: s.pool_difficulty.or_else(|| {
                        s.last_job.as_ref().map(|j| {
                            let d = Difficulty::from_target(j.share_target).as_f64();
                            if d >= 10.0 { d.round() } else { d }
                        })
                    }),
                    shares_low_diff: s.low_diff,
                    connected: s.up,
                    weight: s.weight,
                    accepted: s.accepted,
                    rejected: s.rejected,
                    reject_reasons: s
                        .reject_reasons
                        .iter()
                        .map(|(reason, count)| crate::api_client::types::RejectReason {
                            reason: reason.clone(),
                            count: *count,
                        })
                        .collect(),
                    hashrate_10m: average_recent(&s.samples, 60),
                    hashrate_1h: average_recent(&s.samples, 360),
                })
                .collect(),
        }
    }

    /// Compute the per-thread scheduler target for HashTask.
    ///
    /// Clamps the source's pool difficulty between a measurement floor
    /// (1 share/sec) and a flood ceiling (10 shares/sec). When pool
    /// difficulty falls outside this range, the scheduler target
    /// overrides it; when inside, the source target passes through.
    fn compute_scheduler_target(hashrate: HashRate, source_target: Target) -> Target {
        if hashrate.is_zero() {
            return source_target;
        }

        let measurement_target = MEASUREMENT_SHARE_RATE.to_target(hashrate);
        let flood_cap_target = FLOOD_CAP_RATE.to_target(hashrate);

        source_target.clamp(measurement_target, flood_cap_target)
    }

    /// Pairs every source's command sender with its current hashrate allocation.
    fn collect_hashrate_updates(&self) -> Vec<(mpsc::Sender<SourceCommand>, HashRate)> {
        self.sources
            .iter()
            .map(|(id, s)| (s.command_tx.clone(), self.allocated_hashrate(id)))
            .collect()
    }

    /// React to a change in aggregate hashrate: reset the high-difficulty
    /// warning debounce and resend every source its current allocation.
    async fn broadcast_hashrate_change(&mut self) {
        for source in self.sources.values_mut() {
            source.difficulty_alarm.reset();
        }
        let updates = self.collect_hashrate_updates();
        send_hashrate_updates(updates).await;
    }

    /// Remove tasks matching a predicate, closing their share channels.
    fn remove_tasks_where(
        &mut self,
        share_channels: &mut ShareStream,
        predicate: impl Fn(&TaskEntry) -> bool,
    ) {
        let task_ids: Vec<TaskId> = self
            .tasks
            .iter()
            .filter(|(_, entry)| predicate(entry))
            .map(|(id, _)| id)
            .collect();

        for task_id in task_ids {
            self.tasks.remove(task_id);
            share_channels.remove(&task_id);
        }
    }

    /// Handle registration of a new job source.
    async fn handle_source_registration(
        &mut self,
        registration: SourceRegistration,
        source_events: &mut SourceEventStream,
    ) {
        let source_id = self.sources.insert(SourceEntry {
            name: registration.name.clone(),
            url: registration.url,
            command_tx: registration.command_tx,
            last_job: None,
            difficulty_alarm: DebouncedAlarm::new(HIGH_DIFFICULTY_DEBOUNCE),
            weight: registration.weight,
            up: true,
            credit: 0,
            accepted: 0,
            rejected: 0,
            reject_reasons: Vec::new(),
            samples: VecDeque::new(),
            pool_difficulty: None,
            low_diff: 0,
        });
        source_events.insert(source_id, ReceiverStream::new(registration.event_rx));
        debug!(
            source_id = ?source_id,
            name = %registration.name,
            weight = registration.weight,
            "Source registered"
        );

        // A new source changes how the total is divided, so re-send every
        // source its allocation. While the startup gate holds, skip it; the
        // broadcast when the gate opens covers every source.
        if !self.startup_gate.is_holding() {
            self.broadcast_hashrate_change().await;
        }
    }

    /// Assign or replace work on all threads from a job template.
    async fn assign_job_to_threads(
        &mut self,
        mode: AssignMode,
        source_id: SourceId,
        job_template: JobTemplate,
        share_channels: &mut ShareStream,
        push: bool,
    ) {
        let source_name = self
            .sources
            .get(source_id)
            .map(|s| s.name.clone())
            .unwrap_or_else(|| "unknown".to_string());

        // Extract EN2 range (only supported for computed merkle roots)
        let full_en2_range = match &job_template.merkle_root {
            MerkleRootKind::Computed(template) => template.extranonce2_range.clone(),
            MerkleRootKind::Fixed(_) => {
                error!(job_id = %job_template.id, "Header-only jobs not supported");
                return;
            }
        };

        let template = Arc::new(job_template);

        // Reset debounce when difficulty changes so the alarm doesn't
        // fire during the transient after a pool adjustment.
        if let Some(source) = self.sources.get_mut(source_id) {
            let prev_target = source.last_job.as_ref().map(|j| j.share_target);
            if prev_target != Some(template.share_target) {
                source.difficulty_alarm.reset();
            }
            source.last_job = Some(template.clone());
        }

        // A pool that does not own this slot keeps the job cached. A hard
        // replace still drops that pool's tasks (the pool said the old
        // work is invalid) without touching the chain or the other pool's
        // in-flight shares.
        if !push {
            if matches!(mode, AssignMode::Replace) {
                self.remove_tasks_where(share_channels, |e| e.source_id == source_id);
            }
            debug!(source = %source_name, "Job cached; this pool does not own the current slot");
            return;
        }

        // Skip assignment if no threads registered yet
        if self.threads.is_empty() {
            debug!(source = %source_name, "No threads yet, job cached for later");
            return;
        }

        // Debounced difficulty warning, against this pool's own share.
        let hashrate = self.allocated_hashrate(source_id);
        if let Some(source) = self.sources.get_mut(source_id) {
            let too_high = is_difficulty_too_high(&template, hashrate);
            match source.difficulty_alarm.check(too_high) {
                AlarmStatus::Triggered => {
                    let difficulty = Difficulty::from_target(template.share_target);
                    warn!(
                        source = %source_name,
                        job_id = %template.id,
                        difficulty = %difficulty,
                        hashrate = %hashrate.to_human_readable(),
                        expected_share_interval =
                            %format_duration(expected_time_to_share_from_target(
                                template.share_target, hashrate).as_secs()),
                        "Share difficulty too high for hashrate \
                         (expected > 5 min between shares)"
                    );
                }
                AlarmStatus::Resolved => {
                    info!(
                        source = %source_name,
                        "Share difficulty now acceptable for hashrate"
                    );
                }
                _ => {}
            }
        }

        // If replacing, invalidate old tasks for this source first
        if matches!(mode, AssignMode::Replace) {
            self.remove_tasks_where(share_channels, |e| e.source_id == source_id);
        }

        // Split the EN2 range evenly across the currently-eligible threads.
        //
        // TODO: A thread that becomes eligible later is handed the full EN2
        // range on its first report, overlapping these slices until the next
        // job re-splits.
        let eligible: Vec<ThreadId> = self.eligible_thread_ids().collect();
        if eligible.is_empty() {
            debug!(source = %source_name, "No eligible threads yet, job cached for later");
            return;
        }
        let en2_slices = full_en2_range
            .split(eligible.len())
            .expect("Failed to split EN2 range among threads");

        for (thread_id, en2_range) in eligible.into_iter().zip(en2_slices) {
            let starting_en2 = en2_range.iter().next();
            let entry = self
                .threads
                .get_mut(thread_id)
                .expect("eligible thread present");

            let hashrate = entry
                .hashrate
                .settled_hashrate()
                .or(entry.expected)
                .unwrap_or_default();
            let share_target = Self::compute_scheduler_target(hashrate, template.share_target);

            // Create share channel for this task
            let (share_tx, share_rx) = mpsc::channel(32);

            let hash_task = HashTask {
                template: template.clone(),
                en2_range: Some(en2_range),
                en2: starting_en2,
                share_target,
                ntime: template.time,
                share_tx,
            };

            let result = match mode {
                AssignMode::Update => entry.thread.update_task(hash_task).await,
                AssignMode::Replace => entry.thread.replace_task(hash_task).await,
            };

            if let Err(e) = result {
                error!(thread = %entry.thread.name(), error = %e, "Failed to assign task");
            } else {
                let task_id = self.tasks.insert(TaskEntry {
                    source_id,
                    template: template.clone(),
                    thread_id,
                });
                share_channels.insert(task_id, ReceiverStream::new(share_rx));
            }
        }
    }

    /// Handle ClearJobs event from a source.
    fn handle_clear_jobs(&mut self, source_id: SourceId, share_channels: &mut ShareStream) {
        let source_name = self
            .sources
            .get(source_id)
            .map(|s| s.name.as_str())
            .unwrap_or("unknown");
        debug!(source = %source_name, "ClearJobs received");

        // Clear cached job so newly-arriving threads don't get stale work
        if let Some(source) = self.sources.get_mut(source_id) {
            source.last_job = None;
        }

        // Remove tasks for this source (channels close, stale shares fail)
        self.remove_tasks_where(share_channels, |e| e.source_id == source_id);
    }

    /// Handle a share arriving from a task's channel.
    async fn handle_share(&mut self, task_id: TaskId, share: Share) {
        // Look up task context for routing
        let Some(task_entry) = self.tasks.get(task_id) else {
            // Task was removed (ReplaceJob/ClearJobs) but share arrived
            // before channel closed. This is normal; just drop the share.
            trace!(task_id = ?task_id, "Share for removed task (dropped)");
            return;
        };

        // Extract fields for logging (share may be consumed on submission)
        let nonce = share.nonce;
        let hash = share.hash;
        let share_difficulty = Difficulty::from_hash(&hash);
        let threshold = Difficulty::from_target(task_entry.template.share_target);

        debug!(
            source = %self.sources.get(task_entry.source_id).map(|s| s.name.as_str()).unwrap_or("unknown"),
            job_id = %task_entry.template.id,
            nonce = format!("{:#x}", nonce),
            hash = %hash,
            share_difficulty = %share_difficulty,
            threshold = %threshold,
            "Share found"
        );

        // Feed share work to per-thread hashrate estimator
        if let Some(entry) = self.threads.get_mut(task_entry.thread_id) {
            entry.hashrate.record(share.expected_work);
        }

        // The job target can still be the old, easier one. The pool's latest
        // mining.set_difficulty wins, per connection.
        let need = self
            .sources
            .get(task_entry.source_id)
            .and_then(|s| s.pool_difficulty);
        let share_d = share_difficulty.as_f64();
        if let Some(need) = need
            && !share_meets_pool_difficulty(need, share_d)
        {
            if let Some(source) = self.sources.get_mut(task_entry.source_id) {
                source.low_diff = source.low_diff.saturating_add(1);
            }
            trace!(
                share_difficulty = share_d,
                pool_difficulty = need,
                "Share below pool difficulty (not submitted)"
            );
            return;
        }

        // Check if share meets source threshold
        if task_entry.template.share_target.is_met_by(hash) {
            self.stats.shares_submitted += 1;

            // Submit share to originating source
            if let Some(source) = self.sources.get(task_entry.source_id) {
                let source_share = SourceShare::from((share, task_entry.template.id.clone()));

                if let Err(e) = source
                    .command_tx
                    .send(SourceCommand::SubmitShare(source_share))
                    .await
                {
                    error!(
                        source_id = ?task_entry.source_id,
                        error = %e,
                        "Failed to submit share to source"
                    );
                } else {
                    debug!(source = %source.name, "Share submitted to source");
                }
            } else {
                error!(source_id = ?task_entry.source_id, "Share for unknown source");
            }
        } else {
            trace!(
                source = %self.sources.get(task_entry.source_id).map(|s| s.name.as_str()).unwrap_or("unknown"),
                job_id = %task_entry.template.id,
                nonce = format!("{:#x}", nonce),
                share_difficulty = %share_difficulty,
                threshold = %threshold,
                "Share below source threshold (not submitted)"
            );
        }
    }

    /// Handle an event from a hash thread.
    async fn handle_thread_event(
        &mut self,
        thread_id: ThreadId,
        event: HashThreadEvent,
        share_channels: &mut ShareStream,
    ) {
        let thread_name = self
            .threads
            .get(thread_id)
            .map(|entry| entry.thread.name())
            .unwrap_or("unknown");

        match event {
            HashThreadEvent::WorkExhausted { en2_searched } => {
                info!(thread = %thread_name, en2_searched, "Work exhausted");
                // TODO: Assign new work to this thread
            }

            HashThreadEvent::WorkDepletionWarning {
                estimated_remaining_ms,
            } => {
                debug!(thread = %thread_name, remaining_ms = estimated_remaining_ms, "Work depletion warning");
                // TODO: Prepare next work assignment
            }

            HashThreadEvent::StatusUpdate(status) => {
                trace!(
                    thread = %thread_name,
                    hashrate = %status.hashrate.to_human_readable(),
                    active = status.is_active,
                    "Thread status"
                );
                // Store the directly-measured hardware hashrate. A zero
                // reading is left unstored so callers keep falling back to
                // the shares estimator until a nonzero measurement arrives.
                if !status.hashrate.is_zero() {
                    if let Some(entry) = self.threads.get_mut(thread_id) {
                        entry.hardware_hashrate = Some(status.hashrate);
                    }
                }
            }

            HashThreadEvent::ExpectedHashRate(rate) => {
                let Some(entry) = self.threads.get_mut(thread_id) else {
                    return;
                };
                let first_report = entry.expected.is_none();
                entry.expected = Some(rate);
                let name = entry.thread.name().to_string();
                trace!(
                    thread = %name,
                    expected = %rate.to_human_readable(),
                    "Thread declared expected hashrate"
                );

                // First report is the thread's connect: hand it any cached jobs.
                if first_report {
                    self.assign_cached_jobs_to_thread(thread_id, &name, share_channels)
                        .await;
                }

                // Count the first report toward the startup gate, then
                // broadcast only once the gate is open. While it holds, the
                // report is recorded but the broadcast is suppressed; the
                // report that opens the gate sends the first one.
                if first_report && self.startup_gate.is_holding() {
                    self.startup_gate.record_reported();
                }
                if !self.startup_gate.is_holding() {
                    self.broadcast_hashrate_change().await;
                }
            }
        }
    }

    /// Handle a new thread arriving from the backplane.
    ///
    /// Registers and configures the thread only. Work assignment and the source
    /// broadcast happen when the thread makes its first ExpectedHashRate report,
    /// not on arrival.
    async fn handle_new_thread(
        &mut self,
        mut thread: Box<dyn HashThread>,
        thread_events: &mut ThreadEventStream,
    ) {
        let event_rx = thread
            .take_event_receiver()
            .expect("Thread missing event receiver");

        let thread_name = thread.name().to_string();
        let thread_id = self.threads.insert(ThreadEntry {
            thread,
            hashrate: HashrateEstimator::new(HASHRATE_WINDOW),
            expected: None,
            hardware_hashrate: None,
        });
        self.startup_gate.record_registered();
        thread_events.insert(thread_id, ReceiverStream::new(event_rx));
        debug!(thread = %thread_name, "Thread registered");

        // Configure the thread; it replies with ExpectedHashRate on its event
        // channel, handled in handle_thread_event.
        let entry = self
            .threads
            .get_mut(thread_id)
            .expect("Just inserted thread");
        if let Err(e) = entry.thread.configure().await {
            error!(thread = %thread_name, error = %e, "Failed to configure thread");
        }

        self.last_thread_count = thread_events.len();
    }

    /// Assign each source's cached job to a newly-eligible thread.
    ///
    /// Called from the thread's first ExpectedHashRate report. The thread takes
    /// the full EN2 range for each source, overlapping the other threads until
    /// the next job resplits the range.
    async fn assign_cached_jobs_to_thread(
        &mut self,
        thread_id: ThreadId,
        thread_name: &str,
        share_channels: &mut ShareStream,
    ) {
        let thread_hashrate = {
            let entry = self
                .threads
                .get_mut(thread_id)
                .expect("thread present for cached-job assignment");
            entry
                .hashrate
                .settled_hashrate()
                .or(entry.expected)
                .unwrap_or_default()
        };

        for (source_id, source) in self.sources.iter() {
            if self.mining != Some(source_id) {
                continue;
            }
            let Some(template) = &source.last_job else {
                continue;
            };

            // Extract full EN2 range (new thread overlaps with others)
            let full_en2_range = match &template.merkle_root {
                MerkleRootKind::Computed(t) => t.extranonce2_range.clone(),
                MerkleRootKind::Fixed(_) => continue,
            };

            let share_target =
                Self::compute_scheduler_target(thread_hashrate, template.share_target);

            let (share_tx, share_rx) = mpsc::channel(32);
            let hash_task = HashTask {
                template: template.clone(),
                en2_range: Some(full_en2_range.clone()),
                en2: full_en2_range.iter().next(),
                share_target,
                ntime: template.time,
                share_tx,
            };

            let entry = self
                .threads
                .get_mut(thread_id)
                .expect("Just inserted thread");
            if let Err(e) = entry.thread.update_task(hash_task).await {
                error!(thread = %thread_name, error = %e, "Failed to assign cached job");
            } else {
                let task_id = self.tasks.insert(TaskEntry {
                    source_id,
                    template: template.clone(),
                    thread_id,
                });
                share_channels.insert(task_id, ReceiverStream::new(share_rx));
                debug!(
                    thread = %thread_name,
                    source = %source.name,
                    job_id = %template.id,
                    "Assigned cached job to new thread"
                );
            }
        }
    }

    /// Detect and handle thread disconnections.
    async fn handle_thread_disconnections(
        &mut self,
        thread_events: &ThreadEventStream,
        share_channels: &mut ShareStream,
    ) {
        let current_count = thread_events.len();
        if current_count == self.last_thread_count {
            return;
        }

        debug!(
            previous = self.last_thread_count,
            current = current_count,
            "Thread count changed"
        );

        // Remove threads that no longer have active event streams
        let active_thread_ids: HashSet<_> = thread_events.keys().collect();
        self.threads.retain(|id, _| active_thread_ids.contains(&id));

        // Remove tasks for disconnected threads
        self.remove_tasks_where(share_channels, |e| {
            !active_thread_ids.contains(&e.thread_id)
        });

        self.last_thread_count = current_count;

        self.broadcast_hashrate_change().await;
    }

    /// Handle an API command, sending the result back on the reply channel.
    ///
    /// Publishes an updated state snapshot before replying so the API
    /// handler's subsequent `borrow()` sees the new value.
    fn handle_api_command(
        &mut self,
        cmd: SchedulerCommand,
        miner_telemetry_tx: &watch::Sender<MinerTelemetry>,
    ) {
        match cmd {
            SchedulerCommand::PauseMining { reply } => {
                self.paused = true;
                warn!("Mining paused via API (not yet implemented)");
                let _ = miner_telemetry_tx.send(self.compute_miner_telemetry());
                let _ = reply.send(Ok(()));
            }
            SchedulerCommand::ResumeMining { reply } => {
                self.paused = false;
                warn!("Mining resumed via API (not yet implemented)");
                let _ = miner_telemetry_tx.send(self.compute_miner_telemetry());
                let _ = reply.send(Ok(()));
            }
            SchedulerCommand::SetWeight { name, weight } => {
                if let Some(source) = self.sources.values_mut().find(|s| s.name == name) {
                    source.weight = weight;
                }
                let _ = miner_telemetry_tx.send(self.compute_miner_telemetry());
            }
        }
    }

    /// Give the next slot to whichever live pool the weights pick.
    ///
    /// The previous pool's tasks stay. Shares already found for its job
    /// still route back to it. The thread takes the new pool's cached job
    /// with `Replace`, which swaps the chain's work once.
    async fn rotate_slot(&mut self, share_channels: &mut ShareStream) {
        let next = self.pick_slot();
        if next == self.mining {
            return;
        }
        self.mining = next;
        let Some(source_id) = next else {
            return;
        };
        let Some(template) = self.sources.get(source_id).and_then(|s| s.last_job.clone()) else {
            return;
        };
        self.assign_job_to_threads(
            AssignMode::Replace,
            source_id,
            (*template).clone(),
            share_channels,
            true,
        )
        .await;
    }

    fn record_hashrate_samples(&mut self) {
        let measured = u64::from(self.measured_hashrate());
        let mining = self.mining;
        for (id, source) in self.sources.iter_mut() {
            let sample = if mining == Some(id) { measured } else { 0 };
            source.samples.push_back(sample);
            while source.samples.len() > 360 {
                source.samples.pop_front();
            }
        }
    }

    /// Drop sources whose event channel closed (the director stopped them).
    /// Returns true when the source that owned the chain went away, so the
    /// caller can hand the chain to whoever is left without waiting out
    /// the rest of the slot.
    fn handle_source_disconnections(&mut self, source_events: &SourceEventStream) -> bool {
        let active: HashSet<SourceId> = source_events.keys().copied().collect();
        let before = self.sources.len();
        self.sources.retain(|id, _| active.contains(&id));
        if self.sources.len() == before {
            return false;
        }
        self.mining.is_some_and(|id| !self.sources.contains_key(id))
    }

    /// Main scheduler loop.
    async fn run(
        &mut self,
        running: CancellationToken,
        mut thread_rx: mpsc::Receiver<ThreadRegistration>,
        mut source_reg_rx: mpsc::Receiver<SourceRegistration>,
        miner_telemetry_tx: watch::Sender<MinerTelemetry>,
        mut cmd_rx: mpsc::Receiver<SchedulerCommand>,
    ) {
        // StreamMaps as locals (not in self) to avoid borrow conflicts in select!
        let mut source_events: SourceEventStream = StreamMap::new();
        let mut thread_events: ThreadEventStream = StreamMap::new();
        let mut share_channels: ShareStream = StreamMap::new();

        // Create interval for periodic status logging
        let mut status_interval = tokio::time::interval(Duration::from_secs(30));
        status_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut first_status_tick = true;

        // Create interval for periodic API telemetry publishing
        let mut telemetry_interval = tokio::time::interval(Duration::from_secs(10));
        telemetry_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // One pool owns the chain per slot. The first tick is immediate;
        // skip it so the first job arms the chain instead of an empty pick.
        let mut slot_interval = tokio::time::interval(crate::pool_alloc::SLOT);
        slot_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut slot_primed = false;

        // Deadline for the startup-gate fallback, set when the enumeration-
        // complete signal arrives without immediately opening the gate.
        let mut gate_deadline: Option<tokio::time::Instant> = None;

        while !running.is_cancelled() {
            tokio::select! {
                // Source registration
                Some(registration) = source_reg_rx.recv() => {
                    self.handle_source_registration(registration, &mut source_events).await;
                }

                // Source events
                Some((source_id, event)) = source_events.next() => {
                    let source_name = self.sources.get(source_id)
                        .map(|s| s.name.as_str())
                        .unwrap_or("unknown");

                    match event {
                        SourceEvent::UpdateJob(job_template) => {
                            debug!(
                                source = %source_name,
                                job_id = %job_template.id,
                                "UpdateJob received"
                            );
                            let became_up = self.set_up(source_id, true);
                            if self.mining.is_none() {
                                self.mining = self.pick_slot();
                            }
                            let push = self.mining == Some(source_id);
                            self.assign_job_to_threads(
                                AssignMode::Update,
                                source_id,
                                job_template,
                                &mut share_channels,
                                push,
                            ).await;
                            if became_up && !self.startup_gate.is_holding() {
                                self.broadcast_hashrate_change().await;
                            }
                        }

                        SourceEvent::ReplaceJob(job_template) => {
                            debug!(
                                source = %source_name,
                                job_id = %job_template.id,
                                "ReplaceJob received"
                            );
                            let became_up = self.set_up(source_id, true);
                            if self.mining.is_none() {
                                self.mining = self.pick_slot();
                            }
                            let push = self.mining == Some(source_id);
                            self.assign_job_to_threads(
                                AssignMode::Replace,
                                source_id,
                                job_template,
                                &mut share_channels,
                                push,
                            ).await;
                            if became_up && !self.startup_gate.is_holding() {
                                self.broadcast_hashrate_change().await;
                            }
                        }

                        SourceEvent::ClearJobs => {
                            let became_down = self.set_up(source_id, false);
                            self.handle_clear_jobs(source_id, &mut share_channels);
                            if became_down && !self.startup_gate.is_holding() {
                                self.broadcast_hashrate_change().await;
                            }
                        }

                        SourceEvent::PoolDifficulty(diff) => {
                            if let Some(source) = self.sources.get_mut(source_id) {
                                source.pool_difficulty = Some(diff);
                                if let Some(job) = source.last_job.clone() {
                                    let mut updated = (*job).clone();
                                    updated.share_target = Difficulty::from_f64(diff).to_target();
                                    source.last_job = Some(Arc::new(updated));
                                }
                            }
                            let target = Difficulty::from_f64(diff).to_target();
                            for (_id, task) in self.tasks.iter_mut() {
                                if task.source_id == source_id {
                                    let mut updated = (*task.template).clone();
                                    updated.share_target = target;
                                    task.template = Arc::new(updated);
                                }
                            }
                            debug!(difficulty = diff, "Pool difficulty applied to current jobs");
                        }

                        SourceEvent::ShareResult { accepted, reason } => {
                            if let Some(source) = self.sources.get_mut(source_id) {
                                if accepted {
                                    source.accepted += 1;
                                } else {
                                    source.rejected += 1;
                                    note_reject(&mut source.reject_reasons, reason.as_deref());
                                }
                            }
                        }
                    }
                }

                // Share channels (from tasks)
                Some((task_id, share)) = share_channels.next() => {
                    self.handle_share(task_id, share).await;
                }

                // Thread events
                Some((thread_id, event)) = thread_events.next() => {
                    self.handle_thread_event(thread_id, event, &mut share_channels).await;
                }

                // Thread registration from backplane
                Some(registration) = thread_rx.recv() => {
                    match registration {
                        ThreadRegistration::Thread(thread) => {
                            self.handle_new_thread(thread, &mut thread_events).await;
                        }
                        ThreadRegistration::InitialEnumerationComplete => {
                            self.startup_gate.record_enumeration_complete();
                            if self.startup_gate.is_holding() {
                                gate_deadline =
                                    Some(tokio::time::Instant::now() + STARTUP_GATE_TIMEOUT);
                            } else {
                                self.broadcast_hashrate_change().await;
                            }
                        }
                    }
                }

                // Startup-gate fallback: open after the timeout even if some
                // thread never reported.
                _ = async {
                    match gate_deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                }, if self.startup_gate.is_holding() => {
                    self.startup_gate.record_timeout();
                    debug!("Startup gate opened by fallback timeout; not every thread reported");
                    self.broadcast_hashrate_change().await;
                    gate_deadline = None;
                }

                // Periodic status logging
                _ = status_interval.tick() => {
                    if first_status_tick {
                        first_status_tick = false;
                    } else {
                        let hashrate = self.measured_hashrate();
                        self.stats.log_summary(hashrate);
                    }
                }

                // API commands
                Some(cmd) = cmd_rx.recv() => {
                    let reweight = matches!(cmd, SchedulerCommand::SetWeight { .. });
                    self.handle_api_command(cmd, &miner_telemetry_tx);
                    if reweight && !self.startup_gate.is_holding() {
                        self.broadcast_hashrate_change().await;
                    }
                }

                // Hand the chain to the next pool. At most one switch per slot.
                _ = slot_interval.tick() => {
                    if !slot_primed {
                        slot_primed = true;
                    } else {
                        self.rotate_slot(&mut share_channels).await;
                    }
                }

                // Periodic state publishing
                _ = telemetry_interval.tick() => {
                    self.record_hashrate_samples();
                    let _ = miner_telemetry_tx.send(self.compute_miner_telemetry());
                }

                // Shutdown
                _ = running.cancelled() => {
                    debug!("Scheduler shutdown requested");
                    break;
                }
            }

            // Detect thread disconnections (StreamMap silently removes ended streams)
            self.handle_thread_disconnections(&thread_events, &mut share_channels)
                .await;
            if self.handle_source_disconnections(&source_events) {
                self.mining = None;
                self.rotate_slot(&mut share_channels).await;
            }
        }

        // Log final statistics
        let hashrate = self.measured_hashrate();
        self.stats.log_summary(hashrate);

        debug!("Scheduler shutdown complete");
    }
}

/// Sends each source its hashrate allocation.
///
/// Takes pre-collected (sender, hashrate) pairs to avoid capturing Scheduler
/// across await points (it contains Box<dyn HashThread> which isn't Sync).
/// Remember a pool's reject text. At most eight distinct strings; the
/// rest fold into `other`. The text is the pool's own message, trimmed.
fn note_reject(reasons: &mut Vec<(String, u64)>, reason: Option<&str>) {
    let raw = reason.unwrap_or("").trim();
    let raw = if raw.is_empty() { "unknown" } else { raw };
    let end = raw
        .char_indices()
        .nth(80)
        .map(|(i, _)| i)
        .unwrap_or(raw.len());
    let reason = &raw[..end];
    if let Some((_, count)) = reasons.iter_mut().find(|(r, _)| r == reason) {
        *count += 1;
        return;
    }
    let named = reasons.iter().filter(|(r, _)| r != "other").count();
    if named < 7 {
        reasons.push((reason.to_string(), 1));
        return;
    }
    if let Some((_, count)) = reasons.iter_mut().find(|(r, _)| r == "other") {
        *count += 1;
    } else {
        reasons.push(("other".into(), 1));
    }
}

fn average_recent(samples: &VecDeque<u64>, n: usize) -> u64 {
    let take = n.min(samples.len());
    if take == 0 {
        return 0;
    }
    let sum: u64 = samples.iter().rev().take(take).sum();
    sum / take as u64
}

async fn send_hashrate_updates(updates: Vec<(mpsc::Sender<SourceCommand>, HashRate)>) {
    for (sender, hashrate) in updates {
        let _ = sender.send(SourceCommand::UpdateHashRate(hashrate)).await;
    }
}

/// Threshold for warning about high share difficulty.
///
/// If expected time to find a share exceeds this, warn the operator that the
/// pool difficulty may be misconfigured for this hashrate.
const HIGH_DIFFICULTY_THRESHOLD: Duration = Duration::from_secs(300); // 5 minutes

/// How long difficulty must remain too high before warning.
const HIGH_DIFFICULTY_DEBOUNCE: Duration = Duration::from_secs(30);

/// Check whether job difficulty is unreasonably high for our hashrate.
fn is_difficulty_too_high(job: &JobTemplate, hashrate: HashRate) -> bool {
    if hashrate.is_zero() {
        return false;
    }

    let time_to_share = expected_time_to_share_from_target(job.share_target, hashrate);
    time_to_share > HIGH_DIFFICULTY_THRESHOLD
}

/// Run the scheduler task, receiving hash threads and job sources.
pub async fn task(
    running: CancellationToken,
    thread_rx: mpsc::Receiver<ThreadRegistration>,
    source_reg_rx: mpsc::Receiver<SourceRegistration>,
    miner_telemetry_tx: watch::Sender<MinerTelemetry>,
    cmd_rx: mpsc::Receiver<SchedulerCommand>,
) {
    let mut scheduler = Scheduler::new();
    scheduler
        .run(
            running,
            thread_rx,
            source_reg_rx,
            miner_telemetry_tx,
            cmd_rx,
        )
        .await;
}

/// Format seconds as human-readable duration.
///
/// Scales format based on duration to keep output compact:
/// - Under 1 minute: "45s"
/// - Under 1 hour: "12m 30s"
/// - Under 1 day: "12h 38m"
/// - 1 day or more: "1d 12h"
fn format_duration(secs: u64) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;

    if secs >= DAY {
        let days = secs / DAY;
        let hours = (secs % DAY) / HOUR;
        format!("{}d {}h", days, hours)
    } else if secs >= HOUR {
        let hours = secs / HOUR;
        let mins = (secs % HOUR) / MINUTE;
        format!("{}h {}m", hours, mins)
    } else if secs >= MINUTE {
        let mins = secs / MINUTE;
        let s = secs % MINUTE;
        format!("{}m {}s", mins, s)
    } else {
        format!("{}s", secs)
    }
}

/// One-shot gate that holds the first source broadcast until startup
/// enumeration is provably complete.
///
/// Held until the enumeration-complete signal has been seen (so every starting
/// thread is registered) and every registered thread has reported its expected
/// hashrate, or until a fallback timeout forces it. Once open it stays open.
#[derive(Debug)]
struct StartupGate {
    registered: usize,
    reported: usize,
    enumeration_complete: bool,
    open: bool,
}

impl StartupGate {
    fn new() -> Self {
        Self {
            registered: 0,
            reported: 0,
            enumeration_complete: false,
            open: false,
        }
    }

    fn is_holding(&self) -> bool {
        !self.open
    }

    /// Count a newly-registered thread.
    fn record_registered(&mut self) {
        if !self.open {
            self.registered += 1;
        }
    }

    /// Count a thread's first report.
    fn record_reported(&mut self) {
        if !self.open {
            self.reported += 1;
            self.try_open();
        }
    }

    /// Record the enumeration-complete signal.
    fn record_enumeration_complete(&mut self) {
        if !self.open {
            self.enumeration_complete = true;
            self.try_open();
        }
    }

    /// Force the gate open via fallback timeout.
    fn record_timeout(&mut self) {
        self.open = true;
    }

    fn try_open(&mut self) {
        if self.enumeration_complete && self.reported >= self.registered {
            self.open = true;
        }
    }
}

/// Mining statistics tracker.
#[derive(Debug)]
struct MiningStats {
    start_time: std::time::Instant,
    shares_submitted: u64,
}

impl Default for MiningStats {
    fn default() -> Self {
        Self {
            start_time: std::time::Instant::now(),
            shares_submitted: 0,
        }
    }
}

impl MiningStats {
    fn log_summary(&self, hashrate: HashRate) {
        let elapsed = self.start_time.elapsed();

        let hashrate_str = if hashrate.is_zero() {
            "--".to_string()
        } else {
            hashrate.to_human_readable()
        };

        info!(
            uptime = %format_duration(elapsed.as_secs()),
            hashrate = %hashrate_str,
            shares = self.shares_submitted,
            "Mining status."
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Difficulty;

    #[test]
    fn set_difficulty_94_does_not_submit_a_diff_10_share() {
        assert!(!share_meets_pool_difficulty(94.0, 10.0));
        assert!(share_meets_pool_difficulty(94.0, 94.0));
        assert!(share_meets_pool_difficulty(94.0, 200.0));
    }

    #[test]
    fn scheduler_target_zero_hashrate_passthrough() {
        let source_target = Difficulty::from(1024).to_target();
        let result = Scheduler::compute_scheduler_target(HashRate::from(0), source_target);
        assert_eq!(result, source_target);
    }

    #[test]
    fn scheduler_target_passthrough_when_in_range() {
        // Pick a source target that falls between the two bounds.
        // At 1 TH/s the bounds span roughly difficulty 23 (easiest)
        // to difficulty 233 (hardest). Difficulty 100 sits in between.
        let hashrate = HashRate::from_terahashes(1.0);
        let source_target = Difficulty::from(100).to_target();
        let result = Scheduler::compute_scheduler_target(hashrate, source_target);
        assert_eq!(result, source_target);
    }

    #[test]
    fn scheduler_target_clamps_hard_source_to_easier() {
        // Pool difficulty much higher than what our hashrate warrants.
        // The scheduler should ease it to the measurement floor so the
        // estimator gets samples.
        let hashrate = HashRate::from_terahashes(1.0);
        let very_hard = Difficulty::from(1_000_000).to_target();
        let result = Scheduler::compute_scheduler_target(hashrate, very_hard);

        let measurement_target = MEASUREMENT_SHARE_RATE.to_target(hashrate);
        assert_eq!(
            result, measurement_target,
            "should clamp to measurement floor"
        );
        assert!(result > very_hard, "clamped target should be easier");
    }

    #[test]
    fn scheduler_target_clamps_easy_source_to_harder() {
        // Pool difficulty absurdly low -- would flood the scheduler.
        // The scheduler should harden it to the flood ceiling.
        let hashrate = HashRate::from_terahashes(1.0);
        let very_easy = Target::MAX;
        let result = Scheduler::compute_scheduler_target(hashrate, very_easy);

        let flood_cap_target = FLOOD_CAP_RATE.to_target(hashrate);
        assert_eq!(result, flood_cap_target, "should clamp to flood ceiling");
        assert!(result < very_easy, "clamped target should be harder");
    }

    /// Checks compute_scheduler_target across a wide range of hashrates.
    #[test]
    fn scheduler_target_across_hashrates() {
        let source_target = Difficulty::from(1).to_target();
        for hashrate in [
            HashRate::from(5),
            HashRate::from(5_000),
            HashRate::from_megahashes(5.0),
            HashRate::from_gigahashes(500.0),
            HashRate::from_terahashes(1.0),
            HashRate::from_terahashes(100.0),
        ] {
            let _result = Scheduler::compute_scheduler_target(hashrate, source_target);
        }
    }

    #[test]
    fn startup_gate_opens_on_completion_when_all_reported() {
        let mut gate = StartupGate::new();
        gate.record_registered();
        gate.record_registered();
        gate.record_reported();
        gate.record_reported();
        assert!(gate.is_holding());
        gate.record_enumeration_complete();
        assert!(!gate.is_holding());
    }

    #[test]
    fn startup_gate_opens_on_last_report_after_completion() {
        let mut gate = StartupGate::new();
        gate.record_registered();
        gate.record_registered();
        gate.record_enumeration_complete();
        assert!(gate.is_holding());
        gate.record_reported();
        assert!(gate.is_holding());
        gate.record_reported();
        assert!(!gate.is_holding());
    }

    #[test]
    fn startup_gate_holds_for_completion_even_after_all_reported() {
        let mut gate = StartupGate::new();
        gate.record_registered();
        gate.record_reported();
        assert!(gate.is_holding());
        gate.record_enumeration_complete();
        assert!(!gate.is_holding());
    }

    #[test]
    fn startup_gate_opens_on_completion_with_no_threads() {
        let mut gate = StartupGate::new();
        gate.record_enumeration_complete();
        assert!(!gate.is_holding());
    }

    #[test]
    fn startup_gate_timeout_forces_open() {
        let mut gate = StartupGate::new();
        gate.record_registered();
        gate.record_enumeration_complete();
        assert!(gate.is_holding());
        gate.record_timeout();
        assert!(!gate.is_holding());
    }

    #[test]
    fn startup_gate_stays_open_after_opening() {
        let mut gate = StartupGate::new();
        gate.record_registered();
        gate.record_enumeration_complete();
        gate.record_reported();
        assert!(!gate.is_holding());
        // Later events leave it open.
        gate.record_reported();
        gate.record_enumeration_complete();
        gate.record_timeout();
        assert!(!gate.is_holding());
    }

    #[test]
    fn reject_reasons_count_the_pool_text_and_cap_the_list() {
        let mut reasons = Vec::new();
        super::note_reject(&mut reasons, Some("Job not found"));
        super::note_reject(&mut reasons, Some("Job not found"));
        super::note_reject(&mut reasons, Some("  "));
        assert_eq!(
            reasons,
            vec![("Job not found".into(), 2), ("unknown".into(), 1)]
        );
        for i in 0..10 {
            super::note_reject(&mut reasons, Some(&format!("reason-{i}")));
        }
        assert!(reasons.len() <= 8);
        assert!(reasons.iter().any(|(r, n)| r == "other" && *n >= 1));
    }
}
