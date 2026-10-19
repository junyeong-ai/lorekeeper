//! Prepared exact citation discovery. Graph derives evidence; this module only stores and
//! paginates its complete projection. Explicit link occurrences are a separate read surface.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use lk_core::concept::{Citation, citation_digest};
use lk_core::config::VaultDirs;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::Serialize;

use crate::{VaultError, search_index};

/// Complete graph-derived evidence, including concepts with no current citations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConceptCitationSet {
    pub path: PathBuf,
    pub source_count: usize,
    pub citations: Vec<Citation>,
}

/// Rebuildable projection supplied by graph authority. The input revision covers its
/// captured whole-vault existence and filesystem-resolution inputs, not just search scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConceptCitationProjection {
    pub input_revision: String,
    pub concepts: Vec<ConceptCitationSet>,
}

pub struct CitationOptions<'a> {
    pub snapshot: &'a str,
    pub limit: usize,
    pub offset: usize,
}

#[derive(Debug, Serialize)]
pub struct ConceptCitations {
    pub snapshot: String,
    pub path: String,
    pub source_count: usize,
    pub digest: String,
    pub citations: Vec<Citation>,
    /// Complete citation count; several RSS event selectors can share one source page.
    pub total: usize,
    pub offset: usize,
    pub next_offset: Option<usize>,
}

fn invalid(message: impl Into<String>) -> VaultError {
    std::io::Error::other(message.into()).into()
}

fn integer(value: usize) -> Result<i64, VaultError> {
    i64::try_from(value).map_err(|error| invalid(error.to_string()))
}

pub(super) fn write(
    connection: &Connection,
    projection: Option<&ConceptCitationProjection>,
) -> Result<(), VaultError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS concept_evidence(path TEXT PRIMARY KEY, source_count INTEGER NOT NULL,
             citation_count INTEGER NOT NULL, digest TEXT NOT NULL, input_revision TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS concept_citations(concept TEXT NOT NULL, ordinal INTEGER NOT NULL,
             payload TEXT NOT NULL, PRIMARY KEY(concept, ordinal));",
    )?;
    let mut stored = {
        let mut query = connection.prepare(
            "SELECT path, source_count, citation_count, digest, input_revision FROM concept_evidence",
        )?;
        query
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    (
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ),
                ))
            })?
            .collect::<Result<BTreeMap<_, _>, _>>()?
    };
    let Some(projection) = projection else {
        connection.execute("DELETE FROM concept_citations", [])?;
        connection.execute("DELETE FROM concept_evidence", [])?;
        return Ok(());
    };
    let mut header = connection.prepare("INSERT INTO concept_evidence VALUES (?1,?2,?3,?4,?5)
        ON CONFLICT(path) DO UPDATE SET source_count=excluded.source_count,
            citation_count=excluded.citation_count, digest=excluded.digest, input_revision=excluded.input_revision")?;
    let mut citation = connection.prepare("INSERT INTO concept_citations VALUES (?1,?2,?3)")?;
    let mut seen = BTreeSet::new();
    for concept in &projection.concepts {
        let path = concept
            .path
            .to_str()
            .ok_or_else(|| invalid("concept address is not UTF-8"))?;
        if !seen.insert(path) {
            return Err(invalid("duplicate concept evidence address"));
        }
        // The public digest intentionally ignores order and repetition; row reuse
        // must additionally preserve the provider's exact ordered pagination values.
        let input = serde_json::to_string(&concept.citations)
            .map_err(|error| invalid(error.to_string()))?;
        let state = (
            integer(concept.source_count)?,
            integer(concept.citations.len())?,
            citation_digest(&concept.citations),
            lk_core::concept::file_revision(&input),
        );
        if stored.remove(path).as_ref() == Some(&state) {
            continue;
        }
        header.execute(params![path, state.0, state.1, state.2, state.3,])?;
        connection.execute("DELETE FROM concept_citations WHERE concept=?1", [path])?;
        for (ordinal, value) in concept.citations.iter().enumerate() {
            citation.execute(params![
                concept.path.to_str().expect("validated concept address"),
                integer(ordinal)?,
                serde_json::to_string(value).map_err(|error| invalid(error.to_string()))?,
            ])?;
        }
    }
    for path in stored.keys() {
        connection.execute("DELETE FROM concept_citations WHERE concept=?1", [path])?;
        connection.execute("DELETE FROM concept_evidence WHERE path=?1", [path])?;
    }
    Ok(())
}

/// Query captured exact citations without reading current sources or implying that they
/// remain current. Refreshing the database invalidates pagination against the old snapshot.
pub fn inspect_concept_citations(
    root: &Path,
    dirs: &VaultDirs,
    path: &Path,
    options: &CitationOptions<'_>,
) -> Result<ConceptCitations, VaultError> {
    if !(1..=100).contains(&options.limit)
        || path.extension().is_none_or(|extension| extension != "md")
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(invalid(
            "citations requires an exact vault-relative Markdown concept address and limit 1..100",
        ));
    }
    let path = path
        .to_str()
        .ok_or_else(|| invalid("concept address is not UTF-8"))?;
    let database = crate::page_path(root, Path::new(search_index::INDEX_PATH))?;
    let mut connection = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let transaction = connection.transaction()?;
    search_index::verify_snapshot(&transaction, root, dirs, options.snapshot)?;
    let available: bool =
        transaction.query_row("SELECT citations_available FROM snapshot", [], |row| {
            row.get(0)
        })?;
    if !available {
        return Err(invalid(
            "selected snapshot has no graph evidence capture; run wiki prepare",
        ));
    }
    let header: Option<(i64, i64, String)> = transaction
        .query_row(
            "SELECT source_count, citation_count, digest FROM concept_evidence WHERE path = ?1",
            [path],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let (source_count, total, digest) = header.ok_or_else(|| {
        invalid("exact concept page is absent from the selected evidence snapshot")
    })?;
    let source_count = usize::try_from(source_count).map_err(|error| invalid(error.to_string()))?;
    let total = usize::try_from(total).map_err(|error| invalid(error.to_string()))?;
    let mut citations = Vec::new();
    if options.offset < total {
        let mut statement = transaction.prepare(
            "SELECT payload FROM concept_citations WHERE concept = ?1
             ORDER BY ordinal LIMIT ?2 OFFSET ?3",
        )?;
        let rows = statement.query_map(
            params![path, integer(options.limit)?, integer(options.offset)?],
            |row| row.get::<_, String>(0),
        )?;
        for row in rows {
            citations.push(
                serde_json::from_str(&row?)
                    .map_err(|error| invalid(format!("invalid captured citation: {error}")))?,
            );
        }
    }
    let end = options.offset.saturating_add(citations.len());
    Ok(ConceptCitations {
        snapshot: options.snapshot.to_owned(),
        path: path.to_owned(),
        source_count,
        digest,
        citations,
        total,
        offset: options.offset,
        next_offset: (end < total).then_some(end),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incremental_citations_preserve_exact_order_and_explicit_capture_capability() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let path = Path::new("wiki/concepts/topic.md");
        std::fs::create_dir_all(root.path().join(path.parent().unwrap())).unwrap();
        std::fs::write(
            root.path().join(path),
            "---\ntype: concept\ntitle: Topic\n---\nEvidence.\n",
        )
        .unwrap();
        let session = crate::MutationSession::acquire(root.path()).unwrap();
        let mut projection = ConceptCitationProjection {
            input_revision: "capture".into(),
            concepts: vec![ConceptCitationSet {
                path: path.into(),
                source_count: 2,
                citations: ["daily/a.md", "daily/b.md"]
                    .into_iter()
                    .map(|path| Citation::at_revision(Path::new(path), "revision".into()))
                    .collect(),
            }],
        };
        let before =
            crate::prepare_search_with_citations(&session, &dirs, &projection, |_| Ok(())).unwrap();
        projection.concepts[0].citations.reverse();
        let reordered =
            crate::prepare_search_with_citations(&session, &dirs, &projection, |_| Ok(())).unwrap();
        let read = inspect_concept_citations(
            root.path(),
            &dirs,
            path,
            &CitationOptions {
                snapshot: &reordered.snapshot,
                limit: 100,
                offset: 0,
            },
        )
        .unwrap();
        assert_ne!(before.snapshot, reordered.snapshot);
        assert_eq!(read.citations, projection.concepts[0].citations);
        let retained_digest = read.digest;
        projection.concepts.clear();
        let empty =
            crate::prepare_search_with_citations(&session, &dirs, &projection, |_| Ok(())).unwrap();
        assert!(empty.citations_available);
        assert!(
            inspect_concept_citations(
                root.path(),
                &dirs,
                path,
                &CitationOptions {
                    snapshot: &empty.snapshot,
                    limit: 100,
                    offset: 0,
                }
            )
            .is_err()
        );
        let lexical = crate::prepare_search(&session, &dirs).unwrap();
        assert!(!lexical.citations_available);
        let error = inspect_concept_citations(
            root.path(),
            &dirs,
            path,
            &CitationOptions {
                snapshot: &lexical.snapshot,
                limit: 100,
                offset: 0,
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("no graph evidence capture"));
        assert_eq!(
            retained_digest,
            citation_digest(&[
                Citation::at_revision(Path::new("daily/a.md"), "revision".into()),
                Citation::at_revision(Path::new("daily/b.md"), "revision".into()),
            ])
        );
    }

    #[test]
    fn prepared_citations_paginate_complete_values_and_preserve_snapshot_on_rejection() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let path = Path::new("wiki/concepts/topic.md");
        std::fs::create_dir_all(root.path().join(path.parent().unwrap())).unwrap();
        std::fs::write(
            root.path().join(path),
            "---\ntype: concept\ntitle: Topic\n---\n## Synthesis\nPreserved prose.\n",
        )
        .unwrap();
        let session = crate::MutationSession::acquire(root.path()).unwrap();
        let lexical = crate::prepare_search(&session, &dirs).unwrap();
        assert!(!lexical.citations_available);
        let mut options = CitationOptions {
            snapshot: &lexical.snapshot,
            limit: 100,
            offset: 0,
        };
        assert!(
            inspect_concept_citations(root.path(), &dirs, path, &options)
                .unwrap_err()
                .to_string()
                .contains("no graph evidence capture")
        );
        let mut expected = (0..201)
            .map(|index| {
                Citation::at_revision(
                    Path::new(&format!("daily/source-{index:03}.md")),
                    format!("revision-{index}"),
                )
            })
            .collect::<Vec<_>>();
        for index in 1..=2 {
            expected.push(Citation {
                path: "daily/feed.md".into(),
                revision: "a".repeat(64),
                rss_event: Some(lk_core::rss_evidence::RssEventSelector {
                    schema: lk_core::rss_evidence::RSS_EVIDENCE_SCHEMA.into(),
                    event_id: format!("rss:2026-10-18:{index:016x}"),
                }),
            });
        }
        expected.sort();
        let projection = ConceptCitationProjection {
            input_revision: "captured-input".into(),
            concepts: vec![ConceptCitationSet {
                path: path.into(),
                source_count: 202,
                citations: expected.clone(),
            }],
        };
        let prepared =
            crate::prepare_search_with_citations(&session, &dirs, &projection, |selected| {
                assert_eq!(selected.len(), 1);
                Ok(())
            })
            .unwrap();
        assert!(prepared.citations_available);
        options.snapshot = &prepared.snapshot;
        let mut read = Vec::new();
        loop {
            let page = inspect_concept_citations(root.path(), &dirs, path, &options).unwrap();
            assert_eq!(page.total, 203);
            assert_eq!(page.source_count, 202);
            assert_eq!(page.digest, citation_digest(&expected));
            assert!(page.citations.len() <= 100);
            read.extend(page.citations);
            let Some(offset) = page.next_offset else {
                break;
            };
            options.offset = offset;
        }
        assert_eq!(read, expected);
        options.offset = usize::MAX;
        let empty = inspect_concept_citations(root.path(), &dirs, path, &options).unwrap();
        assert!(empty.citations.is_empty());
        assert_eq!(empty.next_offset, None);
        for limit in [0, 101] {
            options.limit = limit;
            assert!(inspect_concept_citations(root.path(), &dirs, path, &options).is_err());
        }
        options.limit = 1;
        for address in [
            "../wiki/concepts/topic.md",
            "/wiki/concepts/topic.md",
            "wiki/concepts/missing.md",
            "wiki/concepts/topic.md#part",
        ] {
            assert!(
                inspect_concept_citations(root.path(), &dirs, Path::new(address), &options)
                    .is_err()
            );
        }
        let before = std::fs::read(root.path().join(search_index::INDEX_PATH)).unwrap();
        let mut changed = projection.clone();
        changed.input_revision = "changed-input".into();
        assert!(
            crate::prepare_search_with_citations(&session, &dirs, &changed, |_| Err(invalid(
                "graph input drift"
            )))
            .is_err()
        );
        assert_eq!(
            std::fs::read(root.path().join(search_index::INDEX_PATH)).unwrap(),
            before
        );
        options.offset = 0;
        assert_eq!(
            inspect_concept_citations(root.path(), &dirs, path, &options)
                .unwrap()
                .citations[0],
            expected[0]
        );
        assert!(
            crate::prepare_search_with_citations(&session, &dirs, &projection, |_| Err(invalid(
                "reuse must also verify"
            )))
            .is_err()
        );
        // Rebuilding an absent derived database preserves the same evidence and identity.
        std::fs::remove_file(root.path().join(search_index::INDEX_PATH)).unwrap();
        let rebuilt =
            crate::prepare_search_with_citations(&session, &dirs, &projection, |_| Ok(())).unwrap();
        assert_eq!(rebuilt.snapshot, prepared.snapshot);
        assert_eq!(
            inspect_concept_citations(root.path(), &dirs, path, &options)
                .unwrap()
                .digest,
            citation_digest(&expected)
        );
        crate::prepare_search_with_citations(&session, &dirs, &changed, |_| Ok(())).unwrap();
        assert!(inspect_concept_citations(root.path(), &dirs, path, &options).is_err());
    }
}
