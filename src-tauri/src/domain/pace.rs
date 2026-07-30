//! Pace — will the allowance outlast the window?
//!
//! The rest of the app reports *position*: "7% left this week." Position is
//! not risk. 7% left with two hours to go is comfortable; 40% left with five
//! days to go, spent at the rate of the last hour, is not. This module turns
//! a quota reading (and, when they exist, prior samples) into the answer to
//! one question: if the user keeps working like this, does the allowance run
//! out before the window resets, and by how much?
//!
//! Everything reduces to one integer expression:
//!
//! ```text
//! runway_minutes = remaining_tenths * elapsed_minutes / consumed_tenths
//! ```
//!
//! Compare it to `T`, the minutes until reset, and every other number the
//! interface needs falls out: the shortfall is `T - runway`, the slack is
//! `runway - T`, and the projected exhaustion instant is `now + runway`. All
//! arithmetic is integer, matching the codebase rule that percentages never
//! become floats; the one division happens last.
//!
//! Like `notifications.rs`, this module is pure — no I/O, no Tauri, no clock
//! of its own. `now` is injected so the whole thing is testable.

use chrono::{DateTime, Timelike, Utc};
use serde::{Deserialize, Serialize};

use super::baseline::{
    pace_vs_baseline, PaceVsBaseline, SourceBaseline, TokenRate, RATE_WINDOW_MINUTES,
};
use super::quota::{same_window_instance, UsageQuota};
use super::source::SourceApp;

/// A pace measurement coarser than this carries so much relative error from
/// the tenths-of-a-percent quantisation (each step is ~1/consumed) that it
/// cannot support a projection. 20 tenths is two percentage points.
const QUANTIZATION_FLOOR_TENTHS: u16 = 20;

/// A trailing horizon shorter than this measures sample noise, not behaviour:
/// the quota lane polls about once a minute, so a 10-minute horizon spans
/// roughly ten samples.
const MIN_HORIZON_MINUTES: u32 = 10;

/// Windows no longer than this can use short horizons. Inside a day the
/// continuous-use assumption roughly holds, so a recent burst says something
/// about the near future. A weekly or monthly window assumes a duty cycle —
/// days of continuous work that nobody does — so extrapolating a burst across
/// it produces a false alarm, and only the full-window average is permitted.
const DUTY_CYCLE_LIMIT_MINUTES: u32 = 1_440;

/// Even a fresh reading is at least this old in tolerance terms, so a
/// just-synced row still cannot pretend to a projection finer than this.
const MIN_TOLERANCE_MINUTES: u32 = 15;

/// The part of the state a user would act on. Words and position carry it;
/// `Unknown` and `NotStarted` must never be rendered as `Green`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PaceState {
    /// No window is running. Claude reports a rolling session as zero used
    /// with no reset until the first request; the allowance is whole and
    /// there is no pace to measure. A real, current statement, not a failure.
    NotStarted,
    /// Runway exceeds the window by more than the measurement's uncertainty.
    Green,
    /// Within the uncertainty band: the data cannot support a finer claim.
    Amber,
    /// Projected to run out before reset, by more than the uncertainty.
    Red,
    /// Nothing left.
    Exhausted,
    /// No pace could be measured, or the reading is too old to project from.
    Unknown,
}

/// How a pace was measured, so the interface can qualify what it shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "kind"
)]
pub enum PaceBasis {
    /// Two observations, `minutes` apart. Needs no assumption about the
    /// window's shape.
    Trailing { minutes: u32 },
    /// The window's own opening, assumed to start at zero used. Only true for
    /// an anchored window; on a rolling one the runway it yields is a lower
    /// bound, because a rolling allowance also returns.
    SinceWindowOpen { assumed_anchored: bool },
}

/// One allowance window's pace and what it implies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowPace {
    /// Identity, matching `quota_key` exactly so a row joins to its quota.
    pub source_app: SourceApp,
    pub label: Option<String>,
    pub window_minutes: u32,
    pub state: PaceState,
    /// Minutes of usage left at the measured pace. `None` when no pace could
    /// be measured — which is not zero and must never render as zero.
    pub runway_minutes: Option<u32>,
    /// When the allowance is projected to run out, as an absolute instant so
    /// the interface counts down to it rather than re-deriving it. `None`
    /// when the allowance is projected to outlast the window.
    pub projected_exhaustion_at: Option<DateTime<Utc>>,
    /// Minutes earlier than the reset it runs out. `None` unless `Red`.
    pub shortfall_minutes: Option<u32>,
    /// Minutes of allowance to spare at reset. `None` unless it outlasts.
    pub slack_minutes: Option<u32>,
    /// Pace as an integer percentage of the affordable pace (240 = 2.4×).
    /// Derived and carried so Rust and the interface cannot round it apart.
    pub pace_ratio_percent: Option<u32>,
    pub basis: Option<PaceBasis>,
    /// How the current burn compares to this source's own history at this
    /// local hour. `NoBaseline` when there is not enough history to say.
    pub vs_baseline: PaceVsBaseline,
    /// The reading this rests on, so its age can be shown beside it.
    pub observed_at: DateTime<Utc>,
}

/// One historical observation of one allowance window.
///
/// `resets_at` is carried per sample because it identifies the window
/// *instance*: a delta taken across a reset spans a drop to zero and produces
/// a large negative pace, so samples are only ever differenced within one
/// instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaSample {
    pub source_app: SourceApp,
    pub label: Option<String>,
    pub window_minutes: u32,
    pub used_percent_tenths: u16,
    pub resets_at: Option<DateTime<Utc>>,
    pub observed_at: DateTime<Utc>,
}

impl QuotaSample {
    /// A sample from a live quota row, used as the right-hand endpoint of
    /// every measurement so an idle interval reads as zero pace: the endpoint
    /// keeps advancing while `used_percent_tenths` does not.
    fn from_quota(quota: &UsageQuota) -> Self {
        QuotaSample {
            source_app: quota.source_app,
            label: quota.label.clone(),
            window_minutes: quota.window_minutes,
            used_percent_tenths: quota.used_percent_tenths,
            resets_at: quota.resets_at,
            observed_at: quota.observed_at,
        }
    }
}

/// Compute every live window's pace, ordered the way the interface groups
/// them: the source closest to running out first, its own windows
/// shortest-first.
pub fn evaluate_pace(
    quotas: &[UsageQuota],
    samples: &[QuotaSample],
    baselines: &[SourceBaseline],
    recent_rates: &[TokenRate],
    tz_offset_minutes: i64,
    now: DateTime<Utc>,
) -> Vec<WindowPace> {
    let mut paces: Vec<WindowPace> = quotas
        .iter()
        .filter(|quota| quota.is_current_at(now))
        .map(|quota| {
            evaluate_window(
                quota,
                samples,
                baselines,
                recent_rates,
                tz_offset_minutes,
                now,
            )
        })
        .collect();

    paces.sort_by(|left, right| {
        // Sources order by how soon they run out — a projection beats a
        // runway, which beats "no pace" — then shortest window first, so a
        // session sits ahead of the week containing it.
        ordering_key(left)
            .cmp(&ordering_key(right))
            .then(left.window_minutes.cmp(&right.window_minutes))
            .then(left.label.cmp(&right.label))
    });
    paces
}

/// A sortable projection key: soonest exhaustion first, then soonest runway,
/// then windows with no pace at all.
fn ordering_key(pace: &WindowPace) -> (u8, i64) {
    match pace.projected_exhaustion_at {
        Some(at) => (0, at.timestamp()),
        None => match pace.runway_minutes {
            Some(minutes) => (1, i64::from(minutes)),
            None => (2, 0),
        },
    }
}

fn evaluate_window(
    quota: &UsageQuota,
    samples: &[QuotaSample],
    baselines: &[SourceBaseline],
    recent_rates: &[TokenRate],
    tz_offset_minutes: i64,
    now: DateTime<Utc>,
) -> WindowPace {
    let mut pace = WindowPace {
        source_app: quota.source_app,
        label: quota.label.clone(),
        window_minutes: quota.window_minutes,
        state: PaceState::Unknown,
        runway_minutes: None,
        projected_exhaustion_at: None,
        shortfall_minutes: None,
        slack_minutes: None,
        pace_ratio_percent: None,
        basis: None,
        vs_baseline: PaceVsBaseline::NoBaseline,
        observed_at: quota.observed_at,
    };

    let baseline = baselines
        .iter()
        .find(|baseline| baseline.source_app == quota.source_app);
    let duty_cycle_percent = baseline.and_then(|b| b.duty_cycle_percent);

    // Compare the current burn to this source's own history at the local hour
    // the burn actually covers. The rate is measured over a trailing window, so
    // at 14:05 it describes 13:05–14:05 — mostly hour 13, and comparing it to
    // hour 14's median would judge the morning by the afternoon's habits. The
    // window's midpoint picks the hour it overlaps most.
    //
    // Computed before any early return: `NotStarted`, `Exhausted`, `Unknown`
    // and an idle `Green` are exactly the states where no runway can be
    // measured, and the user's own history is the only thing left that can
    // speak.
    let measured_at = now - chrono::Duration::minutes(i64::from(RATE_WINDOW_MINUTES) / 2);
    let local_hour = (measured_at + chrono::Duration::minutes(tz_offset_minutes)).hour() as usize;
    if let Some(baseline) = baseline {
        let current = recent_rates
            .iter()
            .find(|rate| rate.source_app == quota.source_app)
            .map(|rate| rate.tokens_per_hour);
        pace.vs_baseline = pace_vs_baseline(baseline, current, local_hour);
    }

    // Checked in order, first match wins.
    let Some(resets_at) = quota.resets_at else {
        pace.state = PaceState::NotStarted;
        return pace;
    };
    let remaining_tenths = quota.remaining_percent_tenths();
    if remaining_tenths == 0 {
        pace.state = PaceState::Exhausted;
        return pace;
    }

    let t_minutes = minutes_until(resets_at, now);
    let age_minutes = age_in_minutes(quota.observed_at, now);
    if age_minutes > staleness_limit(quota.window_minutes) {
        pace.state = PaceState::Unknown;
        return pace;
    }

    let Some(measurement) = best_runway(quota, samples, t_minutes, duty_cycle_percent) else {
        // Only an exact zero is honest Green. A small non-zero value may have
        // been discarded by the quantisation floor, so calling that idle
        // would turn a fast first burst into "nothing being spent".
        pace.state = if quota.used_percent_tenths == 0 {
            PaceState::Green
        } else {
            PaceState::Unknown
        };
        return pace;
    };

    pace.runway_minutes = Some(measurement.runway_minutes);
    pace.basis = Some(measurement.basis);

    let tolerance = (t_minutes / 10).max(age_minutes).max(MIN_TOLERANCE_MINUTES);
    pace.state = if measurement.runway_minutes > t_minutes.saturating_add(tolerance) {
        pace.slack_minutes = Some(measurement.runway_minutes - t_minutes);
        PaceState::Green
    } else if measurement.runway_minutes < t_minutes.saturating_sub(tolerance) {
        pace.shortfall_minutes = Some(t_minutes - measurement.runway_minutes);
        pace.projected_exhaustion_at =
            Some(now + chrono::Duration::minutes(i64::from(measurement.runway_minutes)));
        PaceState::Red
    } else {
        PaceState::Amber
    };

    if measurement.runway_minutes > 0 {
        pace.pace_ratio_percent =
            Some(((u64::from(t_minutes) * 100) / u64::from(measurement.runway_minutes)) as u32);
    }

    pace
}

/// A runway plus the basis that produced it.
struct RunwayMeasurement {
    runway_minutes: u32,
    basis: PaceBasis,
}

/// The worst (shortest) runway across every valid horizon, including the
/// window's own opening. A burst at any scale the window can contain is
/// caught; a pause cannot declare safety because the opening rung includes it.
fn best_runway(
    quota: &UsageQuota,
    samples: &[QuotaSample],
    t_minutes: u32,
    duty_cycle_percent: Option<u32>,
) -> Option<RunwayMeasurement> {
    let mut best: Option<RunwayMeasurement> = None;
    let mut consider = |runway_minutes: u64, basis: PaceBasis| {
        let runway_minutes = runway_minutes.min(u64::from(u32::MAX)) as u32;
        if best
            .as_ref()
            .is_none_or(|held| runway_minutes < held.runway_minutes)
        {
            best = Some(RunwayMeasurement {
                runway_minutes,
                basis,
            });
        }
    };

    // The window opened at zero used, assumed anchored. Only meaningful while
    // some of the window has actually elapsed and enough was spent to beat
    // quantisation. runway = remaining * elapsed / consumed.
    let observed_time_left = quota
        .resets_at
        .map(|resets_at| minutes_until(resets_at, quota.observed_at))
        .unwrap_or(t_minutes);
    let elapsed = u64::from(quota.window_minutes.saturating_sub(observed_time_left));
    let consumed = u64::from(quota.used_percent_tenths);
    if elapsed > 0 && consumed >= u64::from(QUANTIZATION_FLOOR_TENTHS) {
        let runway = u64::from(remaining_tenths_of(quota)) * elapsed / consumed;
        consider(
            runway,
            PaceBasis::SinceWindowOpen {
                assumed_anchored: true,
            },
        );
    }

    let right = QuotaSample::from_quota(quota);
    for horizon in trailing_horizons(quota.window_minutes, duty_cycle_percent) {
        if let Some(runway) = trailing_runway(&right, samples, horizon) {
            consider(runway, PaceBasis::Trailing { minutes: horizon });
        }
    }

    best
}

/// The runway measured over a trailing horizon: find the oldest sample inside
/// the horizon that shares the right endpoint's window instance, and
/// extrapolate the pace between it and the live row.
fn trailing_runway(
    right: &QuotaSample,
    samples: &[QuotaSample],
    horizon_minutes: u32,
) -> Option<u64> {
    // A horizon describes the period ending at the source's observation, not
    // at the wall clock now. The live row may legitimately be several minutes
    // old; anchoring here to `now` would throw away valid earlier samples and
    // could hide a recent burst.
    let horizon_start = right.observed_at - chrono::Duration::minutes(i64::from(horizon_minutes));

    let mut candidates: Vec<&QuotaSample> = samples
        .iter()
        .filter(|sample| {
            sample.source_app == right.source_app
                && sample.label == right.label
                && sample.window_minutes == right.window_minutes
                // Deltas never span two window instances: a reset drops the
                // used figure to zero, and differencing across it invents a
                // large negative pace. Compared with a tolerance, because the
                // source restates the same reset with a little jitter and an
                // exact match would discard every pair it has.
                && same_window_instance(sample.resets_at, right.resets_at)
                && sample.observed_at >= horizon_start
                && sample.observed_at < right.observed_at
        })
        .collect();
    candidates.sort_by_key(|sample| sample.observed_at);
    let left = candidates.first()?;

    let elapsed = (right.observed_at - left.observed_at).num_minutes().max(0) as u64;
    if elapsed < u64::from(MIN_HORIZON_MINUTES) {
        return None;
    }
    let consumed = i64::from(right.used_percent_tenths) - i64::from(left.used_percent_tenths);
    if consumed < i64::from(QUANTIZATION_FLOOR_TENTHS) {
        // Zero (idle) or negative (a rolling window returning allowance):
        // either way there is no burn to extrapolate.
        return None;
    }

    let remaining = u64::from(1000u16.saturating_sub(right.used_percent_tenths));
    Some(remaining * elapsed / consumed as u64)
}

/// The trailing horizons a window may use, shortest first. Short windows
/// assume continuous use, so any recent burst says something about the near
/// future. A long window (weekly, monthly) assumes a duty cycle nobody keeps;
/// extrapolating a burst across it is a false alarm — unless the *measured*
/// duty cycle says the user is active around the clock, in which case the
/// continuous-use assumption is honest after all.
fn trailing_horizons(window_minutes: u32, duty_cycle_percent: Option<u32>) -> Vec<u32> {
    if window_minutes > DUTY_CYCLE_LIMIT_MINUTES {
        // The burst horizons only become meaningful once we know how much of
        // the day is actually worked. Below that threshold a 30-minute spike
        // must not be read as a week-long rate.
        let near_continuous = duty_cycle_percent.is_some_and(|percent| percent >= 80);
        if !near_continuous {
            return Vec::new();
        }
    }
    let tenth = (window_minutes / 10).max(MIN_HORIZON_MINUTES);
    let quarter = (window_minutes / 4).max(MIN_HORIZON_MINUTES);
    let mut horizons = vec![tenth, quarter];
    if window_minutes >= MIN_HORIZON_MINUTES * 3 {
        horizons.push(30.min(window_minutes / 2).max(MIN_HORIZON_MINUTES));
    }
    horizons.sort_unstable();
    horizons.dedup();
    horizons
}

/// How old a reading may be before it cannot support a projection. Staleness
/// cuts the wrong way here: `resets_at` is absolute so time-to-reset is always
/// live, but `used_percent_tenths` is only as fresh as `observed_at`, and a
/// stale reading *overstates* what remains. Long windows tolerate older
/// readings because a week's pace does not change in an hour.
fn staleness_limit(window_minutes: u32) -> u32 {
    (window_minutes / 10).max(MIN_TOLERANCE_MINUTES)
}

fn minutes_until(instant: DateTime<Utc>, now: DateTime<Utc>) -> u32 {
    if instant <= now {
        return 0;
    }
    (instant - now).num_minutes().max(0) as u32
}

fn age_in_minutes(observed_at: DateTime<Utc>, now: DateTime<Utc>) -> u32 {
    (now - observed_at).num_minutes().max(0) as u32
}

fn remaining_tenths_of(quota: &UsageQuota) -> u32 {
    u32::from(quota.remaining_percent_tenths())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn at(hours: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(hours * 3_600, 0).unwrap()
    }

    fn quota(
        used_tenths: u16,
        resets_in_minutes: i64,
        window_minutes: u32,
        observed_minutes_ago: i64,
        now: DateTime<Utc>,
    ) -> UsageQuota {
        UsageQuota {
            source_app: SourceApp::ClaudeCode,
            label: None,
            window_minutes,
            used_percent_tenths: used_tenths,
            resets_at: Some(now + Duration::minutes(resets_in_minutes)),
            observed_at: now - Duration::minutes(observed_minutes_ago),
        }
    }

    fn pace_of(quota: &UsageQuota, now: DateTime<Utc>) -> WindowPace {
        evaluate_pace(std::slice::from_ref(quota), &[], &[], &[], 0, now)
            .into_iter()
            .next()
            .expect("one live quota yields one pace")
    }

    /// Evaluate with samples but no baseline history — the pre-C behaviour.
    fn pace_with_samples(
        quota: &UsageQuota,
        samples: &[QuotaSample],
        now: DateTime<Utc>,
    ) -> WindowPace {
        evaluate_pace(std::slice::from_ref(quota), samples, &[], &[], 0, now)
            .into_iter()
            .next()
            .expect("one live quota yields one pace")
    }

    #[test]
    fn not_started_when_no_window_is_running() {
        let now = at(10);
        let mut quota = quota(0, 0, 300, 0, now);
        quota.resets_at = None;
        let pace = pace_of(&quota, now);
        assert_eq!(pace.state, PaceState::NotStarted);
        assert_eq!(pace.runway_minutes, None);
        assert_eq!(pace.projected_exhaustion_at, None);
    }

    #[test]
    fn exhausted_when_nothing_is_left() {
        let now = at(10);
        let quota = quota(1_000, 60, 300, 0, now);
        assert_eq!(pace_of(&quota, now).state, PaceState::Exhausted);
    }

    #[test]
    fn unknown_when_the_reading_is_too_old_to_project() {
        let now = at(10);
        // A 5-hour session tolerates max(300/10, 15) = 30 minutes of age.
        let quota = quota(500, 60, 300, 45, now);
        let pace = pace_of(&quota, now);
        assert_eq!(pace.state, PaceState::Unknown);
        assert_eq!(pace.runway_minutes, None, "unknown never carries a runway");
    }

    #[test]
    fn comfortable_billing_cycle_is_green_from_window_open() {
        let now = at(10);
        // 12% used of a 31-day cycle with 21 days left.
        let quota = quota(120, 30_240, 44_640, 0, now);
        let pace = pace_of(&quota, now);
        assert_eq!(pace.state, PaceState::Green);
        assert_eq!(
            pace.runway_minutes,
            Some(105_600),
            "880 remaining * 14400 elapsed / 120 consumed"
        );
        assert_eq!(pace.slack_minutes, Some(75_360));
        assert_eq!(pace.projected_exhaustion_at, None);
        assert_eq!(
            pace.basis,
            Some(PaceBasis::SinceWindowOpen {
                assumed_anchored: true
            })
        );
    }

    #[test]
    fn on_the_line_is_amber() {
        let now = at(10);
        // Half the window elapsed, half consumed: runway == T exactly.
        let quota = quota(500, 150, 300, 0, now);
        let pace = pace_of(&quota, now);
        assert_eq!(pace.state, PaceState::Amber);
        assert_eq!(pace.runway_minutes, Some(150));
        assert_eq!(pace.shortfall_minutes, None);
        assert_eq!(pace.slack_minutes, None);
    }

    #[test]
    fn over_the_line_is_red_with_a_projection() {
        let now = at(10);
        // Three-quarters consumed in half the window: burns at 1.5×.
        let quota = quota(750, 150, 300, 0, now);
        let pace = pace_of(&quota, now);
        assert_eq!(pace.state, PaceState::Red);
        assert_eq!(pace.runway_minutes, Some(50), "250 * 150 / 750");
        assert_eq!(pace.shortfall_minutes, Some(100));
        assert_eq!(
            pace.projected_exhaustion_at,
            Some(now + Duration::minutes(50))
        );
        assert_eq!(pace.pace_ratio_percent, Some(300));
    }

    #[test]
    fn idle_has_no_pace_and_is_green() {
        let now = at(10);
        let quota = quota(0, 60, 300, 0, now);
        let pace = pace_of(&quota, now);
        assert_eq!(pace.state, PaceState::Green);
        assert_eq!(pace.runway_minutes, None);
        assert_eq!(pace.projected_exhaustion_at, None);
        assert_eq!(pace.basis, None);
    }

    #[test]
    fn a_fresh_reading_with_nothing_spent_is_green_not_unknown() {
        let now = at(10);
        // The invariant that matters most: unknown must never render green,
        // and green must never be asserted without confidence.
        let quota = quota(0, 300, 300, 0, now);
        assert_eq!(pace_of(&quota, now).state, PaceState::Green);
    }

    #[test]
    fn small_nonzero_usage_is_unknown_not_idle_green() {
        let now = at(10);
        // 1.9% in the first minute is below the quantisation floor, but it is
        // plainly not idle: extrapolated literally it would exhaust a
        // five-hour window in about 52 minutes.
        let quota = quota(19, 299, 300, 0, now);
        let pace = pace_of(&quota, now);
        assert_eq!(pace.state, PaceState::Unknown);
        assert_eq!(pace.runway_minutes, None);
    }

    fn sample(used: u16, resets: Option<DateTime<Utc>>, observed: DateTime<Utc>) -> QuotaSample {
        QuotaSample {
            source_app: SourceApp::ClaudeCode,
            label: None,
            window_minutes: 300,
            used_percent_tenths: used,
            resets_at: resets,
            observed_at: observed,
        }
    }

    #[test]
    fn a_recent_burst_catches_what_the_window_average_misses() {
        let now = at(10);
        // 20% consumed across 4 elapsed hours — the window-open average is
        // gentle. But all of it landed in the last 30 minutes.
        let quota = quota(200, 60, 300, 0, now);
        let resets = quota.resets_at;
        let samples = vec![sample(0, resets, now - Duration::minutes(30))];
        let pace = pace_with_samples(&quota, &samples, now);
        // Trailing: 800 remaining * 30 / 200 = 120 min vs T = 60 → Green and
        // worse than the window-open 240 min, so the burst is what decides.
        assert_eq!(pace.runway_minutes, Some(120));
        assert_eq!(
            pace.basis,
            Some(PaceBasis::Trailing { minutes: 30 }),
            "the shortest valid horizon should win"
        );
        assert_eq!(pace.state, PaceState::Green);
    }

    #[test]
    fn a_stale_right_endpoint_keeps_its_own_trailing_horizon() {
        let now = at(10);
        // The source's latest reading is 20 minutes old (still within the
        // allowed 30-minute age). A sample 20 minutes before that reading is
        // therefore 40 minutes behind wall-clock now, but it still belongs to
        // the reading's 30-minute trailing horizon.
        let quota = quota(500, 60, 300, 20, now);
        let samples = vec![sample(
            100,
            quota.resets_at,
            quota.observed_at - Duration::minutes(20),
        )];
        let pace = pace_with_samples(&quota, &samples, now);

        assert_eq!(pace.state, PaceState::Red);
        assert_eq!(pace.runway_minutes, Some(25));
        assert_eq!(pace.basis, Some(PaceBasis::Trailing { minutes: 30 }));
    }

    #[test]
    fn serialized_shape_matches_the_typescript_contract() {
        assert_eq!(
            serde_json::to_value(PaceState::NotStarted).unwrap(),
            serde_json::json!("notStarted")
        );
        assert_eq!(
            serde_json::to_value(PaceBasis::SinceWindowOpen {
                assumed_anchored: true,
            })
            .unwrap(),
            serde_json::json!({
                "kind": "sinceWindowOpen",
                "assumedAnchored": true,
            })
        );
    }

    #[test]
    fn deltas_never_span_a_reset() {
        let now = at(10);
        // Half consumed across 4 elapsed hours with 1h left: a gentle
        // window-open pace (runway 7h vs 1h), well green on its own.
        let quota = quota(500, 60, 300, 0, now);
        // A sample from a *previous* window instance (a different reset). If
        // differenced against the live row it would look like the allowance
        // collapsed from 90% to 50% in 20 minutes — a phantom burst.
        let old_reset = Some(now - Duration::minutes(300));
        let samples = vec![sample(900, old_reset, now - Duration::minutes(20))];
        let pace = pace_with_samples(&quota, &samples, now);
        assert_eq!(pace.state, PaceState::Green);
        assert_eq!(
            pace.basis,
            Some(PaceBasis::SinceWindowOpen {
                assumed_anchored: true
            }),
            "the mismatched sample must be discarded, not differenced"
        );
    }

    /// A source whose `hour` is ordinarily worth `median` tokens.
    fn baseline_at(hour: usize, median: u64) -> SourceBaseline {
        let mut median_per_hour = [None; 24];
        median_per_hour[hour] = Some(median);
        SourceBaseline {
            source_app: SourceApp::ClaudeCode,
            median_per_hour,
            slot_days: [10; 24],
            duty_cycle_percent: None,
        }
    }

    fn burning(tokens_per_hour: u64) -> Vec<TokenRate> {
        vec![TokenRate {
            source_app: SourceApp::ClaudeCode,
            tokens_per_hour,
        }]
    }

    #[test]
    fn the_burn_is_banded_against_the_hour_it_covers() {
        // now is 10:00, so the trailing hour is 09:00–10:00: it belongs to
        // hour 9, whose median is what it must be judged against.
        let now = at(10);
        let quota = quota(500, 150, 300, 0, now);
        let pace = evaluate_pace(
            std::slice::from_ref(&quota),
            &[],
            &[baseline_at(9, 10_000)],
            &burning(40_000),
            0,
            now,
        )
        .remove(0);
        assert_eq!(pace.vs_baseline, PaceVsBaseline::FarAbove);

        // The same burn at the same instant is ordinary for this source: four
        // times the hour's median is far above, one times it is typical.
        let pace = evaluate_pace(
            std::slice::from_ref(&quota),
            &[],
            &[baseline_at(9, 40_000)],
            &burning(40_000),
            0,
            now,
        )
        .remove(0);
        assert_eq!(pace.vs_baseline, PaceVsBaseline::Typical);
    }

    #[test]
    fn a_half_hour_zone_lands_in_its_own_local_hour() {
        // 09:30 UTC is 15:00 in India (+05:30): hour 15, which a whole-hour
        // offset would have truncated to 14.
        let now = at(10);
        let quota = quota(500, 150, 300, 0, now);
        let pace = evaluate_pace(
            std::slice::from_ref(&quota),
            &[],
            &[baseline_at(15, 10_000)],
            &burning(10_000),
            5 * 60 + 30,
            now,
        )
        .remove(0);
        assert_eq!(pace.vs_baseline, PaceVsBaseline::Typical);
    }

    #[test]
    fn a_measured_duty_cycle_unlocks_short_horizons_on_a_long_window() {
        let now = at(10);
        // A weekly window, 10% used with six days left, and a burst that spent
        // 3% in the last 30 minutes. Extrapolating that across a week is only
        // honest if the user really does work around the clock.
        let quota = quota(100, 8_640, 10_080, 0, now);
        let samples = vec![QuotaSample {
            source_app: SourceApp::ClaudeCode,
            label: None,
            window_minutes: 10_080,
            used_percent_tenths: 70,
            resets_at: quota.resets_at,
            observed_at: now - Duration::minutes(30),
        }];
        let with_duty_cycle = |percent: Option<u32>| {
            let baseline = SourceBaseline {
                source_app: SourceApp::ClaudeCode,
                median_per_hour: [None; 24],
                slot_days: [0; 24],
                duty_cycle_percent: percent,
            };
            evaluate_pace(
                std::slice::from_ref(&quota),
                &samples,
                &[baseline],
                &[],
                0,
                now,
            )
            .remove(0)
        };

        // Nine-tenths of the day worked: the burst is a real weekly rate, and
        // 900 remaining * 30 elapsed / 30 consumed = 900 minutes of runway
        // against 8_640 until reset.
        let continuous = with_duty_cycle(Some(90));
        assert_eq!(continuous.state, PaceState::Red);
        assert_eq!(continuous.runway_minutes, Some(900));
        assert_eq!(continuous.basis, Some(PaceBasis::Trailing { minutes: 30 }));

        // A third of the day worked, or no measurement at all: the burst says
        // nothing about a week, and only the window-open average is allowed.
        for measured in [Some(33), None] {
            let paced = with_duty_cycle(measured);
            assert_eq!(
                paced.basis,
                Some(PaceBasis::SinceWindowOpen {
                    assumed_anchored: true
                }),
                "a burst must not be read as a week-long rate at {measured:?}% duty cycle"
            );
        }
    }

    #[test]
    fn history_still_speaks_when_no_runway_can_be_measured() {
        // The states with nothing to project from are exactly the ones where
        // the user's own history is the only thing left that can say anything.
        let now = at(10);
        let mut not_started = quota(0, 0, 300, 0, now);
        not_started.resets_at = None;
        let stale = quota(500, 60, 300, 45, now);

        for quota in [not_started, stale] {
            let pace = evaluate_pace(
                std::slice::from_ref(&quota),
                &[],
                &[baseline_at(9, 10_000)],
                &burning(2_000),
                0,
                now,
            )
            .remove(0);
            assert_eq!(pace.runway_minutes, None);
            assert_eq!(
                pace.vs_baseline,
                PaceVsBaseline::Below,
                "a state with no runway still knows how this hour compares"
            );
        }
    }

    #[test]
    fn no_input_produces_green_without_confidence() {
        // Forbidding the dangerous failure mode: Green is only ever asserted
        // from an actual reading or an honestly-measured idle.
        let now = at(10);
        let cases = [
            quota(0, 60, 300, 45, now),    // stale: must not be green
            quota(1_000, 60, 300, 0, now), // empty: must not be green
        ];
        let mut quota = quota(0, 0, 300, 0, now);
        quota.resets_at = None; // not started: must not be green
        let all = cases.iter().chain(std::iter::once(&quota));
        for quota in all {
            assert_ne!(
                pace_of(quota, now).state,
                PaceState::Green,
                "a window without a trustworthy pace asserted green"
            );
        }
    }
}
