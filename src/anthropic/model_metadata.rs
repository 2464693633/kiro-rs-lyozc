//! Published model facts, not an attestation of the upstream's model weights.
//! Resolve the configured backend first; never apply a family-wide fallback.

use serde::Deserialize;
use std::sync::LazyLock;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Catalog {
    verified_on: String,
    models: Vec<KnowledgeCutoff>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KnowledgeCutoff {
    ids: Vec<String>,
    reliable: String,
    training: String,
    source: String,
}

static CATALOG: LazyLock<Catalog> = LazyLock::new(|| {
    serde_json::from_str(include_str!("knowledge_cutoffs.json"))
        .expect("bundled knowledge cutoff catalog must be valid")
});

fn for_backend(backend: &str) -> Option<&'static KnowledgeCutoff> {
    CATALOG
        .models
        .iter()
        .find(|row| row.ids.iter().any(|id| id == backend))
}

pub(super) fn knowledge_context(request_model: &str) -> String {
    let cutoff =
        super::converter::map_model(request_model).and_then(|backend| for_backend(&backend));
    let Some(cutoff) = cutoff else {
        return "No verified knowledge-cutoff metadata is available for this configured model. If asked about its cutoff, say it is unverified; do not estimate a year from remembered events, another model, the release date, or today's date.".into();
    };
    format!(
        "Published knowledge-cutoff metadata for the configured model, checked on {}: reliable_knowledge_cutoff={}; training_data_cutoff={}; source={}. Answer cutoff questions directly from these supplied dates, including follow-ups asking for an approximate date without browsing. For an unqualified knowledge-cutoff question, use the reliable cutoff; distinguish the training cutoff when asked. Use month precision, and do not substitute remembered events or earlier self-estimates. No live search is needed to report this supplied metadata. Mention it only when relevant and preserve the requested output format. This documents the configured model; it does not verify the serving model's weights or guarantee knowledge of every event before that month.",
        CATALOG.verified_on, cutoff.reliable, cutoff.training, cutoff.source
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn knowledge_catalog_has_unique_ids_valid_months_and_official_sources() {
        let mut ids = HashSet::new();
        for row in &CATALOG.models {
            assert!(!row.ids.is_empty());
            for id in &row.ids {
                assert!(ids.insert(id));
            }
            for month in [&row.reliable, &row.training] {
                assert_eq!(month.len(), 7);
                assert!(
                    chrono::NaiveDate::parse_from_str(&format!("{month}-01"), "%Y-%m-%d").is_ok()
                );
                assert!(month.as_str() <= CATALOG.verified_on.as_str());
            }
            assert!(row.reliable <= row.training);
            assert!(
                row.source
                    .starts_with("https://platform.claude.com/docs/en/models/")
            );
        }
    }

    #[test]
    fn knowledge_cutoffs_follow_exact_versions_and_keep_both_definitions() {
        for id in [
            "claude-opus-5-5",
            "claude-opus-5.5",
            "claude-opus-5-5-thinking",
        ] {
            let note = knowledge_context(id);
            assert!(note.contains("reliable_knowledge_cutoff=2026-06"));
            assert!(note.contains("training_data_cutoff=2026-06"));
            assert!(note.contains("/opus-5-5/overview"));
        }
        let sonnet = knowledge_context("claude-sonnet-4-6");
        assert!(sonnet.contains("reliable_knowledge_cutoff=2025-08"));
        assert!(sonnet.contains("training_data_cutoff=2026-01"));
        let haiku = knowledge_context("claude-haiku-4-5-20251001");
        assert!(haiku.contains("reliable_knowledge_cutoff=2025-02"));
        assert!(haiku.contains("training_data_cutoff=2025-07"));
    }

    #[test]
    fn knowledge_cutoffs_never_guess_for_unknown_models_or_families() {
        for id in [
            "claude-opus-5.6",
            "claude-opus-55",
            "claude-unverified",
            "auto",
            "gpt-5.6-sol",
        ] {
            let note = knowledge_context(id);
            assert!(note.starts_with("No verified knowledge-cutoff metadata"));
            assert!(!note.contains("2026-06"));
        }
        assert!(for_backend("claude-opus-5-5-experimental").is_none());
    }
}
