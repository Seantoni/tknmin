//! Run one real log refresh against this machine and print the report.
//!
//! Manual check for the adapters and the import pipeline:
//!
//! ```sh
//! cargo run --example import_logs
//! ```

use tokens_lib::refresh;
use tokens_lib::repository::{InMemoryUsageRepository, UsageRepository};

fn main() {
    let repository = InMemoryUsageRepository::new();
    let started = std::time::Instant::now();
    let report = refresh::refresh_all(&repository);
    let elapsed = started.elapsed();

    println!(
        "imported in {:.1}s — {} records stored",
        elapsed.as_secs_f64(),
        repository.count().unwrap()
    );
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
}
