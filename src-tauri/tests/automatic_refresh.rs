//! Acceptance checks for automatic synchronization.
//!
//! These drive the real coordinator over the real persistent store, with a
//! scripted adapter standing in for a source. What they assert is the set of
//! promises the architecture makes to the user: the app catches up without
//! being asked, a relaunch shows the last committed data immediately, a
//! failure never erases anything, and reconciliation repairs what the
//! watchers missed.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokens_lib::adapters::{AdapterError, DeltaRequest, SourceAdapter, SourceDelta};
use tokens_lib::domain::{
    SourceApp, SourceProvenance, SummaryQuery, SyncState, TokenCounts, TokenField, UsageQuota,
    UsageRecordDraft,
};
use tokens_lib::refresh::{
    DataChanged, RefreshCoordinator, RefreshObserver, RefreshPolicy, RefreshTrigger,
};
use tokens_lib::repository::{SqliteUsageRepository, UsageReader};

/// A source whose answer a test controls, and whose reads it can count.
struct ScriptedSource {
    calls: Arc<AtomicUsize>,
    answer: Arc<Mutex<Result<SourceDelta, AdapterError>>>,
}

impl SourceAdapter for ScriptedSource {
    fn id(&self) -> &'static str {
        "scripted"
    }
    fn version(&self) -> &'static str {
        "1.0.0"
    }
    fn source_app(&self) -> SourceApp {
        SourceApp::Codex
    }
    fn read_delta(&self, _request: &DeltaRequest) -> Result<SourceDelta, AdapterError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.answer
            .lock()
            .map(|answer| answer.clone())
            .unwrap_or_else(|_| Ok(SourceDelta::default()))
    }
}

#[derive(Default)]
struct Published {
    events: Mutex<Vec<DataChanged>>,
}

impl RefreshObserver for Published {
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
        input: TokenField::exact(100),
        output: TokenField::exact(output),
        ..TokenCounts::default()
    })
}

/// Timers pushed out so only the behaviour under test runs, and debounce short
/// enough that a test does not have to wait a human amount of time.
fn brisk() -> RefreshPolicy {
    RefreshPolicy {
        local_debounce: Duration::from_millis(20),
        local_max_delay: Duration::from_millis(60),
        metadata_reconcile: Duration::from_secs(600),
        full_reconcile: Duration::from_secs(600),
        quota_interval: Duration::from_secs(600),
        ..RefreshPolicy::default()
    }
}

/// Wait for a condition rather than for a duration, so a slow machine does not
/// make a correctness test flaky.
fn until(deadline: Duration, mut ready: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    while started.elapsed() < deadline {
        if ready() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    ready()
}

fn scratch(name: &str) -> std::path::PathBuf {
    let directory =
        std::env::temp_dir().join(format!("tokens-acceptance-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    directory
}

#[test]
fn startup_catches_up_without_anyone_asking() {
    let calls = Arc::new(AtomicUsize::new(0));
    let answer = Arc::new(Mutex::new(Ok(SourceDelta {
        drafts: vec![draft("e1", 10)],
        ..SourceDelta::default()
    })));
    let repository = Arc::new(SqliteUsageRepository::in_memory().unwrap());
    let published = Arc::new(Published::default());

    let refresh = RefreshCoordinator::with_adapters(
        vec![Arc::new(ScriptedSource {
            calls: Arc::clone(&calls),
            answer,
        })],
        repository.clone(),
        published.clone(),
    )
    .with_policy(brisk())
    .start();

    refresh.submit(RefreshTrigger::Startup);

    assert!(until(Duration::from_secs(5), || repository
        .count()
        .unwrap()
        == 1));
    // Every event names a revision the store already holds, so a listener
    // that refetches on one cannot read something that does not exist yet.
    let events = published.events.lock().unwrap();
    let current = repository.revision().unwrap();
    assert!(events.iter().all(|event| event.revision <= current));

    // Exactly one of them carried the records; the rest are freshness, and
    // say so, which is what keeps the menu bar still on a quiet minute.
    let with_data: Vec<_> = events.iter().filter(|event| event.data_changed).collect();
    assert_eq!(with_data.len(), 1);
    assert_eq!(with_data[0].inserted, 1);
}

#[test]
fn a_relaunch_shows_the_last_committed_data_immediately() {
    let directory = scratch("relaunch");
    let path = directory.join("usage.sqlite3");

    {
        let repository = Arc::new(SqliteUsageRepository::open(&path).unwrap());
        let refresh = RefreshCoordinator::with_adapters(
            vec![Arc::new(ScriptedSource {
                calls: Arc::new(AtomicUsize::new(0)),
                answer: Arc::new(Mutex::new(Ok(SourceDelta {
                    drafts: vec![draft("e1", 10), draft("e2", 20)],
                    ..SourceDelta::default()
                }))),
            })],
            repository.clone(),
            Arc::new(Published::default()),
        )
        .with_policy(brisk())
        .start();
        refresh.submit(RefreshTrigger::Startup);
        assert!(until(Duration::from_secs(5), || repository
            .count()
            .unwrap()
            == 2));
    }

    // A second launch, before any source is read: the dashboard already has
    // something true to show.
    let reopened = SqliteUsageRepository::open(&path).unwrap();
    let snapshot = reopened
        .snapshot(
            &SummaryQuery::default(),
            &tokens_lib::domain::RecentQuery::default(),
        )
        .unwrap();

    assert_eq!(snapshot.record_count, 2);
    assert_eq!(snapshot.summary.totals.output.tokens, 30);
    assert!(snapshot.revision > 0);

    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn a_failing_source_keeps_its_last_good_values_and_says_it_failed() {
    let repository = Arc::new(SqliteUsageRepository::in_memory().unwrap());
    let answer = Arc::new(Mutex::new(Ok(SourceDelta {
        drafts: vec![draft("e1", 10)],
        quotas: vec![UsageQuota {
            source_app: SourceApp::Codex,
            label: None,
            window_minutes: 10_080,
            used_percent_tenths: 420,
            resets_at: None,
            observed_at: chrono::Utc::now(),
        }],
        replace_quotas: true,
        ..SourceDelta::default()
    })));

    let refresh = RefreshCoordinator::with_adapters(
        vec![Arc::new(ScriptedSource {
            calls: Arc::new(AtomicUsize::new(0)),
            answer: Arc::clone(&answer),
        })],
        repository.clone(),
        Arc::new(Published::default()),
    )
    .with_policy(brisk())
    .start();

    refresh.submit(RefreshTrigger::Startup);
    assert!(until(Duration::from_secs(5), || repository
        .count()
        .unwrap()
        == 1));

    // Now the source breaks.
    *answer.lock().unwrap() = Err(AdapterError::Offline {
        adapter: "scripted",
        reason: "network unreachable".to_string(),
    });
    refresh.submit(RefreshTrigger::Manual);

    assert!(until(Duration::from_secs(5), || repository
        .health()
        .unwrap()
        .first()
        .is_some_and(|health| health.state == SyncState::Offline)));

    // Nothing was erased: the numbers on screen are last-known-good, and only
    // their freshness changed.
    assert_eq!(repository.count().unwrap(), 1);
    let quotas = repository.quotas().unwrap();
    assert_eq!(quotas.len(), 1);
    assert_eq!(quotas[0].used_percent_tenths, 420);
    let health = repository.health().unwrap();
    assert!(health[0].app_synced_at.is_some());
    assert!(health[0].last_error.is_some());
}

#[test]
fn reconciliation_repairs_a_change_no_watcher_reported() {
    let calls = Arc::new(AtomicUsize::new(0));
    let answer = Arc::new(Mutex::new(Ok(SourceDelta::default())));
    let repository = Arc::new(SqliteUsageRepository::in_memory().unwrap());

    // Held only to keep the coordinator addressable; deliberately never used,
    // because the point of this test is that nothing submits a trigger.
    let _refresh = RefreshCoordinator::with_adapters(
        vec![Arc::new(ScriptedSource {
            calls: Arc::clone(&calls),
            answer: Arc::clone(&answer),
        })],
        repository.clone(),
        Arc::new(Published::default()),
    )
    .with_policy(RefreshPolicy {
        // The whole point: no trigger is ever submitted for the change below.
        metadata_reconcile: Duration::from_millis(120),
        ..brisk()
    })
    .start();

    // Data appears with no notification of any kind — a dropped watcher event.
    *answer.lock().unwrap() = Ok(SourceDelta {
        drafts: vec![draft("late", 77)],
        ..SourceDelta::default()
    });

    assert!(
        until(Duration::from_secs(5), || repository.count().unwrap() == 1),
        "the periodic pass did not pick up the missed change"
    );
}

#[test]
fn a_correction_converges_instead_of_double_counting() {
    let repository = Arc::new(SqliteUsageRepository::in_memory().unwrap());
    let answer = Arc::new(Mutex::new(Ok(SourceDelta {
        drafts: vec![draft("e1", 10)],
        ..SourceDelta::default()
    })));

    let refresh = RefreshCoordinator::with_adapters(
        vec![Arc::new(ScriptedSource {
            calls: Arc::new(AtomicUsize::new(0)),
            answer: Arc::clone(&answer),
        })],
        repository.clone(),
        Arc::new(Published::default()),
    )
    .with_policy(brisk())
    .start();

    refresh.submit(RefreshTrigger::Startup);
    assert!(until(Duration::from_secs(5), || repository
        .count()
        .unwrap()
        == 1));

    // The source restates the same event with a different number.
    *answer.lock().unwrap() = Ok(SourceDelta {
        drafts: vec![draft("e1", 999)],
        ..SourceDelta::default()
    });
    refresh.submit(RefreshTrigger::Manual);

    assert!(until(Duration::from_secs(5), || {
        repository
            .summary(&SummaryQuery::default())
            .unwrap()
            .totals
            .output
            .tokens
            == 999
    }));
    assert_eq!(
        repository.count().unwrap(),
        1,
        "the correction was double counted"
    );
}

#[test]
fn revisions_only_move_forward() {
    let repository = Arc::new(SqliteUsageRepository::in_memory().unwrap());
    let published = Arc::new(Published::default());
    let answer = Arc::new(Mutex::new(Ok(SourceDelta {
        drafts: vec![draft("e1", 10)],
        ..SourceDelta::default()
    })));

    let refresh = RefreshCoordinator::with_adapters(
        vec![Arc::new(ScriptedSource {
            calls: Arc::new(AtomicUsize::new(0)),
            answer: Arc::clone(&answer),
        })],
        repository.clone(),
        published.clone(),
    )
    .with_policy(brisk())
    .start();

    for output in [10, 20, 30, 40] {
        *answer.lock().unwrap() = Ok(SourceDelta {
            drafts: vec![draft("e1", output)],
            ..SourceDelta::default()
        });
        refresh.submit(RefreshTrigger::Manual);
        std::thread::sleep(Duration::from_millis(120));
    }

    let events = published.events.lock().unwrap();
    assert!(
        events.len() >= 2,
        "expected several commits, saw {}",
        events.len()
    );
    for pair in events.windows(2) {
        assert!(
            pair[1].revision > pair[0].revision,
            "revision went backwards: {} then {}",
            pair[0].revision,
            pair[1].revision
        );
    }
}
