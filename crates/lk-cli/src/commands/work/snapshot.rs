//! A synthesis request's context snapshot: the exact bytes of each context-only page (a
//! direct link or ranked discovery match) its Work read. A later preparation of the same
//! request reads a page the snapshot holds from those bytes, so a neighbor's resynthesis
//! does not replan the request; its subject, citations and reviews stay current reads, and
//! live page bytes still guard publication. A page that joins the request's context is read
//! as it is now; one that leaves it is dropped at the next record.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use miette::IntoDiagnostic;
use serde::{Deserialize, Serialize};

const STORE: &str = ".lorekeeper/queue/context";
const SCHEMA: &str = "context-snapshot-v1";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    schema: String,
    task_id: String,
    cache_hash: String,
    #[serde(deserialize_with = "super::receipt::unique_entries")]
    pages: BTreeMap<String, String>,
}

impl Record {
    fn decode(raw: &str) -> miette::Result<Self> {
        let record: Self = super::receipt::decode(raw)
            .map_err(|error| miette::miette!("invalid context snapshot: {error}"))?;
        if record.schema != SCHEMA {
            return Err(miette::miette!(
                "unsupported context snapshot schema {}",
                record.schema
            ));
        }
        Ok(record)
    }
}

/// The context pages one synthesis request reads, by address: the stored bytes of a page the
/// snapshot holds, otherwise the bytes captured now.
#[derive(Clone)]
pub(super) struct ContextSnapshot {
    task_id: String,
    cache_hash: String,
    stored: Option<(String, BTreeMap<String, String>)>,
    pages: BTreeMap<String, String>,
}

impl ContextSnapshot {
    pub(super) fn load(
        root: &Path,
        read: &lk_vault::ReadControl,
        task_id: &str,
        cache_hash: &str,
    ) -> miette::Result<Self> {
        read.check().into_diagnostic()?;
        let stored = match read_optional(root, &path(task_id, cache_hash))? {
            Some(raw) => {
                let record = Record::decode(&raw)?;
                if record.task_id != task_id || record.cache_hash != cache_hash {
                    return Err(miette::miette!(
                        "context snapshot belongs to a different synthesis request"
                    ));
                }
                Some((raw, record.pages))
            }
            None => None,
        };
        Ok(Self {
            task_id: task_id.into(),
            cache_hash: cache_hash.into(),
            stored,
            pages: BTreeMap::new(),
        })
    }

    /// Binds the context pages captured now; an address the snapshot holds keeps its bytes.
    pub(super) fn bind(&mut self, captured: BTreeMap<String, String>) {
        let stored = self.stored.as_ref().map(|(_, pages)| pages);
        self.pages = captured
            .into_iter()
            .map(|(address, raw)| {
                let raw = stored
                    .and_then(|pages| pages.get(&address))
                    .cloned()
                    .unwrap_or(raw);
                (address, raw)
            })
            .collect();
    }

    pub(super) fn page(&self, address: &str) -> Option<&str> {
        self.pages.get(address).map(String::as_str)
    }

    /// Guards the snapshot this request was read from, or its absence.
    pub(super) fn guard(&self, plan: &mut lk_vault::MutationPlan) {
        let path = path(&self.task_id, &self.cache_hash);
        match &self.stored {
            Some((raw, _)) => plan.require_unchanged(path, raw.clone()),
            None => plan.require_absent(path),
        }
    }

    /// Records the bound pages unless the stored snapshot already holds exactly them; the
    /// recorded snapshot is then the one later guards expect.
    pub(super) fn plan_record(
        &mut self,
        root: &Path,
        plan: &mut lk_vault::MutationPlan,
    ) -> miette::Result<bool> {
        if self
            .stored
            .as_ref()
            .is_some_and(|(_, pages)| pages == &self.pages)
        {
            return Ok(false);
        }
        let record = Record {
            schema: SCHEMA.into(),
            task_id: self.task_id.clone(),
            cache_hash: self.cache_hash.clone(),
            pages: self.pages.clone(),
        };
        let raw = serde_json::to_string(&record).into_diagnostic()?;
        plan.publish_expected(
            root,
            path(&self.task_id, &self.cache_hash),
            self.stored.as_ref().map(|(raw, _)| raw.clone()),
            raw.clone(),
        )
        .into_diagnostic()?;
        self.stored = Some((raw, record.pages));
        Ok(true)
    }
}

/// Removes the snapshots of requests no longer pending. Only regular JSON files are
/// snapshots; one that does not decode as a snapshot at its own address is kept and reported.
pub(super) fn plan_prune(
    root: &Path,
    pending: &BTreeSet<(String, String)>,
    plan: &mut lk_vault::MutationPlan,
) -> miette::Result<usize> {
    let directory = lk_vault::page_path(root, Path::new(STORE)).into_diagnostic()?;
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(miette::miette!("read context snapshots: {error}")),
    };
    let mut removed = 0;
    for entry in entries {
        let entry = entry.into_diagnostic()?;
        if !entry.file_type().into_diagnostic()?.is_file()
            || entry
                .path()
                .extension()
                .is_none_or(|extension| extension != "json")
        {
            continue;
        }
        let relative = Path::new(STORE).join(entry.file_name());
        let raw = std::fs::read_to_string(entry.path());
        let record = match raw.as_deref().map(Record::decode) {
            Ok(Ok(record)) if relative == path(&record.task_id, &record.cache_hash) => record,
            _ => {
                eprintln!(
                    "preserving unrecognized context snapshot: {}",
                    entry.path().display()
                );
                continue;
            }
        };
        if pending.contains(&(record.task_id, record.cache_hash)) {
            continue;
        }
        plan.remove(relative, raw.into_diagnostic()?);
        removed += 1;
    }
    Ok(removed)
}

fn path(task_id: &str, cache_hash: &str) -> PathBuf {
    let digest = lk_core::rss_original::digest(
        &serde_json::to_vec(&(task_id, cache_hash)).expect("identity serializes"),
    );
    Path::new(STORE).join(format!("{digest}.json"))
}

fn read_optional(root: &Path, path: &Path) -> miette::Result<Option<String>> {
    let full = lk_vault::page_path(root, path).into_diagnostic()?;
    match std::fs::symlink_metadata(&full) {
        Ok(metadata) if !metadata.is_file() || metadata.file_type().is_symlink() => {
            return Err(miette::miette!(
                "context snapshots must be direct regular files: {}",
                path.display()
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).into_diagnostic(),
    }
    std::fs::read_to_string(full).map(Some).into_diagnostic()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recorded_snapshot_guards_its_bytes_and_retention_keeps_unrelated_files() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let session = lk_vault::MutationSession::acquire(&root).unwrap();
        let read = lk_vault::ReadControl::default();
        let neighbor = "wiki/concepts/neighbor.md";
        let mut snapshot = ContextSnapshot::load(&root, &read, "task", "hash").unwrap();
        snapshot.bind(BTreeMap::from([(neighbor.into(), "first".into())]));
        let mut plan = lk_vault::MutationPlan::default();
        assert!(snapshot.plan_record(&root, &mut plan).unwrap());
        session.apply(plan).unwrap();
        let mut unchanged = lk_vault::MutationPlan::default();
        assert!(!snapshot.plan_record(&root, &mut unchanged).unwrap());

        let mut reloaded = ContextSnapshot::load(&root, &read, "task", "hash").unwrap();
        reloaded.bind(BTreeMap::from([(neighbor.into(), "second".into())]));
        assert_eq!(reloaded.page(neighbor), Some("first"));

        let file = root.join(path("task", "hash"));
        let raw = std::fs::read_to_string(&file).unwrap();
        let forged = raw.replace(
            "\"pages\":{",
            &format!("\"pages\":{{\"{neighbor}\":\"forged\","),
        );
        std::fs::write(&file, &forged).unwrap();
        assert!(ContextSnapshot::load(&root, &read, "task", "hash").is_err());
        let mut guard = lk_vault::MutationPlan::default();
        snapshot.guard(&mut guard);
        assert!(session.apply(guard).is_err(), "a replaced snapshot refuses");
        std::fs::write(&file, &raw).unwrap();

        let store = root.join(STORE);
        std::fs::create_dir(store.join("directory.json")).unwrap();
        std::fs::write(store.join("interrupted.json.0.tmp"), [0xff]).unwrap();
        std::fs::write(store.join("unreadable.json"), [0xff]).unwrap();
        let pending = BTreeSet::from([("task".to_owned(), "hash".to_owned())]);
        let mut plan = lk_vault::MutationPlan::default();
        assert_eq!(plan_prune(&root, &pending, &mut plan).unwrap(), 0);
        assert_eq!(plan_prune(&root, &BTreeSet::new(), &mut plan).unwrap(), 1);
        session.apply(plan).unwrap();
        assert!(!file.exists());
        assert!(store.join("unreadable.json").exists());
    }
}
