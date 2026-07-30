//! Token counts.
//!
//! Every category pairs its value with a quality marker so an absent count can
//! never be mistaken for a zero count.

use serde::{Deserialize, Serialize};

use super::quality::FieldQuality;

/// One token category.
///
/// Invariant: `value.is_none()` implies `quality == FieldQuality::Unknown`.
/// The normalizer enforces this; constructors here uphold it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenField {
    pub value: Option<u64>,
    pub quality: FieldQuality,
}

impl TokenField {
    pub const fn unknown() -> Self {
        Self {
            value: None,
            quality: FieldQuality::Unknown,
        }
    }

    pub const fn exact(value: u64) -> Self {
        Self {
            value: Some(value),
            quality: FieldQuality::Exact,
        }
    }

    pub const fn estimated(value: u64) -> Self {
        Self {
            value: Some(value),
            quality: FieldQuality::Estimated,
        }
    }

    pub const fn partial(value: u64) -> Self {
        Self {
            value: Some(value),
            quality: FieldQuality::Partial,
        }
    }

    /// The value only when it is meaningful to sum.
    pub fn countable(&self) -> Option<u64> {
        match self.value {
            Some(value) if self.quality.is_countable() => Some(value),
            _ => None,
        }
    }

    pub fn is_known(&self) -> bool {
        self.value.is_some()
    }

    pub(crate) fn has_consistent_quality(&self) -> bool {
        self.value.is_some() == (self.quality != FieldQuality::Unknown)
    }
}

impl Default for TokenField {
    fn default() -> Self {
        Self::unknown()
    }
}

/// The token categories this application reports on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenCounts {
    pub input: TokenField,
    pub output: TokenField,
    pub cached_input: TokenField,
    pub reasoning: TokenField,
}

impl TokenCounts {
    pub fn fields(&self) -> [(&'static str, TokenField); 4] {
        [
            ("input", self.input),
            ("output", self.output),
            ("cachedInput", self.cached_input),
            ("reasoning", self.reasoning),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_fields_are_not_countable() {
        assert_eq!(TokenField::unknown().countable(), None);
        assert_eq!(TokenField::exact(10).countable(), Some(10));
        assert_eq!(TokenField::estimated(10).countable(), Some(10));
    }

    #[test]
    fn detects_inconsistent_quality() {
        let bad = TokenField {
            value: None,
            quality: FieldQuality::Exact,
        };
        assert!(!bad.has_consistent_quality());
        assert!(TokenField::unknown().has_consistent_quality());
        assert!(TokenField::exact(1).has_consistent_quality());
    }
}
