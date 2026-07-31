//! Application state held by Tauri.
//!
//! What is *not* here matters as much as what is. There is no quota vector, no
//! refresh timer, no "last report": quotas and freshness are committed data,
//! read from the repository at the revision the interface asked for, and
//! refresh timing belongs to `crate::refresh`. A second copy of either would
//! be a second thing that can be out of date.
//!
//! Commands see the repository through [`UsageReader`] only. The write
//! capability exists, but this type never holds it — the coordinator was given
//! the sole production `UsageWriter` when it was built.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use crate::alerts::AlertLedger;
use crate::domain::{AppOptions, UsageQuota, WindowPace};
use crate::fixtures;
use crate::refresh::RefreshHandle;
use crate::repository::{InMemoryUsageRepository, RiskSnapshot, UsageReader};

pub struct AppState {
    reader: Arc<dyn UsageReader>,
    /// The only way anything outside `refresh` can ask for work. Absent in
    /// tests that exercise queries without a running coordinator.
    refresh: RwLock<Option<RefreshHandle>>,
    /// User settings (thresholds, …). Path is set once the app config dir is known.
    options: RwLock<AppOptions>,
    options_path: RwLock<Option<PathBuf>>,
    /// Fired / snoozed threshold alerts for this process.
    alerts: AlertLedger,
}

impl AppState {
    /// An empty in-memory store. Used by tests; the running application opens
    /// the persistent one instead.
    pub fn in_memory() -> Self {
        Self::with_reader(Arc::new(InMemoryUsageRepository::new()))
    }

    /// The deterministic fake dataset, kept for integration tests now that the
    /// application itself imports real logs.
    pub fn with_fake_data() -> Self {
        Self::with_reader(Arc::new(InMemoryUsageRepository::with_records(
            fixtures::fake_records(),
        )))
    }

    pub fn with_reader(reader: Arc<dyn UsageReader>) -> Self {
        Self {
            reader,
            refresh: RwLock::new(None),
            options: RwLock::new(AppOptions::defaults()),
            options_path: RwLock::new(None),
            alerts: AlertLedger::new(),
        }
    }

    /// Read-only access to committed data. There is no writing counterpart on
    /// purpose: see the module comment.
    pub fn reader(&self) -> &dyn UsageReader {
        self.reader.as_ref()
    }

    /// Hand the coordinator's handle to the application, once, during setup.
    pub fn attach_refresh(&self, handle: RefreshHandle) {
        if let Ok(mut current) = self.refresh.write() {
            *current = Some(handle);
        }
    }

    pub fn refresh(&self) -> Option<RefreshHandle> {
        self.refresh.read().ok().and_then(|handle| handle.clone())
    }

    /// The committed quota snapshot. Read through, never cached: the menu bar
    /// and the alerts must see exactly what the dashboard sees.
    pub fn quotas(&self) -> Vec<UsageQuota> {
        self.reader.quotas().unwrap_or_default()
    }

    /// Allowances and their pace in one consistent read.
    ///
    /// The menu bar needs both and draws them on one line, so reading them
    /// separately would let a commit land between the two and pair a
    /// percentage with the pace of a different revision. It also halves the
    /// work: the pace assembly reads the quotas anyway.
    pub fn risk(&self) -> RiskSnapshot {
        self.reader.risk_snapshot().unwrap_or_else(|_| RiskSnapshot {
            revision: 0,
            quotas: Vec::new(),
            health: Vec::new(),
            pace: Vec::new(),
            projections: Vec::new(),
        })
    }

    /// The pace of every live window, computed the same way the dashboard
    /// computes it. Alerts need it to warn on projected exhaustion, and it
    /// must come from the same inputs the interface sees or the banner and
    /// the notification would disagree.
    ///
    /// Reads risk and nothing else. This runs on every refresh tick and every
    /// notification action, so it must not drag two summaries, a count and a
    /// record list along behind it.
    pub fn pace(&self) -> Vec<WindowPace> {
        self.reader.pace().unwrap_or_default()
    }

    pub fn options(&self) -> AppOptions {
        self.options
            .read()
            .map(|options| options.clone())
            .unwrap_or_default()
    }

    pub fn set_options_path(&self, path: PathBuf) {
        if let Ok(mut current) = self.options_path.write() {
            *current = Some(path);
        }
    }

    pub fn options_path(&self) -> Option<PathBuf> {
        self.options_path.read().ok().and_then(|path| path.clone())
    }

    pub fn replace_options(&self, options: AppOptions) {
        if let Ok(mut current) = self.options.write() {
            *current = options;
        }
    }

    pub fn alerts(&self) -> &AlertLedger {
        &self.alerts
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::in_memory()
    }
}
