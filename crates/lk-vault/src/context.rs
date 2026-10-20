use std::path::{Component, Path};

use lk_core::config::VaultDirs;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::Serialize;

use crate::{
    CitationOptions, ConceptCitations, IndexedLinkOptions, IndexedLinks, LinkDirection,
    ProvenanceDirection, ProvenanceOptions, ProvenanceReport, SearchSnapshot, VaultError,
    search_index,
};

pub struct ContextOptions<'a> {
    pub snapshot: &'a str,
    pub limit: usize,
    pub offset: usize,
}

#[derive(Serialize)]
#[serde(tag = "status", content = "data", rename_all = "kebab-case")]
pub enum ContextEvidence {
    Available(ConceptCitations),
    NotCaptured,
    NotApplicable,
}

#[derive(Serialize)]
#[serde(tag = "status", content = "data", rename_all = "kebab-case")]
pub enum ContextRelations {
    Available(ProvenanceReport),
    NotCaptured,
    NotApplicable,
}

#[derive(Serialize)]
pub struct ContextReviews {
    pub reviews: Vec<crate::review::ReviewPreview>,
    pub total: usize,
    pub offset: usize,
    pub next_offset: Option<usize>,
}

#[derive(Serialize)]
pub struct PageContext {
    pub capture: SearchSnapshot,
    pub path: String,
    pub title: String,
    pub file_revision: String,
    pub format: Option<String>,
    pub knowledge_status: lk_core::frontmatter::KnowledgeStatus,
    pub synthesis: crate::SynthesisFreshness,
    /// Bounded opening preview in source spelling; full content uses a revision-pinned read.
    pub summary: String,
    pub links: IndexedLinks,
    pub evidence: ContextEvidence,
    pub declared_relations: ContextRelations,
    pub reviews: ContextReviews,
    pub limit: usize,
    pub offset: usize,
    pub next_offset: Option<usize>,
}

/// A captured first step for relationship exploration. Each relation retains its own
/// authority; explicit links and provider declarations do not become semantic support.
pub fn inspect_page_context(
    root: &Path,
    dirs: &VaultDirs,
    path: &Path,
    options: &ContextOptions<'_>,
) -> Result<PageContext, VaultError> {
    if !(1..=100).contains(&options.limit)
        || path.extension().is_none_or(|value| value != "md")
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(std::io::Error::other(
            "context requires an exact vault-relative Markdown address and limit 1..100",
        )
        .into());
    }
    let path_text = path
        .to_str()
        .ok_or_else(|| std::io::Error::other("page address is not UTF-8"))?;
    let database = crate::page_path(root, Path::new(search_index::INDEX_PATH))?;
    let mut connection = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let transaction = connection.transaction()?;
    search_index::verify_snapshot(&transaction, root, dirs, options.snapshot)?;
    let capture = search_index::read_snapshot(&transaction)?;
    let row: Option<(String, String)> = transaction
        .query_row(
            "SELECT metadata, intro FROM pages WHERE path = ?1",
            [path_text],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (metadata, summary) =
        row.ok_or_else(|| std::io::Error::other("page is absent from the selected snapshot"))?;
    let page: crate::search::Page = serde_json::from_str(&metadata)
        .map_err(|error| VaultError::Serialization(error.to_string()))?;
    let format = page
        .page
        .frontmatter
        .get("type")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let links = crate::indexed_links::inspect_captured(
        &transaction,
        path_text,
        &IndexedLinkOptions {
            snapshot: options.snapshot,
            direction: LinkDirection::Both,
            limit: options.limit,
            offset: options.offset,
        },
    )?;
    let evidence = if !page.concept {
        ContextEvidence::NotApplicable
    } else if !capture.citations_available {
        ContextEvidence::NotCaptured
    } else {
        ContextEvidence::Available(crate::citations::inspect_captured(
            &transaction,
            path_text,
            &CitationOptions {
                snapshot: options.snapshot,
                limit: options.limit,
                offset: options.offset,
            },
        )?)
    };
    let synthesis =
        crate::SynthesisFreshness::from_frontmatter(&page.page.frontmatter, page.concept);
    let synthesis = match &evidence {
        ContextEvidence::Available(evidence) => {
            synthesis.with_evidence(evidence.total, &evidence.digest)
        }
        _ => synthesis,
    };
    let provenance_captured: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM provenance_addresses WHERE path = ?1)",
        [path_text],
        |row| row.get(0),
    )?;
    let declared_relations = if format.as_deref() != Some("document") {
        ContextRelations::NotApplicable
    } else if !provenance_captured {
        ContextRelations::NotCaptured
    } else {
        ContextRelations::Available(crate::provenance_index::inspect_captured(
            &transaction,
            path,
            &ProvenanceOptions {
                snapshot: Some(options.snapshot),
                direction: ProvenanceDirection::Both,
                relation: None,
                limit: options.limit,
                offset: options.offset,
                revision: None,
            },
            options.snapshot,
        )?)
    };
    let total = page.reviews.len();
    let reviews = page
        .reviews
        .into_iter()
        .skip(options.offset)
        .take(options.limit)
        .collect::<Vec<_>>();
    let end = options.offset.saturating_add(reviews.len());
    let reviews = ContextReviews {
        reviews,
        total,
        offset: options.offset,
        next_offset: (end < total).then_some(end),
    };
    let more = links.next_offset.is_some()
        || matches!(&evidence, ContextEvidence::Available(value) if value.next_offset.is_some())
        || matches!(&declared_relations, ContextRelations::Available(value) if value.next_offset.is_some())
        || reviews.next_offset.is_some();
    Ok(PageContext {
        capture,
        path: path_text.to_owned(),
        title: page.title,
        file_revision: page.file_revision,
        format,
        knowledge_status: page.knowledge_status,
        synthesis,
        summary,
        links,
        evidence,
        declared_relations,
        reviews,
        limit: options.limit,
        offset: options.offset,
        next_offset: more.then_some(options.offset.saturating_add(options.limit)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_is_bounded_and_preserves_source_spelling() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        std::fs::create_dir_all(root.path().join("wiki/concepts")).unwrap();
        let path = Path::new("wiki/concepts/topic.md");
        let short = "Keep APIKey and Kelvin distinct.";
        for body in [short.to_owned(), "APIKey Kelvin 보존 정책 ".repeat(400)] {
            std::fs::write(
                root.path().join(path),
                format!("---\ntype: concept\ntitle: Topic\n---\n## Synthesis\n\n{body}\n"),
            )
            .unwrap();
            let session = crate::MutationSession::acquire(root.path()).unwrap();
            let capture = crate::prepare_search(&session, &dirs).unwrap();
            let context = inspect_page_context(
                root.path(),
                &dirs,
                path,
                &ContextOptions {
                    snapshot: &capture.snapshot,
                    limit: 1,
                    offset: 0,
                },
            )
            .unwrap();
            assert!(context.summary.len() <= 203);
            if body == short {
                assert_eq!(context.summary, short);
            } else {
                assert!(context.summary.starts_with("APIKey Kelvin 보존 정책"));
                assert!(context.summary.ends_with('…'));
            }
        }
    }

    #[test]
    fn uncaptured_evidence_is_distinct_from_empty_and_scoped_reviews_paginate() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        for directory in ["wiki/concepts", "wiki/explorations", "daily"] {
            std::fs::create_dir_all(root.path().join(directory)).unwrap();
        }
        let concept = "wiki/concepts/topic.md";
        std::fs::write(
            root.path().join(concept),
            "---\ntype: concept\ntitle: Topic\n---\nScoped knowledge.\n",
        )
        .unwrap();
        let original = "---\ntype: document\ntitle: Observation\n---\nDirect observation.\n";
        std::fs::write(root.path().join("daily/observation.md"), original).unwrap();
        let revision = lk_core::concept::source_revision(&crate::parse_page(original).unwrap());
        for number in 0..3 {
            let review = serde_json::json!({"status":"open","subjects":[concept],"evidence":[{"path":"daily/observation.md","revision":revision}],"resolution":null});
            std::fs::write(root.path().join(format!("wiki/explorations/review-{number}.md")), format!("---\ntype: exploration\ntitle: Review {number}\nreview: {review}\n---\nUnresolved scoped finding {number}.\n")).unwrap();
        }
        let session = crate::MutationSession::acquire(root.path()).unwrap();
        let prepared = crate::prepare_search(&session, &dirs).unwrap();
        let mut options = ContextOptions {
            snapshot: &prepared.snapshot,
            limit: 1,
            offset: 0,
        };
        let first = inspect_page_context(root.path(), &dirs, Path::new(concept), &options).unwrap();
        assert!(matches!(first.evidence, ContextEvidence::NotCaptured));
        assert_eq!(first.reviews.total, 3);
        assert_eq!(first.reviews.reviews.len(), 1);
        assert_eq!(first.next_offset, Some(1));
        options.offset = 1;
        let second =
            inspect_page_context(root.path(), &dirs, Path::new(concept), &options).unwrap();
        assert_ne!(
            serde_json::to_value(first.reviews.reviews).unwrap(),
            serde_json::to_value(second.reviews.reviews).unwrap()
        );
        let outside = inspect_page_context(
            root.path(),
            &dirs,
            Path::new("daily/observation.md"),
            &options,
        )
        .unwrap();
        assert!(matches!(
            outside.declared_relations,
            ContextRelations::NotCaptured
        ));
        assert!(matches!(outside.evidence, ContextEvidence::NotApplicable));
    }
}
