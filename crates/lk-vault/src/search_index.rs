use std::path::{Path, PathBuf};

use lk_core::config::VaultDirs;
use rusqlite::{Connection, OpenFlags, params};
use serde::Serialize;

use crate::search::{Page, SearchOptions, SearchResult, fold, fold_name, search_text};
use crate::{MutationSession, VaultError};

pub(super) const INDEX_PATH: &str = ".lorekeeper/search.sqlite";
const VERSION: i32 = 6;

#[derive(Serialize)]
pub struct SearchSnapshot {
    pub snapshot: String,
    pub pages: usize,
}

fn context(root: &Path, dirs: &VaultDirs) -> Result<String, VaultError> {
    Ok(format!(
        "{}\n{}",
        root.canonicalize()?.display(),
        serde_json::to_string(dirs)
            .map_err(|error| VaultError::Serialization(error.to_string()))?
    ))
}

fn invalid(message: impl Into<String>) -> VaultError {
    std::io::Error::other(message.into()).into()
}

/// Replace a reconstructible search snapshot in one SQLite transaction. The source
/// files are read under the cooperating writer lock; external editors are not frozen.
pub fn prepare_search(
    session: &MutationSession,
    dirs: &VaultDirs,
) -> Result<SearchSnapshot, VaultError> {
    let root = session.root();
    let pages = super::search::load_pages(root, dirs, true)?;
    // Body-search pagination retains traversal order; browse ties use PathBuf ordering.
    let mut ordered_paths = pages.iter().map(|page| &page.rel_path).collect::<Vec<_>>();
    ordered_paths.sort();
    let path_ranks = ordered_paths
        .into_iter()
        .enumerate()
        .map(|(rank, path)| (path, rank))
        .collect::<std::collections::BTreeMap<_, _>>();
    let context = context(root, dirs)?;
    let manifest = pages
        .iter()
        .map(|page| (&page.rel_path, &page.file_revision))
        .collect::<Vec<_>>();
    let identity = serde_json::to_string(&(VERSION, &context, manifest))
        .map_err(|error| VaultError::Serialization(error.to_string()))?;
    let snapshot = lk_core::concept::file_revision(&identity);
    let path = crate::writable_page_path(root, Path::new(INDEX_PATH))?;
    let mut connection = Connection::open(path)?;
    if connection.table_exists(Some("main"), "snapshot")? {
        let current: (String, String, i32) =
            connection.query_row("SELECT id, context, version FROM snapshot", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?;
        if current == (snapshot.clone(), context.clone(), VERSION) {
            return Ok(SearchSnapshot {
                snapshot,
                pages: pages.len(),
            });
        }
    }
    let transaction = connection.transaction()?;
    transaction.execute_batch("DROP TABLE IF EXISTS links;
        DROP TABLE IF EXISTS terms;
        DROP TABLE IF EXISTS pages;
        DROP TABLE IF EXISTS snapshot;
        CREATE TABLE snapshot (id TEXT NOT NULL, context TEXT NOT NULL, version INTEGER NOT NULL);
        CREATE TABLE pages (id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE, title TEXT NOT NULL,
            aliases TEXT NOT NULL, concept INTEGER NOT NULL, knowledge INTEGER NOT NULL, payload TEXT NOT NULL, metadata TEXT NOT NULL, intro TEXT NOT NULL,
            category TEXT NOT NULL, format TEXT NOT NULL, source_status TEXT NOT NULL, created_date TEXT, path_rank INTEGER NOT NULL);
        CREATE INDEX browse_title ON pages(knowledge, title, path_rank);
        CREATE INDEX browse_created ON pages(knowledge, created_date DESC, path_rank);
        CREATE INDEX browse_all_title ON pages(title, path_rank);
        CREATE INDEX browse_all_created ON pages(created_date DESC, path_rank);
        CREATE VIRTUAL TABLE terms USING fts5(text, tokenize='trigram case_sensitive 1');
        CREATE TABLE links (source TEXT NOT NULL, ordinal INTEGER NOT NULL,
            source_revision TEXT NOT NULL, line INTEGER NOT NULL, label TEXT NOT NULL,
            destination TEXT NOT NULL, target TEXT, context TEXT NOT NULL,
            context_truncated INTEGER NOT NULL, rss_evidence TEXT NOT NULL,
            PRIMARY KEY (source, ordinal));
        CREATE INDEX links_target ON links(target, source, ordinal);")?;
    {
        let mut insert_page = transaction.prepare(
            "INSERT INTO pages VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        )?;
        let mut insert_terms =
            transaction.prepare("INSERT INTO terms(rowid, text) VALUES (?1, ?2)")?;
        let mut insert_link = transaction
            .prepare("INSERT INTO links VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)")?;
        for (id, page) in pages.iter().enumerate() {
            let aliases = serde_json::to_string(&page.aliases)
                .map_err(|e| VaultError::Serialization(e.to_string()))?;
            let payload = serde_json::to_string(page)
                .map_err(|e| VaultError::Serialization(e.to_string()))?;
            let mut metadata = page.clone();
            metadata.page.body.clear();
            metadata
                .page
                .frontmatter
                .fields
                .remove(lk_core::rss_evidence::CONCEPT_EVIDENCE_FIELD);
            let metadata = serde_json::to_string(&metadata)
                .map_err(|error| VaultError::Serialization(error.to_string()))?;
            let intro = super::search::excerpt(
                &page.page.body,
                super::search::MatchField::Catalog,
                &[],
                &[],
            );
            let mut text = page.names().map(fold_name).collect::<Vec<_>>().join("\n");
            text.push('\n');
            text.push_str(&fold(&search_text(&page.page.body)));
            let knowledge = [
                lk_core::vault_path::concepts_dir(dirs),
                lk_core::vault_path::documents_dir(dirs),
                lk_core::vault_path::explorations_dir(dirs),
            ]
            .iter()
            .any(|dir| page.rel_path.starts_with(dir));
            insert_page.execute(params![
                id as i64,
                page.rel_path.to_string_lossy(),
                page.title,
                aliases,
                page.concept,
                knowledge,
                payload,
                metadata,
                intro,
                page.page
                    .frontmatter
                    .get("category")
                    .and_then(|v| v.as_str())
                    .unwrap_or(""),
                page.page
                    .frontmatter
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or(""),
                page.page
                    .frontmatter
                    .get("source_record")
                    .and_then(|v| v.get("status"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(""),
                page.created_date().map(|date| date.to_string()),
                path_ranks[&page.rel_path] as i64
            ])?;
            insert_terms.execute(params![id as i64, text])?;
            let evidence_previews = if page
                .page
                .frontmatter
                .get(lk_core::rss_evidence::CONCEPT_EVIDENCE_FIELD)
                .is_some()
            {
                let raw = std::fs::read_to_string(crate::page_path(root, &page.rel_path)?)?;
                if lk_core::concept::file_revision(&raw) != page.file_revision {
                    return Err(invalid("evidence source changed during snapshot"));
                }
                crate::rss_concept_citations(root, &page.rel_path, &raw)?;
                crate::rss_link_previews(root, &page.rel_path, &raw)?
            } else {
                std::collections::BTreeMap::new()
            };
            for (ordinal, occurrence) in
                lk_core::link::extract_page_link_occurrences(&page.page.body)
                    .into_iter()
                    .enumerate()
            {
                let target = lk_core::link::resolve_dest(&page.rel_path, &occurrence.link.dest);
                let (context, context_truncated) =
                    crate::links::context_preview(occurrence.context);
                let evidence = evidence_previews.get(&(page.body_line_offset + occurrence.line));
                let evidence =
                    serde_json::to_string(&evidence).map_err(|e| invalid(e.to_string()))?;
                insert_link.execute(params![
                    page.rel_path.to_string_lossy(),
                    ordinal as i64,
                    page.file_revision,
                    (page.body_line_offset + occurrence.line) as i64,
                    occurrence.link.text,
                    occurrence.link.dest,
                    target.map(|path| path.to_string_lossy().into_owned()),
                    context,
                    context_truncated,
                    evidence,
                ])?;
            }
        }
    }
    transaction.execute(
        "INSERT INTO snapshot VALUES (?1, ?2, ?3)",
        params![snapshot, context, VERSION],
    )?;
    transaction.commit()?;
    Ok(SearchSnapshot {
        snapshot,
        pages: pages.len(),
    })
}

pub(super) fn search(
    root: &Path,
    dirs: &VaultDirs,
    options: &SearchOptions<'_>,
    expected: &str,
) -> Result<SearchResult, VaultError> {
    let path = crate::page_path(root, Path::new(INDEX_PATH))?;
    let mut connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let transaction = connection.transaction()?;
    verify_snapshot(&transaction, root, dirs, expected)?;
    if options.query.is_none() {
        return browse(&transaction, options);
    }
    let query = options.query.unwrap_or_default();
    let identity = lk_core::concept::identity_key(query);
    let terms: Vec<_> = query.split_whitespace().map(fold).collect();
    let mut vocabulary = Vec::new();
    {
        let mut statement = transaction
            .prepare("SELECT path, title, aliases FROM pages WHERE concept = 1 ORDER BY path")?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (path, title, aliases) = row?;
            vocabulary.push(Page {
                file_revision: String::new(),
                body_line_offset: 0,
                id: String::new(),
                title,
                rel_path: PathBuf::from(path),
                concept: true,
                aliases: serde_json::from_str(&aliases)
                    .map_err(|error| VaultError::Serialization(error.to_string()))?,
                page: crate::VaultPage {
                    original: None,
                    frontmatter: crate::Frontmatter::default(),
                    body: String::new(),
                },
            });
        }
    }
    let spellings = super::search::spellings_of(identity.as_deref(), &vocabulary);
    let needles = if spellings.is_empty() {
        &terms
    } else {
        &spellings
    };
    let mut predicates = Vec::new();
    let mut arguments = Vec::new();
    for needle in needles {
        if needle.chars().count() >= 3 && !needle.contains('\0') {
            arguments.push(format!("\"{}\"", needle.replace('"', "\"\"")));
            predicates.push(format!(
                "id IN (SELECT rowid FROM terms WHERE terms MATCH ?{})",
                arguments.len()
            ));
        } else {
            arguments.push(needle.clone());
            predicates.push(format!(
                "id IN (SELECT rowid FROM terms WHERE instr(text, ?{}) > 0)",
                arguments.len()
            ));
        }
    }
    let filter = format!(
        "({})",
        predicates.join(if spellings.is_empty() {
            " AND "
        } else {
            " OR "
        })
    );
    let scope = if options.include_sources {
        "1"
    } else {
        "knowledge = 1"
    };
    let sql = format!("SELECT payload FROM pages WHERE {scope} AND {filter} ORDER BY id");
    let mut statement = transaction.prepare(&sql)?;
    let rows = statement.query_map(rusqlite::params_from_iter(&arguments), |row| {
        row.get::<_, String>(0)
    })?;
    let mut pages = Vec::new();
    for row in rows {
        pages.push(
            serde_json::from_str(&row?)
                .map_err(|error| VaultError::Serialization(error.to_string()))?,
        );
    }
    Ok(super::search::search_pages(
        options,
        &pages,
        &spellings,
        &terms,
        identity.as_deref(),
    ))
}

/// Browse scalar metadata in SQLite; deserialize only the requested page of hits.
fn browse(
    connection: &Connection,
    options: &SearchOptions<'_>,
) -> Result<SearchResult, VaultError> {
    let scope = if options.include_sources {
        "1"
    } else {
        "knowledge = 1"
    };
    let facets = |column: &str| -> Result<std::collections::BTreeMap<String, usize>, VaultError> {
        let mut statement = connection.prepare(&format!(
            "SELECT {column}, COUNT(*) FROM pages WHERE {scope} GROUP BY {column}"
        ))?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as usize))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    };
    let categories = facets("category")?;
    let formats = facets("format")?;
    let source_statuses = facets("source_status")?;
    let mut predicates = vec![scope.to_string()];
    let mut arguments = Vec::<rusqlite::types::Value>::new();
    for (column, value) in [
        ("category", options.category),
        ("format", options.format),
        ("source_status", options.source_status),
    ] {
        if let Some(value) = value {
            arguments.push(value.to_string().into());
            predicates.push(format!("{column} = ?{}", arguments.len()));
        }
    }
    for (operator, value) in [(">=", options.created_from), ("<=", options.created_to)] {
        if let Some(value) = value {
            arguments.push(value.to_string().into());
            predicates.push(format!("created_date {operator} ?{}", arguments.len()));
        }
    }
    let filter = predicates.join(" AND ");
    let total: i64 = connection.query_row(
        &format!("SELECT COUNT(*) FROM pages WHERE {filter}"),
        rusqlite::params_from_iter(&arguments),
        |row| row.get(0),
    )?;
    let order = match options.sort {
        super::search::BrowseSort::Title => "title, path_rank",
        super::search::BrowseSort::Created => "created_date DESC, path_rank",
    };
    arguments.push(i64::try_from(options.limit).unwrap_or(i64::MAX).into());
    arguments.push(i64::try_from(options.offset).unwrap_or(i64::MAX).into());
    let mut statement = connection.prepare(&format!(
        "SELECT metadata, intro FROM pages WHERE {filter} ORDER BY {order} LIMIT ?{} OFFSET ?{}",
        arguments.len() - 1,
        arguments.len()
    ))?;
    let rows = statement.query_map(rusqlite::params_from_iter(&arguments), |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut pages = Vec::new();
    let mut intros = Vec::new();
    for row in rows {
        let (metadata, intro) = row?;
        pages.push(
            serde_json::from_str(&metadata)
                .map_err(|error| VaultError::Serialization(error.to_string()))?,
        );
        intros.push(intro);
    }
    let selected = SearchOptions {
        offset: 0,
        ..*options
    };
    let mut result = super::search::search_pages(&selected, &pages, &[], &[], None);
    for (hit, intro) in result.hits.iter_mut().zip(intros) {
        hit.excerpt = intro;
    }
    let total = total as usize;
    result.total = total;
    result.offset = options.offset;
    let end = options.offset.saturating_add(result.hits.len());
    result.next_offset = (end < total).then_some(end);
    result.categories = categories;
    result.formats = formats;
    result.source_statuses = source_statuses;
    Ok(result)
}

pub(super) fn verify_snapshot(
    connection: &Connection,
    root: &Path,
    dirs: &VaultDirs,
    expected: &str,
) -> Result<(), VaultError> {
    let (snapshot, recorded_context, version): (String, String, i32) =
        connection.query_row("SELECT id, context, version FROM snapshot", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
    if version != VERSION || recorded_context != context(root, dirs)? || snapshot != expected {
        return Err(invalid(
            "search snapshot does not match; run `lore wiki prepare` and restart navigation",
        ));
    }
    Ok(())
}

pub(super) fn visit_body_pages(
    root: &Path,
    dirs: &VaultDirs,
    include_sources: bool,
    expected: &str,
    mut visit: impl FnMut(Page, usize) -> Result<(), VaultError>,
) -> Result<(), VaultError> {
    let path = crate::page_path(root, Path::new(INDEX_PATH))?;
    let mut connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let transaction = connection.transaction()?;
    verify_snapshot(&transaction, root, dirs, expected)?;
    let mut statement =
        transaction.prepare("SELECT payload FROM pages WHERE ?1 OR knowledge = 1 ORDER BY id")?;
    let rows = statement.query_map([include_sources], |row| row.get::<_, String>(0))?;
    for row in rows {
        let page: Page = serde_json::from_str(&row?)
            .map_err(|error| VaultError::Serialization(error.to_string()))?;
        let offset = page.body_line_offset;
        visit(page, offset)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, path: &str, content: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn overlapping_source_roots_exclude_navigation_in_live_body_and_snapshot_searches() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs {
            wiki: "knowledge".into(),
            daily: "knowledge".into(),
            personal: "personal".into(),
            ..Default::default()
        };
        for path in [
            "knowledge/concepts/a.md",
            "knowledge/catalogue/a.md",
            "knowledge/feed/catalog/a.md",
            "knowledge/feed/index.md",
            "personal/catalog/a.md",
        ] {
            write(
                root.path(),
                path,
                "---\ntype: concept\ntitle: Needle\n---\n## Claim\nNeedle evidence.\n",
            );
        }
        for path in [
            "knowledge/index.md",
            "knowledge/map.md",
            "knowledge/log.md",
            "knowledge/AGENTS.md",
            "knowledge/catalog/index/0.md",
            "knowledge/catalog/map/0.md",
            "knowledge/catalog/log/0.md",
        ] {
            // Exclusion must happen before frontmatter parsing, not by generated type.
            write(
                root.path(),
                path,
                "---\ninvalid: [\n---\nNeedle navigation\n",
            );
        }
        let mut options = SearchOptions {
            query: Some("Needle"),
            include_sources: true,
            ..Default::default()
        };
        let live = crate::search(root.path(), &dirs, &options).unwrap();
        assert_eq!(live.total, 5);
        assert!(
            live.hits
                .iter()
                .all(|hit| !hit.path.starts_with("knowledge/catalog/"))
        );
        let body = crate::search_body(
            root.path(),
            &dirs,
            &crate::BodySearchOptions {
                search: SearchOptions {
                    query: Some("Needle"),
                    include_sources: true,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(body.total, 5);
        let session = MutationSession::acquire(root.path()).unwrap();
        let prepared = prepare_search(&session, &dirs).unwrap();
        assert_eq!(prepared.pages, 5);
        options.snapshot = Some(&prepared.snapshot);
        let snapshot = crate::search(root.path(), &dirs, &options).unwrap();
        assert_eq!(snapshot.total, live.total);
        assert_eq!(
            serde_json::to_value(snapshot.hits).unwrap(),
            serde_json::to_value(live.hits).unwrap()
        );
        let connection = Connection::open(root.path().join(INDEX_PATH)).unwrap();
        connection
            .execute("UPDATE snapshot SET version = 1", [])
            .unwrap();
        assert!(crate::search(root.path(), &dirs, &options).is_err());
    }

    #[test]
    fn indexed_queries_preserve_live_semantics_and_pin_captured_revisions() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        write(
            root.path(),
            "wiki/concepts/kg.md",
            "---\ntitle: Knowledge Graph\naliases: [지식그래프, 지식 그래프, KG]\ntype: concept\ncategory: theory\n---\n## Synthesis\nA **knowledge**\ngraph connects evidence.\n",
        );
        write(
            root.path(),
            "wiki/documents/korean.md",
            "---\ntitle: Korean\ntype: document\n---\n## Summary\n지식그래프를 탐색한다. C# and C++ are distinct.\n",
        );
        write(
            root.path(),
            "wiki/documents/words.md",
            "---\ntitle: Words\ntype: document\n---\n## Summary\nknowledge elsewhere.\n\nGraph.\n",
        );
        write(
            root.path(),
            "daily/feed/day.md",
            "---\ntitle: Feed\ntype: daily\n---\n## Summary\nKG, 기호, 그리고 \"quotes\".\n",
        );
        let session = MutationSession::acquire(root.path()).unwrap();
        let prepared = prepare_search(&session, &dirs).unwrap();
        assert_eq!(prepared.pages, 4);
        assert_eq!(
            prepare_search(&session, &dirs).unwrap().snapshot,
            prepared.snapshot
        );
        for query in [
            None,
            Some("Knowledge Graph"),
            Some("지식그래프"),
            Some("KG"),
            Some("기호"),
            Some("C#"),
            Some("C++"),
            Some("\"quotes\""),
            Some("nothing"),
            Some("knowledge elsewhere"),
        ] {
            for include_sources in [false, true] {
                let mut options = SearchOptions {
                    query,
                    include_sources,
                    limit: 2,
                    ..Default::default()
                };
                let live = crate::search(root.path(), &dirs, &options).unwrap();
                options.snapshot = Some(&prepared.snapshot);
                let indexed = crate::search(root.path(), &dirs, &options).unwrap();
                assert_eq!(
                    serde_json::to_value(&live.hits).unwrap(),
                    serde_json::to_value(&indexed.hits).unwrap(),
                    "{query:?}"
                );
                assert_eq!(live.total, indexed.total);
                assert_eq!(live.categories, indexed.categories);
                assert_eq!(live.formats, indexed.formats);
            }
        }
        let options = SearchOptions {
            query: Some("지식그래프"),
            snapshot: Some(&prepared.snapshot),
            ..Default::default()
        };
        let captured = crate::search(root.path(), &dirs, &options).unwrap();
        write(
            root.path(),
            "wiki/concepts/kg.md",
            "---\ntitle: Changed\ntype: concept\n---\n## Synthesis\nNew claim.\n",
        );
        let frozen = crate::search(root.path(), &dirs, &options).unwrap();
        assert_eq!(frozen.hits[0].file_revision, captured.hits[0].file_revision);
        let read = crate::DocumentOptions {
            section: None,
            offset: 0,
            limit: 20,
            revision: Some(&frozen.hits[0].file_revision),
        };
        assert!(
            crate::inspect_document(root.path(), Path::new(&frozen.hits[0].path), &read).is_err()
        );
        prepare_search(&session, &dirs).unwrap();
        assert!(crate::search(root.path(), &dirs, &options).is_err());
    }

    #[test]
    fn failed_refresh_preserves_the_previous_snapshot_and_deletions_change_identity() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        write(
            root.path(),
            "wiki/concepts/a.md",
            "---\ntitle: Alpha\n---\nEvidence.",
        );
        let session = MutationSession::acquire(root.path()).unwrap();
        let snapshot = prepare_search(&session, &dirs).unwrap();
        write(
            root.path(),
            "wiki/concepts/b.md",
            "---\ntitle: [invalid\n---\n",
        );
        assert!(prepare_search(&session, &dirs).is_err());
        let options = SearchOptions {
            query: Some("Alpha"),
            snapshot: Some(&snapshot.snapshot),
            ..Default::default()
        };
        assert_eq!(
            crate::search(root.path(), &dirs, &options).unwrap().total,
            1
        );
        std::fs::remove_file(root.path().join("wiki/concepts/b.md")).unwrap();
        std::fs::remove_file(root.path().join("wiki/concepts/a.md")).unwrap();
        let empty = prepare_search(&session, &dirs).unwrap();
        assert_eq!(empty.pages, 0);
        assert_ne!(empty.snapshot, snapshot.snapshot);
        assert!(crate::search(root.path(), &dirs, &options).is_err());
    }
}
