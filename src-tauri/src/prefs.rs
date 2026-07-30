//! Load and save [`AppOptions`] as a JSON file.
//!
//! Options live beside the app's config directory so they survive restarts
//! without needing the usage store.

use std::fs;
use std::path::{Path, PathBuf};

use crate::domain::AppOptions;
use crate::error::AppError;

pub const OPTIONS_FILE_NAME: &str = "options.json";

/// Read options from disk, or defaults when the file is missing.
pub fn load_options(path: &Path) -> Result<AppOptions, AppError> {
    if !path.exists() {
        return Ok(AppOptions::defaults());
    }
    let raw = fs::read_to_string(path).map_err(|error| {
        AppError::new(
            crate::error::ErrorCode::Storage,
            format!("could not read options: {error}"),
        )
    })?;
    let parsed: AppOptions = serde_json::from_str(&raw).map_err(|error| {
        AppError::new(
            crate::error::ErrorCode::Storage,
            format!("options file is invalid: {error}"),
        )
    })?;
    let options = parsed.normalized();
    options
        .validate()
        .map_err(|error| AppError::new(crate::error::ErrorCode::Storage, error.to_string()))?;
    Ok(options)
}

/// Validate and write options atomically (temp file + rename).
pub fn save_options(path: &Path, options: &AppOptions) -> Result<(), AppError> {
    options
        .validate()
        .map_err(|error| AppError::invalid_request(error.to_string()))?;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            AppError::new(
                crate::error::ErrorCode::Storage,
                format!("could not create options directory: {error}"),
            )
        })?;
    }

    let body = serde_json::to_string_pretty(options).map_err(|error| {
        AppError::new(
            crate::error::ErrorCode::Storage,
            format!("could not serialize options: {error}"),
        )
    })?;

    let temp = path.with_extension("json.tmp");
    fs::write(&temp, body).map_err(|error| {
        AppError::new(
            crate::error::ErrorCode::Storage,
            format!("could not write options: {error}"),
        )
    })?;
    fs::rename(&temp, path).map_err(|error| {
        AppError::new(
            crate::error::ErrorCode::Storage,
            format!("could not finalize options: {error}"),
        )
    })?;
    Ok(())
}

pub fn options_path(config_dir: PathBuf) -> PathBuf {
    config_dir.join(OPTIONS_FILE_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{SourceApp, SourceThreshold, ThresholdMetric};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_file() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("tokens-options-{nanos}.json"))
    }

    #[test]
    fn missing_file_yields_defaults() {
        let path = temp_file();
        let _ = fs::remove_file(&path);
        let options = load_options(&path).unwrap();
        assert_eq!(options, AppOptions::defaults());
    }

    #[test]
    fn round_trip_preserves_edits() {
        let path = temp_file();
        let _ = fs::remove_file(&path);
        let mut options = AppOptions::defaults();
        for row in &mut options.thresholds {
            if row.source_app == SourceApp::ClaudeCode {
                *row = SourceThreshold {
                    source_app: SourceApp::ClaudeCode,
                    enabled: true,
                    metric: ThresholdMetric::MinutesUntilReset,
                    value: 45,
                };
            }
        }
        save_options(&path, &options).unwrap();
        let loaded = load_options(&path).unwrap();
        assert_eq!(loaded, options);
        let _ = fs::remove_file(&path);
    }
}
