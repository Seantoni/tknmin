//! The set of applications whose local logs this app can report on.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A supported source application.
///
/// The wire representation is the stable snake_case identifier used by the
/// React layer, the deduplication key, and any future persisted rows. Adding a
/// variant is a normalization-version change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceApp {
    Cursor,
    ClaudeCode,
    Codex,
}

impl SourceApp {
    pub const ALL: [SourceApp; 3] = [SourceApp::Cursor, SourceApp::ClaudeCode, SourceApp::Codex];

    pub fn as_str(self) -> &'static str {
        match self {
            SourceApp::Cursor => "cursor",
            SourceApp::ClaudeCode => "claude_code",
            SourceApp::Codex => "codex",
        }
    }

    /// Human-readable label. Presentation may override this; it exists so Rust
    /// side logs and errors stay readable.
    pub fn display_name(self) -> &'static str {
        match self {
            SourceApp::Cursor => "Cursor",
            SourceApp::ClaudeCode => "Claude Code",
            SourceApp::Codex => "Codex",
        }
    }
}

impl fmt::Display for SourceApp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown source application: {0}")]
pub struct UnknownSourceApp(pub String);

impl FromStr for SourceApp {
    type Err = UnknownSourceApp;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "cursor" => Ok(SourceApp::Cursor),
            "claude_code" => Ok(SourceApp::ClaudeCode),
            "codex" => Ok(SourceApp::Codex),
            other => Err(UnknownSourceApp(other.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_identifiers_round_trip() {
        for source in SourceApp::ALL {
            assert_eq!(SourceApp::from_str(source.as_str()), Ok(source));
        }
    }

    #[test]
    fn serializes_as_snake_case() {
        let json = serde_json::to_string(&SourceApp::ClaudeCode).unwrap();
        assert_eq!(json, "\"claude_code\"");
    }
}
