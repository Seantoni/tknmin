//! The historical baseline: how much the user typically burns, and how much
//! of the day they are actually active.
//!
//! Two outputs come from the same hourly rollups of `display_total`:
//!
//! - a **median per local hour-of-day**, which lets a projection say "2.4×
//!   your usual Tuesday afternoon" instead of an unanchored "fast";
//! - a **duty cycle** — the fraction of hours that see any activity — which a
//!   long-window projection needs before it can extrapolate a short burst.
//!   Nobody works a 31-day billing cycle continuously, so a pace measured over
//!   30 minutes is meaningless for it until this is known.
//!
//! Everything here is derived from [`UsageRecord`]s, computed on demand. The
//! timezone is injected as an offset in *minutes* so the module stays pure and
//! testable; activity is diurnal in local time, and bucketing by UTC hour would
//! smear the pattern it exists to measure. Minutes rather than hours because
//! India (+05:30) and Nepal (+05:45) are not whole-hour zones, and truncating
//! them would shift every one of their slots.

use std::collections::HashMap;

use chrono::{DateTime, Datelike, Timelike, Utc};
use serde::{Deserialize, Serialize};

use super::record::UsageRecord;
use super::source::SourceApp;

/// How many trailing days the baseline looks back over.
pub const BASELINE_DAYS: u32 = 30;

/// The trailing window a "current burn" is measured over. Long enough to span
/// several agent turns, short enough that a burst still shows. Shared with the
/// hour-of-day the burn is compared against, which is the hour that window
/// mostly covers — not necessarily the hour it ends in.
pub const RATE_WINDOW_MINUTES: u32 = 60;

/// The minimum distinct days a slot must have been active on before its
/// median is allowed to say anything. A handful of busy hours is not a habit.
pub const MIN_SLOT_DAYS: usize = 3;

/// The minimum active hours in the whole window before a duty cycle is
/// trusted. With fewer than this the fraction is noise.
pub const MIN_ACTIVE_HOURS: usize = 24;

/// One source's activity pattern.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceBaseline {
    pub source_app: SourceApp,
    /// Median tokens burned in one local hour-of-day (0–23) — the median over
    /// the *days* that slot was active, not over individual events. `None`
    /// where the slot has too little history to be meaningful. Tokens per
    /// hour, so it can be compared to a measured burn directly.
    pub median_per_hour: [Option<u64>; 24],
    /// Distinct days each slot was active on, kept so callers can judge
    /// confidence rather than trusting the median blindly.
    pub slot_days: [usize; 24],
    /// Active hours per elapsed hour in the window, as an integer percent
    /// (0–100). `None` when there is too little history to say.
    pub duty_cycle_percent: Option<u32>,
}

/// How a measured pace compares to the user's own baseline for this hour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PaceVsBaseline {
    /// No trustworthy baseline exists yet.
    NoBaseline,
    Below,
    Typical,
    Above,
    FarAbove,
}

/// A source's recent burn, in tokens per hour, over a short trailing window.
/// Paired with [`SourceBaseline::median_per_hour`] (same units) so a current
/// pace can be compared to the source's own history at this local hour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenRate {
    pub source_app: SourceApp,
    pub tokens_per_hour: u64,
}

/// Measure each source's current burn from its records over the trailing
/// `window_minutes`, as whole tokens per hour. `None`-producing sources are
/// simply absent from the result.
pub fn recent_token_rates(
    records: &[UsageRecord],
    window_minutes: u32,
    now: DateTime<Utc>,
) -> Vec<TokenRate> {
    let from = now - chrono::Duration::minutes(i64::from(window_minutes));
    let mut totals: Vec<(SourceApp, u64)> = Vec::new();
    for record in records {
        let Some(at) = record.event_timestamp_utc else {
            continue;
        };
        if at < from {
            continue;
        }
        let Some(total) = record.display_total else {
            continue;
        };
        match totals
            .iter_mut()
            .find(|(source, _)| *source == record.source_app)
        {
            Some((_, sum)) => *sum += total.tokens,
            None => totals.push((record.source_app, total.tokens)),
        }
    }
    totals
        .into_iter()
        .map(|(source_app, tokens)| TokenRate {
            source_app,
            tokens_per_hour: tokens * 60 / u64::from(window_minutes.max(1)),
        })
        .collect()
}

/// Build one source's baseline from its records over the trailing
/// `BASELINE_DAYS` days, bucketing `display_total` into local hours shifted by
/// `tz_offset_minutes` from UTC.
pub fn build_baseline(
    source_app: SourceApp,
    records: &[UsageRecord],
    tz_offset_minutes: i64,
    now: DateTime<Utc>,
) -> SourceBaseline {
    let from = now - chrono::Duration::days(i64::from(BASELINE_DAYS));

    // Roll every record up into the (local day, local hour) cell it landed in
    // *before* taking any median. The median has to be over whole hours —
    // "what this source burns in an ordinary 14:00" — because that is the
    // quantity a measured burn is in. A median over individual events would
    // instead measure event size, which is tokens per *record*: a different
    // quantity in different units, and dividing a rate by it yields records per
    // hour rather than a multiple of usual.
    let mut per_cell: HashMap<(i32, usize), u64> = HashMap::new();
    let mut earliest: Option<DateTime<Utc>> = None;

    for record in records {
        if record.source_app != source_app {
            continue;
        }
        let Some(at) = record.event_timestamp_utc else {
            continue;
        };
        if at < from {
            continue;
        }
        let Some(total) = record.display_total else {
            continue;
        };
        let local = at + chrono::Duration::minutes(tz_offset_minutes);
        let cell = (local.num_days_from_ce(), local.hour() as usize);
        *per_cell.entry(cell).or_default() += total.tokens;
        earliest = Some(earliest.map_or(at, |held: DateTime<Utc>| held.min(at)));
    }

    // One entry per active hour: the median is then over days, so one frantic
    // afternoon does not outweigh a dozen ordinary ones.
    let mut per_slot: [Vec<u64>; 24] = Default::default();
    let mut slot_days: [usize; 24] = [0; 24];
    for ((_, hour), tokens) in per_cell {
        per_slot[hour].push(tokens);
        slot_days[hour] += 1;
    }
    let active_hours: usize = slot_days.iter().sum();

    let mut median_per_hour: [Option<u64>; 24] = [None; 24];
    for hour in 0..24 {
        if slot_days[hour] >= MIN_SLOT_DAYS {
            median_per_hour[hour] = median(&mut per_slot[hour]);
        }
    }

    let duty_cycle_percent = if active_hours >= MIN_ACTIVE_HOURS {
        // Against the history actually observed, not a flat 30 days. Someone a
        // week into using Tokens works the same fraction of their day as
        // someone a month in, and dividing both by 720 hours would report the
        // newcomer as a quarter as active — understating every user until the
        // window fills, which is exactly when the duty cycle is consulted.
        let observed_hours = earliest
            .map(|earliest| (now - earliest).num_hours())
            .unwrap_or_default()
            .clamp(1, i64::from(BASELINE_DAYS) * 24);
        // Distinct active hours can exceed a truncated span by one at the edge.
        Some((((active_hours as i64 * 100) / observed_hours) as u32).min(100))
    } else {
        None
    };

    SourceBaseline {
        source_app,
        median_per_hour,
        slot_days,
        duty_cycle_percent,
    }
}

/// The median of a set, biased low on an even count so a skewed afternoon
/// does not inflate the "typical" it sits beside. `None` on an empty set —
/// total rather than panicking, so no caller has to hold that invariant.
fn median(values: &mut [u64]) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    Some(values[(values.len() - 1) / 2])
}

/// Compare the source's current burn against what it typically burns in this
/// local hour. Both sides are tokens per hour, so the ratio is a genuine
/// multiple of "usual". Band edges sit wide apart so an ordinary busy spell
/// does not read as anomalous.
pub fn pace_vs_baseline(
    baseline: &SourceBaseline,
    current_tokens_per_hour: Option<u64>,
    local_hour: usize,
) -> PaceVsBaseline {
    let (Some(current), Some(typical)) = (
        current_tokens_per_hour,
        baseline.median_per_hour.get(local_hour).copied().flatten(),
    ) else {
        return PaceVsBaseline::NoBaseline;
    };
    if typical == 0 {
        return PaceVsBaseline::NoBaseline;
    }
    // Hundredths of the typical rate, integer-only.
    let ratio = (current * 100) / typical.max(1);
    if ratio < 75 {
        PaceVsBaseline::Below
    } else if ratio < 150 {
        PaceVsBaseline::Typical
    } else if ratio < 250 {
        PaceVsBaseline::Above
    } else {
        PaceVsBaseline::FarAbove
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        CostInfo, DisplayTotal, SourceProvenance, TokenCounts, TokenField, TotalRule,
    };
    use chrono::TimeZone;

    fn record(source: SourceApp, day: u32, hour: u32, tokens: u64) -> UsageRecord {
        at_minute(source, day, hour, 0, tokens)
    }

    fn at_minute(source: SourceApp, day: u32, hour: u32, minute: u32, tokens: u64) -> UsageRecord {
        at_instant(
            source,
            Utc.with_ymd_and_hms(2026, 7, day, hour, minute, 0).unwrap(),
            tokens,
        )
    }

    fn at_instant(source: SourceApp, event: DateTime<Utc>, tokens: u64) -> UsageRecord {
        UsageRecord {
            normalization_version: 1,
            id: format!("{source}-{event}-{tokens}"),
            raw_timestamp: None,
            event_timestamp_utc: Some(event),
            timestamp_interpretation: crate::domain::TimestampInterpretation::ExplicitOffset,
            source_app: source,
            source_event_id: None,
            dedupe_key: String::new(),
            dedupe_algorithm_version: 1,
            content_hash: String::new(),
            content_hash_version: 1,
            provider: None,
            model: None,
            tokens: TokenCounts {
                input: TokenField::exact(tokens),
                output: TokenField::exact(0),
                ..TokenCounts::default()
            },
            reported_total_tokens: None,
            display_total: Some(DisplayTotal {
                tokens,
                rule: TotalRule::InputPlusOutput,
            }),
            project: None,
            session_id: None,
            cost: CostInfo::not_available(),
            imported_at: event,
            provenance: SourceProvenance {
                adapter_id: "test".into(),
                adapter_version: "1".into(),
                source_ref: None,
            },
        }
    }

    #[test]
    fn median_is_per_local_hour_and_resists_outliers() {
        // Ten ordinary 14:00 hours of 100, one of 10_000 — the median stays
        // at the ordinary value.
        let mut records = Vec::new();
        for day in 1..=10 {
            records.push(record(SourceApp::Codex, day, 14, 100));
        }
        records.push(record(SourceApp::Codex, 11, 14, 10_000));
        let baseline = build_baseline(
            SourceApp::Codex,
            &records,
            0,
            Utc.with_ymd_and_hms(2026, 7, 30, 0, 0, 0).unwrap(),
        );
        assert_eq!(baseline.median_per_hour[14], Some(100));
    }

    #[test]
    fn a_slot_with_too_few_days_reports_nothing() {
        let records = vec![
            record(SourceApp::Codex, 1, 3, 100),
            record(SourceApp::Codex, 2, 3, 100),
        ];
        let baseline = build_baseline(
            SourceApp::Codex,
            &records,
            0,
            Utc.with_ymd_and_hms(2026, 7, 30, 0, 0, 0).unwrap(),
        );
        assert_eq!(baseline.median_per_hour[3], None);
    }

    #[test]
    fn an_hours_records_sum_into_one_hourly_total() {
        // Four records of 250 inside one hour are one 1_000-token hour, not
        // four 250-token observations. Getting this wrong scales the median
        // down by the number of records per hour, and the comparison against a
        // tokens/hour burn up by the same factor.
        let mut records = Vec::new();
        for day in 1..=5 {
            for minute in [0, 15, 30, 45] {
                records.push(at_minute(SourceApp::Codex, day, 14, minute, 250));
            }
        }
        let baseline = build_baseline(
            SourceApp::Codex,
            &records,
            0,
            Utc.with_ymd_and_hms(2026, 7, 30, 0, 0, 0).unwrap(),
        );
        assert_eq!(baseline.median_per_hour[14], Some(1_000));
        assert_eq!(baseline.slot_days[14], 5, "five days, not twenty records");
    }

    #[test]
    fn a_steady_burn_reads_as_typical_against_its_own_history() {
        // The test the unit mismatch could not survive: a user whose current
        // hour burns at exactly their historical rate must read Typical. With
        // a per-record median this said FarAbove for anyone doing more than
        // three records an hour — which is every agent session there is.
        let now = Utc.with_ymd_and_hms(2026, 7, 30, 15, 0, 0).unwrap();
        // Ten records of 1_000 tokens in every hour, 09:00–17:00, for 20 days.
        let mut records = Vec::new();
        let work_an_hour = |records: &mut Vec<UsageRecord>, start: DateTime<Utc>| {
            for n in 0..10 {
                records.push(at_instant(
                    SourceApp::Codex,
                    start + chrono::Duration::minutes(n * 5),
                    1_000,
                ));
            }
        };
        for day_back in 1..=20 {
            for hour in 9..17 {
                let day = (now - chrono::Duration::days(day_back)).date_naive();
                work_an_hour(&mut records, day.and_hms_opt(hour, 0, 0).unwrap().and_utc());
            }
        }
        // The hour just gone burns at exactly the same rate as every other.
        work_an_hour(
            &mut records,
            now.date_naive().and_hms_opt(14, 0, 0).unwrap().and_utc(),
        );

        let baseline = build_baseline(SourceApp::Codex, &records, 0, now);
        assert_eq!(baseline.median_per_hour[14], Some(10_000));

        let rates = recent_token_rates(&records, RATE_WINDOW_MINUTES, now);
        let current = rates
            .iter()
            .find(|rate| rate.source_app == SourceApp::Codex)
            .map(|rate| rate.tokens_per_hour);
        assert_eq!(
            pace_vs_baseline(&baseline, current, 14),
            PaceVsBaseline::Typical
        );
        // Eight hours a day for twenty days, measured against the span
        // actually observed rather than a flat 30 days.
        assert_eq!(baseline.duty_cycle_percent, Some(33));
    }

    #[test]
    fn timezone_shifts_the_local_hour() {
        // 02:00 UTC is 21:00 the previous day at UTC-5.
        let records = vec![
            record(SourceApp::Codex, 1, 2, 100),
            record(SourceApp::Codex, 2, 2, 100),
            record(SourceApp::Codex, 3, 2, 100),
        ];
        let baseline = build_baseline(
            SourceApp::Codex,
            &records,
            -5 * 60,
            Utc.with_ymd_and_hms(2026, 7, 30, 0, 0, 0).unwrap(),
        );
        assert_eq!(baseline.median_per_hour[21], Some(100));
        assert_eq!(baseline.median_per_hour[2], None);
    }

    #[test]
    fn a_half_hour_zone_keeps_its_own_hour() {
        // 04:00 UTC is 09:30 in India (+05:30) — hour 9, not the hour 9 a
        // whole-hour offset would reach by accident. 04:45 UTC is 10:15, which
        // a truncating offset would put in hour 9 with it.
        let records = vec![
            record(SourceApp::Codex, 1, 4, 100),
            record(SourceApp::Codex, 2, 4, 100),
            record(SourceApp::Codex, 3, 4, 100),
            at_minute(SourceApp::Codex, 1, 4, 45, 500),
            at_minute(SourceApp::Codex, 2, 4, 45, 500),
            at_minute(SourceApp::Codex, 3, 4, 45, 500),
        ];
        let baseline = build_baseline(
            SourceApp::Codex,
            &records,
            5 * 60 + 30,
            Utc.with_ymd_and_hms(2026, 7, 30, 0, 0, 0).unwrap(),
        );
        assert_eq!(baseline.median_per_hour[9], Some(100));
        assert_eq!(baseline.median_per_hour[10], Some(500));
    }

    #[test]
    fn duty_cycle_needs_enough_active_hours() {
        let sparse = vec![record(SourceApp::Codex, 1, 10, 100)];
        let baseline = build_baseline(
            SourceApp::Codex,
            &sparse,
            0,
            Utc.with_ymd_and_hms(2026, 7, 30, 0, 0, 0).unwrap(),
        );
        assert_eq!(baseline.duty_cycle_percent, None);
    }

    #[test]
    fn pace_vs_baseline_bands_the_ratio() {
        let mut median_per_hour = [None; 24];
        median_per_hour[9] = Some(1_000);
        let baseline = SourceBaseline {
            source_app: SourceApp::Codex,
            median_per_hour,
            slot_days: [10; 24],
            duty_cycle_percent: Some(20),
        };
        assert_eq!(
            pace_vs_baseline(&baseline, Some(500), 9),
            PaceVsBaseline::Below
        );
        assert_eq!(
            pace_vs_baseline(&baseline, Some(1_000), 9),
            PaceVsBaseline::Typical
        );
        assert_eq!(
            pace_vs_baseline(&baseline, Some(2_000), 9),
            PaceVsBaseline::Above
        );
        assert_eq!(
            pace_vs_baseline(&baseline, Some(4_000), 9),
            PaceVsBaseline::FarAbove
        );
        assert_eq!(
            pace_vs_baseline(&baseline, None, 9),
            PaceVsBaseline::NoBaseline
        );
    }
}
