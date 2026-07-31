//! User options — settings the interface owns, not usage data.
//!
//! Thresholds decide when Tokens should warn that an allowance window is
//! running low. They are keyed by source application so Claude, Cursor, and
//! Codex can be tuned independently.

use serde::{Deserialize, Serialize};

use super::source::SourceApp;

/// What a per-source threshold measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThresholdMetric {
    /// Warn when remaining allowance is at or below this percent (0–100).
    RemainingPercent,
    /// Warn when the window resets within this many minutes.
    MinutesUntilReset,
    /// Warn when the measured pace projects the allowance to run out at least
    /// this many minutes before the window resets. Position-blind: a window
    /// can sit at 60% and still be projected to exhaust.
    ProjectedExhaustion,
}

impl ThresholdMetric {
    pub const ALL: [ThresholdMetric; 3] = [
        ThresholdMetric::RemainingPercent,
        ThresholdMetric::MinutesUntilReset,
        ThresholdMetric::ProjectedExhaustion,
    ];
}

/// How the compact window lays its allowance rows out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MiniLayout {
    /// Two lines around a bar per allowance, stacked down the panel.
    #[default]
    Stacked,
    /// Every fact about one allowance on a single horizontal row.
    Horizontal,
}

/// One source's alert threshold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceThreshold {
    pub source_app: SourceApp,
    pub enabled: bool,
    pub metric: ThresholdMetric,
    /// Percent 0..=100 for [`ThresholdMetric::RemainingPercent`], or minutes
    /// for [`ThresholdMetric::MinutesUntilReset`].
    pub value: u32,
}

/// All persisted user options.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppOptions {
    pub thresholds: Vec<SourceThreshold>,
    /// Whether the compact window floats above other applications. Defaulted
    /// on deserialize so an options file written before this existed loads.
    #[serde(default = "always_on_top_default")]
    pub mini_always_on_top: bool,
    /// How the compact window lays out its rows. Defaulted on deserialize so
    /// an options file written before this existed loads.
    #[serde(default)]
    pub mini_layout: MiniLayout,
}

/// The compact window exists to stay visible next to an editor, so floating is
/// the default; the setting is there to turn it off.
fn always_on_top_default() -> bool {
    true
}

impl AppOptions {
    /// Sensible starting point: warn at 25% remaining on every source.
    pub fn defaults() -> Self {
        Self {
            thresholds: SourceApp::ALL
                .into_iter()
                .map(|source_app| SourceThreshold {
                    source_app,
                    enabled: true,
                    metric: ThresholdMetric::RemainingPercent,
                    value: 25,
                })
                .collect(),
            mini_always_on_top: always_on_top_default(),
            mini_layout: MiniLayout::default(),
        }
    }

    /// Ensure every known source has exactly one threshold row, filling gaps
    /// with defaults and dropping unknown duplicates.
    pub fn normalized(mut self) -> Self {
        let mut by_source: Vec<Option<SourceThreshold>> = SourceApp::ALL.map(|_| None).to_vec();
        for threshold in self.thresholds.drain(..) {
            if let Some(slot) = SourceApp::ALL
                .iter()
                .position(|&source| source == threshold.source_app)
            {
                by_source[slot] = Some(threshold);
            }
        }
        self.thresholds = SourceApp::ALL
            .into_iter()
            .zip(by_source)
            .map(|(source_app, existing)| {
                existing.unwrap_or(SourceThreshold {
                    source_app,
                    enabled: true,
                    metric: ThresholdMetric::RemainingPercent,
                    value: 25,
                })
            })
            .collect();
        self
    }

    /// Reject values the interface should never send.
    pub fn validate(&self) -> Result<(), OptionsError> {
        if self.thresholds.len() != SourceApp::ALL.len() {
            return Err(OptionsError::Invalid(
                "options must include one threshold per source".into(),
            ));
        }

        let mut seen = [false; SourceApp::ALL.len()];
        for threshold in &self.thresholds {
            let Some(index) = SourceApp::ALL
                .iter()
                .position(|&source| source == threshold.source_app)
            else {
                return Err(OptionsError::Invalid(format!(
                    "unknown source in thresholds: {}",
                    threshold.source_app
                )));
            };
            if seen[index] {
                return Err(OptionsError::Invalid(format!(
                    "duplicate threshold for {}",
                    threshold.source_app
                )));
            }
            seen[index] = true;

            match threshold.metric {
                ThresholdMetric::RemainingPercent if threshold.value > 100 => {
                    return Err(OptionsError::Invalid(
                        "remaining-percent threshold must be between 0 and 100".into(),
                    ));
                }
                ThresholdMetric::MinutesUntilReset if threshold.value > 10_080 => {
                    return Err(OptionsError::Invalid(
                        "minutes-until-reset threshold must be at most one week".into(),
                    ));
                }
                ThresholdMetric::ProjectedExhaustion if threshold.value > 10_080 => {
                    return Err(OptionsError::Invalid(
                        "projected-exhaustion threshold must be at most one week".into(),
                    ));
                }
                _ => {}
            }
        }

        if seen.iter().any(|present| !present) {
            return Err(OptionsError::Invalid(
                "options must include one threshold per source".into(),
            ));
        }

        Ok(())
    }
}

impl Default for AppOptions {
    fn default() -> Self {
        Self::defaults()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OptionsError {
    #[error("{0}")]
    Invalid(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_cover_every_source() {
        let options = AppOptions::defaults();
        assert_eq!(options.thresholds.len(), SourceApp::ALL.len());
        for source in SourceApp::ALL {
            assert!(options
                .thresholds
                .iter()
                .any(|row| row.source_app == source));
        }
    }

    #[test]
    fn normalize_fills_missing_sources() {
        let partial = AppOptions {
            thresholds: vec![SourceThreshold {
                source_app: SourceApp::Codex,
                enabled: false,
                metric: ThresholdMetric::MinutesUntilReset,
                value: 60,
            }],
            ..AppOptions::defaults()
        };
        let normalized = partial.normalized();
        assert_eq!(normalized.thresholds.len(), 3);
        let codex = normalized
            .thresholds
            .iter()
            .find(|row| row.source_app == SourceApp::Codex)
            .unwrap();
        assert!(!codex.enabled);
        assert_eq!(codex.value, 60);
    }

    #[test]
    fn rejects_percent_above_100() {
        let mut options = AppOptions::defaults();
        options.thresholds[0].value = 101;
        assert!(options.validate().is_err());
    }

    #[test]
    fn round_trips_as_camel_case_json() {
        let json = serde_json::to_string(&AppOptions::defaults()).unwrap();
        assert!(json.contains("\"thresholds\""));
        assert!(json.contains("\"sourceApp\""));
        assert!(json.contains("\"remaining_percent\""));
        assert!(json.contains("\"miniAlwaysOnTop\""));
        assert!(json.contains("\"miniLayout\""));
        let back: AppOptions = serde_json::from_str(&json).unwrap();
        assert_eq!(back, AppOptions::defaults());
    }

    #[test]
    fn options_written_before_the_compact_window_still_load() {
        let json = r#"{"thresholds":[]}"#;
        let loaded: AppOptions = serde_json::from_str(json).unwrap();
        assert!(loaded.mini_always_on_top);
        assert_eq!(loaded.mini_layout, MiniLayout::Stacked);
    }
}
