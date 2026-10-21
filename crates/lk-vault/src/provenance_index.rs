use std::collections::BTreeMap;
use std::path::Path;

use lk_core::config::VaultDirs;
use rusqlite::{Connection, OpenFlags, params};
use serde::Serialize;
use serde_json::{Value, json};

use crate::provenance::{
    Declaration, Document, ProvenanceDirection, ProvenanceOptions, ProvenanceReport,
};
use crate::{VaultError, search::Page};

#[derive(Serialize)]
pub(super) struct Capture {
    documents: Vec<Document>,
    addresses: Vec<(String, String)>,
    coverage: Value,
    revisions: Vec<(String, Value, std::path::PathBuf)>,
}

pub(super) fn capture(
    root: &Path,
    dirs: &VaultDirs,
    pages: &[Page],
    authored_revisions: &BTreeMap<std::path::PathBuf, String>,
) -> Result<Capture, VaultError> {
    let directory = lk_core::vault_path::documents_dir(dirs);
    let mut by_path = BTreeMap::new();
    let mut by_canonical = BTreeMap::new();
    for page in pages
        .iter()
        .filter(|page| page.rel_path.starts_with(&directory))
    {
        by_path.insert(page.rel_path.clone(), page);
        by_canonical.insert(
            crate::page_path(root, &page.rel_path)?.canonicalize()?,
            page,
        );
    }
    let collection = crate::provenance::collect_documents(root, dirs, |path, full, canonical| {
        let page = by_path.get(path).or_else(|| by_canonical.get(canonical));
        match page {
            Some(page) => {
                if page.page.frontmatter.get("type").and_then(Value::as_str) != Some("document") {
                    return Ok(None);
                }
                crate::provenance::document_page(
                    path.to_string_lossy().into_owned(),
                    &page.page,
                    page.file_revision.clone(),
                    authored_revisions[&page.rel_path].clone(),
                )
                .map(Some)
            }
            None => crate::provenance::read_document(path, full),
        }
    })?;
    let documents = collection.documents;
    let addresses = collection.addresses;
    let coverage = json!({
        "documents":documents.len(), "scanned_document_addresses":addresses.len(),
        "filesystem_aliases":addresses.len()-documents.len(),
        "identities_unavailable":documents.iter().filter(|d|d.metadata["identity_status"]=="unavailable").count(),
        "identities_invalid":documents.iter().filter(|d|d.metadata["identity_status"]=="invalid").count(),
        "documents_with_invalid_metadata":documents.iter().filter(|d|!d.issues.is_empty()).count(),
        "invalid_metadata_issues":documents.iter().map(|d|d.issues.len()).sum::<usize>(),
        "scope":"captured configured document collection; exact declared identities and relations",
        "snapshot":"prepared SQLite snapshot; filesystem aliases resolved at capture"
    });
    Ok(Capture {
        documents,
        addresses,
        coverage,
        revisions: collection.revisions,
    })
}

pub(super) fn write(connection: &Connection, captured: &Capture) -> Result<(), VaultError> {
    connection.execute_batch("CREATE TABLE IF NOT EXISTS provenance_documents(path TEXT PRIMARY KEY, namespace TEXT, document TEXT, input_revision TEXT NOT NULL, metadata TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS provenance_identity ON provenance_documents(namespace, document, path);
        CREATE TABLE IF NOT EXISTS provenance_addresses(path TEXT PRIMARY KEY, owner TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS provenance_relations(source TEXT NOT NULL, relation TEXT NOT NULL, namespace TEXT, target TEXT NOT NULL, fields TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS provenance_outgoing ON provenance_relations(source, relation, target);
        CREATE INDEX IF NOT EXISTS provenance_incoming ON provenance_relations(namespace, target, relation, source);
        CREATE TABLE IF NOT EXISTS provenance_issues(path TEXT NOT NULL, ordinal INTEGER NOT NULL,
            issue TEXT NOT NULL, PRIMARY KEY(path, ordinal));
        CREATE TABLE IF NOT EXISTS provenance_coverage(payload TEXT NOT NULL);")?;
    let mut stored_documents = {
        let mut query =
            connection.prepare("SELECT path, input_revision FROM provenance_documents")?;
        query
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<BTreeMap<_, _>, _>>()?
    };
    let mut stored_addresses = {
        let mut query = connection.prepare("SELECT path, owner FROM provenance_addresses")?;
        query
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<BTreeMap<_, _>, _>>()?
    };
    let mut document =
        connection.prepare("INSERT INTO provenance_documents VALUES (?1,?2,?3,?4,?5)
            ON CONFLICT(path) DO UPDATE SET namespace=excluded.namespace, document=excluded.document, input_revision=excluded.input_revision, metadata=excluded.metadata")?;
    let mut address = connection.prepare(
        "INSERT INTO provenance_addresses VALUES (?1,?2)
        ON CONFLICT(path) DO UPDATE SET owner=excluded.owner",
    )?;
    let mut relation =
        connection.prepare("INSERT INTO provenance_relations VALUES (?1,?2,?3,?4,?5)")?;
    let mut issue = connection.prepare("INSERT INTO provenance_issues VALUES (?1,?2,?3)")?;
    for page in &captured.documents {
        let input_revision = lk_core::concept::file_revision(
            &serde_json::to_string(page)
                .map_err(|error| VaultError::Serialization(error.to_string()))?,
        );
        if stored_documents.remove(&page.path).as_ref() == Some(&input_revision) {
            continue;
        }
        let repository = page.identity.as_ref().map(|(repository, _)| repository);
        let id = page.identity.as_ref().map(|(_, id)| id);
        let metadata = serde_json::to_string(&page.metadata)
            .map_err(|error| VaultError::Serialization(error.to_string()))?;
        document.execute(params![page.path, repository, id, input_revision, metadata])?;
        connection.execute(
            "DELETE FROM provenance_relations WHERE source=?1",
            [&page.path],
        )?;
        for declaration in &page.declarations {
            relation.execute(params![
                page.path,
                declaration.relation,
                repository,
                declaration.target,
                serde_json::to_string(&declaration.fields)
                    .map_err(|error| VaultError::Serialization(error.to_string()))?
            ])?;
        }
        connection.execute("DELETE FROM provenance_issues WHERE path=?1", [&page.path])?;
        for (ordinal, message) in page.issues.iter().enumerate() {
            issue.execute(params![
                page.path,
                i64::try_from(ordinal).map_err(std::io::Error::other)?,
                message
            ])?;
        }
    }
    for path in stored_documents.keys() {
        connection.execute("DELETE FROM provenance_relations WHERE source=?1", [path])?;
        connection.execute("DELETE FROM provenance_issues WHERE path=?1", [path])?;
        connection.execute("DELETE FROM provenance_documents WHERE path=?1", [path])?;
    }
    for (path, owner) in &captured.addresses {
        if stored_addresses.remove(path).as_ref() != Some(owner) {
            address.execute(params![path, owner])?;
        }
    }
    for path in stored_addresses.keys() {
        connection.execute("DELETE FROM provenance_addresses WHERE path=?1", [path])?;
    }
    let coverage = serde_json::to_string(&captured.coverage)
        .map_err(|error| VaultError::Serialization(error.to_string()))?;
    connection.execute(
        "UPDATE provenance_coverage SET payload=?1 WHERE payload!=?1",
        [&coverage],
    )?;
    connection.execute(
        "INSERT INTO provenance_coverage SELECT ?1
        WHERE NOT EXISTS (SELECT 1 FROM provenance_coverage)",
        [&coverage],
    )?;
    Ok(())
}

fn decode<T: serde::de::DeserializeOwned>(raw: &str) -> Result<T, VaultError> {
    serde_json::from_str(raw).map_err(|error| VaultError::Serialization(error.to_string()))
}

const ELIGIBLE_RELATIONS: &str = "WITH eligible AS (
    SELECT source, relation, namespace, target FROM provenance_relations
    WHERE (?6 IS NULL OR relation=?6)
        AND ((?4 AND source=?1) OR (?5 AND namespace=?2 AND target=?3))
)";

enum CapturedRow {
    Issue {
        ordinal: i64,
    },
    Relation {
        source: String,
        relation: String,
        target: String,
    },
    Candidate {
        source: String,
        relation: String,
        target: String,
        owner: String,
    },
}

pub(super) fn inspect(
    root: &Path,
    dirs: &VaultDirs,
    path: &Path,
    options: &ProvenanceOptions<'_>,
    snapshot: &str,
) -> Result<ProvenanceReport, VaultError> {
    let mut connection = Connection::open_with_flags(
        crate::page_path(root, Path::new(crate::search_index::INDEX_PATH))?,
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let transaction = connection.transaction()?;
    crate::search_index::verify_snapshot(&transaction, root, dirs, snapshot)?;
    inspect_captured(&transaction, path, options, snapshot)
}

pub(super) fn inspect_captured(
    transaction: &Connection,
    path: &Path,
    options: &ProvenanceOptions<'_>,
    snapshot: &str,
) -> Result<ProvenanceReport, VaultError> {
    let (center, namespace, document, origin): (String, Option<String>, Option<String>, String) =
        transaction.query_row(
            "SELECT d.path, d.namespace, d.document, d.metadata FROM provenance_addresses a
         JOIN provenance_documents d ON d.path=a.owner WHERE a.path=?1",
            [path.to_string_lossy().as_ref()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
    let revision = lk_core::concept::file_revision(&serde_json::to_string(&json!({"snapshot":snapshot,"path":center,"direction":format!("{:?}",options.direction),"relation":options.relation})).map_err(|error| VaultError::Serialization(error.to_string()))?);
    if options
        .revision
        .is_some_and(|expected| expected != revision)
    {
        return Err(
            std::io::Error::other("provenance collection changed; restart pagination").into(),
        );
    }
    let outgoing = options.direction != ProvenanceDirection::Incoming;
    let incoming = options.direction != ProvenanceDirection::Outgoing;
    let parameters = params![
        center,
        namespace,
        document,
        outgoing,
        incoming,
        options.relation
    ];
    let total: i64 = transaction.query_row(
        &format!(
            "{ELIGIBLE_RELATIONS}
            SELECT (SELECT COUNT(*) FROM provenance_issues WHERE path=?1)
                + COALESCE(SUM(1 + (SELECT COUNT(*) FROM provenance_documents d
                    WHERE d.namespace=r.namespace AND d.document=r.target)), 0)
            FROM eligible r"
        ),
        parameters,
        |row| row.get(0),
    )?;
    let total = usize::try_from(total).map_err(std::io::Error::other)?;
    let mut selected = Vec::new();
    if options.offset < total {
        // Only stable scalar keys participate in counting, expansion and sorting.
        // Exact totals still visit all matching keys; payload decoding follows LIMIT.
        let mut query = transaction.prepare(&format!(
            "{ELIGIBLE_RELATIONS}, row_keys AS (
            SELECT path AS source, 0 AS row_group, 0 AS kind, NULL AS relation,
                NULL AS target, ordinal, NULL AS owner FROM provenance_issues WHERE path=?1
            UNION ALL
            SELECT source, 1, 1, relation, target, NULL, NULL FROM eligible
            UNION ALL
            SELECT r.source, 1, 2, r.relation, r.target, NULL, d.path
                FROM eligible r JOIN provenance_documents d
                    ON d.namespace=r.namespace AND d.document=r.target
        ) SELECT source, kind, relation, target, ordinal, owner FROM row_keys
            ORDER BY source, row_group, relation, target, kind, ordinal, owner
            LIMIT ?7 OFFSET ?8"
        ))?;
        let rows = query.query_map(
            params![
                center,
                namespace,
                document,
                outgoing,
                incoming,
                options.relation,
                i64::try_from(options.limit).unwrap_or(i64::MAX),
                i64::try_from(options.offset).unwrap_or(i64::MAX)
            ],
            |row| match row.get::<_, i64>(1)? {
                0 => Ok(CapturedRow::Issue {
                    ordinal: row.get(4)?,
                }),
                1 => Ok(CapturedRow::Relation {
                    source: row.get(0)?,
                    relation: row.get(2)?,
                    target: row.get(3)?,
                }),
                2 => Ok(CapturedRow::Candidate {
                    source: row.get(0)?,
                    relation: row.get(2)?,
                    target: row.get(3)?,
                    owner: row.get(5)?,
                }),
                _ => Err(rusqlite::Error::InvalidQuery),
            },
        )?;
        selected = rows.collect::<Result<Vec<_>, _>>()?;
    }
    let origin: Value = decode(&origin)?;
    let mut metadata =
        transaction.prepare("SELECT metadata FROM provenance_documents WHERE path=?1")?;
    let mut declaration = transaction.prepare(
        "SELECT namespace, fields, (SELECT COUNT(*) FROM provenance_documents d
            WHERE d.namespace=r.namespace AND d.document=r.target)
         FROM provenance_relations r WHERE source=?1 AND relation=?2 AND target=?3",
    )?;
    let mut issue =
        transaction.prepare("SELECT issue FROM provenance_issues WHERE path=?1 AND ordinal=?2")?;
    let mut entries = Vec::with_capacity(selected.len());
    for row in selected {
        entries.push(match row {
            CapturedRow::Issue { ordinal } => {
                let issue: String = issue.query_row(params![center, ordinal], |row| row.get(0))?;
                json!({"kind":"invalid-metadata", "declaring":origin,"issue":issue})
            }
            CapturedRow::Relation {
                source,
                relation,
                target,
            } => {
                let declaring: String = metadata.query_row([&source], |row| row.get(0))?;
                let (scope, fields, candidates): (Option<String>, String, i64) = declaration
                    .query_row(params![source, relation, target], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                    })?;
                crate::provenance::build_relation_entry(
                    &decode::<Value>(&declaring)?,
                    &Declaration {
                        relation,
                        target: target.clone(),
                        fields: decode(&fields)?,
                    },
                    incoming
                        && namespace.as_ref().zip(document.as_ref()).is_some_and(
                            |(namespace, document)| {
                                scope.as_ref() == Some(namespace) && target == *document
                            },
                        ),
                    outgoing && source == center,
                    usize::try_from(candidates).map_err(std::io::Error::other)?,
                )
            }
            CapturedRow::Candidate {
                source,
                relation,
                target,
                owner,
            } => {
                let target_metadata: String = metadata.query_row([owner], |row| row.get(0))?;
                crate::provenance::build_target_entry(
                    &source,
                    &relation,
                    &target,
                    &decode::<Value>(&target_metadata)?,
                )
            }
        });
    }
    let coverage: String =
        transaction.query_row("SELECT payload FROM provenance_coverage", [], |row| {
            row.get(0)
        })?;
    let end = options.offset.saturating_add(entries.len());
    Ok(ProvenanceReport {
        path: center,
        revision,
        origin,
        coverage: decode(&coverage)?,
        entries,
        total,
        offset: options.offset,
        next_offset: (end < total).then_some(end),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn high_degree_pages_select_scalar_rows_before_decoding_payloads() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let documents = root.path().join("wiki/documents");
        std::fs::create_dir_all(&documents).unwrap();
        let write = |name: &str, identity: Value, record: Value, extra: &str| {
            std::fs::write(
                documents.join(format!("{name}.md")),
                format!("---\ntype: document\ntitle: {name}\nsource_identity: {identity}\nsource_record: {record}\n{extra}---\nSource.\n"),
            ).unwrap();
        };
        write(
            "m-center",
            json!({"repository":"repo", "document":"center"}),
            json!({"id":"mismatch", "superseded_by":"target", "relations":[
                {"relation":"extends", "target":"missing"},
                {"relation":"references", "target":"center"}, {}
            ]}),
            "source_file: 7\n",
        );
        for index in 0..24 {
            write(
                &format!("target-{index:02}"),
                json!({"repository":"repo", "document":"target"}),
                json!({}),
                "",
            );
        }
        for index in 0..16 {
            write(
                &format!("{}-in-{index:02}", if index < 8 { "a" } else { "z" }),
                json!({"repository":"repo", "document":format!("incoming-{index}")}),
                json!({"relations":[{"relation":"superseded_by", "target":"center"},
                    {"relation":"irrelevant", "target":"elsewhere"}]}),
                "",
            );
        }
        for (name, identity) in [
            ("invalid", json!({"document":"unknown"})),
            ("unavailable", Value::Null),
        ] {
            write(name, identity, json!({"superseded_by":"target"}), "");
        }
        let session = crate::MutationSession::acquire(root.path()).unwrap();
        let snapshot = crate::prepare_search(&session, &dirs).unwrap();
        let center = Path::new("wiki/documents/m-center.md");
        let mut all_entries = Vec::new();
        for name in ["m-center", "target-00", "invalid", "unavailable"] {
            let path = documents.join(format!("{name}.md"));
            let path = path.strip_prefix(root.path()).unwrap();
            for direction in [
                ProvenanceDirection::Both,
                ProvenanceDirection::Incoming,
                ProvenanceDirection::Outgoing,
            ] {
                for relation in [None, Some("superseded_by"), Some("absent")] {
                    let mut options = ProvenanceOptions {
                        direction,
                        relation,
                        limit: 100,
                        offset: 0,
                        revision: None,
                        snapshot: None,
                    };
                    let live =
                        crate::inspect_provenance(root.path(), &dirs, path, &options).unwrap();
                    assert_eq!(live.entries.len(), live.total);
                    options.snapshot = Some(&snapshot.snapshot);
                    options.limit = 1;
                    for offset in 0..=live.total + 1 {
                        options.offset = offset;
                        let captured =
                            crate::inspect_provenance(root.path(), &dirs, path, &options).unwrap();
                        assert_eq!(captured.total, live.total);
                        assert_eq!(captured.origin, live.origin);
                        assert_eq!(
                            captured.entries,
                            live.entries
                                .iter()
                                .skip(offset)
                                .take(1)
                                .cloned()
                                .collect::<Vec<_>>()
                        );
                        assert_eq!(
                            captured.next_offset,
                            (offset + 1 < live.total).then_some(offset + 1)
                        );
                    }
                    if path == center
                        && direction == ProvenanceDirection::Both
                        && relation.is_none()
                    {
                        all_entries = live.entries;
                    }
                }
            }
        }
        assert!(
            all_entries
                .iter()
                .any(|row| row["resolution"] == "ambiguous" && row["candidate_total"] == 24)
        );
        assert!(
            all_entries
                .iter()
                .any(|row| row["incoming"] == true && row["outgoing"] == true)
        );
        let owner = "wiki/documents/target-23.md";
        let owner_offset = all_entries
            .iter()
            .position(|row| row["target"]["path"] == owner)
            .unwrap();
        let relation_offset = all_entries
            .iter()
            .position(|row| {
                row["declaring"]["path"] == center.to_str().unwrap()
                    && row["relation"] == "superseded_by"
            })
            .unwrap();
        let connection =
            Connection::open(root.path().join(crate::search_index::INDEX_PATH)).unwrap();
        // Unselected scalar payloads are irrelevant to this page.
        connection
            .execute(
                "UPDATE provenance_documents SET metadata='invalid JSON' WHERE path=?1",
                [owner],
            )
            .unwrap();
        connection.execute("UPDATE provenance_relations SET fields='invalid JSON' WHERE source=?1 AND relation='superseded_by'", [center.to_str().unwrap()]).unwrap();
        let mut options = ProvenanceOptions {
            direction: ProvenanceDirection::Both,
            relation: None,
            limit: 1,
            offset: 0,
            revision: None,
            snapshot: Some(&snapshot.snapshot),
        };
        let first = inspect_captured(&connection, center, &options, &snapshot.snapshot).unwrap();
        assert_eq!(first.entries, all_entries[..1]);
        assert_eq!(first.total, all_entries.len());
        for offset in [owner_offset, relation_offset] {
            options.offset = offset;
            assert!(matches!(
                inspect_captured(&connection, center, &options, &snapshot.snapshot),
                Err(VaultError::Serialization(_))
            ));
        }
    }

    #[test]
    fn incremental_issue_rows_follow_changed_and_removed_documents() {
        let connection = Connection::open_in_memory().unwrap();
        let raw = "---\ntype: document\nsource_identity: {repository: repo, document: a}\nsource_file: 7\n---\nSource.\n";
        let document = crate::provenance::document_page(
            "wiki/documents/a.md".into(),
            &crate::parse_page(raw).unwrap(),
            "file".into(),
            "authored".into(),
        )
        .unwrap();
        let mut capture = Capture {
            documents: vec![document.clone()],
            addresses: vec![("wiki/documents/a.md".into(), "wiki/documents/a.md".into())],
            coverage: json!({}),
            revisions: Vec::new(),
        };
        write(&connection, &capture).unwrap();
        let count = || {
            connection
                .query_row("SELECT COUNT(*) FROM provenance_issues", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap()
        };
        assert_eq!(count(), 1);
        write(&connection, &capture).unwrap();
        assert_eq!(count(), 1);
        capture.documents[0] = crate::provenance::document_page(
            "wiki/documents/a.md".into(),
            &crate::parse_page(&raw.replace("source_file: 7", "source_file: valid.md")).unwrap(),
            "changed-file".into(),
            "changed-authored".into(),
        )
        .unwrap();
        write(&connection, &capture).unwrap();
        assert_eq!(count(), 0);
        capture.documents[0] = document;
        write(&connection, &capture).unwrap();
        assert_eq!(count(), 1);
        capture.documents.clear();
        capture.addresses.clear();
        write(&connection, &capture).unwrap();
        assert_eq!(count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn indexed_filesystem_aliases_match_live_owners_and_keep_hard_links_distinct() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let documents = root.path().join("wiki/documents");
        std::fs::create_dir_all(&documents).unwrap();
        let old = documents.join("old.md");
        let new = documents.join("new.md");
        std::fs::write(&old, "---\ntype: document\ntitle: Old\nsource_identity: {repository: r, document: old}\nsource_record: {superseded_by: new}\n---\nOld scope.\n").unwrap();
        std::fs::write(&new, "---\ntype: document\ntitle: New\nsource_identity: {repository: r, document: new}\n---\nNew scope.\n").unwrap();
        let alias = documents.join("alias.md");
        std::os::unix::fs::symlink("new.md", &alias).unwrap();
        let session = crate::MutationSession::acquire(root.path()).unwrap();
        let prepared = crate::prepare_search(&session, &dirs).unwrap();
        for path in ["old", "new", "alias"] {
            for direction in [
                ProvenanceDirection::Outgoing,
                ProvenanceDirection::Incoming,
                ProvenanceDirection::Both,
            ] {
                let path = std::path::PathBuf::from(format!("wiki/documents/{path}.md"));
                let mut options = ProvenanceOptions {
                    direction,
                    relation: None,
                    limit: 100,
                    offset: 0,
                    revision: None,
                    snapshot: None,
                };
                let live = crate::inspect_provenance(root.path(), &dirs, &path, &options).unwrap();
                options.snapshot = Some(&prepared.snapshot);
                let indexed =
                    crate::inspect_provenance(root.path(), &dirs, &path, &options).unwrap();
                assert_eq!(live.path, indexed.path);
                assert_eq!(live.origin, indexed.origin);
                assert_eq!(live.entries, indexed.entries);
                assert_eq!(live.total, indexed.total);
                for key in [
                    "documents",
                    "scanned_document_addresses",
                    "filesystem_aliases",
                    "identities_unavailable",
                    "identities_invalid",
                    "documents_with_invalid_metadata",
                    "invalid_metadata_issues",
                ] {
                    assert_eq!(live.coverage[key], indexed.coverage[key], "{key}");
                }
                assert_eq!(indexed.coverage["scanned_document_addresses"], 3);
                assert_eq!(indexed.coverage["filesystem_aliases"], 1);
            }
        }
        let mut options = ProvenanceOptions {
            direction: ProvenanceDirection::Outgoing,
            relation: None,
            limit: 100,
            offset: 0,
            revision: None,
            snapshot: Some(&prepared.snapshot),
        };
        let path = Path::new("wiki/documents/old.md");
        let before = crate::inspect_provenance(root.path(), &dirs, path, &options).unwrap();
        assert_eq!(before.entries[0]["candidate_total"], 1);
        let distinct = documents.join("distinct.md");
        std::fs::hard_link(&new, &distinct).unwrap();
        let refreshed = crate::prepare_search(&session, &dirs).unwrap();
        options.snapshot = Some(&refreshed.snapshot);
        let duplicate = crate::inspect_provenance(root.path(), &dirs, path, &options).unwrap();
        assert_eq!(duplicate.entries[0]["resolution"], "ambiguous");
        assert_eq!(duplicate.entries[0]["candidate_total"], 2);
        assert_eq!(duplicate.coverage["documents"], 3);
        assert_eq!(duplicate.coverage["filesystem_aliases"], 1);
        std::fs::remove_file(distinct).unwrap();
        let external_collection = root.path().join("other.md");
        std::fs::copy(&new, &external_collection).unwrap();
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&external_collection, &alias).unwrap();
        let refreshed = crate::prepare_search(&session, &dirs).unwrap();
        options.snapshot = Some(&refreshed.snapshot);
        let independent = crate::inspect_provenance(root.path(), &dirs, path, &options).unwrap();
        assert_eq!(independent.entries[0]["candidate_total"], 2);
        assert_eq!(independent.coverage["filesystem_aliases"], 0);
        let outside_vault = tempfile::tempdir().unwrap();
        std::fs::copy(&new, outside_vault.path().join("new.md")).unwrap();
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(outside_vault.path().join("new.md"), &alias).unwrap();
        let live = ProvenanceOptions {
            snapshot: None,
            ..options
        };
        assert!(crate::inspect_provenance(root.path(), &dirs, path, &live).is_err());
        assert!(crate::prepare_search(&session, &dirs).is_err());
        let retained = crate::inspect_provenance(root.path(), &dirs, path, &options).unwrap();
        assert_eq!(retained.entries, independent.entries);
    }

    #[cfg(unix)]
    #[test]
    fn document_root_alias_supports_live_and_prepared_provenance_without_following_nested_aliases()
    {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let backing = root.path().join("wiki/pages");
        std::fs::create_dir_all(&backing).unwrap();
        std::fs::write(backing.join("old.md"), "---\ntype: document\ntitle: Old\nsource_identity: {repository: r, document: old}\nsource_record: {superseded_by: new}\n---\nOld scope.\n").unwrap();
        std::fs::write(backing.join("new.md"), "---\ntype: document\ntitle: New\nsource_identity: {repository: r, document: new}\n---\nNew scope.\n").unwrap();
        std::os::unix::fs::symlink(".", backing.join("redundant")).unwrap();
        let documents = root.path().join("wiki/documents");
        std::os::unix::fs::symlink("pages", &documents).unwrap();
        let path = Path::new("wiki/documents/old.md");
        let mut options = ProvenanceOptions {
            direction: ProvenanceDirection::Outgoing,
            relation: None,
            limit: 100,
            offset: 0,
            revision: None,
            snapshot: None,
        };
        let live = crate::inspect_provenance(root.path(), &dirs, path, &options).unwrap();
        assert_eq!(live.path, "wiki/documents/old.md");
        assert_eq!(live.total, 2);
        assert_eq!(live.entries[0]["resolution"], "resolved");
        assert_eq!(live.entries[0]["candidate_total"], 1);
        assert_eq!(live.entries[1]["target"]["path"], "wiki/documents/new.md");
        assert_eq!(live.coverage["documents"], 2);
        let session = crate::MutationSession::acquire(root.path()).unwrap();
        let prepared = crate::prepare_search(&session, &dirs).unwrap();
        options.snapshot = Some(&prepared.snapshot);
        let indexed = crate::inspect_provenance(root.path(), &dirs, path, &options).unwrap();
        assert_eq!(indexed.path, live.path);
        assert_eq!(indexed.origin, live.origin);
        assert_eq!(indexed.entries, live.entries);
        assert_eq!(indexed.total, live.total);
        assert_eq!(indexed.coverage["documents"], 2);

        let other = root.path().join("other");
        std::fs::create_dir(&other).unwrap();
        std::os::unix::fs::symlink(&other, backing.join("linked")).unwrap();
        let live_options = ProvenanceOptions {
            snapshot: None,
            ..options
        };
        assert!(crate::inspect_provenance(root.path(), &dirs, path, &live_options).is_err());
        assert!(crate::prepare_search(&session, &dirs).is_err());
        let retained = crate::inspect_provenance(root.path(), &dirs, path, &options).unwrap();
        assert_eq!(retained.entries, indexed.entries);
        std::fs::remove_file(backing.join("linked")).unwrap();

        let outside = tempfile::tempdir().unwrap();
        std::fs::remove_file(&documents).unwrap();
        std::os::unix::fs::symlink(outside.path(), &documents).unwrap();
        assert!(crate::inspect_provenance(root.path(), &dirs, path, &live_options).is_err());
        assert!(crate::prepare_search(&session, &dirs).is_err());
        let retained = crate::inspect_provenance(root.path(), &dirs, path, &options).unwrap();
        assert_eq!(retained.entries, indexed.entries);
    }

    #[cfg(unix)]
    #[test]
    fn untraversed_document_directory_aliases_refuse_capture_and_preserve_the_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let documents = root.path().join("wiki/documents");
        std::fs::create_dir_all(&documents).unwrap();
        let content = "---\ntype: document\ntitle: Kept\nsource_identity: {repository: r, document: kept}\n---\nSource.\n";
        std::fs::write(documents.join("kept.md"), content).unwrap();
        let outside_collection = root.path().join("other");
        std::fs::create_dir(&outside_collection).unwrap();
        std::fs::write(outside_collection.join("kept.md"), content).unwrap();
        let session = crate::MutationSession::acquire(root.path()).unwrap();
        let prepared = crate::prepare_search(&session, &dirs).unwrap();
        std::os::unix::fs::symlink(&outside_collection, documents.join("linked")).unwrap();
        let path = Path::new("wiki/documents/kept.md");
        let mut options = ProvenanceOptions {
            direction: ProvenanceDirection::Outgoing,
            relation: None,
            limit: 100,
            offset: 0,
            revision: None,
            snapshot: None,
        };
        assert!(crate::inspect_provenance(root.path(), &dirs, path, &options).is_err());
        assert!(crate::prepare_search(&session, &dirs).is_err());
        options.snapshot = Some(&prepared.snapshot);
        assert_eq!(
            crate::inspect_provenance(root.path(), &dirs, path, &options)
                .unwrap()
                .coverage["documents"],
            1
        );
    }

    #[test]
    fn indexed_relations_preserve_ambiguity_scope_pagination_and_captured_state() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let documents = root.path().join("wiki/documents");
        std::fs::create_dir_all(&documents).unwrap();
        for (path, identity, record) in [
            (
                "old",
                json!({"repository":"repo", "document":"old"}),
                json!({"superseded_by":"new", "relations":[{"relation":"extends", "target":"missing"}]}),
            ),
            (
                "new",
                json!({"repository":"repo", "document":"new"}),
                json!({}),
            ),
            (
                "duplicate",
                json!({"repository":"repo", "document":"new"}),
                json!({}),
            ),
            (
                "unrelated",
                json!({"repository":"other", "document":"old"}),
                json!({"superseded_by":"new"}),
            ),
            (
                "provider-old",
                json!({"provider":"google-drive", "namespace":"files", "document":"old"}),
                json!({"superseded_by":"new"}),
            ),
            (
                "provider-new",
                json!({"provider":"google-drive", "namespace":"files", "document":"new"}),
                json!({}),
            ),
            (
                "repository-files",
                json!({"repository":"files", "document":"new"}),
                json!({}),
            ),
            (
                "broken",
                json!({"document":"invalid"}),
                json!({"relations":[{}]}),
            ),
        ] {
            std::fs::write(documents.join(format!("{path}.md")), format!("---\ntype: document\ntitle: {path}\nsource_identity: {identity}\nsource_record: {record}\n---\n# {path}\n")).unwrap();
        }
        let session = crate::MutationSession::acquire(root.path()).unwrap();
        let snapshot = crate::prepare_search(&session, &dirs).unwrap();
        for path in [
            "old",
            "new",
            "broken",
            "provider-old",
            "provider-new",
            "repository-files",
        ] {
            for direction in [
                ProvenanceDirection::Both,
                ProvenanceDirection::Incoming,
                ProvenanceDirection::Outgoing,
            ] {
                for relation in [None, Some("superseded_by"), Some("extends")] {
                    let mut options = ProvenanceOptions {
                        direction,
                        relation,
                        limit: 1,
                        offset: 0,
                        revision: None,
                        snapshot: None,
                    };
                    let path = std::path::PathBuf::from(format!("wiki/documents/{path}.md"));
                    let live =
                        crate::inspect_provenance(root.path(), &dirs, &path, &options).unwrap();
                    options.snapshot = Some(&snapshot.snapshot);
                    let indexed =
                        crate::inspect_provenance(root.path(), &dirs, &path, &options).unwrap();
                    assert_eq!(live.total, indexed.total);
                    assert_eq!(live.entries, indexed.entries);
                    assert_eq!(live.origin, indexed.origin);
                    assert_eq!(live.next_offset, indexed.next_offset);
                    if let Some(offset) = indexed.next_offset {
                        options.offset = offset;
                        options.revision = Some(&indexed.revision);
                        let next =
                            crate::inspect_provenance(root.path(), &dirs, &path, &options).unwrap();
                        assert_ne!(next.entries, indexed.entries);
                    }
                }
            }
        }
        let path = Path::new("wiki/documents/old.md");
        let options = ProvenanceOptions {
            direction: ProvenanceDirection::Both,
            relation: None,
            limit: 100,
            offset: 0,
            revision: None,
            snapshot: Some(&snapshot.snapshot),
        };
        let before = crate::inspect_provenance(root.path(), &dirs, path, &options).unwrap();
        assert!(
            before
                .entries
                .iter()
                .any(|row| row["resolution"] == "ambiguous" && row["candidate_total"] == 2)
        );
        std::fs::remove_file(documents.join("duplicate.md")).unwrap();
        assert_eq!(
            before.entries,
            crate::inspect_provenance(root.path(), &dirs, path, &options)
                .unwrap()
                .entries
        );
        crate::prepare_search(&session, &dirs).unwrap();
        assert!(crate::inspect_provenance(root.path(), &dirs, path, &options).is_err());
        let connection =
            Connection::open(root.path().join(crate::search_index::INDEX_PATH)).unwrap();
        let plan:String=connection.query_row("EXPLAIN QUERY PLAN SELECT metadata FROM provenance_documents WHERE namespace='repo' AND document='new'",[],|row|row.get(3)).unwrap();
        assert!(plan.contains("provenance_identity"), "{plan}");
    }
}
