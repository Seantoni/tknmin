//! Application state held by Tauri.
//!
//! Commands see the repository only through the trait, so swapping the
//! in-memory backend for SQLite later touches this file and nothing else.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use crate::alerts::AlertLedger;
use crate::domain::{AppOptions, UsageQuota};
use crate::fixtures;
use crate::repository::{InMemoryUsageRepository, UsageRepository};

pub struct AppState {
    repository: Arc<dyn UsageRepository>,
    /// Freshest quota snapshot per source, replaced on every refresh. Kept
    /// outside the repository: quota is account state, not usage data.
    quotas: RwLock<Vec<UsageQuota>>,
    /// User settings (thresholds, …). Path is set once the app config dir is known.
    options: RwLock<AppOptions>,
    options_path: RwLock<Option<PathBuf>>,
    /// Fired / snoozed threshold alerts for this process.
    alerts: AlertLedger,
}

impl AppState {
    /// An empty store, kept for the life of the process. Logs are imported
    /// into it after startup by the refresh pass.
    pub fn in_memory() -> Self {
        Self {
            repository: Arc::new(InMemoryUsageRepository::new()),
            quotas: RwLock::new(Vec::new()),
            options: RwLock::new(AppOptions::defaults()),
            options_path: RwLock::new(None),
            alerts: AlertLedger::new(),
        }
    }

    /// The deterministic fake dataset, kept for integration tests now that the
    /// application itself imports real logs.
    pub fn with_fake_data() -> Self {
        Self {
            repository: Arc::new(InMemoryUsageRepository::with_records(fixtures::fake_records())),
            quotas: RwLock::new(Vec::new()),
            options: RwLock::new(AppOptions::defaults()),
            options_path: RwLock::new(None),
            alerts: AlertLedger::new(),
        }
    }

    pub fn with_repository(repository: Arc<dyn UsageRepository>) -> Self {
        Self {
            repository,
            quotas: RwLock::new(Vec::new()),
            options: RwLock::new(AppOptions::defaults()),
            options_path: RwLock::new(None),
            alerts: AlertLedger::new(),
        }
    }

    pub fn repository(&self) -> &dyn UsageRepository {
        self.repository.as_ref()
    }

    /// A shared handle for work off the main thread, like the startup import.
    pub fn repository_handle(&self) -> Arc<dyn UsageRepository> {
        Arc::clone(&self.repository)
    }

    pub fn quotas(&self) -> Vec<UsageQuota> {
        self.quotas.read().map(|quotas| quotas.clone()).unwrap_or_default()
    }

    pub fn set_quotas(&self, quotas: Vec<UsageQuota>) {
        if let Ok(mut current) = self.quotas.write() {
            *current = quotas;
        }
    }

    pub fn options(&self) -> AppOptions {
        self.options.read().map(|options| options.clone()).unwrap_or_default()
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
