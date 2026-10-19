use std::path::Path;

use lk_core::config::VaultDirs;
use rusqlite::{Connection, OpenFlags, params};
use serde::Serialize;

use crate::search::{MatchProjection, Page, SearchOptions, SearchResult, fold_name, spacing_key};
use crate::{MutationSession, VaultError};

pub(super) const INDEX_PATH: &str = ".lorekeeper/search.sqlite";
const VERSION: i32 = 10;

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
    let mut pages = super::search::load_pages(root, dirs, true)?;
    super::search::attach_reviews(root, dirs, &mut pages)?;
    let provenance = crate::provenance_index::capture(root, dirs, &pages)?;
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
        .map(|page| (&page.rel_path, &page.file_revision, &page.reviews))
        .collect::<Vec<_>>();
    let identity = serde_json::to_string(&(VERSION, &context, manifest, &provenance))
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
        DROP TABLE IF EXISTS names;
        DROP TABLE IF EXISTS tags;
        DROP TABLE IF EXISTS pages;
        DROP TABLE IF EXISTS snapshot;
        CREATE TABLE snapshot (id TEXT NOT NULL, context TEXT NOT NULL, version INTEGER NOT NULL);
        CREATE TABLE pages (id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE, title TEXT NOT NULL,
            aliases TEXT NOT NULL, concept INTEGER NOT NULL, knowledge INTEGER NOT NULL, body TEXT NOT NULL, metadata TEXT NOT NULL, intro TEXT NOT NULL,
            category TEXT NOT NULL, format TEXT NOT NULL, source_status TEXT NOT NULL, created_date TEXT, path_rank INTEGER NOT NULL,
            knowledge_status TEXT NOT NULL, summary TEXT NOT NULL, text TEXT NOT NULL, name_text TEXT NOT NULL, spacing_text TEXT NOT NULL, spacing_name_text TEXT NOT NULL);
        CREATE INDEX browse_title ON pages(knowledge, title, path_rank);
        CREATE INDEX browse_created ON pages(knowledge, created_date DESC, path_rank);
        CREATE INDEX browse_all_title ON pages(title, path_rank);
        CREATE INDEX browse_all_created ON pages(created_date DESC, path_rank);
        CREATE VIRTUAL TABLE terms USING fts5(name_text, text, spacing_name_text, spacing_text, content='pages', content_rowid='id', tokenize='trigram case_sensitive 1');
        CREATE TABLE names (page_id INTEGER NOT NULL, name TEXT NOT NULL, identity_key TEXT NOT NULL, key TEXT NOT NULL, spacing_key TEXT NOT NULL, PRIMARY KEY(page_id, name));
        CREATE INDEX names_identity ON names(identity_key, page_id);
        CREATE INDEX names_key ON names(key, page_id);
        CREATE INDEX names_spacing ON names(spacing_key, page_id);
        CREATE TABLE tags (page_id INTEGER NOT NULL, tag TEXT NOT NULL, PRIMARY KEY(page_id, tag));
        CREATE INDEX tags_tag ON tags(tag, page_id);
        CREATE TABLE links (source TEXT NOT NULL, ordinal INTEGER NOT NULL,
            source_revision TEXT NOT NULL, line INTEGER NOT NULL, label TEXT NOT NULL,
            destination TEXT NOT NULL, target TEXT, context TEXT NOT NULL,
            context_truncated INTEGER NOT NULL, rss_evidence TEXT NOT NULL,
            PRIMARY KEY (source, ordinal));
        CREATE INDEX links_target ON links(target, source, ordinal);")?;
    {
        let mut insert_page = transaction.prepare(
            "INSERT INTO pages VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
        )?;
        let mut insert_terms =
            transaction.prepare("INSERT INTO terms(rowid, name_text, text, spacing_name_text, spacing_text) VALUES (?1, ?2, ?3, ?4, ?5)")?;
        let mut insert_link = transaction
            .prepare("INSERT INTO links VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)")?;
        let mut insert_name =
            transaction.prepare("INSERT OR IGNORE INTO names VALUES (?1, ?2, ?3, ?4, ?5)")?;
        let mut insert_tag = transaction.prepare("INSERT INTO tags VALUES (?1, ?2)")?;
        for (id, page) in pages.iter().enumerate() {
            let aliases = serde_json::to_string(&page.aliases)
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
            let projection = page.match_projection();
            let name_text = page.names().map(fold_name).collect::<Vec<_>>().join("\n");
            let spacing_name_text = super::search::compact_blocks(&name_text);
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
                page.page.body,
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
                path_ranks[&page.rel_path] as i64,
                page.knowledge_status().as_str(),
                projection.summary,
                projection.text,
                name_text,
                projection.spacing_text,
                spacing_name_text,
            ])?;
            insert_terms.execute(params![
                id as i64,
                name_text,
                projection.text,
                spacing_name_text,
                projection.spacing_text
            ])?;
            for name in page.names() {
                insert_name.execute(params![
                    id as i64,
                    name,
                    lk_core::concept::identity_key(name).unwrap_or_default(),
                    fold_name(name),
                    spacing_key(name)
                ])?;
            }
            for tag in super::search::search_tags(&page.page) {
                insert_tag.execute(params![id as i64, tag])?;
            }
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
    crate::provenance_index::write(&transaction, &provenance)?;
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
    let terms = super::search::query_terms(query, options.phrase);
    let vocabulary = |column: &str, key: &str| -> Result<Vec<Page>, VaultError> {
        let mut statement = transaction.prepare(&format!(
            "SELECT DISTINCT pages.metadata FROM names JOIN pages ON pages.id = names.page_id WHERE pages.concept = 1 AND names.{column} = ?1 ORDER BY pages.path"
        ))?;
        let rows = statement.query_map([key], |row| row.get::<_, String>(0))?;
        rows.map(|row| {
            serde_json::from_str(&row?)
                .map_err(|error| VaultError::Serialization(error.to_string()))
        })
        .collect()
    };
    let mut owners = vocabulary("identity_key", identity.as_deref().unwrap_or_default())?;
    if owners.is_empty() {
        owners = vocabulary("key", &fold_name(query))?;
    }
    if owners.is_empty() {
        owners = vocabulary("spacing_key", &spacing_key(query))?;
    }
    let spellings = super::search::spellings_of(identity.as_deref(), &owners);
    let needles = if spellings.is_empty() {
        &terms
    } else {
        &spellings
    };
    let mut predicates = Vec::new();
    let mut arguments = Vec::new();
    for needle in needles {
        let mut variants = Vec::new();
        for (value, columns) in std::iter::once((needle.clone(), ["name_text", "text"])).chain(
            options
                .spacing
                .then(|| (spacing_key(needle), ["spacing_name_text", "spacing_text"])),
        ) {
            if value.chars().count() >= 3 && !value.contains('\0') {
                arguments.push(format!(
                    "{{{} {}}} : \"{}\"",
                    columns[0],
                    columns[1],
                    value.replace('"', "\"\"")
                ));
                variants.push(format!(
                    "id IN (SELECT rowid FROM terms WHERE terms MATCH ?{})",
                    arguments.len()
                ));
            } else {
                arguments.push(value);
                variants.push(format!(
                    "(instr({}, ?{2}) > 0 OR instr({}, ?{2}) > 0)",
                    columns[0],
                    columns[1],
                    arguments.len()
                ));
            }
        }
        predicates.push(format!("({})", variants.join(" OR ")));
    }
    arguments.push(spacing_key(query));
    let filter = format!(
        "(({}) OR id IN (SELECT page_id FROM names WHERE spacing_key = ?{}))",
        predicates.join(if spellings.is_empty() {
            " AND "
        } else {
            " OR "
        }),
        arguments.len()
    );
    let scope = if options.include_sources {
        "1"
    } else {
        "knowledge = 1"
    };
    let sql = format!(
        "SELECT metadata, summary, text, spacing_text FROM pages WHERE {scope} AND {filter} ORDER BY id"
    );
    let mut statement = transaction.prepare(&sql)?;
    let rows = statement.query_map(rusqlite::params_from_iter(&arguments), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    let mut pages = Vec::new();
    for row in rows {
        let (metadata, summary, text, spacing_text) = row?;
        let mut page: Page = serde_json::from_str(&metadata)
            .map_err(|error| VaultError::Serialization(error.to_string()))?;
        page.projection = Some(MatchProjection {
            summary,
            text,
            spacing_text,
        });
        pages.push(page);
    }
    let mut result =
        super::search::search_pages(options, &pages, &spellings, &terms, identity.as_deref());
    let mut body = transaction.prepare("SELECT body FROM pages WHERE path = ?1")?;
    for hit in &mut result.hits {
        let captured: String = body.query_row([&hit.path], |row| row.get(0))?;
        hit.excerpt = super::search::excerpt_with_spacing(
            &captured,
            hit.matched,
            &terms,
            &spellings,
            hit.spacing_match,
        );
    }
    Ok(result)
}

/// Exact captured metadata filters shared by browse and raw-body traversal.
fn scalar_filter(options: &SearchOptions<'_>) -> (String, Vec<rusqlite::types::Value>) {
    let scope = if options.include_sources {
        "1"
    } else {
        "knowledge = 1"
    };
    let mut predicates = vec![scope.to_string()];
    let mut arguments = Vec::<rusqlite::types::Value>::new();
    for (column, value) in [
        ("category", options.category),
        ("format", options.format),
        ("source_status", options.source_status),
        (
            "knowledge_status",
            options.knowledge_status.map(|status| status.as_str()),
        ),
    ] {
        if let Some(value) = value {
            arguments.push(value.to_string().into());
            predicates.push(format!("{column} = ?{}", arguments.len()));
        }
    }
    for tag in options.tags {
        arguments.push(super::search::tag_key(tag).into());
        predicates.push(format!(
            "id IN (SELECT page_id FROM tags WHERE tag = ?{})",
            arguments.len()
        ));
    }
    for (operator, value) in [(">=", options.created_from), ("<=", options.created_to)] {
        if let Some(value) = value {
            arguments.push(value.to_string().into());
            predicates.push(format!("created_date {operator} ?{}", arguments.len()));
        }
    }
    let filter = predicates.join(" AND ");
    (filter, arguments)
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
    let knowledge_statuses = facets("knowledge_status")?;
    let mut tag_statement = connection.prepare(&format!(
        "SELECT tag, COUNT(*) FROM tags JOIN pages ON pages.id = tags.page_id WHERE {scope} GROUP BY tag"
    ))?;
    let tags = tag_statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as usize))
        })?
        .collect::<Result<_, _>>()?;
    let (filter, mut arguments) = scalar_filter(options);
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
    result.knowledge_statuses = knowledge_statuses;
    result.tags = tags;
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
    options: &SearchOptions<'_>,
    expected: &str,
    mut visit: impl FnMut(Page, usize) -> Result<(), VaultError>,
) -> Result<(), VaultError> {
    let path = crate::page_path(root, Path::new(INDEX_PATH))?;
    let mut connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let transaction = connection.transaction()?;
    verify_snapshot(&transaction, root, dirs, expected)?;
    let (filter, arguments) = scalar_filter(options);
    let mut statement = transaction.prepare(&format!(
        "SELECT metadata, body FROM pages WHERE {filter} ORDER BY id"
    ))?;
    let rows = statement.query_map(rusqlite::params_from_iter(&arguments), |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (metadata, body) = row?;
        let mut page: Page = serde_json::from_str(&metadata)
            .map_err(|error| VaultError::Serialization(error.to_string()))?;
        page.page.body = body;
        let offset = page.body_line_offset;
        visit(page, offset)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_html_search_matches_live_and_indexed_without_crossing_cells() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        write(
            root.path(),
            "wiki/documents/table.md",
            "---\ntype: document\ntitle: Result\n---\n## Summary\nObserved table.\n\n<table><tr><td rowspan=\"2\">검색 <strong>품질</strong></td><td>별도 셀</td></tr><tr><td>추가</td></tr></table>\n",
        );
        let session = MutationSession::acquire(root.path()).unwrap();
        let snapshot = prepare_search(&session, &dirs).unwrap();
        for (query, spacing, total) in [
            ("검색 품질", false, 1),
            ("검색품질", true, 1),
            ("품질 별도", false, 0),
            ("품질별도", true, 0),
        ] {
            let mut options = SearchOptions {
                query: Some(query),
                phrase: true,
                spacing,
                ..Default::default()
            };
            let live = crate::search(root.path(), &dirs, &options).unwrap();
            options.snapshot = Some(&snapshot.snapshot);
            let indexed = crate::search(root.path(), &dirs, &options).unwrap();
            assert_eq!(live.total, total, "{query}");
            assert_eq!(
                serde_json::to_value(&live.hits).unwrap(),
                serde_json::to_value(&indexed.hits).unwrap(),
                "{query}"
            );
            if total > 0 {
                assert_eq!(live.hits[0].excerpt, "검색 품질");
            }
        }
    }

    #[test]
    fn spacing_preserves_original_ascii_word_edges_in_sentences() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        for (name, body) in [
            ("spaced", "We run Cloud Run for hosting."),
            ("joined", "We run CloudRun for hosting."),
            ("prefix", "We run MyCloudRun for hosting."),
            ("suffix", "We run CloudRunTime for hosting."),
            ("paragraphs", "We run Cloud\n\nRun for hosting."),
            ("other", "Cloudflare runs the service."),
        ] {
            write(
                root.path(),
                &format!("wiki/documents/{name}.md"),
                &format!("---\ntype: document\ntitle: {name}\n---\n## Summary\n{body}\n"),
            );
        }
        let session = MutationSession::acquire(root.path()).unwrap();
        let snapshot = prepare_search(&session, &dirs).unwrap();
        for query in ["CloudRun", "Cloud Run"] {
            for spacing in [false, true] {
                let mut options = SearchOptions {
                    query: Some(query),
                    phrase: true,
                    spacing,
                    ..Default::default()
                };
                let live = crate::search(root.path(), &dirs, &options).unwrap();
                options.snapshot = Some(&snapshot.snapshot);
                let indexed = crate::search(root.path(), &dirs, &options).unwrap();
                assert_eq!(
                    serde_json::to_value(&live.hits).unwrap(),
                    serde_json::to_value(&indexed.hits).unwrap()
                );
                assert_eq!(live.total, if spacing { 2 } else { 1 });
                assert!(!live.hits[0].spacing_match);
                if spacing {
                    assert!(live.hits[1].spacing_match);
                    assert!(live.hits[1].excerpt.contains(if query == "CloudRun" {
                        "Cloud Run"
                    } else {
                        "CloudRun"
                    }));
                }
            }
        }
    }

    #[test]
    fn exact_identity_precedes_folded_and_spacing_alias_owners() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        for (name, title, alias) in [
            ("upper", "Go", "Language Alias"),
            ("lower", "go", "Board Alias"),
            ("circled", "①", "Circled Alias"),
            ("number", "1", "Number Alias"),
            ("spaced", "AB C", "Spaced Alias"),
            ("joined", "ABC", "Joined Alias"),
        ] {
            write(
                root.path(),
                &format!("wiki/concepts/{name}.md"),
                &format!(
                    "---\ntype: concept\ntitle: '{title}'\naliases: [{alias}]\n---\n## Summary\nDistinct {name} subject.\n"
                ),
            );
            write(
                root.path(),
                &format!("wiki/documents/{name}.md"),
                &format!(
                    "---\ntype: document\ntitle: {name} source\n---\n## Summary\n{alias} is described here.\n"
                ),
            );
        }
        let session = MutationSession::acquire(root.path()).unwrap();
        let snapshot = prepare_search(&session, &dirs).unwrap();
        for (query, owner) in [
            ("Go", "upper"),
            ("go", "lower"),
            ("①", "circled"),
            ("1", "number"),
            ("AB C", "spaced"),
            ("ABC", "joined"),
        ] {
            let mut options = SearchOptions {
                query: Some(query),
                format: Some("document"),
                ..Default::default()
            };
            let live = crate::search(root.path(), &dirs, &options).unwrap();
            options.snapshot = Some(&snapshot.snapshot);
            let indexed = crate::search(root.path(), &dirs, &options).unwrap();
            assert_eq!(
                serde_json::to_value(&live.hits).unwrap(),
                serde_json::to_value(&indexed.hits).unwrap(),
                "{query}"
            );
            assert_eq!(live.total, 1, "{query}");
            assert_eq!(live.hits[0].path, format!("wiki/documents/{owner}.md"));
        }
        let plan: String = rusqlite::Connection::open(root.path().join(INDEX_PATH))
            .unwrap()
            .query_row(
                "EXPLAIN QUERY PLAN SELECT page_id FROM names WHERE identity_key='Go'",
                [],
                |row| row.get(3),
            )
            .unwrap();
        assert!(plan.contains("names_identity"), "{plan}");
    }

    #[test]
    fn spacing_operator_is_explicit_block_bounded_and_identical_in_sqlite() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        for (name, body) in [
            ("spaced", "서비스의 인증 정보를 보호한다."),
            ("joined", "서비스의 인증정보를 보호한다."),
            ("paragraphs", "인증\n\n정보"),
            ("list", "- 인증\n- 정보"),
            ("table", "| 인증 | 정보 |\n| --- | --- |\n| other | value |"),
            ("link", "[참고](https://example.com/인증정보)"),
        ] {
            write(
                root.path(),
                &format!("wiki/documents/{name}.md"),
                &format!("---\ntype: document\ntitle: {name}\n---\n## Summary\n{body}\n"),
            );
        }
        let session = MutationSession::acquire(root.path()).unwrap();
        let snapshot = prepare_search(&session, &dirs).unwrap();
        for query in ["인증정보", "인증 정보"] {
            for spacing in [false, true] {
                let mut options = SearchOptions {
                    query: Some(query),
                    phrase: true,
                    spacing,
                    ..Default::default()
                };
                let live = crate::search(root.path(), &dirs, &options).unwrap();
                options.snapshot = Some(&snapshot.snapshot);
                let indexed = crate::search(root.path(), &dirs, &options).unwrap();
                assert_eq!(
                    serde_json::to_value(&live.hits).unwrap(),
                    serde_json::to_value(&indexed.hits).unwrap()
                );
                assert_eq!(live.total, if spacing { 2 } else { 1 });
                if spacing {
                    assert!(!live.hits[0].spacing_match);
                    assert!(live.hits[1].spacing_match);
                    assert!(live.hits[1].excerpt.contains(if query == "인증정보" {
                        "인증 정보"
                    } else {
                        "인증정보"
                    }));
                }
            }
        }
        for (phrase, spacing) in [(true, false), (false, true)] {
            assert!(
                crate::search_body(
                    root.path(),
                    &dirs,
                    &crate::BodySearchOptions {
                        search: SearchOptions {
                            query: Some("인증정보"),
                            phrase,
                            spacing,
                            ..Default::default()
                        },
                        ..Default::default()
                    }
                )
                .is_err()
            );
        }
    }

    #[test]
    fn discovery_names_tags_phrases_and_retirement_share_one_projection() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        for (path, content) in [
            (
                "wiki/concepts/injection.md",
                "---\ntype: concept\ntitle: Prompt Injection\naliases: [프롬프트 인젝션]\n---\n## 핵심\n비신뢰 입력이 지시로 해석되는 문제.",
            ),
            (
                "wiki/documents/credential.md",
                "---\ntype: document\ntitle: Credential setup\nsource_record:\n  manual_frontmatter:\n    tags: [Security, api]\n---\n## Summary\nAPI 토큰 설정을 설명한다.",
            ),
            (
                "wiki/documents/model.md",
                "---\ntype: document\ntitle: Model\nsource_count: 999\n---\n## Summary\nAPI 응답 형식과 토큰 한도.",
            ),
            (
                "wiki/concepts/retired.md",
                "---\ntype: concept\ntitle: Unsupported\nknowledge_status: retired\n---\n## Summary\nUnverified historical definition.",
            ),
            (
                "wiki/concepts/one.md",
                "---\ntype: concept\ntitle: AB C\naliases: [First meaning]\n---\n## Summary\nFirst.",
            ),
            (
                "wiki/concepts/two.md",
                "---\ntype: concept\ntitle: A BC\naliases: [Second meaning]\n---\n## Summary\nSecond.",
            ),
            (
                "wiki/documents/meaning.md",
                "---\ntype: document\ntitle: Other\n---\n## Summary\nFirst meaning explained.",
            ),
        ] {
            write(root.path(), path, content);
        }
        let session = MutationSession::acquire(root.path()).unwrap();
        let snapshot = prepare_search(&session, &dirs).unwrap();
        for query in ["프롬프트인젝션", "API 토큰", "ABC", "Unsupported"] {
            let mut options = SearchOptions {
                query: Some(query),
                limit: 100,
                ..Default::default()
            };
            let live = crate::search(root.path(), &dirs, &options).unwrap();
            options.snapshot = Some(&snapshot.snapshot);
            let indexed = crate::search(root.path(), &dirs, &options).unwrap();
            assert_eq!(
                serde_json::to_value(&live.hits).unwrap(),
                serde_json::to_value(&indexed.hits).unwrap(),
                "{query}"
            );
            match query {
                "프롬프트인젝션" => {
                    assert_eq!(indexed.hits[0].matched, crate::MatchField::NameVariant)
                }
                "API 토큰" => {
                    assert_eq!(indexed.hits[0].path, "wiki/documents/credential.md");
                    options.phrase = true;
                    assert_eq!(
                        crate::search(root.path(), &dirs, &options).unwrap().total,
                        1
                    );
                }
                "ABC" => assert_eq!(
                    indexed.total, 2,
                    "ambiguous variants must not borrow either owner's aliases"
                ),
                _ => {
                    assert_eq!(indexed.total, 0);
                    options.knowledge_status = Some(lk_core::frontmatter::KnowledgeStatus::Retired);
                    assert_eq!(
                        crate::search(root.path(), &dirs, &options).unwrap().total,
                        1
                    );
                }
            }
        }
        let tags = vec!["SECURITY".to_owned(), "api".to_owned()];
        let options = SearchOptions {
            tags: &tags,
            snapshot: Some(&snapshot.snapshot),
            ..Default::default()
        };
        let indexed = crate::search(root.path(), &dirs, &options).unwrap();
        assert_eq!(indexed.total, 1);
        assert_eq!(indexed.hits[0].tags, ["api", "security"]);
        let plan: String = rusqlite::Connection::open(root.path().join(INDEX_PATH))
            .unwrap()
            .query_row(
                "EXPLAIN QUERY PLAN SELECT page_id FROM names WHERE key='prompt injection'",
                [],
                |row| row.get(3),
            )
            .unwrap();
        assert!(plan.contains("names_key"), "{plan}");
    }

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
            "knowledge/map.md",
            "knowledge/log.md",
            "knowledge/catalog/index/0.md",
            "knowledge/catalog/map/0.md",
            "knowledge/catalog/log/0.md",
        ] {
            write(
                root.path(),
                path,
                "---\ntype: concept\ntitle: Needle\n---\n## Claim\nNeedle evidence.\n",
            );
        }
        for path in ["knowledge/index.md", "knowledge/AGENTS.md"] {
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
        assert_eq!(live.total, 10);
        for path in [
            "knowledge/map.md",
            "knowledge/log.md",
            "knowledge/catalog/index/0.md",
        ] {
            assert!(live.hits.iter().any(|hit| hit.path == path));
        }
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
        assert_eq!(body.total, 10);
        let session = MutationSession::acquire(root.path()).unwrap();
        let prepared = prepare_search(&session, &dirs).unwrap();
        assert_eq!(prepared.pages, 10);
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
