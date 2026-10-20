use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use lk_core::config::VaultDirs;
use rusqlite::{Connection, OpenFlags, params};
use serde::Serialize;

use crate::search::{MatchProjection, Page, SearchOptions, SearchResult, fold_name, spacing_key};
use crate::{MutationSession, VaultError};

pub(super) const INDEX_PATH: &str = ".lorekeeper/search.sqlite";
const VERSION: i32 = 14;

#[derive(Serialize)]
pub struct SearchSnapshot {
    pub snapshot: String,
    pub pages: usize,
    pub citations_available: bool,
    pub captured_at: jiff::Timestamp,
    pub verified_at: jiff::Timestamp,
}

/// Read the prepared capture without consulting source pages. The recorded verification
/// time describes the last full preparation, never the currency of external edits.
pub fn inspect_search_snapshot(
    root: &Path,
    dirs: &VaultDirs,
) -> Result<SearchSnapshot, VaultError> {
    let path = crate::page_path(root, Path::new(INDEX_PATH))?;
    let mut connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let transaction = connection.transaction()?;
    let expected: String =
        transaction.query_row("SELECT id FROM snapshot", [], |row| row.get(0))?;
    verify_snapshot(&transaction, root, dirs, &expected)?;
    read_snapshot(&transaction)
}

pub(super) fn read_snapshot(connection: &Connection) -> Result<SearchSnapshot, VaultError> {
    let (snapshot, pages, citations_available, captured_at, verified_at): (
        String,
        i64,
        bool,
        String,
        String,
    ) = connection.query_row(
        "SELECT id, pages, citations_available, captured_at, verified_at FROM snapshot",
        [],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        },
    )?;
    Ok(SearchSnapshot {
        snapshot,
        pages: usize::try_from(pages).map_err(|error| invalid(error.to_string()))?,
        citations_available,
        captured_at: captured_at
            .parse::<jiff::Timestamp>()
            .map_err(|error| invalid(error.to_string()))?,
        verified_at: verified_at
            .parse::<jiff::Timestamp>()
            .map_err(|error| invalid(error.to_string()))?,
    })
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

/// Refresh a reconstructible search snapshot in one SQLite transaction. The source
/// files are read under the cooperating writer lock; external editors are not frozen.
/// This lexical-only entry point captures no graph evidence; citation queries refuse it.
pub fn prepare_search(
    session: &MutationSession,
    dirs: &VaultDirs,
) -> Result<SearchSnapshot, VaultError> {
    prepare_search_inner(session, dirs, None, |_| Ok(()))
}

/// Include graph-authoritative citations in the same snapshot transaction. The provider's
/// verifier checks both its complete captured graph inputs and the selected search file
/// revisions immediately before commit, including when the snapshot can be reused.
pub fn prepare_search_with_citations(
    session: &MutationSession,
    dirs: &VaultDirs,
    citations: &crate::ConceptCitationProjection,
    verify: impl FnOnce(&[(PathBuf, String)]) -> Result<(), VaultError>,
) -> Result<SearchSnapshot, VaultError> {
    prepare_search_inner(session, dirs, Some(citations), verify)
}

fn prepare_search_inner(
    session: &MutationSession,
    dirs: &VaultDirs,
    citations: Option<&crate::ConceptCitationProjection>,
    verify: impl FnOnce(&[(PathBuf, String)]) -> Result<(), VaultError>,
) -> Result<SearchSnapshot, VaultError> {
    let root = session.root();
    let mut pages = Vec::new();
    let mut authored_revisions = BTreeMap::new();
    super::search::visit_pages(root, dirs, true, |mut page, _| {
        authored_revisions.insert(
            page.rel_path.clone(),
            lk_core::concept::source_revision(&page.page),
        );
        page.page.body.clear();
        page.page.original = None;
        pages.push(page);
        Ok(())
    })?;
    super::search::attach_reviews(root, dirs, &mut pages)?;
    let provenance = crate::provenance_index::capture(root, dirs, &pages, &authored_revisions)?;
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
    let selected_revisions = pages
        .iter()
        .map(|page| (page.rel_path.clone(), page.file_revision.clone()))
        .collect::<Vec<_>>();
    let identity = serde_json::to_string(&(VERSION, &context, manifest, &provenance, citations))
        .map_err(|error| VaultError::Serialization(error.to_string()))?;
    let snapshot = lk_core::concept::file_revision(&identity);
    let path = crate::writable_page_path(root, Path::new(INDEX_PATH))?;
    let mut connection = Connection::open(path)?;
    let mut compatible = false;
    if connection.table_exists(Some("main"), "snapshot")? {
        let current: (String, String, i32) =
            connection.query_row("SELECT id, context, version FROM snapshot", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?;
        if current == (snapshot.clone(), context.clone(), VERSION) {
            if citations.is_some() {
                verify_selected_capture(root, dirs, &selected_revisions)?;
            }
            verify(&selected_revisions)?;
            connection.execute(
                "UPDATE snapshot SET verified_at = ?1",
                [jiff::Timestamp::now().to_string()],
            )?;
            return read_snapshot(&connection);
        }
        compatible = current.1 == context && current.2 == VERSION;
    }
    let transaction = connection.transaction()?;
    if !compatible {
        transaction.execute_batch("DROP TABLE IF EXISTS links;
        DROP TABLE IF EXISTS terms;
        DROP TABLE IF EXISTS names;
        DROP TABLE IF EXISTS tags;
        DROP TABLE IF EXISTS pages;
        DROP TABLE IF EXISTS snapshot;
        CREATE TABLE snapshot (id TEXT NOT NULL, context TEXT NOT NULL, version INTEGER NOT NULL,
            citations_available INTEGER NOT NULL, pages INTEGER NOT NULL,
            captured_at TEXT NOT NULL, verified_at TEXT NOT NULL);
        CREATE TABLE pages (id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE, title TEXT NOT NULL,
            aliases TEXT NOT NULL, concept INTEGER NOT NULL, knowledge INTEGER NOT NULL, body TEXT NOT NULL, metadata TEXT NOT NULL, intro TEXT NOT NULL,
            category TEXT NOT NULL, format TEXT NOT NULL, source_status TEXT NOT NULL, created_date TEXT, path_rank INTEGER NOT NULL,
            knowledge_status TEXT NOT NULL, summary TEXT NOT NULL, text TEXT NOT NULL, name_text TEXT NOT NULL, spacing_text TEXT NOT NULL, spacing_name_text TEXT NOT NULL,
            input_revision TEXT NOT NULL, traversal_rank INTEGER NOT NULL);
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
        CREATE INDEX links_target ON links(target, source, ordinal);
        CREATE INDEX pages_traversal ON pages(traversal_rank);")?;
        // These projections have their own writers, but share this schema version.
        transaction.execute_batch(
            "DROP TABLE IF EXISTS provenance_documents;
            DROP TABLE IF EXISTS provenance_addresses;
            DROP TABLE IF EXISTS provenance_relations;
            DROP TABLE IF EXISTS provenance_coverage;
            DROP TABLE IF EXISTS concept_citations;
            DROP TABLE IF EXISTS concept_evidence;",
        )?;
    }
    let mut stored = {
        let mut statement = transaction
            .prepare("SELECT path, id, input_revision, path_rank, traversal_rank FROM pages")?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                (
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ),
            ))
        })?;
        rows.collect::<Result<BTreeMap<_, _>, _>>()?
    };
    let mut next_id = stored.values().map(|row| row.0).max().unwrap_or(-1);
    {
        let mut insert_page = transaction.prepare(
            "INSERT INTO pages VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)",
        )?;
        let mut insert_terms =
            transaction.prepare("INSERT INTO terms(rowid, name_text, text, spacing_name_text, spacing_text) VALUES (?1, ?2, ?3, ?4, ?5)")?;
        let mut insert_link = transaction
            .prepare("INSERT INTO links VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)")?;
        let mut insert_name =
            transaction.prepare("INSERT OR IGNORE INTO names VALUES (?1, ?2, ?3, ?4, ?5)")?;
        let mut insert_tag = transaction.prepare("INSERT INTO tags VALUES (?1, ?2)")?;
        for (traversal_rank, page) in pages.iter().enumerate() {
            let page_path = page.rel_path.to_string_lossy();
            let traversal_rank = integer(traversal_rank)?;
            let path_rank = integer(path_ranks[&page.rel_path])?;
            // Immutable RSS payloads must still verify on a changed snapshot even
            // when their origin page's exact bytes and lexical projection are retained.
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
                BTreeMap::new()
            };
            let input_revision = lk_core::concept::file_revision(
                &serde_json::to_string(&(&page.file_revision, &page.reviews, &evidence_previews))
                    .map_err(|error| VaultError::Serialization(error.to_string()))?,
            );
            let id = if let Some((id, previous, old_path_rank, old_traversal_rank)) =
                stored.remove(page_path.as_ref())
            {
                if previous == input_revision {
                    if old_path_rank != path_rank || old_traversal_rank != traversal_rank {
                        transaction.execute(
                            "UPDATE pages SET path_rank=?1, traversal_rank=?2 WHERE id=?3",
                            params![path_rank, traversal_rank, id],
                        )?;
                    }
                    continue;
                }
                remove_page(&transaction, id, &page_path)?;
                id
            } else {
                next_id = next_id
                    .checked_add(1)
                    .ok_or_else(|| invalid("page ID overflow"))?;
                next_id
            };
            let raw = std::fs::read_to_string(crate::page_path(root, &page.rel_path)?)?;
            if lk_core::concept::file_revision(&raw) != page.file_revision {
                return Err(invalid("page changed during snapshot preparation"));
            }
            let mut page = page.clone();
            page.page = crate::parse_page(&raw).map_err(VaultError::Frontmatter)?;
            page.page.original = None;
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
                id,
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
                path_rank,
                page.knowledge_status().as_str(),
                projection.summary,
                projection.text,
                name_text,
                projection.spacing_text,
                spacing_name_text,
                input_revision,
                traversal_rank,
            ])?;
            insert_terms.execute(params![
                id,
                name_text,
                projection.text,
                spacing_name_text,
                projection.spacing_text
            ])?;
            for name in page.names() {
                insert_name.execute(params![
                    id,
                    name,
                    lk_core::concept::identity_key(name).unwrap_or_default(),
                    fold_name(name),
                    spacing_key(name)
                ])?;
            }
            for tag in super::search::search_tags(&page.page) {
                insert_tag.execute(params![id, tag])?;
            }
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
                    integer(ordinal)?,
                    page.file_revision,
                    integer(page.body_line_offset + occurrence.line)?,
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
    for (path, (id, ..)) in stored {
        remove_page(&transaction, id, &path)?;
    }
    crate::provenance_index::write(&transaction, &provenance)?;
    crate::citations::write(&transaction, citations)?;
    if citations.is_some() {
        verify_selected_capture(root, dirs, &selected_revisions)?;
    }
    verify(&selected_revisions)?;
    let verified_at = jiff::Timestamp::now();
    transaction.execute("DELETE FROM snapshot", [])?;
    transaction.execute(
        "INSERT INTO snapshot VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
        params![
            snapshot,
            context,
            VERSION,
            citations.is_some(),
            integer(pages.len())?,
            verified_at.to_string()
        ],
    )?;
    transaction.commit()?;
    Ok(SearchSnapshot {
        snapshot,
        pages: pages.len(),
        citations_available: citations.is_some(),
        captured_at: verified_at,
        verified_at,
    })
}

fn integer(value: usize) -> Result<i64, VaultError> {
    i64::try_from(value).map_err(|error| invalid(error.to_string()))
}

/// FTS5 external-content deletion needs the old indexed values before the content
/// row changes; deleting only the page would leave old term matches behind.
fn remove_page(connection: &Connection, id: i64, path: &str) -> Result<(), VaultError> {
    connection.execute(
        "INSERT INTO terms(terms, rowid, name_text, text, spacing_name_text, spacing_text)
         SELECT 'delete', id, name_text, text, spacing_name_text, spacing_text FROM pages WHERE id=?1",
        [id],
    )?;
    connection.execute("DELETE FROM names WHERE page_id=?1", [id])?;
    connection.execute("DELETE FROM tags WHERE page_id=?1", [id])?;
    connection.execute("DELETE FROM links WHERE source=?1", [path])?;
    connection.execute("DELETE FROM pages WHERE id=?1", [id])?;
    Ok(())
}

/// Search and graph have deliberately different inventories. Guard the selected search
/// collection itself, including additions invisible to graph policy, without adopting that
/// collection as the graph's existence or citation-source universe.
fn verify_selected_capture(
    root: &Path,
    dirs: &VaultDirs,
    selected: &[(PathBuf, String)],
) -> Result<(), VaultError> {
    let expected = selected
        .iter()
        .cloned()
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut current = std::collections::BTreeMap::new();
    super::search::visit_pages(root, dirs, true, |page, _| {
        current.insert(page.rel_path, page.file_revision);
        Ok(())
    })?;
    if current != expected {
        return Err(invalid(
            "selected search inputs changed during citation snapshot",
        ));
    }
    Ok(())
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
        "SELECT metadata, summary, text, spacing_text FROM pages WHERE {scope} AND {filter} ORDER BY traversal_rank"
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
    let mut collector = super::search::SearchCollector::new(options);
    for row in rows {
        let (metadata, summary, text, spacing_text) = row?;
        let mut page: Page = serde_json::from_str(&metadata)
            .map_err(|error| VaultError::Serialization(error.to_string()))?;
        page.projection = Some(MatchProjection {
            summary,
            text,
            spacing_text,
        });
        collector.push(
            std::borrow::Cow::Owned(page),
            options,
            &spellings,
            &terms,
            identity.as_deref(),
        );
    }
    let mut result = collector.finish(options, &spellings, &terms);
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
        "SELECT metadata, body FROM pages WHERE {filter} ORDER BY traversal_rank"
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
    fn snapshot_inspection_is_read_only_and_does_not_certify_current_pages() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        assert!(inspect_search_snapshot(root.path(), &dirs).is_err());
        assert!(!root.path().join(".lorekeeper").exists());
        let path = "wiki/concepts/topic.md";
        write(
            root.path(),
            path,
            "---\ntype: concept\ntitle: Topic\n---\nOriginal.\n",
        );
        let session = MutationSession::acquire(root.path()).unwrap();
        let prepared = prepare_search(&session, &dirs).unwrap();
        let index_before = std::fs::read(root.path().join(INDEX_PATH)).unwrap();
        write(root.path(), path, "---\ninvalid: [\n---\nChanged.\n");
        let captured = inspect_search_snapshot(root.path(), &dirs).unwrap();
        assert_eq!(captured.snapshot, prepared.snapshot);
        assert_eq!(captured.pages, 1);
        assert_eq!(captured.captured_at, prepared.captured_at);
        assert_eq!(captured.verified_at, prepared.verified_at);
        assert_eq!(
            std::fs::read(root.path().join(INDEX_PATH)).unwrap(),
            index_before
        );
        assert!(prepare_search(&session, &dirs).is_err());
        assert_eq!(
            inspect_search_snapshot(root.path(), &dirs)
                .unwrap()
                .verified_at,
            prepared.verified_at
        );
        let incompatible = VaultDirs {
            wiki: "other".into(),
            ..dirs
        };
        assert!(inspect_search_snapshot(root.path(), &incompatible).is_err());
    }

    #[test]
    fn successful_reverification_preserves_capture_and_updates_verification_time() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        write(
            root.path(),
            "wiki/concepts/topic.md",
            "---\ntype: concept\ntitle: Topic\n---\nOriginal.\n",
        );
        let session = MutationSession::acquire(root.path()).unwrap();
        let prepared = prepare_search(&session, &dirs).unwrap();
        let connection = Connection::open(root.path().join(INDEX_PATH)).unwrap();
        connection
            .execute(
                "UPDATE snapshot SET verified_at = '2000-01-01T00:00:00Z'",
                [],
            )
            .unwrap();
        let reused = prepare_search(&session, &dirs).unwrap();
        assert_eq!(reused.snapshot, prepared.snapshot);
        assert_eq!(reused.captured_at, prepared.captured_at);
        assert!(reused.verified_at >= prepared.verified_at);
        assert_eq!(
            inspect_search_snapshot(root.path(), &dirs)
                .unwrap()
                .verified_at,
            reused.verified_at
        );
    }

    #[test]
    fn citation_capture_guards_the_selected_search_inventory_separately() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        verify_selected_capture(root.path(), &dirs, &[]).unwrap();
        let path = PathBuf::from("wiki/documents/.drafts/hidden.md");
        let raw = "---\ntype: document\n---\nSearchable hidden draft.\n";
        write(root.path(), path.to_str().unwrap(), raw);
        assert!(verify_selected_capture(root.path(), &dirs, &[]).is_err());
        let selected = vec![(path.clone(), lk_core::concept::file_revision(raw))];
        verify_selected_capture(root.path(), &dirs, &selected).unwrap();
        std::fs::write(root.path().join(&path), raw.replace("draft", "revision")).unwrap();
        assert!(verify_selected_capture(root.path(), &dirs, &selected).is_err());
    }

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

    fn citation_projection(paths: &[(&str, &[&str])]) -> crate::ConceptCitationProjection {
        crate::ConceptCitationProjection {
            input_revision: "captured-graph".into(),
            concepts: paths
                .iter()
                .map(|(path, sources)| crate::ConceptCitationSet {
                    path: (*path).into(),
                    source_count: sources.len(),
                    citations: sources
                        .iter()
                        .map(|source| {
                            lk_core::concept::Citation::at_revision(
                                Path::new(source),
                                "captured-source".into(),
                            )
                        })
                        .collect(),
                })
                .collect(),
        }
    }

    #[test]
    fn incremental_refresh_matches_fresh_views_without_replacing_unchanged_owners() {
        let root = tempfile::tempdir().unwrap();
        let fresh = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let mut files = BTreeMap::from([
            (
                "wiki/concepts/a.md",
                "---\ntype: concept\ntitle: Alpha\naliases: [Old Alias]\ntags: [old]\n---\n## Synthesis\nOldneedle evidence. [Old](../documents/old.md)\n",
            ),
            (
                "wiki/concepts/z.md",
                "---\ntype: concept\ntitle: Zulu\ntags: [stable]\n---\n## Synthesis\nEvidence remains. [Alpha](a.md)\n",
            ),
            (
                "wiki/documents/old.md",
                "---\ntype: document\ntitle: Old\nsource_identity: {repository: r, document: old}\nsource_record: {superseded_by: new}\n---\nOldneedle evidence.\n",
            ),
            (
                "wiki/documents/new.md",
                "---\ntype: document\ntitle: New\nsource_identity: {repository: r, document: new}\n---\nEvidence retained.\n",
            ),
        ]);
        for (path, text) in &files {
            write(root.path(), path, text);
        }
        let session = MutationSession::acquire(root.path()).unwrap();
        let before_projection = citation_projection(&[
            ("wiki/concepts/a.md", &["wiki/documents/old.md"]),
            ("wiki/concepts/z.md", &[]),
        ]);
        let before =
            prepare_search_with_citations(&session, &dirs, &before_projection, |_| Ok(())).unwrap();
        let connection = Connection::open(root.path().join(INDEX_PATH)).unwrap();
        let ids = |connection: &Connection| {
            connection
                .prepare("SELECT path, id FROM pages")
                .unwrap()
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })
                .unwrap()
                .collect::<Result<BTreeMap<_, _>, _>>()
                .unwrap()
        };
        let before_ids = ids(&connection);
        connection.execute_batch("CREATE TRIGGER keep_page BEFORE DELETE ON pages
            WHEN old.path='wiki/concepts/z.md' BEGIN SELECT RAISE(ABORT, 'unchanged page deleted'); END;
            CREATE TRIGGER keep_body BEFORE UPDATE OF body,metadata ON pages
            WHEN old.path='wiki/concepts/z.md' BEGIN SELECT RAISE(ABORT, 'unchanged page replaced'); END;
            CREATE TRIGGER keep_names BEFORE DELETE ON names
            WHEN old.page_id=(SELECT id FROM pages WHERE path='wiki/concepts/z.md')
            BEGIN SELECT RAISE(ABORT, 'unchanged names deleted'); END;
            CREATE TRIGGER keep_tags BEFORE DELETE ON tags
            WHEN old.page_id=(SELECT id FROM pages WHERE path='wiki/concepts/z.md')
            BEGIN SELECT RAISE(ABORT, 'unchanged tags deleted'); END;
            CREATE TRIGGER keep_links BEFORE DELETE ON links WHEN old.source='wiki/concepts/z.md'
            BEGIN SELECT RAISE(ABORT, 'unchanged links deleted'); END;
            CREATE TRIGGER keep_provenance BEFORE UPDATE ON provenance_documents
            WHEN old.path='wiki/documents/new.md' BEGIN SELECT RAISE(ABORT, 'unchanged provenance replaced'); END;
            CREATE TRIGGER keep_citations BEFORE UPDATE ON concept_evidence
            WHEN old.path='wiki/concepts/z.md' BEGIN SELECT RAISE(ABORT, 'unchanged citations replaced'); END;").unwrap();
        files.insert("wiki/concepts/a.md", "---\ntype: concept\ntitle: Beta\naliases: [New Alias]\ncategory: updated\ntags: [new]\n---\n## Synthesis\nNewneedle evidence. [First](z.md) [Second](z.md)\n");
        files.insert(
            "wiki/concepts/000.md",
            "---\ntype: concept\ntitle: First\n---\n## Synthesis\nNewneedle evidence first.\n",
        );
        files.insert("wiki/documents/child.md", "---\ntype: document\ntitle: Child\nsource_identity: {repository: r, document: child}\nsource_record: {superseded_by: new}\n---\nNewneedle evidence.\n");
        files.remove("wiki/documents/old.md");
        std::fs::remove_file(root.path().join("wiki/documents/old.md")).unwrap();
        for (path, text) in &files {
            write(root.path(), path, text);
            write(fresh.path(), path, text);
        }
        let projection = citation_projection(&[
            ("wiki/concepts/000.md", &[]),
            ("wiki/concepts/a.md", &["wiki/documents/new.md"]),
            ("wiki/concepts/z.md", &[]),
        ]);
        let updated =
            prepare_search_with_citations(&session, &dirs, &projection, |_| Ok(())).unwrap();
        let fresh_session = MutationSession::acquire(fresh.path()).unwrap();
        let rebuilt =
            prepare_search_with_citations(&fresh_session, &dirs, &projection, |_| Ok(())).unwrap();
        assert_ne!(updated.snapshot, before.snapshot);
        let after_ids = ids(&connection);
        for path in [
            "wiki/concepts/a.md",
            "wiki/concepts/z.md",
            "wiki/documents/new.md",
        ] {
            assert_eq!(before_ids[path], after_ids[path], "{path}");
        }
        assert!(after_ids["wiki/concepts/000.md"] > *before_ids.values().max().unwrap());
        assert!(!after_ids.contains_key("wiki/documents/old.md"));
        let triggers: i64 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE type='trigger' AND name LIKE 'keep_%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            triggers, 7,
            "compatible refresh must not rebuild the schema"
        );
        connection
            .execute(
                "INSERT INTO terms(terms, rank) VALUES ('integrity-check', 1)",
                [],
            )
            .unwrap();
        for query in [
            None,
            Some("Oldneedle"),
            Some("Newneedle"),
            Some("Old Alias"),
            Some("New Alias"),
            Some("evidence"),
        ] {
            for tags in [vec![], vec!["old".to_owned()], vec!["new".to_owned()]] {
                let mut options = SearchOptions {
                    query,
                    include_sources: true,
                    tags: &tags,
                    limit: 100,
                    ..Default::default()
                };
                let live = crate::search(root.path(), &dirs, &options).unwrap();
                options.snapshot = Some(&updated.snapshot);
                let mut incremental = crate::search(root.path(), &dirs, &options).unwrap();
                options.snapshot = Some(&rebuilt.snapshot);
                let mut full = crate::search(fresh.path(), &dirs, &options).unwrap();
                incremental.snapshot = None;
                full.snapshot = None;
                assert_eq!(
                    serde_json::to_value(&incremental).unwrap(),
                    serde_json::to_value(&full).unwrap(),
                    "{query:?} {tags:?}"
                );
                assert_eq!(
                    serde_json::to_value(&incremental.hits).unwrap(),
                    serde_json::to_value(&live.hits).unwrap(),
                    "{query:?} {tags:?}"
                );
            }
        }
        let mut body = crate::BodySearchOptions {
            search: SearchOptions {
                query: Some("evidence"),
                include_sources: true,
                limit: 2,
                offset: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        let live = crate::search_body(root.path(), &dirs, &body).unwrap();
        body.search.snapshot = Some(&updated.snapshot);
        let captured = crate::search_body(root.path(), &dirs, &body).unwrap();
        assert_eq!(
            serde_json::to_value(live.hits).unwrap(),
            serde_json::to_value(captured.hits).unwrap(),
            "stable IDs must not change traversal pagination"
        );
        let links = crate::inspect_indexed_links(
            root.path(),
            &dirs,
            Path::new("wiki/concepts/a.md"),
            &crate::IndexedLinkOptions {
                snapshot: &updated.snapshot,
                direction: crate::LinkDirection::Outgoing,
                limit: 100,
                offset: 0,
            },
        )
        .unwrap();
        assert_eq!(
            links
                .links
                .iter()
                .map(|link| link.label.as_str())
                .collect::<Vec<_>>(),
            ["First", "Second"]
        );
        let provenance = crate::ProvenanceOptions {
            direction: crate::ProvenanceDirection::Incoming,
            relation: None,
            limit: 100,
            offset: 0,
            revision: None,
            snapshot: None,
        };
        let live = crate::inspect_provenance(
            root.path(),
            &dirs,
            Path::new("wiki/documents/new.md"),
            &provenance,
        )
        .unwrap();
        let captured = crate::inspect_provenance(
            root.path(),
            &dirs,
            Path::new("wiki/documents/new.md"),
            &crate::ProvenanceOptions {
                snapshot: Some(&updated.snapshot),
                ..provenance
            },
        )
        .unwrap();
        assert_eq!(live.entries, captured.entries);
        assert_eq!(live.total, captured.total);
        let citations = crate::inspect_concept_citations(
            root.path(),
            &dirs,
            Path::new("wiki/concepts/a.md"),
            &crate::CitationOptions {
                snapshot: &updated.snapshot,
                limit: 100,
                offset: 0,
            },
        )
        .unwrap();
        assert_eq!(citations.citations, projection.concepts[1].citations);
        assert!(
            crate::search(
                root.path(),
                &dirs,
                &SearchOptions {
                    query: None,
                    snapshot: Some(&before.snapshot),
                    ..Default::default()
                }
            )
            .is_err()
        );
    }

    fn database_rows(root: &Path) -> Vec<Vec<Vec<rusqlite::types::Value>>> {
        let connection = Connection::open(root.join(INDEX_PATH)).unwrap();
        [
            "snapshot",
            "pages",
            "names",
            "tags",
            "links",
            "provenance_documents",
            "provenance_addresses",
            "provenance_relations",
            "provenance_coverage",
            "concept_evidence",
            "concept_citations",
        ]
        .into_iter()
        .map(|table| {
            let mut query = connection
                .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
                .unwrap();
            let count = query.column_count();
            query
                .query_map([], |row| (0..count).map(|column| row.get(column)).collect())
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        })
        .collect()
    }

    #[test]
    fn incremental_refresh_rolls_back_all_views_on_verifier_or_sql_rejection() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let path = "wiki/concepts/a.md";
        write(
            root.path(),
            path,
            "---\ntype: concept\ntitle: Alpha\naliases: [Old Alias]\ntags: [old]\n---\nOldneedle [old](../documents/old.md).\n",
        );
        write(
            root.path(),
            "wiki/documents/old.md",
            "---\ntype: document\ntitle: Old\nsource_identity: {repository: r, document: old}\n---\nOldneedle.\n",
        );
        let session = MutationSession::acquire(root.path()).unwrap();
        let projection = citation_projection(&[(path, &["wiki/documents/old.md"])]);
        let before =
            prepare_search_with_citations(&session, &dirs, &projection, |_| Ok(())).unwrap();
        let rows = database_rows(root.path());
        write(
            root.path(),
            path,
            "---\ntype: concept\ntitle: Beta\ntags: [new]\n---\nNewneedle [new](../documents/new.md).\n",
        );
        std::fs::remove_file(root.path().join("wiki/documents/old.md")).unwrap();
        write(
            root.path(),
            "wiki/documents/new.md",
            "---\ntype: document\ntitle: New\nsource_identity: {repository: r, document: new}\n---\nNewneedle.\n",
        );
        let changed = citation_projection(&[(path, &["wiki/documents/new.md"])]);
        let error = prepare_search_with_citations(&session, &dirs, &changed, |_| {
            Err(invalid("deterministic capture drift"))
        })
        .err()
        .unwrap();
        assert!(error.to_string().contains("deterministic capture drift"));
        assert_eq!(database_rows(root.path()), rows);
        let connection = Connection::open(root.path().join(INDEX_PATH)).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER reject_provenance BEFORE INSERT ON provenance_documents
            BEGIN SELECT RAISE(ABORT, 'deterministic SQL rejection'); END;",
            )
            .unwrap();
        let error = prepare_search_with_citations(&session, &dirs, &changed, |_| Ok(()))
            .err()
            .unwrap();
        assert!(error.to_string().contains("deterministic SQL rejection"));
        assert_eq!(database_rows(root.path()), rows);
        let captured = crate::search(
            root.path(),
            &dirs,
            &SearchOptions {
                query: Some("Oldneedle"),
                snapshot: Some(&before.snapshot),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(captured.total, 2);
        connection
            .execute(
                "INSERT INTO terms(terms, rank) VALUES ('integrity-check', 1)",
                [],
            )
            .unwrap();
    }

    #[test]
    fn incremental_refresh_updates_review_previews_without_target_byte_changes() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let concept = "wiki/concepts/a.md";
        let source = "wiki/documents/evidence.md";
        write(
            root.path(),
            concept,
            "---\ntype: concept\ntitle: Alpha\n---\nEvidence.\n",
        );
        let raw = "---\ntype: document\ntitle: Evidence\n---\nOriginal evidence.\n";
        write(root.path(), source, raw);
        let revision = lk_core::concept::source_revision(&crate::parse_page(raw).unwrap());
        let review = serde_json::json!({"status":"open", "subjects":[concept], "evidence":[{"path":source,"revision":revision}], "resolution":null});
        write(
            root.path(),
            "wiki/explorations/review.md",
            &format!(
                "---\ntype: exploration\ntitle: Review\nreview: {review}\n---\nAuthored judgment.\n"
            ),
        );
        let session = MutationSession::acquire(root.path()).unwrap();
        let prepared = prepare_search(&session, &dirs).unwrap();
        let mut options = SearchOptions {
            query: Some("Alpha"),
            snapshot: Some(&prepared.snapshot),
            ..Default::default()
        };
        let before = crate::search(root.path(), &dirs, &options).unwrap();
        crate::archive_evidence(root.path(), &[source.into()]).unwrap();
        write(root.path(), source, &raw.replace("Original", "Changed"));
        let refreshed = prepare_search(&session, &dirs).unwrap();
        options.snapshot = Some(&refreshed.snapshot);
        let after = crate::search(root.path(), &dirs, &options).unwrap();
        assert_eq!(before.hits[0].file_revision, after.hits[0].file_revision);
        assert_ne!(
            serde_json::to_value(&before.hits[0]).unwrap(),
            serde_json::to_value(&after.hits[0]).unwrap()
        );
        options.snapshot = None;
        let live = crate::search(root.path(), &dirs, &options).unwrap();
        assert_eq!(
            serde_json::to_value(after.hits).unwrap(),
            serde_json::to_value(live.hits).unwrap()
        );
    }

    #[test]
    fn incompatible_version_or_configuration_rebuilds_the_current_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        write(
            root.path(),
            "wiki/concepts/a.md",
            "---\ntype: concept\ntitle: Alpha\n---\nEvidence.\n",
        );
        write(
            root.path(),
            "daily/feed/old.md",
            "---\ntype: daily\ntitle: Old\n---\nOld scope.\n",
        );
        write(
            root.path(),
            "observations/feed/new.md",
            "---\ntype: daily\ntitle: New\n---\nNew scope.\n",
        );
        let session = MutationSession::acquire(root.path()).unwrap();
        let before = prepare_search(&session, &dirs).unwrap();
        let connection = Connection::open(root.path().join(INDEX_PATH)).unwrap();
        // Simulate the absence of maintenance columns in a previous schema.
        connection
            .execute_batch(
                "DROP INDEX pages_traversal;
            ALTER TABLE pages DROP COLUMN input_revision;
            ALTER TABLE pages DROP COLUMN traversal_rank;
            ALTER TABLE concept_evidence DROP COLUMN input_revision;
            UPDATE snapshot SET version=0;",
            )
            .unwrap();
        let migrated = prepare_search(&session, &dirs).unwrap();
        assert_eq!(migrated.snapshot, before.snapshot);
        connection
            .execute(
                "INSERT INTO terms(terms, rank) VALUES ('integrity-check', 1)",
                [],
            )
            .unwrap();
        let changed = VaultDirs {
            daily: "observations".into(),
            ..dirs.clone()
        };
        let prepared = prepare_search(&session, &changed).unwrap();
        assert_ne!(prepared.snapshot, before.snapshot);
        let options = SearchOptions {
            query: None,
            include_sources: true,
            limit: 100,
            snapshot: Some(&prepared.snapshot),
            ..Default::default()
        };
        let captured = crate::search(root.path(), &changed, &options).unwrap();
        assert_eq!(captured.total, 2);
        assert!(
            captured
                .hits
                .iter()
                .any(|hit| hit.path == "observations/feed/new.md")
        );
        assert!(
            !captured
                .hits
                .iter()
                .any(|hit| hit.path == "daily/feed/old.md")
        );
        assert!(crate::search(root.path(), &dirs, &options).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn unchanged_rss_origin_refreshes_alias_previews_and_still_verifies_immutable_payloads() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        for name in ["a", "b"] {
            write(
                root.path(),
                &format!("wiki/concepts/{name}.md"),
                &format!("---\ntype: concept\ntitle: {name}\n---\nEvidence.\n"),
            );
        }
        let alias = root.path().join("wiki/concepts/alias.md");
        std::os::unix::fs::symlink("a.md", &alias).unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let event: lk_core::event::Event = serde_json::from_value(serde_json::json!({
            "id":"rss:2026-10-18:0000000000000001", "source_id":"rss", "source_type":"rss",
            "timestamp":"2026-10-18T00:00:00Z", "date":"2026-10-18", "title":"Title", "body":"Original evidence.",
            "url":"https://example.com", "author":null, "labels":[], "category":null,
            "performance_category":null, "is_self":false, "is_personal":false, "metadata":{}
        })).unwrap();
        let capture = lk_core::rss_original::RssOriginalCapture::from_events(&[&event]).unwrap();
        let reference = crate::publish_rss_original(&session, &capture).unwrap();
        let entry = &capture.manifest.events[0];
        let bindings = serde_json::json!([{
            "schema":lk_core::rss_evidence::RSS_EVIDENCE_SCHEMA, "concept":"wiki/concepts/a.md",
            "manifest":reference, "events":[{"event_id":entry.event_id,"revision":entry.revision}]
        }]);
        let source = "daily/rss.md";
        let raw = format!(
            "---\ntype: daily\nsource: rss\ncreated: 2026-10-18\nsource_record: {{rss_original: {}}}\nconcept_evidence: {bindings}\n---\n## Related Concepts\n- [Canonical](../wiki/concepts/a.md)\n- [Alias](../wiki/concepts/alias.md)\n",
            serde_json::to_string(&reference).unwrap()
        );
        write(root.path(), source, &raw);
        let prepared = prepare_search(&session, &dirs).unwrap();
        let links = |snapshot: &str| {
            crate::inspect_indexed_links(
                root.path(),
                &dirs,
                Path::new(source),
                &crate::IndexedLinkOptions {
                    snapshot,
                    direction: crate::LinkDirection::Outgoing,
                    limit: 100,
                    offset: 0,
                },
            )
            .unwrap()
        };
        let before = links(&prepared.snapshot);
        assert_eq!(before.links.len(), 2);
        assert!(before.links.iter().all(|link| link.rss_evidence.is_some()));
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink("b.md", &alias).unwrap();
        // Lexical-only capture does not enumerate concept leaf aliases. Change
        // its selected inventory so this exercises incremental refresh rather
        // than the existing identical-snapshot fast path.
        write(
            root.path(),
            "wiki/documents/new.md",
            "---\ntype: document\ntitle: New\n---\nNew evidence.\n",
        );
        let refreshed = prepare_search(&session, &dirs).unwrap();
        assert_ne!(prepared.snapshot, refreshed.snapshot);
        let after = links(&refreshed.snapshot);
        assert_eq!(before.file_revision, after.file_revision);
        assert!(after.links[0].rss_evidence.is_some());
        assert!(after.links[1].rss_evidence.is_none());
        std::fs::write(
            root.path().join(format!(
                "{}/events/{}.json",
                lk_core::rss_original::RSS_ORIGINAL_STORE,
                entry.revision
            )),
            "corrupt",
        )
        .unwrap();
        write(
            root.path(),
            "wiki/concepts/b.md",
            "---\ntype: concept\ntitle: Changed\n---\nNew evidence.\n",
        );
        assert!(prepare_search(&session, &dirs).is_err());
        assert!(links(&refreshed.snapshot).links[1].rss_evidence.is_none());
    }
}
