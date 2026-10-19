//! Exact imported-document identities and declared relationships; never inferred evidence.
use crate::{VaultError, page_path, parse_page};
use lk_core::{
    concept::{file_revision, source_revision},
    config::VaultDirs,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvenanceDirection {
    Outgoing,
    Incoming,
    Both,
}

pub struct ProvenanceOptions<'a> {
    pub direction: ProvenanceDirection,
    pub relation: Option<&'a str>,
    pub limit: usize,
    pub offset: usize,
    pub revision: Option<&'a str>,
    pub snapshot: Option<&'a str>,
}

#[derive(Debug, Serialize)]
pub struct ProvenanceReport {
    pub path: String,
    pub revision: String,
    pub origin: Value,
    pub coverage: Value,
    pub entries: Vec<Value>,
    pub total: usize,
    pub offset: usize,
    pub next_offset: Option<usize>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Document {
    pub(super) path: String,
    pub(super) metadata: Value,
    pub(super) identity: Option<(String, String)>,
    pub(super) declarations: Vec<Declaration>,
    pub(super) issues: Vec<String>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Declaration {
    pub(super) relation: String,
    pub(super) target: String,
    pub(super) fields: Vec<String>,
}
fn invalid(message: &str) -> VaultError {
    VaultError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        message,
    ))
}
fn nonblank(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
}
pub(super) fn read_document(path: &Path, full: &Path) -> Result<Option<Document>, VaultError> {
    let raw = std::fs::read_to_string(full)?;
    let page = parse_page(&raw).map_err(VaultError::Frontmatter)?;
    if page.frontmatter.get("type").and_then(Value::as_str) != Some("document") {
        return Ok(None);
    }
    document_page(
        path.to_string_lossy().into_owned(),
        &page,
        file_revision(&raw),
    )
    .map(Some)
}

pub(super) fn document_page(
    path: String,
    page: &crate::VaultPage,
    revision: String,
) -> Result<Document, VaultError> {
    let fm = &page.frontmatter;
    let identity_value = fm.get("source_identity");
    let parsed_identity = identity_value
        .filter(|value| !value.is_null())
        .map(|value| lk_core::source_identity::SourceIdentity::from_value(value.clone()));
    let mut issues = Vec::new();
    let source_file = fm.source_file().unwrap_or_else(|issue| {
        issues.push(issue);
        None
    });
    let identity = match &parsed_identity {
        Some(Ok(identity)) => Some(identity.key()),
        Some(Err(error)) => {
            issues.push(error.clone());
            None
        }
        None => None,
    };
    let record = fm.get("source_record");
    if record.is_some_and(|r| !r.is_null() && !r.is_object()) {
        issues.push("source_record must be an object or null".into());
    }
    if let (Some((_, id)), Some(record_id)) = (&identity, record.and_then(|r| r.get("id")))
        && !record_id.is_null()
        && record_id.as_str() != Some(id.as_str())
    {
        issues.push("source_record.id disagrees with source_identity.document".into());
    }
    let mut declarations: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    if let Some(relations) = record
        .and_then(|r| r.get("relations"))
        .filter(|r| !r.is_null())
    {
        if let Some(rows) = relations.as_array() {
            for (index, row) in rows.iter().enumerate() {
                if let (Some(relation), Some(target)) =
                    (nonblank(row.get("relation")), nonblank(row.get("target")))
                {
                    declarations
                        .entry((relation.into(), target.into()))
                        .or_default()
                        .push(format!("source_record.relations[{index}]"));
                } else {
                    issues.push(format!("invalid source_record.relations[{index}]: expected nonblank relation and target"));
                }
            }
        } else {
            issues.push("source_record.relations must be an array or null".into());
        }
    }
    if let Some(target) = record
        .and_then(|r| r.get("superseded_by"))
        .filter(|v| !v.is_null())
    {
        if let Some(target) = nonblank(Some(target)) {
            declarations
                .entry(("superseded_by".into(), target.into()))
                .or_default()
                .push("source_record.superseded_by".into());
        } else {
            issues.push("source_record.superseded_by must be a nonblank string or null".into());
        }
    }
    let identity_status = if identity.is_some() {
        "declared"
    } else if identity_value.is_none_or(Value::is_null) {
        "unavailable"
    } else {
        "invalid"
    };
    let metadata = json!({
        "path": path,
        "title": fm.get("title"),
        "file_revision": revision,
        "source_revision": source_revision(page),
        "source_identity": parsed_identity.as_ref().and_then(|identity|identity.as_ref().ok()),
        "identity_status": identity_status,
        "metadata_issue_count": issues.len(),
        "source_status": record.and_then(|r| r.get("status")),
        "status_basis": "recorded-source-metadata-not-current-truth",
        "timestamp_basis": record.and_then(|r| r.get("timestamp_basis")),
        "source_url": fm.get("source_url"),
        "source_file": source_file
    });
    Ok(Document {
        path,
        metadata,
        identity,
        issues,
        declarations: declarations
            .into_iter()
            .map(|((relation, target), fields)| Declaration {
                relation,
                target,
                fields,
            })
            .collect(),
    })
}

#[derive(Default)]
pub(super) struct DocumentCollection {
    pub(super) documents: Vec<Document>,
    pub(super) addresses: Vec<(String, String)>,
    pub(super) revisions: Vec<(String, Value, std::path::PathBuf)>,
}

pub(super) fn collect_documents(
    root: &Path,
    dirs: &VaultDirs,
    mut load: impl FnMut(&Path, &Path, &Path) -> Result<Option<Document>, VaultError>,
) -> Result<DocumentCollection, VaultError> {
    let directory = lk_core::vault_path::documents_dir(dirs);
    let full = page_path(root, &directory)?;
    match std::fs::metadata(&full) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Err(invalid("document collection is not a directory")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DocumentCollection::default());
        }
        Err(error) => return Err(error.into()),
    }
    let mut owners: BTreeMap<std::path::PathBuf, (Document, bool)> = BTreeMap::new();
    let mut addresses = Vec::new();
    let mut revisions = Vec::new();
    for entry in walkdir::WalkDir::new(full).follow_links(false) {
        let entry = entry.map_err(|error| VaultError::Io(std::io::Error::other(error)))?;
        // WalkDir traverses a validated collection-root alias, but not nested aliases.
        if entry.depth() > 0
            && entry.file_type().is_symlink()
            && std::fs::metadata(entry.path())?.is_dir()
        {
            return Err(invalid(
                "document collection contains an untraversed directory alias",
            ));
        }
        if entry
            .path()
            .extension()
            .is_none_or(|extension| extension != "md")
            || entry.file_type().is_dir()
        {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|_| invalid("document escaped vault"))?;
        let checked = page_path(root, relative)?;
        let metadata = std::fs::metadata(&checked)?;
        if !metadata.is_file() {
            return Err(invalid("document address is not a regular file"));
        }
        let canonical = checked.canonicalize()?;
        let Some(document) = load(relative, &checked, &canonical)? else {
            continue;
        };
        let path = document.path.clone();
        addresses.push((path.clone(), canonical.clone()));
        revisions.push((
            path,
            document.metadata["file_revision"].clone(),
            canonical.clone(),
        ));
        let alias = entry.file_type().is_symlink();
        match owners.entry(canonical) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert((document, alias));
            }
            std::collections::btree_map::Entry::Occupied(mut entry) if !alias => {
                entry.insert((document, alias));
            }
            _ => {}
        }
    }
    addresses.sort();
    revisions.sort_by(|a, b| a.0.cmp(&b.0));
    let addresses = addresses
        .into_iter()
        .map(|(path, canonical)| (path, owners[&canonical].0.path.clone()))
        .collect();
    let mut documents = owners
        .into_values()
        .map(|(document, _)| document)
        .collect::<Vec<_>>();
    documents.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(DocumentCollection {
        documents,
        addresses,
        revisions,
    })
}

/// Scan the configured document collection. Exact typed namespace/document keys are the only
/// resolution keys. The collection revision pins additions and ambiguity as well as source edits.
/// This is a read-only observation, not an atomic filesystem snapshot against external editors.
pub fn inspect_provenance(
    root: &Path,
    dirs: &VaultDirs,
    path: &Path,
    options: &ProvenanceOptions<'_>,
) -> Result<ProvenanceReport, VaultError> {
    if !(1..=100).contains(&options.limit) || path.extension().is_none_or(|e| e != "md") {
        return Err(invalid(
            "provenance requires a Markdown document and --limit in 1..100",
        ));
    }
    if let Some(snapshot) = options.snapshot {
        return crate::provenance_index::inspect(root, dirs, path, options, snapshot);
    }
    let center_full = page_path(root, path)?;
    let collection = collect_documents(root, dirs, |relative, checked, _| {
        read_document(relative, checked)
    })?;
    let documents = collection.documents;
    let center_canonical = std::fs::canonicalize(center_full)?;
    let center = documents
        .iter()
        .find(|d| std::fs::canonicalize(root.join(&d.path)).is_ok_and(|p| p == center_canonical))
        .ok_or_else(|| {
            invalid("provenance requires a document in the configured documents collection")
        })?;
    let revision = file_revision(
        &serde_json::to_string(&json!({"version":1,"path":path,
            "direction":format!("{:?}",options.direction),"relation":options.relation,
            "collection":collection.revisions
        }))
        .map_err(|e| VaultError::Serialization(e.to_string()))?,
    );
    if options.revision.is_some_and(|r| r != revision) {
        return Err(invalid("provenance collection changed; restart pagination"));
    }
    let coverage = json!({
        "documents":documents.len(),
        "scanned_document_addresses":collection.addresses.len(),
        "filesystem_aliases":collection.addresses.len() - documents.len(),
        "identities_unavailable":documents.iter().filter(|d|d.metadata["identity_status"]=="unavailable").count(),
        "identities_invalid":documents.iter().filter(|d|d.metadata["identity_status"]=="invalid").count(),
        "documents_with_invalid_metadata":documents.iter().filter(|d|!d.issues.is_empty()).count(),
        "invalid_metadata_issues":documents.iter().map(|d|d.issues.len()).sum::<usize>(),
        "scope":"configured document collection; unavailable identities cannot resolve or contribute incoming matches; malformed declarations are not guessed",
        "snapshot":"read-only live scan; external editors are not locked"
    });
    build_report(center, &documents, options, revision, coverage)
}

pub(super) fn build_report(
    center: &Document,
    documents: &[Document],
    options: &ProvenanceOptions<'_>,
    revision: String,
    coverage: Value,
) -> Result<ProvenanceReport, VaultError> {
    let mut registry: BTreeMap<&(String, String), Vec<&Document>> = BTreeMap::new();
    for d in documents {
        if let Some(id) = &d.identity {
            registry.entry(id).or_default().push(d);
        }
    }
    let mut rows = Vec::new();
    for d in documents {
        let is_center = d.path == center.path;
        // Unrelated malformed metadata is disclosed by coverage counts, not navigation rows.
        if is_center {
            for issue in &d.issues {
                rows.push(json!({"kind":"invalid-metadata","declaring":d.metadata,"issue":issue}));
            }
        }
        for declaration in &d.declarations {
            let target_identity = d
                .identity
                .as_ref()
                .map(|(repo, _)| (repo.clone(), declaration.target.clone()));
            let outgoing = is_center && options.direction != ProvenanceDirection::Incoming;
            let incoming = options.direction != ProvenanceDirection::Outgoing
                && center.identity.is_some()
                && target_identity == center.identity;
            if !(outgoing || incoming)
                || options.relation.is_some_and(|r| r != declaration.relation)
            {
                continue;
            }
            let owners = target_identity
                .as_ref()
                .and_then(|id| registry.get(id))
                .cloned()
                .unwrap_or_default();
            let status = if target_identity.is_none() {
                if d.metadata["identity_status"] == "unavailable" {
                    "unavailable-identity"
                } else {
                    "invalid-identity"
                }
            } else {
                match owners.len() {
                    0 => "unresolved",
                    1 => "resolved",
                    _ => "ambiguous",
                }
            };
            // Owners are separately paginated to bound ambiguous results without dropping names.
            rows.push(json!({"kind":"declared-relation", "declaring":d.metadata,
                "relation":declaration.relation,"fields":declaration.fields,"target_identity": d.metadata.get("source_identity").filter(|value|!value.is_null()).and_then(|value|lk_core::source_identity::SourceIdentity::from_value(value.clone()).ok()).map(|identity|identity.with_document(&declaration.target)),
                "target_id":declaration.target,"incoming":incoming,"outgoing":outgoing,
                "resolution":status,"candidate_total":owners.len()}));
            for owner in owners {
                rows.push(json!({"kind":"target-candidate","declaring_path":d.path,
                "relation":declaration.relation,"target_id":declaration.target,"target":owner.metadata}));
            }
        }
    }
    let total = rows.len();
    let entries: Vec<_> = rows
        .into_iter()
        .skip(options.offset)
        .take(options.limit)
        .collect();
    let end = options.offset.saturating_add(entries.len());
    Ok(ProvenanceReport {
        path: center.path.clone(),
        revision,
        origin: center.metadata.clone(),
        coverage,
        entries,
        total,
        offset: options.offset,
        next_offset: (end < total).then_some(end),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn write(root: &Path, name: &str, identity: Value, record: Value) {
        let dir = root.join("wiki/documents");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{name}.md")),format!("---\ntype: document\ntitle: {name}\nsource_identity: {identity}\nsource_record: {record}\n---\nNo Markdown links.\n")).unwrap();
    }
    fn options() -> ProvenanceOptions<'static> {
        ProvenanceOptions {
            direction: ProvenanceDirection::Outgoing,
            relation: None,
            limit: 100,
            offset: 0,
            revision: None,
            snapshot: None,
        }
    }
    #[test]
    fn exact_repository_resolution_preserves_fields_and_does_not_infer_relations() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(
            root,
            "old",
            json!({"repository":"r","document":"old"}),
            json!({"superseded_by":"new","relations":[{"relation":"superseded_by","target":"new"},{"relation":"superseded_by","target":"new"},{"relation":"references","target":"absent"}]}),
        );
        write(
            root,
            "new",
            json!({"repository":"r","document":"new"}),
            json!({"status":"superseded"}),
        );
        write(
            root,
            "foreign",
            json!({"repository":"elsewhere","document":"new"}),
            json!({}),
        );
        let report = inspect_provenance(
            root,
            &VaultDirs::default(),
            Path::new("wiki/documents/old.md"),
            &options(),
        )
        .unwrap();
        let declared: Vec<_> = report
            .entries
            .iter()
            .filter(|r| r["kind"] == "declared-relation")
            .collect();
        assert_eq!(declared.len(), 2);
        assert_eq!(declared[0]["resolution"], "unresolved");
        assert_eq!(declared[1]["resolution"], "resolved");
        assert_eq!(declared[1]["fields"].as_array().unwrap().len(), 3);
        assert_eq!(declared[1]["candidate_total"], 1);
        assert!(
            report
                .entries
                .iter()
                .all(|r| r["relation"] != "support" && r["relation"] != "contradiction")
        );
        let incoming = inspect_provenance(
            root,
            &VaultDirs::default(),
            Path::new("wiki/documents/new.md"),
            &ProvenanceOptions {
                direction: ProvenanceDirection::Incoming,
                ..options()
            },
        )
        .unwrap();
        assert!(incoming.entries.iter().any(|r|r["declaring"]["path"]=="wiki/documents/old.md" && r["incoming"]==true));
    }
    #[test]
    fn ambiguity_and_invalid_metadata_are_visible_and_collection_changes_break_pagination() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(
            root,
            "old",
            json!({"repository":"r","document":"old"}),
            json!({"superseded_by":"new","relations":[{"target":"broken"}]}),
        );
        write(
            root,
            "new",
            json!({"repository":"r","document":"new"}),
            json!({}),
        );
        let path = Path::new("wiki/documents/old.md");
        let dirs = VaultDirs::default();
        let first = inspect_provenance(
            root,
            &dirs,
            path,
            &ProvenanceOptions {
                limit: 1,
                ..options()
            },
        )
        .unwrap();
        assert_eq!(first.entries[0]["kind"], "invalid-metadata");
        assert_eq!(first.next_offset, Some(1));
        let pinned = ProvenanceOptions {
            revision: Some(&first.revision),
            offset: 1,
            limit: 1,
            ..options()
        };
        assert_eq!(
            inspect_provenance(root, &dirs, path, &pinned)
                .unwrap()
                .entries
                .len(),
            1
        );
        write(
            root,
            "duplicate",
            json!({"repository":"r","document":"new"}),
            json!({}),
        );
        assert!(inspect_provenance(root, &dirs, path, &pinned).is_err());
        let report = inspect_provenance(root, &dirs, path, &options()).unwrap();
        assert!(
            report
                .entries
                .iter()
                .any(|r| r["resolution"] == "ambiguous" && r["candidate_total"] == 2)
        );
        write(
            root,
            "invalid",
            json!({"document":"x"}),
            json!({"superseded_by":"new"}),
        );
        let report = inspect_provenance(
            root,
            &dirs,
            Path::new("wiki/documents/invalid.md"),
            &options(),
        )
        .unwrap();
        assert!(
            report
                .entries
                .iter()
                .any(|r| r["resolution"] == "invalid-identity")
        );
        assert!(
            inspect_provenance(
                root,
                &dirs,
                path,
                &ProvenanceOptions {
                    limit: 101,
                    ..options()
                }
            )
            .is_err()
        );
        assert!(inspect_provenance(root, &dirs, Path::new("../outside.md"), &options()).is_err());
    }

    #[test]
    fn missing_identity_has_no_filename_or_record_id_fallback_and_deleted_target_is_unresolved() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let dir = root.join("wiki/documents");
        std::fs::create_dir_all(&dir).unwrap();
        let path = Path::new("wiki/documents/old.md");
        std::fs::write(
            root.join(path),
            "---\ntype: document\nsource_record: {id: old, superseded_by: target}\n---\n",
        )
        .unwrap();
        let options = ProvenanceOptions {
            direction: ProvenanceDirection::Outgoing,
            relation: None,
            limit: 100,
            offset: 0,
            revision: None,
            snapshot: None,
        };
        let report = inspect_provenance(root, &VaultDirs::default(), path, &options).unwrap();
        assert_eq!(report.origin["identity_status"], "unavailable");
        assert!(
            report
                .entries
                .iter()
                .any(|r| r["resolution"] == "unavailable-identity")
        );
        std::fs::write(root.join(path), "---\ntype: document\nsource_identity: {repository: r, document: old}\nsource_record: {superseded_by: target}\n---\n").unwrap();
        std::fs::write(
            dir.join("target.md"),
            "---\ntype: document\nsource_identity: {repository: r, document: target}\n---\n",
        )
        .unwrap();
        let before = inspect_provenance(root, &VaultDirs::default(), path, &options).unwrap();
        std::fs::remove_file(dir.join("target.md")).unwrap();
        assert!(
            inspect_provenance(
                root,
                &VaultDirs::default(),
                path,
                &ProvenanceOptions {
                    revision: Some(&before.revision),
                    ..options
                }
            )
            .is_err()
        );
        let after = inspect_provenance(root, &VaultDirs::default(), path, &options).unwrap();
        assert!(
            after
                .entries
                .iter()
                .any(|r| r["resolution"] == "unresolved" && r["target_id"] == "target")
        );
    }
    #[test]
    fn incoming_is_not_flooded_by_unrelated_unknowns_and_records_are_compact() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(
            root,
            "center",
            json!({"repository":"r","document":"center"}),
            json!({"relations":[]}),
        );
        write(root, "unknown", Value::Null, json!({"relations":[]}));
        write(
            root,
            "malformed",
            json!({"repository":"r","document":"bad"}),
            json!({"relations":"broken"}),
        );
        let report = inspect_provenance(
            root,
            &VaultDirs::default(),
            Path::new("wiki/documents/center.md"),
            &ProvenanceOptions {
                direction: ProvenanceDirection::Incoming,
                ..options()
            },
        )
        .unwrap();
        assert!(report.entries.is_empty());
        assert_eq!(report.coverage["identities_unavailable"], 1);
        assert_eq!(report.coverage["documents_with_invalid_metadata"], 1);
        assert!(report.origin.get("source_record").is_none());
        assert_eq!(report.origin["metadata_issue_count"], 0);
    }
    #[cfg(unix)]
    #[test]
    fn filesystem_symlink_aliases_share_one_owner_and_remain_traversable() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(
            root,
            "old",
            json!({"repository":"r","document":"old"}),
            json!({"superseded_by":"new"}),
        );
        write(
            root,
            "new",
            json!({"repository":"r","document":"new"}),
            json!({}),
        );
        let alias = root.join("wiki/documents/alias.md");
        std::os::unix::fs::symlink("new.md", &alias).unwrap();
        let dirs = VaultDirs::default();
        let report =
            inspect_provenance(root, &dirs, Path::new("wiki/documents/old.md"), &options())
                .unwrap();
        assert_eq!(report.entries[0]["resolution"], "resolved");
        assert_eq!(report.entries[0]["candidate_total"], 1);
        assert_eq!(report.entries[1]["target"]["path"], "wiki/documents/new.md");
        assert_eq!(report.coverage["filesystem_aliases"], 1);
        let incoming = inspect_provenance(
            root,
            &dirs,
            Path::new("wiki/documents/alias.md"),
            &ProvenanceOptions {
                direction: ProvenanceDirection::Incoming,
                ..options()
            },
        )
        .unwrap();
        assert_eq!(incoming.path, "wiki/documents/new.md");
        assert!(incoming.entries.iter().any(|e| e["incoming"] == true));
        // A hard link is another directory entry, not a symlink spelling: preserve ambiguity.
        std::fs::hard_link(
            root.join("wiki/documents/new.md"),
            root.join("wiki/documents/distinct.md"),
        )
        .unwrap();
        let duplicate =
            inspect_provenance(root, &dirs, Path::new("wiki/documents/old.md"), &options())
                .unwrap();
        assert_eq!(duplicate.entries[0]["resolution"], "ambiguous");
        assert_eq!(duplicate.entries[0]["candidate_total"], 2);
        std::fs::remove_file(&alias).unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(
            outside.path().join("outside.md"),
            "---\ntype: document\n---\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(outside.path().join("outside.md"), &alias).unwrap();
        assert!(
            inspect_provenance(root, &dirs, Path::new("wiki/documents/old.md"), &options())
                .is_err()
        );
    }
    #[cfg(unix)]
    #[test]
    fn alias_destination_changes_bind_revision_even_when_bytes_match() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(
            root,
            "old",
            json!({"repository":"r","document":"old"}),
            json!({"superseded_by":"new"}),
        );
        write(
            root,
            "new",
            json!({"repository":"r","document":"new"}),
            json!({}),
        );
        let target = root.join("wiki/documents/new.md");
        let alias = root.join("wiki/documents/alias.md");
        let outside_collection = root.join("other.md");
        std::fs::copy(&target, &outside_collection).unwrap();
        std::os::unix::fs::symlink("new.md", &alias).unwrap();
        let path = Path::new("wiki/documents/old.md");
        let dirs = VaultDirs::default();
        let before = inspect_provenance(root, &dirs, path, &options()).unwrap();
        assert_eq!(before.entries[0]["resolution"], "resolved");
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&outside_collection, &alias).unwrap();
        let after = inspect_provenance(root, &dirs, path, &options()).unwrap();
        assert_eq!(after.entries[0]["resolution"], "ambiguous");
        assert_ne!(before.revision, after.revision);
        assert!(
            inspect_provenance(
                root,
                &dirs,
                path,
                &ProvenanceOptions {
                    revision: Some(&before.revision),
                    ..options()
                }
            )
            .is_err()
        );
    }
}
