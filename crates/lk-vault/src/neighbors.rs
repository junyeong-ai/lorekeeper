//! Captured one-hop page connections. Occurrences and semantic evidence retain their
//! separate read surfaces; a neighboring page is not an inferred supporting source.
use std::path::{Component, Path};

use lk_core::{config::VaultDirs, frontmatter::KnowledgeStatus};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::Serialize;

use crate::{VaultError, search_index};

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NeighborDirection {
    Incoming,
    Outgoing,
    Both,
    Mutual,
}

pub struct IndexedNeighborOptions<'a> {
    pub snapshot: &'a str,
    pub direction: NeighborDirection,
    pub format: Option<&'a str>,
    /// None includes active and retired pages; the center's status is unrestricted.
    pub knowledge_status: Option<KnowledgeStatus>,
    pub limit: usize,
    pub offset: usize,
}

#[derive(Debug, Serialize)]
pub struct IndexedNeighbor {
    pub path: String,
    pub title: String,
    pub format: Option<String>,
    pub knowledge_status: KnowledgeStatus,
    pub file_revision: String,
    pub incoming: bool,
    pub outgoing: bool,
}

#[derive(Debug, Serialize)]
pub struct IndexedNeighbors {
    pub snapshot: String,
    pub path: String,
    pub file_revision: String,
    pub direction: NeighborDirection,
    #[serde(rename = "type")]
    pub format: Option<String>,
    pub knowledge_status: Option<KnowledgeStatus>,
    pub neighbors: Vec<IndexedNeighbor>,
    pub total: usize,
    pub offset: usize,
    pub next_offset: Option<usize>,
}

const CONNECTIONS: &str = "WITH occurrences AS (
    SELECT source AS path, 1 AS incoming, 0 AS outgoing FROM links WHERE target=?1 AND source!=?1
    UNION ALL
    SELECT target AS path, 0 AS incoming, 1 AS outgoing FROM links WHERE source=?1 AND target!=?1
), connections AS (
    SELECT path, MAX(incoming) AS incoming, MAX(outgoing) AS outgoing
    FROM occurrences GROUP BY path
)";

/// List distinct captured neighboring pages in exact path order. Missing targets and
/// self-links are occurrences, not neighboring pages. Filesystem aliases are not inferred.
pub fn inspect_indexed_neighbors(
    root: &Path,
    dirs: &VaultDirs,
    path: &Path,
    options: &IndexedNeighborOptions<'_>,
) -> Result<IndexedNeighbors, VaultError> {
    if !(1..=100).contains(&options.limit)
        || path.extension().is_none_or(|extension| extension != "md")
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(std::io::Error::other(
            "neighbors requires an exact vault-relative Markdown address and limit 1..100",
        )
        .into());
    }
    let path = path
        .to_str()
        .ok_or_else(|| std::io::Error::other("page address is not UTF-8"))?;
    let database = crate::page_path(root, Path::new(search_index::INDEX_PATH))?;
    let mut connection = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let transaction = connection.transaction()?;
    search_index::verify_snapshot(&transaction, root, dirs, options.snapshot)?;
    let revision: Option<String> = transaction
        .query_row(
            "SELECT json_extract(metadata, '$.file_revision') FROM pages WHERE path=?1",
            [path],
            |row| row.get(0),
        )
        .optional()?;
    let revision = revision
        .ok_or_else(|| std::io::Error::other("page is absent from the selected snapshot"))?;
    let direction = match options.direction {
        NeighborDirection::Incoming => "c.incoming=1",
        NeighborDirection::Outgoing => "c.outgoing=1",
        NeighborDirection::Both => "1",
        NeighborDirection::Mutual => "c.incoming=1 AND c.outgoing=1",
    };
    let filter = format!(
        "{direction} AND (?2 IS NULL OR p.format=?2) AND (?3 IS NULL OR p.knowledge_status=?3)"
    );
    let status = options.knowledge_status.map(|status| status.as_str());
    let total: i64 = transaction.query_row(
        &format!("{CONNECTIONS} SELECT COUNT(*) FROM connections c JOIN pages p ON p.path=c.path WHERE {filter}"),
        params![path, options.format, status], |row| row.get(0),
    )?;
    let total = usize::try_from(total).map_err(std::io::Error::other)?;
    let mut neighbors = Vec::new();
    if options.offset < total {
        let mut query = transaction.prepare(&format!(
            "{CONNECTIONS} SELECT p.path, p.title, p.format, p.knowledge_status,
                json_extract(p.metadata, '$.file_revision'), c.incoming, c.outgoing
             FROM connections c JOIN pages p ON p.path=c.path WHERE {filter}
             ORDER BY p.path LIMIT ?4 OFFSET ?5"
        ))?;
        let rows = query.query_map(
            params![
                path,
                options.format,
                status,
                options.limit as i64,
                i64::try_from(options.offset).unwrap_or(i64::MAX)
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, bool>(5)?,
                    row.get::<_, bool>(6)?,
                ))
            },
        )?;
        for row in rows {
            let (path, title, format, status, file_revision, incoming, outgoing) = row?;
            let knowledge_status = serde_json::from_value(serde_json::Value::String(status))
                .map_err(|error| VaultError::Serialization(error.to_string()))?;
            neighbors.push(IndexedNeighbor {
                path,
                title,
                format: (!format.is_empty()).then_some(format),
                knowledge_status,
                file_revision,
                incoming,
                outgoing,
            });
        }
    }
    let end = options.offset.saturating_add(neighbors.len());
    Ok(IndexedNeighbors {
        snapshot: options.snapshot.to_owned(),
        path: path.to_owned(),
        file_revision: revision,
        direction: options.direction,
        format: options.format.map(str::to_owned),
        knowledge_status: options.knowledge_status,
        neighbors,
        total,
        offset: options.offset,
        next_offset: (end < total).then_some(end),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, path: &str, text: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn captured_neighbors_deduplicate_and_filter_before_pagination_without_live_reads() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let center = Path::new("wiki/concepts/center.md");
        write(
            root.path(),
            center.to_str().unwrap(),
            "---\ntype: concept\ntitle: Center\n---\n[B](../documents/b.md) [B again](../documents/b.md#section)\n[D](../../daily/feed/d.md)\n[Missing](missing.md) [Self](center.md)\n",
        );
        write(
            root.path(),
            "wiki/documents/b.md",
            "---\ntype: document\ntitle: B\n---\n[A](../concepts/center.md) [Again](../concepts/center.md)\n",
        );
        write(
            root.path(),
            "wiki/concepts/c.md",
            "---\ntype: concept\ntitle: C\nknowledge_status: retired\n---\n[A](center.md)\n",
        );
        write(
            root.path(),
            "daily/feed/d.md",
            "---\ntype: daily\ntitle: D\n---\nEvidence.\n",
        );
        let session = crate::MutationSession::acquire(root.path()).unwrap();
        let capture = crate::prepare_search(&session, &dirs).unwrap();
        let mut options = IndexedNeighborOptions {
            snapshot: &capture.snapshot,
            direction: NeighborDirection::Both,
            format: None,
            knowledge_status: Some(KnowledgeStatus::Active),
            limit: 1,
            offset: 0,
        };
        let first = inspect_indexed_neighbors(root.path(), &dirs, center, &options).unwrap();
        assert_eq!(first.total, 2);
        assert_eq!(first.next_offset, Some(1));
        assert_eq!(first.neighbors[0].path, "daily/feed/d.md");
        assert!(!first.neighbors[0].incoming && first.neighbors[0].outgoing);
        options.offset = 1;
        let next = inspect_indexed_neighbors(root.path(), &dirs, center, &options).unwrap();
        assert_eq!(next.neighbors[0].path, "wiki/documents/b.md");
        assert!(next.neighbors[0].incoming && next.neighbors[0].outgoing);
        assert_eq!(next.next_offset, None);
        let selected = crate::inspect_document(
            root.path(),
            Path::new(&next.neighbors[0].path),
            &crate::DocumentOptions {
                section: None,
                offset: 0,
                limit: 20,
                revision: Some(&next.neighbors[0].file_revision),
            },
        )
        .unwrap();
        assert_eq!(selected.title, "B");
        options.offset = 0;
        options.knowledge_status = None;
        assert_eq!(
            inspect_indexed_neighbors(root.path(), &dirs, center, &options)
                .unwrap()
                .total,
            3
        );
        options.knowledge_status = Some(KnowledgeStatus::Retired);
        let retired = inspect_indexed_neighbors(root.path(), &dirs, center, &options).unwrap();
        assert_eq!(retired.total, 1);
        assert_eq!(retired.neighbors[0].path, "wiki/concepts/c.md");
        options.knowledge_status = None;
        options.direction = NeighborDirection::Mutual;
        assert_eq!(
            inspect_indexed_neighbors(root.path(), &dirs, center, &options)
                .unwrap()
                .neighbors[0]
                .path,
            "wiki/documents/b.md"
        );
        options.direction = NeighborDirection::Incoming;
        assert_eq!(
            inspect_indexed_neighbors(root.path(), &dirs, center, &options)
                .unwrap()
                .total,
            2
        );
        options.direction = NeighborDirection::Outgoing;
        assert_eq!(
            inspect_indexed_neighbors(root.path(), &dirs, center, &options)
                .unwrap()
                .total,
            2
        );
        options.format = Some("document");
        std::fs::remove_file(root.path().join("wiki/documents/b.md")).unwrap();
        let retained = inspect_indexed_neighbors(root.path(), &dirs, center, &options).unwrap();
        assert_eq!(retained.total, 1);
        assert_eq!(
            retained.neighbors[0].file_revision,
            next.neighbors[0].file_revision
        );
        options.offset = usize::MAX;
        assert!(
            inspect_indexed_neighbors(root.path(), &dirs, center, &options)
                .unwrap()
                .neighbors
                .is_empty()
        );
        let occurrences = crate::inspect_indexed_links(
            root.path(),
            &dirs,
            center,
            &crate::IndexedLinkOptions {
                snapshot: &capture.snapshot,
                direction: crate::LinkDirection::Both,
                limit: 100,
                offset: 0,
            },
        )
        .unwrap();
        assert_eq!(occurrences.total, 8);
        assert!(
            inspect_indexed_neighbors(
                root.path(),
                &dirs,
                Path::new("wiki/concepts/missing.md"),
                &options
            )
            .is_err()
        );
        options.limit = 101;
        assert!(inspect_indexed_neighbors(root.path(), &dirs, center, &options).is_err());
        options.limit = 1;
        options.snapshot = "stale";
        assert!(inspect_indexed_neighbors(root.path(), &dirs, center, &options).is_err());
    }
}
