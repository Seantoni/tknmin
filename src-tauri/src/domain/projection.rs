//! Projection — carrying a confirmed allowance reading forward.
//!
//! Claude Code does not publish its allowance percentages continuously. It
//! caches them in `~/.claude.json` and refreshes that cache on its own terms —
//! measured at roughly seven to fourteen minutes apart while a session is
//! actively making requests, and not at all while it is idle. The adapter reads
//! that cache honestly ([`crate::adapters::claude_code`]), which means the
//! percentage on screen can be minutes behind the truth during exactly the
//! stretch it matters: a fast burst.
//!
//! The token records, by contrast, are current to seconds. This module closes
//! the gap by learning the exchange rate between the two:
//!
//! ```text
//! projected_tenths = confirmed_tenths + units_spent_since * tenths_per_unit
//! ```
//!
//! `tenths_per_unit` is not assumed, it is **fitted** from the source's own
//! history. Confirmed readings are walked into non-overlapping spans, each
//! wide enough that the movement across it is signal rather than rounding, and
//! each bracketing a period whose token spend is known exactly — so every span
//! is one observation of the rate. The fit improves as the app runs, and — the
//! property that makes this safe — every new confirmed reading also
//! *re-anchors* the projection, so error cannot accumulate across anchors.
//!
//! Three rules keep a derived number from being passed off as a measured one:
//!
//! - a projection is only offered when the fit is backed by enough pairs and
//!   its residual spread is small enough to mean anything;
//! - a projection spanning longer than [`MAX_PROJECTION_MINUTES`] is refused,
//!   because open-loop error grows with the span and no anchor has arrived to
//!   bound it;
//! - the confirmed reading and the instant it was confirmed travel with every
//!   projection, so the interface can always show what is measured beside what
//!   is inferred.
//!
//! Like `pace.rs` and `notifications.rs`, this module is pure: no I/O, no
//! clock of its own, `now` injected.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::pace::QuotaSample;
use super::quota::{same_window_instance, UsageQuota};
use super::record::UsageRecord;
use super::source::SourceApp;

/// The fixed-point scale for the fitted rate: tenths of a percent per 10^12
/// cost units. Chosen so a realistic rate lands in the hundreds of thousands —
/// fine enough that the quantisation is invisible, small enough that a month of
/// spend multiplied by it stays far inside `u64`.
const RATE_SCALE: u64 = 1_000_000_000_000;

/// How many usable spans a fit needs before it is allowed to say anything.
/// Two spans cannot disagree, so they cannot reveal that the model is wrong.
const MIN_PAIRS: usize = 3;

/// The smallest movement a span must accumulate before it can be fitted. The
/// source reports tenths, so a two-point move carries a quantisation error of
/// roughly one part in twenty; anything smaller is mostly rounding.
///
/// This is a threshold on the span, not on adjacent readings: a span keeps
/// extending until it clears this, which is what lets a long window — where
/// each stored reading moves by a tenth or two — ever be fitted at all.
const MIN_PAIR_TENTHS: u16 = 20;

/// The widest spread a fit may show, as a percentage of its own median, before
/// it is treated as not having found a stable rate. A shifting model mix or an
/// allowance being spent somewhere this app cannot see both show up here.
const MAX_RESIDUAL_PERCENT: u32 = 35;

/// The longest a projection may run ahead of its confirmed reading. Beyond
/// this the open-loop error is no longer bounded by anything, and a stale
/// measured number is more honest than a confident derived one.
const MAX_PROJECTION_MINUTES: u32 = 120;

/// Per-model weights that turn a token count into a comparable unit of
/// allowance spend.
///
/// Allowance is not consumed per token — an Opus output token costs many times
/// a Haiku input one, and a cache read costs a fraction of a fresh input. The
/// weights below are proportional to published relative pricing, expressed in
/// hundredths of a Haiku input token so every value stays an integer.
///
/// The overall level does not matter: the fitted rate absorbs any constant
/// factor. Only the *ratios* do, which is why they are stated per model rather
/// than folded into one number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ModelWeights {
    input: u64,
    output: u64,
    cached_input: u64,
}

/// Opus. `input` covers cache creation too — the adapter sums the two, and they
/// are within a quarter of each other in price.
const OPUS: ModelWeights = ModelWeights {
    input: 1_500,
    output: 7_500,
    cached_input: 30,
};

const SONNET: ModelWeights = ModelWeights {
    input: 300,
    output: 1_500,
    cached_input: 6,
};

const HAIKU: ModelWeights = ModelWeights {
    input: 100,
    output: 500,
    cached_input: 2,
};

/// Which weights a model name earns. An unrecognised name gets the middle tier
/// rather than nothing: excluding its tokens would silently understate spend,
/// and understating is the dangerous direction for a warning.
fn weights_for(model: Option<&str>) -> ModelWeights {
    let Some(model) = model else {
        return SONNET;
    };
    let model = model.to_ascii_lowercase();
    if model.contains("opus") {
        OPUS
    } else if model.contains("haiku") {
        HAIKU
    } else {
        SONNET
    }
}

/// One record's contribution to allowance spend, in weighted cost units.
///
/// Unknown categories contribute nothing, which is the same treatment
/// [`crate::domain::TokenField::countable`] gives them everywhere else: an
/// absent count is not a zero count, but it also cannot be summed.
pub fn cost_units(record: &UsageRecord) -> u64 {
    let weights = weights_for(record.model.as_deref());
    let part = |field: super::tokens::TokenField, weight: u64| {
        field.countable().unwrap_or(0).saturating_mul(weight)
    };
    part(record.tokens.input, weights.input)
        .saturating_add(part(record.tokens.output, weights.output))
        .saturating_add(part(record.tokens.cached_input, weights.cached_input))
}

/// Cost units spent by one source in `(from, to]`.
fn units_between(
    records: &[UsageRecord],
    source_app: SourceApp,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> u64 {
    records
        .iter()
        .filter(|record| record.source_app == source_app)
        .filter(|record| {
            record
                .event_timestamp_utc
                .is_some_and(|at| at > from && at <= to)
        })
        .fold(0u64, |sum, record| sum.saturating_add(cost_units(record)))
}

/// The learned exchange rate between token spend and one allowance window.
///
/// Fitted per window, not per source: the same tokens move a five-hour session
/// window far further than the week that contains it, so the two rates differ
/// by orders of magnitude and share nothing but their inputs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaCalibration {
    pub source_app: SourceApp,
    pub label: Option<String>,
    pub window_minutes: u32,
    /// Tenths of a percent per [`RATE_SCALE`] cost units.
    pub tenths_per_tera_unit: u64,
    /// How many confirmed-reading spans the fit rests on.
    pub pairs: usize,
    /// The widest deviation from the median rate, as an integer percent of it.
    /// Small means the model explains the data; large means something else is
    /// moving the allowance.
    pub residual_percent: u32,
}

impl QuotaCalibration {
    /// Whether this fit has earned the right to move a number on screen.
    pub fn is_trustworthy(&self) -> bool {
        self.pairs >= MIN_PAIRS && self.residual_percent <= MAX_RESIDUAL_PERCENT
    }

    /// Tenths of a percent that `units` of spend represent, at this rate.
    ///
    /// Rounded, not truncated, and in `u128` throughout: the rate was already
    /// rounded once when it was fitted, and a second floor here would compound
    /// into a systematic under-report — the direction that hides a burst.
    pub fn tenths_for(&self, units: u64) -> u16 {
        let scaled = u128::from(units).saturating_mul(u128::from(self.tenths_per_tera_unit));
        let tenths = (scaled + u128::from(RATE_SCALE) / 2) / u128::from(RATE_SCALE);
        tenths.min(1_000) as u16
    }
}

/// A confirmed reading carried forward to the present.
///
/// Both numbers are kept. The interface needs the confirmed one to stay honest
/// about what the source actually said, and the projected one to be useful.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaProjection {
    /// Identity, matching `quota_key` exactly so a row joins to its quota.
    pub source_app: SourceApp,
    pub label: Option<String>,
    pub window_minutes: u32,
    /// What the source last actually reported.
    pub confirmed_percent_tenths: u16,
    /// When it reported it.
    pub confirmed_at: DateTime<Utc>,
    /// The confirmed reading plus the spend since — what the window is believed
    /// to be at right now.
    pub projected_percent_tenths: u16,
    /// The derived part alone, so the interface can show the increment.
    pub added_percent_tenths: u16,
    /// The instant projected to, which is the caller's `now`.
    pub projected_at: DateTime<Utc>,
    /// Minutes of open-loop projection. Small is good; the interface can use
    /// this to decide how firmly to state the result.
    pub span_minutes: u32,
    /// The fit behind it, carried so confidence is inspectable rather than
    /// implicit.
    pub pairs: usize,
    pub residual_percent: u32,
}

/// Fit one rate per live window, from the confirmed readings already stored.
///
/// Only windows present in `quotas` are fitted: a rate nothing will consume is
/// wasted work, and the quota set is what the snapshot is about to render.
pub fn fit_calibrations(
    quotas: &[UsageQuota],
    samples: &[QuotaSample],
    records: &[UsageRecord],
) -> Vec<QuotaCalibration> {
    quotas
        .iter()
        .filter_map(|quota| fit_window(quota, samples, records))
        .collect()
}

fn fit_window(
    quota: &UsageQuota,
    samples: &[QuotaSample],
    records: &[UsageRecord],
) -> Option<QuotaCalibration> {
    let mut window: Vec<&QuotaSample> = samples
        .iter()
        .filter(|sample| {
            sample.source_app == quota.source_app
                && sample.label == quota.label
                && sample.window_minutes == quota.window_minutes
        })
        .collect();
    window.sort_by_key(|sample| sample.observed_at);

    // Each span inside one window instance is an independent observation of
    // the rate. A span is anchored at a sample and extended until the reading
    // has moved far enough to mean something, then the next span is anchored
    // where it ended — so spans never overlap and every reading is used.
    //
    // Extending rather than only comparing adjacent samples is what makes this
    // work on a long window. A sample is stored on every tenth of a percent
    // that moves, so on a monthly billing cycle consecutive readings differ by
    // one or two tenths and no adjacent pair ever clears the threshold; the
    // rate could never be fitted at all, and the window that most needs
    // carrying forward is the one that never gets it. Spans spanning a reset
    // are still refused: the used figure drops to zero across one, and
    // differencing it invents a negative.
    let mut rates: Vec<u64> = Vec::new();
    let mut anchor = 0;
    for index in 1..window.len() {
        let (left, right) = (window[anchor], window[index]);
        if !same_window_instance(left.resets_at, right.resets_at) {
            // The reset landed between these two. Nothing before it can be
            // differenced against anything after, so start again here.
            anchor = index;
            continue;
        }
        let moved = right
            .used_percent_tenths
            .saturating_sub(left.used_percent_tenths);
        if moved < MIN_PAIR_TENTHS {
            continue;
        }
        anchor = index;
        let units = units_between(
            records,
            quota.source_app,
            left.observed_at,
            right.observed_at,
        );
        if units == 0 {
            // The allowance moved with no local spend to explain it — another
            // surface drawing on the same account, or history older than the
            // records kept. Either way this span teaches nothing.
            continue;
        }
        // Rounded so the rate a span yields, applied back to that span's own
        // spend, reproduces the movement it was fitted from.
        let numerator = u64::from(moved).saturating_mul(RATE_SCALE);
        rates.push((numerator + units / 2) / units);
    }

    if rates.len() < MIN_PAIRS {
        return None;
    }

    rates.sort_unstable();
    let median = rates[rates.len() / 2];
    if median == 0 {
        return None;
    }
    // Spread rather than variance: the question is not how noisy the fit is on
    // average but how wrong one projection could be, and the worst pair
    // answers that directly.
    let worst = rates
        .iter()
        .map(|rate| rate.abs_diff(median))
        .max()
        .unwrap_or(0);

    Some(QuotaCalibration {
        source_app: quota.source_app,
        label: quota.label.clone(),
        window_minutes: quota.window_minutes,
        tenths_per_tera_unit: median,
        pairs: rates.len(),
        residual_percent: (worst.saturating_mul(100) / median).min(u64::from(u32::MAX)) as u32,
    })
}

/// Carry every window that has a trustworthy rate forward to `now`.
///
/// Windows without one are simply absent from the result — the caller keeps
/// showing the confirmed reading, which is what it was already doing.
pub fn project_quotas(
    quotas: &[UsageQuota],
    calibrations: &[QuotaCalibration],
    records: &[UsageRecord],
    now: DateTime<Utc>,
) -> Vec<QuotaProjection> {
    quotas
        .iter()
        .filter_map(|quota| project_window(quota, calibrations, records, now))
        .collect()
}

fn project_window(
    quota: &UsageQuota,
    calibrations: &[QuotaCalibration],
    records: &[UsageRecord],
    now: DateTime<Utc>,
) -> Option<QuotaProjection> {
    let calibration = calibrations.iter().find(|fit| {
        fit.source_app == quota.source_app
            && fit.label == quota.label
            && fit.window_minutes == quota.window_minutes
    })?;
    if !calibration.is_trustworthy() {
        return None;
    }

    let span_minutes = (now - quota.observed_at).num_minutes();
    if span_minutes <= 0 || span_minutes > i64::from(MAX_PROJECTION_MINUTES) {
        return None;
    }
    let span_minutes = span_minutes as u32;

    let units = units_between(records, quota.source_app, quota.observed_at, now);
    let added = calibration.tenths_for(units);
    if added == 0 {
        // Idle since the reading. The confirmed number is already correct, and
        // republishing it as a projection would only add a caveat to a fact.
        return None;
    }

    Some(QuotaProjection {
        source_app: quota.source_app,
        label: quota.label.clone(),
        window_minutes: quota.window_minutes,
        confirmed_percent_tenths: quota.used_percent_tenths,
        confirmed_at: quota.observed_at,
        projected_percent_tenths: quota.used_percent_tenths.saturating_add(added).min(1_000),
        added_percent_tenths: added,
        projected_at: now,
        span_minutes,
        pairs: calibration.pairs,
        residual_percent: calibration.residual_percent,
    })
}

/// The quota set as it is believed to stand *now*, for anything that computes
/// on it rather than displaying it.
///
/// Pace asks "will this outlast the window", and answering it from a reading
/// ten minutes stale understates the burn by exactly the ten minutes that
/// matter most. Projected rows carry `observed_at` forward too: the value is
/// current as of `projected_at`, and pretending otherwise would have the
/// staleness gate in `pace.rs` discard the very rows this module exists to
/// refresh.
///
/// Display paths must keep using the original quotas plus the projections
/// beside them, so a derived number is never rendered as a reported one.
pub fn apply_projections(
    quotas: &[UsageQuota],
    projections: &[QuotaProjection],
) -> Vec<UsageQuota> {
    quotas
        .iter()
        .map(|quota| {
            let found = projections.iter().find(|projection| {
                projection.source_app == quota.source_app
                    && projection.label == quota.label
                    && projection.window_minutes == quota.window_minutes
            });
            match found {
                Some(projection) => UsageQuota {
                    used_percent_tenths: projection.projected_percent_tenths,
                    observed_at: projection.projected_at,
                    ..quota.clone()
                },
                None => quota.clone(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        CostInfo, DisplayTotal, SourceProvenance, TimestampInterpretation, TokenCounts, TokenField,
        TotalRule,
    };
    use chrono::TimeZone;

    fn at(minute: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 30, 12, 0, 0).unwrap() + chrono::Duration::minutes(minute)
    }

    fn record(source: SourceApp, minute: i64, model: &str, input: u64, output: u64) -> UsageRecord {
        UsageRecord {
            normalization_version: 1,
            id: format!("{source}-{minute}-{input}-{output}"),
            raw_timestamp: None,
            event_timestamp_utc: Some(at(minute)),
            timestamp_interpretation: TimestampInterpretation::ExplicitOffset,
            source_app: source,
            source_event_id: None,
            dedupe_key: String::new(),
            dedupe_algorithm_version: 1,
            content_hash: String::new(),
            content_hash_version: 1,
            provider: None,
            model: Some(model.to_string()),
            tokens: TokenCounts {
                input: TokenField::exact(input),
                output: TokenField::exact(output),
                cached_input: TokenField::exact(0),
                reasoning: TokenField::unknown(),
            },
            reported_total_tokens: None,
            display_total: Some(DisplayTotal {
                tokens: input + output,
                rule: TotalRule::InputPlusOutput,
            }),
            project: None,
            session_id: None,
            cost: CostInfo::not_available(),
            imported_at: at(minute),
            provenance: SourceProvenance {
                adapter_id: "test".into(),
                adapter_version: "1".into(),
                source_ref: None,
            },
        }
    }

    fn sample(minute: i64, used: u16) -> QuotaSample {
        QuotaSample {
            source_app: SourceApp::ClaudeCode,
            label: None,
            window_minutes: 300,
            used_percent_tenths: used,
            resets_at: Some(at(300)),
            observed_at: at(minute),
        }
    }

    fn quota(used: u16, observed_minute: i64) -> UsageQuota {
        UsageQuota {
            source_app: SourceApp::ClaudeCode,
            label: None,
            window_minutes: 300,
            used_percent_tenths: used,
            resets_at: Some(at(300)),
            observed_at: at(observed_minute),
        }
    }

    /// Four readings ten minutes apart, each preceded by the same spend, so
    /// the rate is identical across every pair.
    fn steady_history() -> (Vec<QuotaSample>, Vec<UsageRecord>) {
        let samples = vec![
            sample(0, 0),
            sample(10, 100),
            sample(20, 200),
            sample(30, 300),
        ];
        let mut records = Vec::new();
        for step in 0..3 {
            // 100 tenths per 10 minutes, spent as one Opus call.
            records.push(record(
                SourceApp::ClaudeCode,
                step * 10 + 5,
                "claude-opus-5",
                100_000,
                10_000,
            ));
        }
        (samples, records)
    }

    #[test]
    fn a_steady_history_fits_a_rate_with_no_spread() {
        let (samples, records) = steady_history();
        let fits = fit_calibrations(&[quota(300, 30)], &samples, &records);

        assert_eq!(fits.len(), 1);
        assert_eq!(fits[0].pairs, 3);
        assert_eq!(fits[0].residual_percent, 0);
        assert!(fits[0].is_trustworthy());
        // 100 tenths bought 100_000*1500 + 10_000*7500 = 225_000_000 units.
        assert_eq!(fits[0].tenths_per_tera_unit, 100 * RATE_SCALE / 225_000_000);
    }

    #[test]
    fn the_fitted_rate_reproduces_the_spend_it_was_fitted_from() {
        let (samples, records) = steady_history();
        let fits = fit_calibrations(&[quota(300, 30)], &samples, &records);
        // The same spend that moved the window 100 tenths must project 100.
        let units = 100_000 * OPUS.input + 10_000 * OPUS.output;
        assert_eq!(fits[0].tenths_for(units), 100);
    }

    #[test]
    fn too_few_pairs_produce_no_fit() {
        let samples = vec![sample(0, 0), sample(10, 100)];
        let records = vec![record(
            SourceApp::ClaudeCode,
            5,
            "claude-opus-5",
            100_000,
            10_000,
        )];
        assert!(fit_calibrations(&[quota(100, 10)], &samples, &records).is_empty());
    }

    /// Cursor's monthly billing pool.
    const BILLING_MONTH: u32 = 30 * 24 * 60;

    fn monthly_sample(minute: i64, used: u16) -> QuotaSample {
        QuotaSample {
            source_app: SourceApp::Cursor,
            label: Some("Cursor Models".to_string()),
            window_minutes: BILLING_MONTH,
            used_percent_tenths: used,
            resets_at: Some(at(i64::from(BILLING_MONTH))),
            observed_at: at(minute),
        }
    }

    fn monthly_quota(used: u16, observed_minute: i64) -> UsageQuota {
        UsageQuota {
            source_app: SourceApp::Cursor,
            label: Some("Cursor Models".to_string()),
            window_minutes: BILLING_MONTH,
            used_percent_tenths: used,
            resets_at: Some(at(i64::from(BILLING_MONTH))),
            observed_at: at(observed_minute),
        }
    }

    #[test]
    fn a_long_window_sampled_finely_still_fits_a_rate() {
        // A month-long pool moves half a percentage point between stored
        // readings — below what a single comparison can tell from rounding.
        // Comparing only neighbours, every one of these is discarded and the
        // window can never be fitted however long the app runs, which is
        // exactly the window that most needs carrying forward.
        let mut samples = Vec::new();
        let mut records = Vec::new();
        for step in 0..=12i64 {
            samples.push(monthly_sample(step * 2, (step * 5) as u16));
            if step > 0 {
                records.push(record(
                    SourceApp::Cursor,
                    step * 2 - 1,
                    "claude-4.6-opus",
                    100_000,
                    10_000,
                ));
            }
        }

        let fits = fit_calibrations(&[monthly_quota(60, 24)], &samples, &records);
        assert_eq!(fits.len(), 1, "the monthly window earns a fit");
        // Three spans of four readings each: every span accumulates the twenty
        // tenths no single step ever reaches.
        assert_eq!(fits[0].pairs, 3);
        assert_eq!(fits[0].residual_percent, 0);
        assert!(fits[0].is_trustworthy());
    }

    #[test]
    fn spans_do_not_overlap_so_spend_is_counted_once() {
        // Readings ten tenths apart: spans close every second one. Were they
        // to overlap, the same records would be counted twice and the fitted
        // rate would come out half what the data says.
        let samples: Vec<QuotaSample> = (0..=6i64)
            .map(|step| monthly_sample(step * 10, (step * 10) as u16))
            .collect();
        let records: Vec<UsageRecord> = (1..=6i64)
            .map(|step| record(SourceApp::Cursor, step * 10 - 5, "claude-4.6-opus", 1_000, 100))
            .collect();

        let fits = fit_calibrations(&[monthly_quota(60, 60)], &samples, &records);
        assert_eq!(fits.len(), 1);
        assert_eq!(fits[0].pairs, 3);
        // Twenty tenths per span, bought by two records' worth of spend.
        let units = 2 * (1_000 * OPUS.input + 100 * OPUS.output);
        assert_eq!(fits[0].tenths_for(units), 20);
    }

    #[test]
    fn pairs_spanning_a_reset_are_not_differenced() {
        // The window rolls between the second and third reading: used drops
        // back to zero, and treating that as a delta would invent a negative.
        let mut samples = steady_history().0;
        samples[2].resets_at = Some(at(600));
        samples[2].used_percent_tenths = 0;
        samples[3].resets_at = Some(at(600));
        samples[3].used_percent_tenths = 100;
        let records = steady_history().1;

        // Only the first pair and the last survive — two, below the minimum.
        assert!(fit_calibrations(&[quota(100, 30)], &samples, &records).is_empty());
    }

    #[test]
    fn a_pair_with_no_local_spend_teaches_nothing() {
        // The allowance moved but this app saw no tokens: another surface on
        // the same account. Including it would fit an infinite rate.
        let (samples, mut records) = steady_history();
        records.retain(|record| record.event_timestamp_utc != Some(at(15)));
        let fits = fit_calibrations(&[quota(300, 30)], &samples, &records);
        assert!(fits.is_empty(), "a pair with no spend must be dropped");
    }

    #[test]
    fn a_scattered_rate_is_refused_rather_than_averaged() {
        let samples = vec![
            sample(0, 0),
            sample(10, 100),
            sample(20, 200),
            sample(30, 300),
        ];
        // Wildly different spend behind identical movements: the model does
        // not explain this data and must not be trusted to project it.
        let records = vec![
            record(SourceApp::ClaudeCode, 5, "claude-opus-5", 10_000, 1_000),
            record(SourceApp::ClaudeCode, 15, "claude-opus-5", 100_000, 10_000),
            record(SourceApp::ClaudeCode, 25, "claude-opus-5", 400_000, 40_000),
        ];
        let fits = fit_calibrations(&[quota(300, 30)], &samples, &records);
        assert_eq!(fits.len(), 1);
        assert!(fits[0].residual_percent > MAX_RESIDUAL_PERCENT);
        assert!(!fits[0].is_trustworthy());
        // And nothing is projected from it.
        assert!(project_quotas(&[quota(300, 30)], &fits, &records, at(35)).is_empty());
    }

    #[test]
    fn spend_since_the_reading_moves_the_projection() {
        let (samples, mut records) = steady_history();
        let fits = fit_calibrations(&[quota(300, 30)], &samples, &records);
        // Half the usual ten-minute spend, five minutes after the reading.
        records.push(record(
            SourceApp::ClaudeCode,
            35,
            "claude-opus-5",
            50_000,
            5_000,
        ));

        let projections = project_quotas(&[quota(300, 30)], &fits, &records, at(40));
        assert_eq!(projections.len(), 1);
        let projected = &projections[0];
        assert_eq!(projected.confirmed_percent_tenths, 300);
        assert_eq!(projected.added_percent_tenths, 50);
        assert_eq!(projected.projected_percent_tenths, 350);
        assert_eq!(projected.span_minutes, 10);
        assert_eq!(projected.confirmed_at, at(30));
        assert_eq!(projected.projected_at, at(40));
    }

    #[test]
    fn an_idle_stretch_projects_nothing() {
        let (samples, records) = steady_history();
        let fits = fit_calibrations(&[quota(300, 30)], &samples, &records);
        // No records after the reading: the confirmed value is still the truth.
        assert!(project_quotas(&[quota(300, 30)], &fits, &records, at(40)).is_empty());
    }

    #[test]
    fn a_projection_never_runs_further_than_its_leash() {
        let (samples, mut records) = steady_history();
        let fits = fit_calibrations(&[quota(300, 30)], &samples, &records);
        records.push(record(
            SourceApp::ClaudeCode,
            40,
            "claude-opus-5",
            100_000,
            10_000,
        ));

        let inside = at(30 + i64::from(MAX_PROJECTION_MINUTES));
        assert_eq!(
            project_quotas(&[quota(300, 30)], &fits, &records, inside).len(),
            1
        );
        let beyond = inside + chrono::Duration::minutes(1);
        assert!(project_quotas(&[quota(300, 30)], &fits, &records, beyond).is_empty());
    }

    #[test]
    fn a_projection_cannot_exceed_a_full_window() {
        let (samples, mut records) = steady_history();
        let fits = fit_calibrations(&[quota(950, 30)], &samples, &records);
        records.push(record(
            SourceApp::ClaudeCode,
            35,
            "claude-opus-5",
            10_000_000,
            1_000_000,
        ));

        let projections = project_quotas(&[quota(950, 30)], &fits, &records, at(40));
        assert_eq!(projections[0].projected_percent_tenths, 1_000);
    }

    #[test]
    fn applying_a_projection_advances_both_the_value_and_its_clock() {
        let (samples, mut records) = steady_history();
        let fits = fit_calibrations(&[quota(300, 30)], &samples, &records);
        records.push(record(
            SourceApp::ClaudeCode,
            35,
            "claude-opus-5",
            50_000,
            5_000,
        ));
        let quotas = vec![quota(300, 30)];
        let projections = project_quotas(&quotas, &fits, &records, at(40));

        let applied = apply_projections(&quotas, &projections);
        assert_eq!(applied[0].used_percent_tenths, 350);
        // The clock moves with the value: a projected row is current as of the
        // instant it was projected to, and pace's staleness gate must see that.
        assert_eq!(applied[0].observed_at, at(40));
        // Identity and the window itself are untouched.
        assert_eq!(applied[0].resets_at, quotas[0].resets_at);
        assert_eq!(applied[0].window_minutes, 300);
    }

    #[test]
    fn a_window_without_a_projection_passes_through_unchanged() {
        let quotas = vec![quota(300, 30)];
        let applied = apply_projections(&quotas, &[]);
        assert_eq!(applied, quotas);
    }

    #[test]
    fn models_are_weighted_apart_and_cache_reads_count_little() {
        let opus = record(SourceApp::Codex, 0, "claude-opus-5", 1_000, 0);
        let sonnet = record(SourceApp::Codex, 0, "claude-sonnet-5", 1_000, 0);
        let haiku = record(SourceApp::Codex, 0, "claude-haiku-4-5", 1_000, 0);
        assert_eq!(cost_units(&opus), 1_000 * 1_500);
        assert_eq!(cost_units(&sonnet), 1_000 * 300);
        assert_eq!(cost_units(&haiku), 1_000 * 100);

        // Output dominates input at the same model.
        let output = record(SourceApp::Codex, 0, "claude-opus-5", 0, 1_000);
        assert!(cost_units(&output) > cost_units(&opus));

        // An unrecognised name gets the middle tier, never zero: understating
        // spend is the dangerous direction for a warning.
        let unknown = record(SourceApp::Codex, 0, "some-future-model", 1_000, 0);
        assert_eq!(cost_units(&unknown), 1_000 * 300);
    }

    #[test]
    fn unknown_token_counts_contribute_nothing() {
        let mut record = record(SourceApp::Codex, 0, "claude-opus-5", 1_000, 500);
        record.tokens.output = TokenField::unknown();
        assert_eq!(cost_units(&record), 1_000 * OPUS.input);
    }

    #[test]
    fn cache_reads_are_counted_but_cheaply() {
        let mut record = record(SourceApp::Codex, 0, "claude-opus-5", 0, 0);
        record.tokens.cached_input = TokenField::exact(1_000_000);
        // Fifty times cheaper than fresh input, matching the observed fit.
        assert_eq!(cost_units(&record), 1_000_000 * OPUS.cached_input);
        assert_eq!(OPUS.input / OPUS.cached_input, 50);
    }

    #[test]
    fn a_rate_converts_without_overflowing_on_a_months_spend() {
        let fit = QuotaCalibration {
            source_app: SourceApp::ClaudeCode,
            label: None,
            window_minutes: 10_080,
            tenths_per_tera_unit: 500_000,
            pairs: 10,
            residual_percent: 5,
        };
        // Far more than any real month, and still clamped rather than wrapped.
        assert_eq!(fit.tenths_for(u64::MAX), 1_000);
        assert_eq!(fit.tenths_for(2 * RATE_SCALE), 1_000);
        assert_eq!(fit.tenths_for(RATE_SCALE / 1_000), 500);
    }

    /// The weights and the trust threshold, checked against real readings.
    ///
    /// Sampled from one machine's `~/.claude.json` on 2026-07-30: four
    /// consecutive cache refreshes of the five-hour window, with the Claude
    /// Code transcript totals for each interval between them. Nothing here is
    /// constructed to fit — if the model were wrong about how output and cache
    /// reads are weighted, three intervals with this much variation in mix
    /// would not agree to within a few percent.
    #[test]
    fn the_weights_hold_against_measured_readings() {
        // 8% -> 19% -> 32% -> 42%, at 0, 14, 23 and 30 minutes.
        let samples: Vec<QuotaSample> = [(0i64, 80u16), (14, 190), (23, 320), (30, 420)]
            .into_iter()
            .map(|(minute, used)| QuotaSample {
                used_percent_tenths: used,
                observed_at: at(minute),
                ..sample(0, 0)
            })
            .collect();

        // (minute, input incl. cache creation, output, cache read) per interval.
        let measured = [
            (7i64, 141_242u64, 30_339u64, 2_922_062u64),
            (18, 126_600, 38_618, 5_163_869),
            (26, 137_784, 21_425, 4_387_091),
        ];
        let records: Vec<UsageRecord> = measured
            .into_iter()
            .map(|(minute, input, output, cached)| {
                let mut record = record(
                    SourceApp::ClaudeCode,
                    minute,
                    "claude-opus-5",
                    input,
                    output,
                );
                record.tokens.cached_input = TokenField::exact(cached);
                record
            })
            .collect();

        let fits = fit_calibrations(&[quota(420, 30)], &samples, &records);
        assert_eq!(fits.len(), 1);
        assert_eq!(fits[0].pairs, 3);
        assert!(
            fits[0].residual_percent <= 5,
            "three real intervals disagreed by {}%, so the weights are wrong",
            fits[0].residual_percent
        );
        assert!(fits[0].is_trustworthy());

        // And the one fitted rate reproduces every one of those intervals to
        // within two tenths — 0.2 percentage points of the window, which is
        // the error a projection actually inherits.
        for (moved, input, output, cached) in [
            (110u16, 141_242u64, 30_339u64, 2_922_062u64),
            (130, 126_600, 38_618, 5_163_869),
            (100, 137_784, 21_425, 4_387_091),
        ] {
            let units = input * OPUS.input + output * OPUS.output + cached * OPUS.cached_input;
            let reproduced = fits[0].tenths_for(units);
            assert!(
                reproduced.abs_diff(moved) <= 2,
                "fitted rate turned {moved} tenths into {reproduced}"
            );
        }
    }

    #[test]
    fn only_the_windows_asked_for_are_fitted() {
        let (samples, records) = steady_history();
        // Samples exist only for the 300-minute window, so the weekly one
        // yields no fit and is simply absent.
        let weekly = UsageQuota {
            window_minutes: 10_080,
            ..quota(300, 30)
        };
        let fits = fit_calibrations(&[weekly], &samples, &records);
        assert!(fits.is_empty());
    }
}
