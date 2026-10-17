use std::path::{Component, Path};

use lk_core::config::VaultDirs;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::Serialize;

use crate::{VaultError, search_index};

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LinkDirection {
    Outgoing,
    Incoming,
    Both,
}

pub struct IndexedLinkOptions<'a> {
    pub snapshot: &'a str,
    pub direction: LinkDirection,
    pub limit: usize,
    pub offset: usize,
}

#[derive(Debug, Serialize)]
pub struct IndexedLink {
    pub source: String,
    pub source_revision: String,
    pub line: usize,
    pub label: String,
    pub destination: String,
    pub target: Option<String>,
    pub context: String,
    pub context_truncated: bool,
}

#[derive(Debug, Serialize)]
pub struct IndexedLinks {
    pub snapshot: String,
    pub path: String,
    pub direction: LinkDirection,
    pub file_revision: Option<String>,
    pub links: Vec<IndexedLink>,
    pub total: usize,
    pub offset: usize,
    pub next_offset: Option<usize>,
}

/// Query captured explicit link addresses, without consulting current source files.
/// Incoming links can address a missing page. No name or filesystem alias is inferred.
pub fn inspect_indexed_links(
    root: &Path,
    dirs: &VaultDirs,
    path: &Path,
    options: &IndexedLinkOptions<'_>,
) -> Result<IndexedLinks, VaultError> {
    if !(1..=100).contains(&options.limit)
        || path.extension().is_none_or(|extension| extension != "md")
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(std::io::Error::other(
            "indexed links requires an exact vault-relative Markdown address and limit 1..100",
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
            "SELECT json_extract(metadata, '$.file_revision') FROM pages WHERE path = ?1",
            [path],
            |row| row.get(0),
        )
        .optional()?;
    if revision.is_none() && matches!(options.direction, LinkDirection::Outgoing) {
        return Err(
            std::io::Error::other("source page is absent from the selected snapshot").into(),
        );
    }
    let predicate = match options.direction {
        LinkDirection::Outgoing => "source = ?1",
        LinkDirection::Incoming => "target = ?1",
        LinkDirection::Both => "source = ?1 OR target = ?1",
    };
    let total: i64 = transaction.query_row(
        &format!("SELECT count(*) FROM links WHERE {predicate}"),
        [path],
        |row| row.get(0),
    )?;
    let total = usize::try_from(total).map_err(std::io::Error::other)?;
    let mut links = Vec::new();
    if options.offset < total {
        let mut statement = transaction.prepare(&format!(
            "SELECT source, source_revision, line, label, destination, target, context, context_truncated
             FROM links WHERE {predicate} ORDER BY source, ordinal LIMIT ?2 OFFSET ?3",
        ))?;
        let rows = statement.query_map(
            params![path, options.limit as i64, options.offset as i64],
            |row| {
                Ok(IndexedLink {
                    source: row.get(0)?,
                    source_revision: row.get(1)?,
                    line: usize::try_from(row.get::<_, i64>(2)?).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            2,
                            rusqlite::types::Type::Integer,
                            Box::new(error),
                        )
                    })?,
                    label: row.get(3)?,
                    destination: row.get(4)?,
                    target: row.get(5)?,
                    context: row.get(6)?,
                    context_truncated: row.get(7)?,
                })
            },
        )?;
        for row in rows {
            links.push(row?);
        }
    }
    let end = options.offset.saturating_add(links.len());
    Ok(IndexedLinks {
        snapshot: options.snapshot.to_owned(),
        path: path.to_owned(),
        direction: options.direction,
        file_revision: revision,
        links,
        total,
        offset: options.offset,
        next_offset: (end < total).then_some(end),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MutationSession, prepare_search};

    #[test]
    fn captured_links_preserve_evidence_and_paginate_both_directions() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let concepts = root.path().join(&dirs.wiki).join("concepts");
        std::fs::create_dir_all(&concepts).unwrap();
        let a = format!("{}/concepts/a.md", dirs.wiki);
        let missing = format!("{}/concepts/missing.md", dirs.wiki);
        std::fs::write(concepts.join("a.md"),
            "---\ntype: concept\ntitle: A\n---\n[Self](a.md)\n[Missing](missing.md)\n[Again](missing.md#x)\n`[Code](missing.md)`\n").unwrap();
        std::fs::write(
            concepts.join("b.md"),
            format!("{}[A](a.md)\n", "지식".repeat(500)),
        )
        .unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let snapshot = prepare_search(&session, &dirs).unwrap();
        let mut options = IndexedLinkOptions {
            snapshot: &snapshot.snapshot,
            direction: LinkDirection::Both,
            limit: 2,
            offset: 0,
        };
        let first = inspect_indexed_links(root.path(), &dirs, Path::new(&a), &options).unwrap();
        assert_eq!(first.total, 4);
        assert_eq!(first.links[0].line, 5);
        assert_eq!(
            first.links[0].source_revision,
            first.file_revision.as_ref().unwrap().as_str()
        );
        assert_eq!(first.next_offset, Some(2));
        options.offset = 2;
        let next = inspect_indexed_links(root.path(), &dirs, Path::new(&a), &options).unwrap();
        assert_eq!(next.links[0].label, "Again");
        assert_eq!(next.links[1].label, "A");
        assert_eq!(next.links[1].context.chars().count(), 400);
        assert!(next.links[1].context_truncated);
        assert!(!next.links[0].context_truncated);
        assert_eq!(next.next_offset, None);
        options.direction = LinkDirection::Incoming;
        options.offset = 0;
        let absent =
            inspect_indexed_links(root.path(), &dirs, Path::new(&missing), &options).unwrap();
        assert_eq!(absent.total, 2);
        assert!(absent.file_revision.is_none());
        assert_eq!(absent.links[1].destination, "missing.md");
        assert!(absent.links[1].context.contains("missing.md#x"));
        std::fs::remove_file(concepts.join("a.md")).unwrap();
        assert_eq!(
            inspect_indexed_links(root.path(), &dirs, Path::new(&missing), &options)
                .unwrap()
                .total,
            2
        );
        std::fs::write(concepts.join("b.md"), "---\ninvalid: [\n---\n").unwrap();
        assert!(prepare_search(&session, &dirs).is_err());
        assert_eq!(
            inspect_indexed_links(root.path(), &dirs, Path::new(&missing), &options)
                .unwrap()
                .total,
            2
        );
        std::fs::write(concepts.join("b.md"), "No links.\n").unwrap();
        let updated = prepare_search(&session, &dirs).unwrap();
        assert!(inspect_indexed_links(root.path(), &dirs, Path::new(&missing), &options).is_err());
        options.snapshot = &updated.snapshot;
        assert_eq!(
            inspect_indexed_links(root.path(), &dirs, Path::new(&missing), &options)
                .unwrap()
                .total,
            0
        );
        options.limit = 101;
        assert!(inspect_indexed_links(root.path(), &dirs, Path::new(&missing), &options).is_err());
    }
}
