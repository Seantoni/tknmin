//! Run one real synchronization pass against this machine and print what it
//! found.
//!
//! Manual check for the adapters and the coordinator:
//!
//! ```sh
//! cargo run --example import_logs
//! ```
//!
//! This is the coordinator doing the work, not a second import path: it starts
//! a real one, submits `Startup`, and waits for the lanes to go quiet. What it
//! prints is therefore what the application itself would have committed.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokens_lib::refresh::{RefreshCoordinator, RefreshTrigger, SilentObserver};
use tokens_lib::repository::{InMemoryUsageRepository, UsageReader};

fn main() {
    let repository = Arc::new(InMemoryUsageRepository::new());
    let started = Instant::now();

    let refresh = RefreshCoordinator::new(repository.clone(), Arc::new(SilentObserver)).start();
    refresh.submit(RefreshTrigger::Startup);

    // Give the lanes a moment to claim their work before watching them empty.
    std::thread::sleep(Duration::from_millis(200));
    while refresh.is_busy() && started.elapsed() < Duration::from_secs(120) {
        std::thread::sleep(Duration::from_millis(100));
    }

    println!(
        "synchronized in {:.1}s — {} records at revision {}",
        started.elapsed().as_secs_f64(),
        repository.count().unwrap(),
        repository.revision().unwrap(),
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&repository.health().unwrap()).unwrap()
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&repository.quotas().unwrap()).unwrap()
    );
}
