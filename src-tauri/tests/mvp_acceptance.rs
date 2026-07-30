//! MVP acceptance checks.
//!
//! These exercise the crate the way the Tauri commands do — through
//! `AppState` — so what they assert is what the dashboard receives.

use tokens_lib::domain::{CostTotal, RecentQuery, SummaryQuery, UsageRecord};
use tokens_lib::state::AppState;

fn dashboard_data() -> (tokens_lib::domain::UsageSummary, Vec<UsageRecord>) {
    let state = AppState::with_fake_data();
    let summary = state
        .reader()
        .summary(&SummaryQuery::default())
        .unwrap();
    // The table asks for the same records, so every row is covered below.
    let recent = state
        .reader()
        .recent(&RecentQuery {
            limit: RecentQuery::MAX_LIMIT,
            ..RecentQuery::default()
        })
        .unwrap();
    (summary, recent)
}

/// The MVP checklist's "totals agree with the displayed records" item: the
/// summary cards are recomputed from the rows the table shows.
#[test]
fn totals_agree_with_the_records_on_screen() {
    let (summary, recent) = dashboard_data();

    assert_eq!(summary.totals.record_count, recent.len());

    let sum = |values: Vec<Option<u64>>| -> (u64, usize, usize) {
        let mut total = 0;
        let mut counted = 0;
        let mut unknown = 0;
        for value in values {
            match value {
                Some(value) => {
                    total += value;
                    counted += 1;
                }
                None => unknown += 1,
            }
        }
        (total, counted, unknown)
    };

    let input = sum(recent
        .iter()
        .map(|record| record.tokens.input.countable())
        .collect());
    assert_eq!(
        (
            summary.totals.input.tokens,
            summary.totals.input.counted_records,
            summary.totals.input.unknown_records
        ),
        input
    );

    let output = sum(recent
        .iter()
        .map(|record| record.tokens.output.countable())
        .collect());
    assert_eq!(
        (
            summary.totals.output.tokens,
            summary.totals.output.counted_records,
            summary.totals.output.unknown_records
        ),
        output
    );

    let displayed = sum(recent
        .iter()
        .map(|record| record.display_total.map(|t| t.tokens))
        .collect());
    assert_eq!(
        (
            summary.totals.display_total.tokens,
            summary.totals.display_total.counted_records,
            summary.totals.display_total.unknown_records
        ),
        displayed
    );
}

/// Cost is summed per currency and never mixed, and the count of records
/// without cost matches the rows showing no cost.
#[test]
fn cost_totals_agree_with_the_records_on_screen() {
    let (summary, recent) = dashboard_data();
    let CostTotal {
        by_currency,
        records_without_cost,
    } = summary.totals.cost;

    let priced: Vec<_> = recent
        .iter()
        .filter_map(|record| record.cost.amount.clone())
        .collect();
    assert_eq!(records_without_cost, recent.len() - priced.len());

    for entry in &by_currency {
        let expected = priced
            .iter()
            .filter(|amount| amount.currency == entry.amount.currency)
            .fold(
                None,
                |accumulated: Option<tokens_lib::domain::Money>, amount| match accumulated {
                    Some(running) => Some(running.checked_add(amount).unwrap()),
                    None => Some(amount.clone()),
                },
            )
            .unwrap();
        assert_eq!(entry.amount, expected);
    }
}

/// Breakdown rows must account for every record exactly once, so no usage is
/// dropped from a group and none is double counted.
#[test]
fn breakdowns_account_for_every_record() {
    let (summary, recent) = dashboard_data();

    let by_source: usize = summary
        .by_source
        .iter()
        .map(|group| group.totals.record_count)
        .sum();
    let by_model: usize = summary
        .by_model
        .iter()
        .map(|group| group.totals.record_count)
        .sum();

    assert_eq!(by_source, recent.len());
    assert_eq!(by_model, recent.len());
}

/// "Fake values remain predictable between launches": two independently
/// constructed states produce identical records and identical totals.
#[test]
fn two_launches_show_the_same_usage() {
    let (first_summary, first_recent) = dashboard_data();
    let (second_summary, second_recent) = dashboard_data();

    assert_eq!(first_recent, second_recent);
    assert_eq!(first_summary.totals, second_summary.totals);
    assert_eq!(first_summary.by_source, second_summary.by_source);
    assert_eq!(first_summary.by_model, second_summary.by_model);
}

/// The dashboard distinguishes an empty store from a filter matching nothing,
/// so the empty state has to be reachable.
#[test]
fn an_empty_store_reports_zero_records() {
    let state = AppState::in_memory();
    assert_eq!(state.reader().count().unwrap(), 0);

    let summary = state
        .reader()
        .summary(&SummaryQuery::default())
        .unwrap();
    assert_eq!(summary.totals.record_count, 0);
    assert!(summary.by_source.is_empty());
    assert!(state
        .reader()
        .recent(&RecentQuery::default())
        .unwrap()
        .is_empty());
}
