//! Deterministic identity and content keys.
//!
//! Two different questions, two different hashes:
//!
//! - [`dedupe_key`] answers *which source event is this*. It is stable across
//!   re-reads and across corrections, so a later snapshot of the same event
//!   lands on the row the earlier one created.
//! - [`content_hash`] answers *has anything meaningful changed*. It covers the
//!   fields a correction can move — tokens, cost, model, timestamp — so an
//!   unchanged replay is recognised as a no-op instead of a rewrite.
//!
//! Both deliberately exclude adapter version and import time: upgrading an
//! adapter must not resurrect records that were already imported, nor make
//! every replay look like a change.

use sha2::{Digest, Sha256};

use crate::domain::UsageRecordDraft;

/// Bumped whenever the key's inputs or encoding change. Stored on every record
/// so a future migration can tell which keys need recomputing.
pub const DEDUPE_ALGORITHM_VERSION: u32 = 1;

/// Field separator. A unit separator cannot appear in the values being joined,
/// so distinct field sets can never collide by concatenation.
const SEP: char = '\u{1f}';

/// Compute the deduplication key for a draft.
///
/// When the source supplies its own event identifier that identifier is
/// authoritative. Otherwise the key is built from the event's observable
/// content, which is the best available approximation of identity.
pub fn dedupe_key(draft: &UsageRecordDraft) -> String {
    let mut parts: Vec<String> = vec![
        format!("v{DEDUPE_ALGORITHM_VERSION}"),
        draft.source_app.as_str().to_string(),
    ];

    match draft
        .source_event_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        Some(event_id) => {
            parts.push("event".to_string());
            parts.push(event_id.to_string());
        }
        None => {
            parts.push("content".to_string());
            parts.push(optional(draft.raw_timestamp.as_deref()));
            parts.push(optional(draft.provider.as_deref()));
            parts.push(optional(draft.model.as_deref()));
            for (_, field) in draft.tokens.fields() {
                parts.push(
                    field
                        .value
                        .map(|value| value.to_string())
                        .unwrap_or_default(),
                );
            }
            parts.push(
                draft
                    .reported_total_tokens
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
            );
            parts.push(optional(draft.project.as_deref()));
            parts.push(optional(draft.session_id.as_deref()));
        }
    }

    let canonical = parts.join(&SEP.to_string());
    let digest = Sha256::digest(canonical.as_bytes());
    hex::encode(digest)
}

/// A stable internal identifier derived from the deduplication key, so the
/// same source event always resolves to the same identifier across imports.
pub fn record_id(source: &str, dedupe_key: &str) -> String {
    format!("{source}_{}", &dedupe_key[..16])
}

/// Bumped whenever the set of fields a correction is allowed to move changes.
pub const CONTENT_HASH_VERSION: u32 = 1;

/// Digest of every field a later snapshot of the same event may correct.
///
/// Identity is excluded on purpose: two drafts with the same
/// [`dedupe_key`] and the same content hash are the same observation, and
/// storing the second one again would be pure churn.
pub fn content_hash(draft: &UsageRecordDraft) -> String {
    let mut parts: Vec<String> = vec![
        format!("v{CONTENT_HASH_VERSION}"),
        optional(draft.raw_timestamp.as_deref()),
        optional(draft.provider.as_deref()),
        optional(draft.model.as_deref()),
    ];
    for (_, field) in draft.tokens.fields() {
        parts.push(
            field
                .value
                .map(|value| value.to_string())
                .unwrap_or_default(),
        );
        parts.push(format!("{:?}", field.quality));
    }
    parts.push(
        draft
            .reported_total_tokens
            .map(|value| value.to_string())
            .unwrap_or_default(),
    );
    parts.push(optional(draft.project.as_deref()));
    parts.push(optional(draft.session_id.as_deref()));
    match &draft.cost {
        Some(cost) => {
            parts.push(format!("{:?}", cost.status));
            parts.push(optional(cost.pricing_version.as_deref()));
            match &cost.amount {
                Some(money) => parts.push(format!(
                    "{}/{}/{}",
                    money.amount_minor, money.currency, money.minor_unit_exponent
                )),
                None => parts.push(String::new()),
            }
        }
        None => parts.push(String::new()),
    }

    let canonical = parts.join(&SEP.to_string());
    hex::encode(Sha256::digest(canonical.as_bytes()))
}

fn optional(value: Option<&str>) -> String {
    value.map(str::trim).unwrap_or_default().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{SourceApp, SourceProvenance, TokenCounts, TokenField};

    fn provenance() -> SourceProvenance {
        SourceProvenance {
            adapter_id: "cursor".to_string(),
            adapter_version: "0.1.0".to_string(),
            source_ref: None,
        }
    }

    fn draft() -> UsageRecordDraft {
        UsageRecordDraft::new(SourceApp::Cursor, provenance())
            .with_raw_timestamp("2026-07-29T10:00:00Z")
            .with_model(Some("anthropic"), "claude-opus-5")
            .with_tokens(TokenCounts {
                input: TokenField::exact(100),
                output: TokenField::exact(20),
                ..TokenCounts::default()
            })
    }

    #[test]
    fn identical_drafts_share_a_key() {
        assert_eq!(dedupe_key(&draft()), dedupe_key(&draft()));
    }

    #[test]
    fn adapter_version_does_not_affect_the_key() {
        let mut upgraded = draft();
        upgraded.provenance.adapter_version = "9.9.9".to_string();
        upgraded.provenance.source_ref = Some("other-file".to_string());
        assert_eq!(dedupe_key(&draft()), dedupe_key(&upgraded));
    }

    #[test]
    fn differing_content_changes_the_key() {
        let mut other = draft();
        other.tokens.output = TokenField::exact(21);
        assert_ne!(dedupe_key(&draft()), dedupe_key(&other));
    }

    #[test]
    fn source_event_id_takes_priority_over_content() {
        let a = draft().with_source_event_id("evt-1");
        let mut b = draft().with_source_event_id("evt-1");
        b.model = Some("something-else".to_string());
        assert_eq!(dedupe_key(&a), dedupe_key(&b));
    }

    #[test]
    fn same_content_from_different_sources_differs() {
        let mut codex = draft();
        codex.source_app = SourceApp::Codex;
        assert_ne!(dedupe_key(&draft()), dedupe_key(&codex));
    }

    #[test]
    fn record_id_is_derived_from_the_key() {
        let key = dedupe_key(&draft());
        assert_eq!(record_id("cursor", &key), format!("cursor_{}", &key[..16]));
    }
}
