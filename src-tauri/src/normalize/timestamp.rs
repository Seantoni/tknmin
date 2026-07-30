//! Turns a source's raw timestamp string into an instant, recording how much
//! interpretation that required.

use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};

use crate::domain::TimestampInterpretation;

/// Formats accepted for zone-less values, tried in order.
const NAIVE_FORMATS: [&str; 4] = [
    "%Y-%m-%dT%H:%M:%S%.f",
    "%Y-%m-%d %H:%M:%S%.f",
    "%Y-%m-%dT%H:%M",
    "%Y-%m-%d %H:%M",
];

/// Parse a raw timestamp.
///
/// A value without zone information is interpreted as UTC rather than as this
/// Mac's local zone: the choice has to be deterministic, because the same log
/// must normalize identically regardless of where or when it is imported. An
/// adapter that knows its source writes local time can convert the value
/// itself and report [`TimestampInterpretation::AssumedLocal`].
pub fn parse_source_timestamp(
    raw: Option<&str>,
) -> (Option<DateTime<Utc>>, TimestampInterpretation) {
    let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return (None, TimestampInterpretation::Missing);
    };

    if let Some(parsed) = parse_epoch(raw) {
        return (Some(parsed), TimestampInterpretation::ExplicitOffset);
    }

    if let Ok(parsed) = DateTime::parse_from_rfc3339(raw) {
        return (
            Some(parsed.with_timezone(&Utc)),
            TimestampInterpretation::ExplicitOffset,
        );
    }

    for format in NAIVE_FORMATS {
        if let Ok(naive) = NaiveDateTime::parse_from_str(raw, format) {
            return (
                Some(Utc.from_utc_datetime(&naive)),
                TimestampInterpretation::AssumedUtc,
            );
        }
    }

    if let Ok(date) = NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        let naive = date.and_hms_opt(0, 0, 0).expect("midnight is always valid");
        return (
            Some(Utc.from_utc_datetime(&naive)),
            TimestampInterpretation::AssumedUtc,
        );
    }

    (None, TimestampInterpretation::Unparsable)
}

/// Numeric epoch values, with the unit inferred from the digit count.
fn parse_epoch(raw: &str) -> Option<DateTime<Utc>> {
    let digits = raw.strip_prefix('-').unwrap_or(raw);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let value: i64 = raw.parse().ok()?;
    match digits.len() {
        1..=11 => Utc.timestamp_opt(value, 0).single(),
        12..=13 => Utc.timestamp_millis_opt(value).single(),
        14..=16 => Utc.timestamp_micros(value).single(),
        17..=19 => Some(Utc.timestamp_nanos(value)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_and_blank_values_are_missing() {
        assert_eq!(
            parse_source_timestamp(None).1,
            TimestampInterpretation::Missing
        );
        assert_eq!(
            parse_source_timestamp(Some("   ")).1,
            TimestampInterpretation::Missing
        );
    }

    #[test]
    fn rfc3339_keeps_its_offset() {
        let (instant, interpretation) = parse_source_timestamp(Some("2026-07-29T10:30:00-05:00"));
        assert_eq!(interpretation, TimestampInterpretation::ExplicitOffset);
        assert_eq!(instant.unwrap().to_rfc3339(), "2026-07-29T15:30:00+00:00");
    }

    #[test]
    fn zoneless_values_are_assumed_utc() {
        let (instant, interpretation) = parse_source_timestamp(Some("2026-07-29 10:30:00"));
        assert_eq!(interpretation, TimestampInterpretation::AssumedUtc);
        assert_eq!(instant.unwrap().to_rfc3339(), "2026-07-29T10:30:00+00:00");
    }

    #[test]
    fn epoch_units_are_inferred_from_length() {
        let seconds = parse_source_timestamp(Some("1785000000")).0.unwrap();
        let millis = parse_source_timestamp(Some("1785000000000")).0.unwrap();
        let micros = parse_source_timestamp(Some("1785000000000000")).0.unwrap();
        assert_eq!(seconds, millis);
        assert_eq!(seconds, micros);
    }

    #[test]
    fn garbage_is_unparsable_but_not_fatal() {
        let (instant, interpretation) = parse_source_timestamp(Some("last tuesday"));
        assert!(instant.is_none());
        assert_eq!(interpretation, TimestampInterpretation::Unparsable);
    }
}
