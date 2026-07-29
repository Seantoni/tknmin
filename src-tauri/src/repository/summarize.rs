//! Filtering, ordering, and aggregation shared by every repository backend.
//!
//! Keeping this logic out of the backends means the SQLite implementation
//! added in a later phase can be checked against the in-memory one.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};

use crate::domain::{
    CostTotal, CurrencyTotal, GroupSummary, RecentQuery, SummaryQuery, UsageRecord, UsageSummary,
    UsageTotals,
};

/// Whether a record passes the query's filters.
///
/// A date-bounded query can only judge records whose timestamp was resolved;
/// undated records are excluded and counted separately rather than being
/// silently kept or silently dropped.
pub fn matches(record: &UsageRecord, query: &SummaryQuery) -> bool {
    if let Some(sources) = &query.sources {
        if !sources.contains(&record.source_app) {
            return false;
        }
    }

    if let Some(models) = &query.models {
        let matched = match &record.model {
            Some(model) => models.iter().any(|candidate| candidate == model),
            None => false,
        };
        if !matched {
            return false;
        }
    }

    if query.has_date_bounds() {
        let Some(event) = record.event_timestamp_utc else {
            return false;
        };
        if query.from.is_some_and(|from| event < from) {
            return false;
        }
        if query.until.is_some_and(|until| event >= until) {
            return false;
        }
    }

    true
}

/// Records the query excludes solely because their timestamp is unresolved.
pub fn undated_excluded<'a>(
    records: impl Iterator<Item = &'a UsageRecord>,
    query: &SummaryQuery,
) -> usize {
    if !query.has_date_bounds() {
        return 0;
    }
    let mut without_dates = query.clone();
    without_dates.from = None;
    without_dates.until = None;
    records
        .filter(|record| record.event_timestamp_utc.is_none() && matches(record, &without_dates))
        .count()
}

/// Sort key for the recent list: newest event first, with undated records last
/// and ties broken by identifier so the order is stable between calls.
pub fn recency_key(record: &UsageRecord) -> (bool, DateTime<Utc>, &str) {
    match record.event_timestamp_utc {
        Some(event) => (false, event, record.id.as_str()),
        None => (true, record.imported_at, record.id.as_str()),
    }
}

/// Apply a recent-records query to an already-filtered, unsorted slice.
pub fn take_recent(records: Vec<&UsageRecord>, query: &RecentQuery) -> Vec<UsageRecord> {
    let mut records = records;
    records.sort_by(|left, right| {
        let (left_undated, left_time, left_id) = recency_key(left);
        let (right_undated, right_time, right_id) = recency_key(right);
        left_undated
            .cmp(&right_undated)
            .then(right_time.cmp(&left_time))
            .then(left_id.cmp(right_id))
    });
    records.into_iter().take(query.effective_limit()).cloned().collect()
}

/// Accumulate one record into a set of totals.
pub fn accumulate(totals: &mut UsageTotals, record: &UsageRecord) {
    totals.record_count += 1;
    totals.input.add(record.tokens.input.countable());
    totals.output.add(record.tokens.output.countable());
    totals.cached_input.add(record.tokens.cached_input.countable());
    totals.reasoning.add(record.tokens.reasoning.countable());
    totals.display_total.add(record.display_total.map(|total| total.tokens));
    accumulate_cost(&mut totals.cost, record);
}

fn accumulate_cost(cost: &mut CostTotal, record: &UsageRecord) {
    let Some(amount) = &record.cost.amount else {
        cost.records_without_cost += 1;
        return;
    };

    match cost.by_currency.iter_mut().find(|entry| entry.amount.currency == amount.currency) {
        Some(entry) => {
            // Only an i64 overflow can fail here, which no realistic token
            // spend reaches; the running total is kept rather than poisoned.
            if let Ok(sum) = entry.amount.checked_add(amount) {
                entry.amount = sum;
                entry.counted_records += 1;
            }
        }
        None => cost
            .by_currency
            .push(CurrencyTotal { amount: amount.clone(), counted_records: 1 }),
    }
}

/// Build the full summary for a set of records that already passed filtering.
pub fn summarize<'a>(
    records: impl Iterator<Item = &'a UsageRecord>,
    generated_at: DateTime<Utc>,
    undated_records_excluded: usize,
) -> UsageSummary {
    let mut totals = UsageTotals::default();
    let mut by_source: BTreeMap<String, GroupSummary> = BTreeMap::new();
    let mut by_model: BTreeMap<String, GroupSummary> = BTreeMap::new();

    for record in records {
        accumulate(&mut totals, record);

        let source = by_source
            .entry(record.source_app.as_str().to_string())
            .or_insert_with(|| GroupSummary {
                key: Some(record.source_app.as_str().to_string()),
                label: record.source_app.display_name().to_string(),
                totals: UsageTotals::default(),
            });
        accumulate(&mut source.totals, record);

        // Records with no model form their own group instead of being dropped
        // from the breakdown.
        let model_key = record.model.clone();
        let group = by_model
            .entry(model_key.clone().unwrap_or_default())
            .or_insert_with(|| GroupSummary {
                key: model_key,
                label: record.model.clone().unwrap_or_else(|| "Unknown model".to_string()),
                totals: UsageTotals::default(),
            });
        accumulate(&mut group.totals, record);
    }

    UsageSummary {
        generated_at,
        totals,
        by_source: sorted_groups(by_source),
        by_model: sorted_groups(by_model),
        undated_records_excluded,
    }
}

/// Largest total first, then alphabetically, so the order never depends on
/// insertion sequence.
fn sorted_groups(groups: BTreeMap<String, GroupSummary>) -> Vec<GroupSummary> {
    let mut groups: Vec<GroupSummary> = groups.into_values().collect();
    groups.sort_by(|left, right| {
        right
            .totals
            .display_total
            .tokens
            .cmp(&left.totals.display_total.tokens)
            .then(left.label.cmp(&right.label))
    });
    groups
}
