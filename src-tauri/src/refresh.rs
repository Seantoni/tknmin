//! The single owner of refresh.
//!
//! Everything that decides *when* data is read lives here and nowhere else:
//! filesystem watchers, debounce and maximum-delay policy, reconciliation
//! schedules, per-source single flight, network backoff, transaction
//! boundaries, and publication of a committed revision to the interface, the
//! menu bar, and the alerts.
//!
//! The boundary that keeps this file from becoming a parser monolith:
//!
//! - here: when, whether, and in what order work runs
//! - `adapters/*`: how one explicitly requested delta is read and parsed
//! - `repository/*`: how a transaction is persisted and queried
//! - React: how a committed snapshot is rendered
//!
//! ```text
//! startup / file event / focus / wake / network / timer / manual
//!                            |
//!                            v
//!            policy -> queue -> debounce -> single-flight worker
//!                            |
//!                 explicit per-source delta request
//!                            |
//!                            v
//!                  one repository transaction
//!         records + quotas + checkpoints + health, one revision
//!                            |
//!                     commit succeeds
//!                            |
//!                            v
//!            publish revision -> React, mini, menu bar, alerts
//! ```
//!
//! There is deliberately no second entry point. A manual "sync now" submits a
//! trigger to this coordinator like everything else, and removing it entirely
//! would not change automatic correctness.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::adapters::{self, AdapterError, DeltaRequest, SourceAdapter, SourceDelta, SyncMode};
use crate::domain::{RepositoryRevision, SourceApp};
use crate::pipeline;
use crate::repository::{CommitCounts, HealthOutcome, SourceTransaction, UsageWriter};

/// Why a refresh was asked for.
///
/// The set is exhaustive on purpose: every automatic path in the application
/// is one of these, and adding a path means adding a variant here rather than
/// starting a timer somewhere else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefreshTrigger {
    /// The application launched. Catch up on whatever happened while it was
    /// closed, behind the already-painted persisted data.
    Startup,
    /// A watcher saw this source's files change.
    LocalSourceChanged(SourceApp),
    /// Claude Code rewrote its allowance cache.
    ClaudeQuotaCacheChanged,
    /// Cursor wrote locally, so billed data may follow.
    CursorLocalActivity,
    /// A dashboard session was connected or disconnected, which swaps which
    /// dataset is authoritative for Cursor.
    CursorConnectionChanged,
    /// The quota lane's own deadline came round.
    ScheduledQuota,
    /// The periodic safety net: repair whatever the watchers missed.
    Reconciliation,
    /// A window was focused or shown. A cheap catch-up, never a scan.
    WindowFocus,
    /// The machine woke, so every watcher missed everything.
    Wake,
    /// The network came back after a failure that looked like connectivity.
    NetworkRecovery,
    /// The user pressed "sync now". Recovery and diagnostics only.
    Manual,
}

/// Which half of a source's data a job covers.
///
/// Split so a slow account endpoint cannot delay local token ingestion, and so
/// an unchanged quota poll cannot make the interface re-render.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefreshCategory {
    Usage,
    Quota,
}

/// One unit of exclusive work: at most one job runs per lane at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaneKey {
    pub source_app: SourceApp,
    pub category: RefreshCategory,
}

/// Every timing decision the application makes, in one place.
///
/// Nothing else in the codebase may hold a refresh duration. When these need
/// tuning against real workloads, this struct is the only thing to change.
#[derive(Debug, Clone, Copy)]
pub struct RefreshPolicy {
    /// How long a source must be quiet before its changes are read. Long
    /// enough to coalesce an editor's write burst, short enough to stay
    /// imperceptible.
    pub local_debounce: Duration,
    /// The longest a source may be held back by continuing activity. Without
    /// this, a file written to continuously would never be read at all.
    pub local_max_delay: Duration,
    /// The cheap safety net: catches a dropped watcher event using nothing but
    /// file metadata.
    pub metadata_reconcile: Duration,
    /// The thorough safety net: re-reads bounded windows so corrections and
    /// deletions converge.
    pub full_reconcile: Duration,
    /// How often allowance windows are re-read. More frequent than a full
    /// import because thresholds are time-sensitive and the read is cheap.
    pub quota_interval: Duration,
    /// First retry delay after a failure that looked like connectivity.
    pub backoff_base: Duration,
    /// The ceiling on that retry delay.
    pub backoff_max: Duration,
    /// How long after detected Cursor activity to keep asking whether billing
    /// has caught up, and how far apart those questions are.
    pub upstream_retry_window: Duration,
    pub upstream_retry_interval: Duration,
}

impl Default for RefreshPolicy {
    fn default() -> Self {
        Self {
            local_debounce: Duration::from_millis(350),
            local_max_delay: Duration::from_secs(2),
            metadata_reconcile: Duration::from_secs(45),
            full_reconcile: Duration::from_secs(12 * 60),
            quota_interval: Duration::from_secs(60),
            backoff_base: Duration::from_secs(5),
            backoff_max: Duration::from_secs(5 * 60),
            upstream_retry_window: Duration::from_secs(15 * 60),
            upstream_retry_interval: Duration::from_secs(90),
        }
    }
}

/// What the interface is told after a transaction commits.
///
/// Every field describes work that is already durable: the event is emitted
/// after the commit returns, never before, so a listener that refetches on it
/// cannot read a revision that does not exist yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataChanged {
    pub revision: RepositoryRevision,
    pub affected_sources: Vec<SourceApp>,
    pub affected_categories: Vec<RefreshCategory>,
    pub inserted: usize,
    pub updated: usize,
    pub deleted: usize,
    /// True when records or quotas moved, as opposed to only freshness.
    ///
    /// Consumers that recompute something expensive — the menu bar's totals,
    /// the alert evaluation — use this to stay still through a quiet minute.
    /// The interface still refetches either way, because "app synced … ago"
    /// ages whether or not the numbers did.
    pub data_changed: bool,
    pub app_synced_at: chrono::DateTime<Utc>,
}

/// Where a committed revision goes.
///
/// A trait so the coordinator can be tested without a running Tauri
/// application — and so there is exactly one implementation in production,
/// rather than each consumer subscribing to its own event.
pub trait RefreshObserver: Send + Sync {
    fn publish(&self, event: &DataChanged);
}

/// An observer that drops everything, for tests and headless use.
pub struct SilentObserver;

impl RefreshObserver for SilentObserver {
    fn publish(&self, _event: &DataChanged) {}
}

/// What one lane is currently doing, for diagnostics and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceJobState {
    pub running: bool,
    pub queued: bool,
    /// Incremented every time a job starts, and every time the lane's dataset
    /// changes underneath it. A result carrying an older generation than the
    /// lane holds is discarded rather than applied.
    pub generation: u64,
    pub consecutive_failures: u32,
}

/// The handle every other module gets.
///
/// It can ask for work and ask what happened. It cannot scan, write, or
/// publish — which is what keeps the single-owner rule true by construction
/// rather than by review.
#[derive(Clone)]
pub struct RefreshHandle {
    triggers: Sender<Message>,
    status: Arc<Mutex<HashMap<LaneKey, SourceJobState>>>,
}

impl RefreshHandle {
    /// Ask for work. Never blocks on the work itself.
    ///
    /// Submitting the same trigger repeatedly is free: the coordinator
    /// coalesces, so a burst of a thousand filesystem events becomes one job.
    pub fn submit(&self, trigger: RefreshTrigger) {
        // A closed channel means the coordinator is gone, which happens only
        // during shutdown. Dropping the trigger is the right answer.
        let _ = self.triggers.send(Message::Trigger(trigger));
    }

    pub fn status(&self) -> Vec<(LaneKey, SourceJobState)> {
        let Ok(status) = self.status.lock() else {
            return Vec::new();
        };
        let mut lanes: Vec<_> = status.iter().map(|(key, state)| (*key, *state)).collect();
        lanes.sort_by_key(|(key, _)| *key);
        lanes
    }

    /// Whether anything at all is in flight, for a "syncing" indicator.
    pub fn is_busy(&self) -> bool {
        self.status()
            .iter()
            .any(|(_, state)| state.running || state.queued)
    }
}

enum Message {
    Trigger(RefreshTrigger),
    Finished(JobResult),
}

struct JobResult {
    lane: LaneKey,
    generation: u64,
    outcome: Result<JobOutcome, AdapterError>,
}

struct JobOutcome {
    /// Cursor saw local activity that the authoritative data has not caught up
    /// with, so the lane keeps asking for a bounded while.
    awaiting_upstream: bool,
}

/// Work a lane owes, with the two deadlines that decide when it runs.
#[derive(Debug, Clone, Copy)]
struct Pending {
    mode: SyncMode,
    /// The quiet-period deadline: pushed back by each new trigger.
    due: Instant,
    /// The hard deadline: fixed when the lane first went dirty, so continuous
    /// activity cannot starve it.
    latest: Instant,
}

struct Lane {
    running: bool,
    pending: Option<Pending>,
    generation: u64,
    consecutive_failures: u32,
    /// Set while a connectivity failure is being backed off from.
    backoff_until: Option<Instant>,
    /// While set, the lane keeps re-asking whether upstream has published.
    upstream_deadline: Option<Instant>,
}

impl Lane {
    fn new() -> Self {
        Self {
            running: false,
            pending: None,
            generation: 0,
            consecutive_failures: 0,
            backoff_until: None,
            upstream_deadline: None,
        }
    }
}

/// The coordinator. Built once, started once, owned by the application.
pub struct RefreshCoordinator {
    adapters: Vec<Arc<dyn SourceAdapter>>,
    writer: Arc<dyn UsageWriter>,
    observer: Arc<dyn RefreshObserver>,
    policy: RefreshPolicy,
}

impl RefreshCoordinator {
    pub fn new(writer: Arc<dyn UsageWriter>, observer: Arc<dyn RefreshObserver>) -> Self {
        Self {
            adapters: adapters::registry().into_iter().map(Arc::from).collect(),
            writer,
            observer,
            policy: RefreshPolicy::default(),
        }
    }

    /// Build over an explicit adapter set, so tests never touch real logs.
    pub fn with_adapters(
        adapters: Vec<Arc<dyn SourceAdapter>>,
        writer: Arc<dyn UsageWriter>,
        observer: Arc<dyn RefreshObserver>,
    ) -> Self {
        Self {
            adapters,
            writer,
            observer,
            policy: RefreshPolicy::default(),
        }
    }

    pub fn with_policy(mut self, policy: RefreshPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Start the scheduler and the watchers, and return the handle.
    ///
    /// Everything after this point is driven by triggers. The caller submits
    /// `Startup` and then never speaks to a source again.
    pub fn start(self) -> RefreshHandle {
        let (triggers, inbox) = mpsc::channel();
        let status = Arc::new(Mutex::new(HashMap::new()));
        let handle = RefreshHandle {
            triggers: triggers.clone(),
            status: Arc::clone(&status),
        };

        let watch_roots = self.watch_roots();
        let scheduler = Scheduler {
            adapters: self.adapters,
            writer: self.writer,
            observer: self.observer,
            policy: self.policy,
            lanes: HashMap::new(),
            status,
            outbox: triggers.clone(),
        };

        std::thread::Builder::new()
            .name("refresh-scheduler".to_string())
            .spawn(move || scheduler.run(inbox))
            .expect("the refresh scheduler thread could not be started");

        // Watchers exist for speed; reconciliation exists for correctness. If
        // registration fails the application still converges, just later — so
        // this is reported and not treated as fatal.
        match watch::install(watch_roots, triggers) {
            Ok(watcher) => {
                // The watcher must outlive this call or the registrations are
                // dropped with it, and it has no other owner: the coordinator
                // itself is consumed here.
                std::mem::forget(watcher);
            }
            Err(error) => eprintln!("tokens: filesystem watching unavailable: {error}"),
        }

        handle
    }

    /// Which directory belongs to which source. Adapters name their roots;
    /// registering them is refresh work, so it happens here.
    fn watch_roots(&self) -> Vec<(PathBuf, SourceApp)> {
        self.adapters
            .iter()
            .flat_map(|adapter| {
                let source = adapter.source_app();
                adapter
                    .watch_roots()
                    .into_iter()
                    .map(move |root| (root, source))
            })
            .collect()
    }
}

struct Scheduler {
    adapters: Vec<Arc<dyn SourceAdapter>>,
    writer: Arc<dyn UsageWriter>,
    observer: Arc<dyn RefreshObserver>,
    policy: RefreshPolicy,
    lanes: HashMap<LaneKey, Lane>,
    status: Arc<Mutex<HashMap<LaneKey, SourceJobState>>>,
    outbox: Sender<Message>,
}

impl Scheduler {
    fn run(mut self, inbox: Receiver<Message>) {
        let mut next_metadata_reconcile = Instant::now() + self.policy.metadata_reconcile;
        let mut next_full_reconcile = Instant::now() + self.policy.full_reconcile;
        let mut next_quota = Instant::now() + self.policy.quota_interval;
        // A tick that took far longer than it was asked to wait means the
        // machine slept. Every watcher missed everything in between, so the
        // honest response is to reconcile rather than trust the queue.
        let mut last_tick = Instant::now();

        loop {
            let wait = self.next_wakeup(&[next_metadata_reconcile, next_full_reconcile, next_quota]);

            match inbox.recv_timeout(wait) {
                Ok(Message::Trigger(trigger)) => self.accept(trigger),
                Ok(Message::Finished(result)) => self.finish(result),
                Err(RecvTimeoutError::Timeout) => {}
                // Every handle is gone: the application is shutting down.
                Err(RecvTimeoutError::Disconnected) => return,
            }

            let now = Instant::now();
            if now.saturating_duration_since(last_tick) > wait + SLEEP_SUSPICION {
                self.accept(RefreshTrigger::Wake);
            }
            last_tick = now;

            if now >= next_metadata_reconcile {
                next_metadata_reconcile = now + self.policy.metadata_reconcile;
                self.accept(RefreshTrigger::Reconciliation);
            }
            if now >= next_full_reconcile {
                next_full_reconcile = now + self.policy.full_reconcile;
                self.enqueue_all(SyncMode::Reconcile, RefreshCategory::Usage, Duration::ZERO);
                // Quota samples exist only to measure a trailing pace. Once
                // they are older than the longest horizon in use they are
                // weight, so the idle path drops them.
                let cutoff = Utc::now() - chrono::Duration::days(7);
                let _ = self.writer.prune_quota_samples(cutoff);
            }
            if now >= next_quota {
                next_quota = now + self.policy.quota_interval;
                self.accept(RefreshTrigger::ScheduledQuota);
            }

            self.dispatch();
            self.publish_status();
        }
    }

    /// How long to sleep: until the earliest thing that has to happen.
    fn next_wakeup(&self, timers: &[Instant]) -> Duration {
        let now = Instant::now();
        let mut earliest = timers.iter().copied().min().unwrap_or(now + IDLE_TICK);
        for lane in self.lanes.values() {
            if let Some(pending) = lane.pending {
                let due = pending.due.min(pending.latest);
                let due = lane.backoff_until.map_or(due, |until| due.max(until));
                earliest = earliest.min(due);
            }
        }
        earliest.saturating_duration_since(now).max(MIN_WAIT)
    }

    /// Translate a trigger into work on lanes. This is the only place a
    /// trigger becomes a job, which is what lets the trigger list be read as
    /// the complete set of reasons this application ever reads a source.
    fn accept(&mut self, trigger: RefreshTrigger) {
        use RefreshTrigger::*;

        let debounce = self.policy.local_debounce;
        match trigger {
            LocalSourceChanged(source) => self.enqueue(
                LaneKey {
                    source_app: source,
                    category: RefreshCategory::Usage,
                },
                SyncMode::Incremental,
                debounce,
            ),
            ClaudeQuotaCacheChanged => {
                // Claude Code writes its allowance cache the moment it learns
                // a new number, and publishing that immediately is the point.
                self.enqueue(
                    LaneKey {
                        source_app: SourceApp::ClaudeCode,
                        category: RefreshCategory::Quota,
                    },
                    SyncMode::QuotaOnly,
                    debounce,
                )
            }
            CursorLocalActivity => self.enqueue(
                LaneKey {
                    source_app: SourceApp::Cursor,
                    category: RefreshCategory::Usage,
                },
                SyncMode::Incremental,
                debounce,
            ),
            CursorConnectionChanged => {
                // Connecting or disconnecting swaps which Cursor dataset is
                // authoritative. Bumping the generation is what stops a job
                // already in flight for the old mode from committing rows of
                // the wrong kind on top of the new ones.
                let usage = LaneKey {
                    source_app: SourceApp::Cursor,
                    category: RefreshCategory::Usage,
                };
                self.lane(usage).generation += 1;
                self.enqueue(usage, SyncMode::Reconcile, Duration::ZERO);
                self.enqueue(
                    LaneKey {
                        source_app: SourceApp::Cursor,
                        category: RefreshCategory::Quota,
                    },
                    SyncMode::QuotaOnly,
                    Duration::ZERO,
                );
            }
            ScheduledQuota => {
                self.enqueue_all(SyncMode::QuotaOnly, RefreshCategory::Quota, Duration::ZERO)
            }
            Startup | Manual | Wake | NetworkRecovery => {
                self.enqueue_all(SyncMode::Reconcile, RefreshCategory::Usage, Duration::ZERO);
                self.enqueue_all(SyncMode::QuotaOnly, RefreshCategory::Quota, Duration::ZERO);
                // A failure being backed off from is exactly what these
                // triggers exist to retry, so they clear the wait.
                for lane in self.lanes.values_mut() {
                    lane.backoff_until = None;
                }
            }
            Reconciliation | WindowFocus => {
                // The cheap pass: incremental mode means each source checks
                // metadata and reads only what actually moved, so a dropped
                // watcher event is repaired without re-reading any history.
                self.enqueue_all(SyncMode::Incremental, RefreshCategory::Usage, Duration::ZERO)
            }
        }
    }

    fn enqueue_all(&mut self, mode: SyncMode, category: RefreshCategory, debounce: Duration) {
        for source in self.sources() {
            self.enqueue(
                LaneKey {
                    source_app: source,
                    category,
                },
                mode,
                debounce,
            );
        }
    }

    fn sources(&self) -> Vec<SourceApp> {
        self.adapters
            .iter()
            .map(|adapter| adapter.source_app())
            .collect()
    }

    fn lane(&mut self, key: LaneKey) -> &mut Lane {
        self.lanes.entry(key).or_insert_with(Lane::new)
    }

    /// Merge new work into whatever the lane already owed.
    ///
    /// Two rules do all the work. The quiet deadline moves forward with each
    /// trigger, so a burst coalesces into one job; the hard deadline never
    /// moves, so a source under continuous write pressure is still read on
    /// time. A lane asked for both an incremental and a reconciling pass does
    /// the reconciling one, which subsumes it.
    fn enqueue(&mut self, key: LaneKey, mode: SyncMode, debounce: Duration) {
        let max_delay = self.policy.local_max_delay;
        let now = Instant::now();
        let lane = self.lane(key);

        lane.pending = Some(match lane.pending {
            Some(existing) => Pending {
                mode: wider(existing.mode, mode),
                due: (now + debounce).min(existing.latest),
                latest: existing.latest,
            },
            None => Pending {
                mode,
                due: now + debounce,
                latest: now + max_delay.max(debounce),
            },
        });
    }

    /// Start every lane whose moment has come and which is not already busy.
    fn dispatch(&mut self) {
        let now = Instant::now();
        let ready: Vec<(LaneKey, Pending)> = self
            .lanes
            .iter()
            .filter_map(|(key, lane)| {
                if lane.running {
                    return None;
                }
                if lane.backoff_until.is_some_and(|until| now < until) {
                    return None;
                }
                let pending = lane.pending?;
                (now >= pending.due.min(pending.latest)).then_some((*key, pending))
            })
            .collect();

        for (key, pending) in ready {
            let Some(adapter) = self.adapter_for(key.source_app) else {
                self.lane(key).pending = None;
                continue;
            };

            // Claiming the lane before the thread starts is what makes single
            // flight true: a trigger arriving one instruction later becomes
            // the *next* job rather than a second concurrent one.
            let generation = {
                let lane = self.lane(key);
                debug_assert!(
                    !lane.running,
                    "two jobs would be in flight for one lane at once"
                );
                lane.pending = None;
                lane.running = true;
                lane.generation += 1;
                lane.generation
            };

            let writer = Arc::clone(&self.writer);
            let observer = Arc::clone(&self.observer);
            let outbox = self.outbox.clone();
            let started = std::thread::Builder::new()
                .name(format!(
                    "refresh-{}-{:?}",
                    key.source_app.as_str(),
                    key.category
                ))
                .spawn(move || {
                    let outcome = run_job(
                        adapter.as_ref(),
                        writer.as_ref(),
                        observer.as_ref(),
                        key,
                        pending.mode,
                        generation,
                    );
                    let _ = outbox.send(Message::Finished(JobResult {
                        lane: key,
                        generation,
                        outcome,
                    }));
                });

            if started.is_err() {
                // Nothing will report back, so release the lane rather than
                // wedge it as permanently running.
                self.lane(key).running = false;
            }
        }
    }

    fn finish(&mut self, result: JobResult) {
        let policy = self.policy;
        let now = Instant::now();
        let lane = self.lane(result.lane);
        lane.running = false;

        // A result from an older generation belongs to a world that no longer
        // exists — a Cursor connection change, most often. It does not get to
        // set this lane's backoff or its retry schedule.
        if result.generation < lane.generation {
            return;
        }

        match result.outcome {
            Ok(outcome) => {
                lane.consecutive_failures = 0;
                lane.backoff_until = None;
                match (outcome.awaiting_upstream, lane.upstream_deadline) {
                    (false, _) => lane.upstream_deadline = None,
                    (true, deadline) => {
                        // Cursor publishes billing after the fact. Keep asking
                        // for a bounded while, then stop and let periodic
                        // reconciliation carry it — an unbounded retry would
                        // be exactly the polling loop this design removes.
                        let deadline = deadline.unwrap_or(now + policy.upstream_retry_window);
                        if now < deadline {
                            lane.upstream_deadline = Some(deadline);
                            lane.pending = Some(Pending {
                                mode: SyncMode::Incremental,
                                due: now + policy.upstream_retry_interval,
                                latest: now + policy.upstream_retry_interval,
                            });
                        } else {
                            lane.upstream_deadline = None;
                        }
                    }
                }
            }
            Err(error) => {
                lane.consecutive_failures = lane.consecutive_failures.saturating_add(1);
                if error.is_offline() {
                    // Exponential, capped, and deferring to the provider's own
                    // answer when it gave one.
                    let doubling = policy
                        .backoff_base
                        .saturating_mul(1u32 << lane.consecutive_failures.min(6));
                    let wait = error
                        .retry_after()
                        .unwrap_or(doubling)
                        .min(policy.backoff_max);
                    lane.backoff_until = Some(now + wait);
                    lane.pending = Some(Pending {
                        mode: SyncMode::Incremental,
                        due: now + wait,
                        latest: now + wait,
                    });
                }
            }
        }
    }

    fn adapter_for(&self, source: SourceApp) -> Option<Arc<dyn SourceAdapter>> {
        self.adapters
            .iter()
            .find(|adapter| adapter.source_app() == source)
            .map(Arc::clone)
    }

    fn publish_status(&self) {
        let Ok(mut status) = self.status.lock() else {
            return;
        };
        status.clear();
        for (key, lane) in &self.lanes {
            status.insert(
                *key,
                SourceJobState {
                    running: lane.running,
                    queued: lane.pending.is_some(),
                    generation: lane.generation,
                    consecutive_failures: lane.consecutive_failures,
                },
            );
        }
    }
}

/// A tick this much longer than it asked to wait means the machine slept.
const SLEEP_SUSPICION: Duration = Duration::from_secs(20);
/// The longest the scheduler sleeps with nothing at all scheduled.
const IDLE_TICK: Duration = Duration::from_secs(30);
/// Never spin: even a deadline already past waits a moment.
const MIN_WAIT: Duration = Duration::from_millis(10);

/// The wider of two modes, since a lane asked for both should do the one that
/// subsumes the other.
fn wider(left: SyncMode, right: SyncMode) -> SyncMode {
    let rank = |mode: SyncMode| match mode {
        SyncMode::QuotaOnly => 0,
        SyncMode::Incremental => 1,
        SyncMode::Reconcile => 2,
    };
    if rank(left) >= rank(right) {
        left
    } else {
        right
    }
}

/// One job: read the requested delta, commit it atomically, publish it.
///
/// The order is the invariant. Nothing is published that is not committed, and
/// a failure commits a health row and leaves every value the interface is
/// showing exactly where it was.
fn run_job(
    adapter: &dyn SourceAdapter,
    writer: &dyn UsageWriter,
    observer: &dyn RefreshObserver,
    lane: LaneKey,
    mode: SyncMode,
    generation: u64,
) -> Result<JobOutcome, AdapterError> {
    let started = Utc::now();
    let request = DeltaRequest {
        mode,
        checkpoints: writer.checkpoints(adapter.id()).unwrap_or_default(),
        now: started,
    };

    let delta = match adapter.read_delta(&request) {
        Ok(delta) => delta,
        Err(error) => {
            // Record what happened without touching data, so the last-known-
            // good values stay on screen, marked stale rather than erased.
            let transaction = SourceTransaction::health_only(
                lane.source_app,
                adapter.id(),
                HealthOutcome::Failed {
                    offline: error.is_offline(),
                    error: error.to_string(),
                },
                Utc::now(),
            );
            if let Ok(outcome) = writer.commit(transaction) {
                if outcome.advanced {
                    observer.publish(&DataChanged {
                        revision: outcome.revision,
                        affected_sources: vec![lane.source_app],
                        affected_categories: vec![lane.category],
                        inserted: 0,
                        updated: 0,
                        deleted: 0,
                        data_changed: false,
                        app_synced_at: Utc::now(),
                    });
                }
            }
            log_job(lane, mode, generation, started, Err(&error));
            return Err(error);
        }
    };

    let awaiting_upstream = delta.awaiting_upstream;
    let outcome = match writer.commit(build_transaction(adapter, lane, delta)) {
        Ok(outcome) => outcome,
        Err(error) => {
            // A failed transaction changed neither data nor revision, and the
            // checkpoint did not move: the next attempt reads the same bytes
            // again rather than skipping past them.
            let error = AdapterError::Unreadable {
                adapter: "repository",
                reason: error.to_string(),
            };
            log_job(lane, mode, generation, started, Err(&error));
            return Err(error);
        }
    };

    if outcome.advanced {
        // The commit has already returned, so the revision named here is
        // durable and readable. Publishing before the commit would let a
        // listener refetch a revision that does not exist yet.
        debug_assert!(
            writer.revision().is_ok_and(|current| current >= outcome.revision),
            "published a revision the store does not hold"
        );
        observer.publish(&DataChanged {
            revision: outcome.revision,
            affected_sources: vec![lane.source_app],
            affected_categories: vec![lane.category],
            inserted: outcome.counts.inserted,
            updated: outcome.counts.updated,
            deleted: outcome.counts.deleted,
            data_changed: outcome.data_changed,
            app_synced_at: Utc::now(),
        });
    }

    log_job(lane, mode, generation, started, Ok(&outcome.counts));
    Ok(JobOutcome { awaiting_upstream })
}

/// Assemble one source's atomic unit of change from what the adapter read.
fn build_transaction(
    adapter: &dyn SourceAdapter,
    lane: LaneKey,
    delta: SourceDelta,
) -> SourceTransaction {
    let (records, report) = pipeline::normalize_drafts(delta.drafts);
    if report.rejected_count() > 0 {
        eprintln!(
            "tokens: {} rejected {} malformed entries",
            adapter.id(),
            report.rejected_count()
        );
    }
    for failure in &delta.failures {
        eprintln!("tokens: {}: {failure}", adapter.id());
    }

    SourceTransaction {
        source_app: lane.source_app,
        adapter_id: adapter.id().to_string(),
        records,
        replace_scopes: delta.replace_scopes,
        checkpoints: delta.checkpoints,
        quotas: delta.quotas,
        replace_quotas: delta.replace_quotas,
        health: HealthOutcome::Succeeded {
            source_observed_at: delta.source_observed_at,
            awaiting_upstream: delta.awaiting_upstream,
        },
        committed_at: Utc::now(),
    }
}

/// One line per job, carrying no secret and no path — the audit trail the plan
/// asks for, and the only logging this module does.
fn log_job(
    lane: LaneKey,
    mode: SyncMode,
    generation: u64,
    started: chrono::DateTime<Utc>,
    result: Result<&CommitCounts, &AdapterError>,
) {
    let elapsed = (Utc::now() - started).num_milliseconds();
    match result {
        Ok(counts) => eprintln!(
            "tokens: job {}/{:?} gen {generation} {mode:?} ok in {elapsed}ms (+{} ~{} -{} ={})",
            lane.source_app.as_str(),
            lane.category,
            counts.inserted,
            counts.updated,
            counts.deleted,
            counts.unchanged,
        ),
        Err(error) => eprintln!(
            "tokens: job {}/{:?} gen {generation} {mode:?} failed in {elapsed}ms: {error}",
            lane.source_app.as_str(),
            lane.category,
        ),
    }
}

/// Filesystem watching.
///
/// The callbacks do exactly one thing: enqueue a dirty-source trigger. They do
/// not read, parse, write, or emit — a watcher that did any of those would be
/// a second refresh owner running on the notification thread.
mod watch {
    use super::{Message, RefreshTrigger};
    use crate::domain::SourceApp;
    use notify::{RecommendedWatcher, RecursiveMode, Watcher};
    use std::path::{Path, PathBuf};
    use std::sync::mpsc::Sender;

    pub(super) fn install(
        roots: Vec<(PathBuf, SourceApp)>,
        outbox: Sender<Message>,
    ) -> notify::Result<RecommendedWatcher> {
        let owners = roots.clone();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                let Ok(event) = event else { return };
                for path in &event.paths {
                    if let Some(trigger) = classify(path, &owners) {
                        let _ = outbox.send(Message::Trigger(trigger));
                    }
                }
            })?;

        for (root, _) in roots {
            // A root that does not exist yet is not an error: Codex creates
            // `archived_sessions/` the first time a session ends, and
            // reconciliation covers it until a watch can be placed.
            if root.exists() {
                let _ = watcher.watch(&root, RecursiveMode::Recursive);
            }
        }

        Ok(watcher)
    }

    /// Which source a changed path belongs to, and what that change means.
    ///
    /// The one special case is `~/.claude.json`: it lives beside the
    /// transcripts rather than under them, and a change to it is a new
    /// allowance reading rather than new usage.
    fn classify(path: &Path, owners: &[(PathBuf, SourceApp)]) -> Option<RefreshTrigger> {
        let name = path.file_name().and_then(|name| name.to_str())?;
        let owner = owners
            .iter()
            .filter(|(root, _)| path.starts_with(root))
            // The most specific root wins, so a nested watch is not shadowed
            // by the broader one that contains it.
            .max_by_key(|(root, _)| root.as_os_str().len())?;

        if name == ".claude.json" {
            return Some(RefreshTrigger::ClaudeQuotaCacheChanged);
        }
        match owner.1 {
            SourceApp::Cursor => Some(RefreshTrigger::CursorLocalActivity),
            SourceApp::ClaudeCode if !name.ends_with(".jsonl") => None,
            source => Some(RefreshTrigger::LocalSourceChanged(source)),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn owners() -> Vec<(PathBuf, SourceApp)> {
            vec![
                (PathBuf::from("/home/.codex/sessions"), SourceApp::Codex),
                (
                    PathBuf::from("/home/.claude/projects"),
                    SourceApp::ClaudeCode,
                ),
                (PathBuf::from("/home"), SourceApp::ClaudeCode),
                (
                    PathBuf::from("/apps/Cursor/globalStorage"),
                    SourceApp::Cursor,
                ),
            ]
        }

        #[test]
        fn a_rollout_write_makes_its_own_source_dirty() {
            assert_eq!(
                classify(
                    Path::new("/home/.codex/sessions/2026/07/30/rollout-x.jsonl"),
                    &owners()
                ),
                Some(RefreshTrigger::LocalSourceChanged(SourceApp::Codex))
            );
        }

        #[test]
        fn the_claude_config_is_an_allowance_change_not_a_usage_change() {
            assert_eq!(
                classify(Path::new("/home/.claude.json"), &owners()),
                Some(RefreshTrigger::ClaudeQuotaCacheChanged)
            );
        }

        #[test]
        fn a_nested_subagent_transcript_belongs_to_claude_code() {
            assert_eq!(
                classify(
                    Path::new("/home/.claude/projects/app/session/subagents/agent-1.jsonl"),
                    &owners()
                ),
                Some(RefreshTrigger::LocalSourceChanged(SourceApp::ClaudeCode))
            );
        }

        #[test]
        fn cursors_write_ahead_log_counts_as_activity() {
            assert_eq!(
                classify(
                    Path::new("/apps/Cursor/globalStorage/state.vscdb-wal"),
                    &owners()
                ),
                Some(RefreshTrigger::CursorLocalActivity)
            );
        }

        #[test]
        fn unrelated_files_are_ignored() {
            assert_eq!(classify(Path::new("/elsewhere/thing.txt"), &owners()), None);
            assert_eq!(
                classify(Path::new("/home/.claude/projects/app/notes.md"), &owners()),
                None
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        SourceCheckpoint, SourceProvenance, SummaryQuery, SyncState, TokenCounts, TokenField,
        UsageQuota, UsageRecordDraft,
    };
    use crate::repository::{InMemoryUsageRepository, UsageReader};
    use std::sync::atomic::{AtomicUsize, Ordering};

    type Answer = Arc<Mutex<Result<SourceDelta, AdapterError>>>;

    /// An adapter that returns what a test tells it to, and counts its calls.
    struct ScriptedAdapter {
        source: SourceApp,
        calls: Arc<AtomicUsize>,
        answer: Answer,
    }

    impl ScriptedAdapter {
        fn new(source: SourceApp) -> (Arc<Self>, Arc<AtomicUsize>, Answer) {
            let calls = Arc::new(AtomicUsize::new(0));
            let answer: Answer = Arc::new(Mutex::new(Ok(SourceDelta::default())));
            let adapter = Arc::new(Self {
                source,
                calls: Arc::clone(&calls),
                answer: Arc::clone(&answer),
            });
            (adapter, calls, answer)
        }
    }

    impl SourceAdapter for ScriptedAdapter {
        fn id(&self) -> &'static str {
            "scripted"
        }
        fn version(&self) -> &'static str {
            "1.0.0"
        }
        fn source_app(&self) -> SourceApp {
            self.source
        }
        fn read_delta(&self, _request: &DeltaRequest) -> Result<SourceDelta, AdapterError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.answer
                .lock()
                .map(|answer| answer.clone())
                .unwrap_or_else(|_| Ok(SourceDelta::default()))
        }
    }

    /// Records every published event, so ordering can be asserted.
    #[derive(Default)]
    struct RecordingObserver {
        events: Mutex<Vec<DataChanged>>,
    }

    impl RefreshObserver for RecordingObserver {
        fn publish(&self, event: &DataChanged) {
            if let Ok(mut events) = self.events.lock() {
                events.push(event.clone());
            }
        }
    }

    fn draft(id: &str, output: u64) -> UsageRecordDraft {
        UsageRecordDraft::new(
            SourceApp::Codex,
            SourceProvenance {
                adapter_id: "scripted".to_string(),
                adapter_version: "1.0.0".to_string(),
                source_ref: None,
            },
        )
        .with_source_event_id(id)
        .with_raw_timestamp("2026-07-29T10:00:00Z")
        .with_tokens(TokenCounts {
            input: TokenField::exact(10),
            output: TokenField::exact(output),
            ..TokenCounts::default()
        })
    }

    fn usage_lane(source: SourceApp) -> LaneKey {
        LaneKey {
            source_app: source,
            category: RefreshCategory::Usage,
        }
    }

    fn quiet_policy() -> RefreshPolicy {
        // Timers pushed far out so only the behaviour under test runs.
        RefreshPolicy {
            metadata_reconcile: Duration::from_secs(600),
            full_reconcile: Duration::from_secs(600),
            quota_interval: Duration::from_secs(600),
            ..RefreshPolicy::default()
        }
    }

    #[test]
    fn a_committed_batch_is_published_with_its_revision() {
        let repository = Arc::new(InMemoryUsageRepository::new());
        let observer = Arc::new(RecordingObserver::default());
        let (adapter, _, answer) = ScriptedAdapter::new(SourceApp::Codex);
        *answer.lock().unwrap() = Ok(SourceDelta {
            drafts: vec![draft("e1", 5)],
            ..SourceDelta::default()
        });

        run_job(
            adapter.as_ref(),
            repository.as_ref(),
            observer.as_ref(),
            usage_lane(SourceApp::Codex),
            SyncMode::Incremental,
            1,
        )
        .unwrap();

        let events = observer.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].inserted, 1);
        // The event names a revision that is already durable.
        assert_eq!(repository.revision().unwrap(), events[0].revision);
    }

    #[test]
    fn a_failed_read_keeps_the_data_and_marks_the_source() {
        let repository = Arc::new(InMemoryUsageRepository::new());
        let observer = Arc::new(RecordingObserver::default());
        let (adapter, _, answer) = ScriptedAdapter::new(SourceApp::Codex);

        *answer.lock().unwrap() = Ok(SourceDelta {
            drafts: vec![draft("e1", 5)],
            ..SourceDelta::default()
        });
        run_job(
            adapter.as_ref(),
            repository.as_ref(),
            observer.as_ref(),
            usage_lane(SourceApp::Codex),
            SyncMode::Incremental,
            1,
        )
        .unwrap();

        *answer.lock().unwrap() = Err(AdapterError::Offline {
            adapter: "scripted",
            reason: "no network".to_string(),
        });
        let failed = run_job(
            adapter.as_ref(),
            repository.as_ref(),
            observer.as_ref(),
            usage_lane(SourceApp::Codex),
            SyncMode::Incremental,
            2,
        );

        assert!(failed.is_err());
        assert_eq!(repository.count().unwrap(), 1);
        let health = repository.health().unwrap();
        assert_eq!(health[0].state, SyncState::Offline);
        // The successful sync's own freshness survives the failure.
        assert!(health[0].app_synced_at.is_some());
    }

    #[test]
    fn a_later_snapshot_of_an_event_converges_rather_than_duplicating() {
        let repository = Arc::new(InMemoryUsageRepository::new());
        let observer = Arc::new(SilentObserver);
        let (adapter, _, answer) = ScriptedAdapter::new(SourceApp::Codex);

        for output in [5, 120] {
            *answer.lock().unwrap() = Ok(SourceDelta {
                drafts: vec![draft("e1", output)],
                ..SourceDelta::default()
            });
            run_job(
                adapter.as_ref(),
                repository.as_ref(),
                observer.as_ref(),
                usage_lane(SourceApp::Codex),
                SyncMode::Incremental,
                1,
            )
            .unwrap();
        }

        assert_eq!(repository.count().unwrap(), 1);
        let summary = repository.summary(&SummaryQuery::default()).unwrap();
        assert_eq!(summary.totals.output.tokens, 120);
    }

    #[test]
    fn a_checkpoint_reaches_the_next_request() {
        let repository = Arc::new(InMemoryUsageRepository::new());
        let (adapter, _, answer) = ScriptedAdapter::new(SourceApp::Codex);
        *answer.lock().unwrap() = Ok(SourceDelta {
            checkpoints: vec![SourceCheckpoint {
                adapter_id: "scripted".to_string(),
                source_key: "file-1".to_string(),
                payload: serde_json::json!({ "offset": 900 }),
            }],
            ..SourceDelta::default()
        });

        run_job(
            adapter.as_ref(),
            repository.as_ref(),
            &SilentObserver,
            usage_lane(SourceApp::Codex),
            SyncMode::Incremental,
            1,
        )
        .unwrap();

        let stored = repository.checkpoints("scripted").unwrap();
        assert_eq!(stored[0].payload["offset"], 900);
    }

    #[test]
    fn one_failing_source_does_not_remove_another_sources_windows() {
        let repository = Arc::new(InMemoryUsageRepository::new());
        let quota = |source: SourceApp| UsageQuota {
            source_app: source,
            label: None,
            window_minutes: 10_080,
            used_percent_tenths: 250,
            resets_at: None,
            observed_at: Utc::now(),
        };

        for source in [SourceApp::Codex, SourceApp::Cursor] {
            let (adapter, _, answer) = ScriptedAdapter::new(source);
            *answer.lock().unwrap() = Ok(SourceDelta {
                quotas: vec![quota(source)],
                replace_quotas: true,
                ..SourceDelta::default()
            });
            run_job(
                adapter.as_ref(),
                repository.as_ref(),
                &SilentObserver,
                LaneKey {
                    source_app: source,
                    category: RefreshCategory::Quota,
                },
                SyncMode::QuotaOnly,
                1,
            )
            .unwrap();
        }

        let (adapter, _, answer) = ScriptedAdapter::new(SourceApp::Cursor);
        *answer.lock().unwrap() = Err(AdapterError::RateLimited {
            adapter: "scripted",
            retry_after: Some(Duration::from_secs(30)),
        });
        let _ = run_job(
            adapter.as_ref(),
            repository.as_ref(),
            &SilentObserver,
            LaneKey {
                source_app: SourceApp::Cursor,
                category: RefreshCategory::Quota,
            },
            SyncMode::QuotaOnly,
            2,
        );

        assert_eq!(repository.quotas().unwrap().len(), 2);
    }

    #[test]
    fn a_burst_of_triggers_becomes_one_job() {
        let (adapter, calls, _) = ScriptedAdapter::new(SourceApp::Codex);
        let handle = RefreshCoordinator::with_adapters(
            vec![adapter],
            Arc::new(InMemoryUsageRepository::new()),
            Arc::new(SilentObserver),
        )
        .with_policy(RefreshPolicy {
            local_debounce: Duration::from_millis(80),
            local_max_delay: Duration::from_millis(400),
            ..quiet_policy()
        })
        .start();

        for _ in 0..200 {
            handle.submit(RefreshTrigger::LocalSourceChanged(SourceApp::Codex));
        }
        std::thread::sleep(Duration::from_millis(700));

        let ran = calls.load(Ordering::SeqCst);
        assert!(ran >= 1, "the burst should have produced a job");
        assert!(ran <= 2, "the burst became {ran} jobs, not one");
    }

    #[test]
    fn a_source_that_goes_dirty_while_running_gets_exactly_one_follow_up() {
        let (adapter, calls, _) = ScriptedAdapter::new(SourceApp::Codex);
        let handle = RefreshCoordinator::with_adapters(
            vec![adapter],
            Arc::new(InMemoryUsageRepository::new()),
            Arc::new(SilentObserver),
        )
        .with_policy(RefreshPolicy {
            local_debounce: Duration::from_millis(20),
            local_max_delay: Duration::from_millis(60),
            ..quiet_policy()
        })
        .start();

        handle.submit(RefreshTrigger::LocalSourceChanged(SourceApp::Codex));
        std::thread::sleep(Duration::from_millis(200));
        handle.submit(RefreshTrigger::LocalSourceChanged(SourceApp::Codex));
        std::thread::sleep(Duration::from_millis(300));

        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_slow_source_does_not_delay_another_one() {
        // Cursor's scripted adapter blocks; Codex's must still run.
        struct SlowAdapter {
            started: Arc<AtomicUsize>,
        }
        impl SourceAdapter for SlowAdapter {
            fn id(&self) -> &'static str {
                "slow"
            }
            fn version(&self) -> &'static str {
                "1.0.0"
            }
            fn source_app(&self) -> SourceApp {
                SourceApp::Cursor
            }
            fn read_delta(&self, _request: &DeltaRequest) -> Result<SourceDelta, AdapterError> {
                self.started.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(400));
                Ok(SourceDelta::default())
            }
        }

        let slow_calls = Arc::new(AtomicUsize::new(0));
        let (fast, fast_calls, _) = ScriptedAdapter::new(SourceApp::Codex);
        let handle = RefreshCoordinator::with_adapters(
            vec![
                Arc::new(SlowAdapter {
                    started: Arc::clone(&slow_calls),
                }),
                fast,
            ],
            Arc::new(InMemoryUsageRepository::new()),
            Arc::new(SilentObserver),
        )
        .with_policy(RefreshPolicy {
            local_debounce: Duration::from_millis(10),
            local_max_delay: Duration::from_millis(20),
            ..quiet_policy()
        })
        .start();

        handle.submit(RefreshTrigger::CursorLocalActivity);
        handle.submit(RefreshTrigger::LocalSourceChanged(SourceApp::Codex));
        std::thread::sleep(Duration::from_millis(150));

        assert_eq!(slow_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            fast_calls.load(Ordering::SeqCst),
            1,
            "Codex waited behind the slow Cursor job"
        );
    }

    #[test]
    fn the_widest_requested_mode_wins() {
        assert_eq!(
            wider(SyncMode::Incremental, SyncMode::Reconcile),
            SyncMode::Reconcile
        );
        assert_eq!(
            wider(SyncMode::Reconcile, SyncMode::Incremental),
            SyncMode::Reconcile
        );
        assert_eq!(
            wider(SyncMode::QuotaOnly, SyncMode::Incremental),
            SyncMode::Incremental
        );
    }
}
