use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use lk_core::config::VaultDirs;
use rusqlite::{Connection, OpenFlags, params};
use serde::Serialize;
use serde_json::{Value, json};

use crate::provenance::{Document, ProvenanceDirection, ProvenanceOptions, ProvenanceReport};
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
    connection.execute_batch("CREATE TABLE IF NOT EXISTS provenance_documents(path TEXT PRIMARY KEY, namespace TEXT, document TEXT, payload TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS provenance_identity ON provenance_documents(namespace, document, path);
        CREATE TABLE IF NOT EXISTS provenance_addresses(path TEXT PRIMARY KEY, owner TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS provenance_relations(source TEXT NOT NULL, relation TEXT NOT NULL, namespace TEXT, target TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS provenance_outgoing ON provenance_relations(source, relation, target);
        CREATE INDEX IF NOT EXISTS provenance_incoming ON provenance_relations(namespace, target, relation, source);
        CREATE TABLE IF NOT EXISTS provenance_coverage(payload TEXT NOT NULL);")?;
    let mut stored_documents = {
        let mut query = connection.prepare("SELECT path, payload FROM provenance_documents")?;
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
        connection.prepare("INSERT INTO provenance_documents VALUES (?1,?2,?3,?4)
            ON CONFLICT(path) DO UPDATE SET namespace=excluded.namespace, document=excluded.document, payload=excluded.payload")?;
    let mut address = connection.prepare(
        "INSERT INTO provenance_addresses VALUES (?1,?2)
        ON CONFLICT(path) DO UPDATE SET owner=excluded.owner",
    )?;
    let mut relation =
        connection.prepare("INSERT INTO provenance_relations VALUES (?1,?2,?3,?4)")?;
    for page in &captured.documents {
        let payload = serde_json::to_string(page)
            .map_err(|error| VaultError::Serialization(error.to_string()))?;
        if stored_documents.remove(&page.path).as_ref() == Some(&payload) {
            continue;
        }
        let repository = page.identity.as_ref().map(|(repository, _)| repository);
        let id = page.identity.as_ref().map(|(_, id)| id);
        document.execute(params![page.path, repository, id, payload,])?;
        connection.execute(
            "DELETE FROM provenance_relations WHERE source=?1",
            [&page.path],
        )?;
        for declaration in &page.declarations {
            relation.execute(params![
                page.path,
                declaration.relation,
                repository,
                declaration.target
            ])?;
        }
    }
    for path in stored_documents.keys() {
        connection.execute("DELETE FROM provenance_relations WHERE source=?1", [path])?;
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

fn decode(raw: &str) -> Result<Document, VaultError> {
    serde_json::from_str(raw).map_err(|error| VaultError::Serialization(error.to_string()))
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
    let center = transaction.query_row("SELECT d.payload FROM provenance_addresses a JOIN provenance_documents d ON d.path=a.owner WHERE a.path=?1", [path.to_string_lossy().as_ref()], |row|row.get::<_,String>(0))?;
    let center = decode(&center)?;
    let revision = lk_core::concept::file_revision(&serde_json::to_string(&json!({"snapshot":snapshot,"path":center.path,"direction":format!("{:?}",options.direction),"relation":options.relation})).map_err(|error| VaultError::Serialization(error.to_string()))?);
    if options
        .revision
        .is_some_and(|expected| expected != revision)
    {
        return Err(
            std::io::Error::other("provenance collection changed; restart pagination").into(),
        );
    }
    let mut selected = BTreeMap::from([(center.path.clone(), center.clone())]);
    if options.direction != ProvenanceDirection::Outgoing
        && let Some((repository, id)) = &center.identity
    {
        let mut query = transaction.prepare("SELECT DISTINCT d.payload FROM provenance_relations r JOIN provenance_documents d ON d.path=r.source WHERE r.namespace=?1 AND r.target=?2 AND (?3 IS NULL OR r.relation=?3) ORDER BY d.path")?;
        let rows = query.query_map(params![repository, id, options.relation], |row| {
            row.get::<_, String>(0)
        })?;
        for row in rows {
            let page = decode(&row?)?;
            selected.insert(page.path.clone(), page);
        }
    }
    let mut targets = BTreeSet::new();
    for page in selected.values() {
        for declaration in &page.declarations {
            if options
                .relation
                .is_some_and(|relation| relation != declaration.relation)
            {
                continue;
            }
            let identity = page
                .identity
                .as_ref()
                .map(|(repository, _)| (repository.clone(), declaration.target.clone()));
            let outgoing =
                page.path == center.path && options.direction != ProvenanceDirection::Incoming;
            let incoming = options.direction != ProvenanceDirection::Outgoing
                && center.identity.is_some()
                && identity == center.identity;
            if (outgoing || incoming)
                && let Some(identity) = identity
            {
                targets.insert(identity);
            }
        }
    }
    let mut owners = transaction.prepare(
        "SELECT payload FROM provenance_documents WHERE namespace=?1 AND document=?2 ORDER BY path",
    )?;
    for (repository, id) in targets {
        let rows = owners.query_map(params![repository, id], |row| row.get::<_, String>(0))?;
        for row in rows {
            let page = decode(&row?)?;
            selected.insert(page.path.clone(), page);
        }
    }
    let coverage: String =
        transaction.query_row("SELECT payload FROM provenance_coverage", [], |row| {
            row.get(0)
        })?;
    let coverage = serde_json::from_str(&coverage)
        .map_err(|error| VaultError::Serialization(error.to_string()))?;
    crate::provenance::build_report(
        &center,
        &selected.into_values().collect::<Vec<_>>(),
        options,
        revision,
        coverage,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let plan:String=connection.query_row("EXPLAIN QUERY PLAN SELECT payload FROM provenance_documents WHERE namespace='repo' AND document='new'",[],|row|row.get(3)).unwrap();
        assert!(plan.contains("provenance_identity"), "{plan}");
    }
}
