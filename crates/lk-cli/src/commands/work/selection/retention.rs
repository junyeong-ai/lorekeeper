use super::*;

struct QueueReferences {
    identities: BTreeSet<(String, String)>,
    snapshots: BTreeMap<PathBuf, String>,
}

fn retained_queue_references(
    root: &Path,
    pending: &BTreeSet<(String, String)>,
    retained_pending: &BTreeSet<(String, String)>,
) -> miette::Result<QueueReferences> {
    let mut observed = BTreeSet::new();
    let mut retained = BTreeSet::new();
    let mut snapshots = BTreeMap::new();
    for (directory, extension, staged) in [
        (".lorekeeper/queue", "jsonl", false),
        (".lorekeeper/queue/results", "json", true),
    ] {
        let full = lk_vault::page_path(root, Path::new(directory)).into_diagnostic()?;
        let entries = match std::fs::read_dir(full) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).into_diagnostic(),
        };
        for entry in entries {
            let entry = entry.into_diagnostic()?;
            let path = Path::new(directory).join(entry.file_name());
            if !path.extension().is_some_and(|value| value == extension) {
                continue;
            }
            let raw =
                read_optional(root, &path)?.ok_or_else(|| fail("queue reference disappeared"))?;
            if staged {
                let name = path.to_string_lossy();
                let identity = if name.ends_with(lk_queue::SECTION_RESULT_SUFFIX) {
                    let result: lk_queue::SectionResult =
                        serde_json::from_str(&raw).into_diagnostic()?;
                    (result.task_id, result.cache_hash)
                } else if name.ends_with(lk_queue::SYNTHESIS_RESULT_SUFFIX) {
                    let result: lk_queue::SynthesisResult =
                        serde_json::from_str(&raw).into_diagnostic()?;
                    (result.task_id, result.cache_hash)
                } else {
                    let result: lk_queue::TaskResult =
                        serde_json::from_str(&raw).into_diagnostic()?;
                    (result.task_id, result.cache_hash)
                };
                retained.insert(identity);
            } else {
                let (bytes, tasks) =
                    lk_queue::read_tasks_snapshot(&root.join(&path)).into_diagnostic()?;
                if bytes != raw.as_bytes() || !tasks.is_rewritable() {
                    return Err(fail(
                        "pending queue changed or is unparseable during screening retention",
                    ));
                }
                for task in tasks.tasks {
                    task.validate_retained_input().map_err(fail)?;
                    task.generation_policy().map_err(fail)?;
                    if !pending.contains(&(task.task_id.clone(), task.cache_hash.clone())) {
                        return Err(fail(
                            "pending queue identities changed during screening retention",
                        ));
                    }
                    let key = (task.task_id, task.cache_hash);
                    observed.insert(key.clone());
                    if retained_pending.contains(&key) {
                        retained.insert(key);
                    }
                }
            }
            snapshots.insert(path, raw);
        }
    }
    if !pending.is_subset(&observed) {
        return Err(fail(
            "pending queue reference disappeared during screening retention",
        ));
    }
    Ok(QueueReferences {
        identities: retained,
        snapshots,
    })
}

/// Every artifact of one task's screening store, with the task each one names.
struct OwnedArtifacts {
    files: BTreeMap<PathBuf, String>,
    identity: Option<(String, String)>,
}

fn owned_artifacts(root: &Path, owner: &Path) -> miette::Result<OwnedArtifacts> {
    let mut artifacts = OwnedArtifacts {
        files: BTreeMap::new(),
        identity: None,
    };
    let mut claim = |identity: (&str, &str)| {
        let identity = (identity.0.to_owned(), identity.1.to_owned());
        match &artifacts.identity {
            Some(known) if *known != identity => {
                Err(fail("screening artifacts name different tasks"))
            }
            _ => {
                artifacts.identity = Some(identity);
                Ok(())
            }
        }
    };
    let mut files = BTreeMap::new();
    for entry in
        std::fs::read_dir(lk_vault::page_path(root, owner).into_diagnostic()?).into_diagnostic()?
    {
        let entry = entry.into_diagnostic()?;
        let name = entry.file_name();
        let path = owner.join(&name);
        let kind = entry.file_type().into_diagnostic()?;
        if kind.is_dir() && (name == "packets" || name == "rosters") {
            for artifact in std::fs::read_dir(entry.path()).into_diagnostic()? {
                let artifact = artifact.into_diagnostic()?;
                let path = path.join(artifact.file_name());
                let revision = path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .filter(|stem| {
                        is_revision(stem) && path.extension().is_some_and(|value| value == "json")
                    })
                    .ok_or_else(|| fail("unknown screening artifact prevents retention"))?
                    .to_owned();
                let raw = read_optional(root, &path)?
                    .ok_or_else(|| fail("screening artifact disappeared"))?;
                if lk_core::rss_original::digest(raw.as_bytes()) != revision {
                    return Err(fail("screening artifact differs from its revision"));
                }
                if name == "packets" {
                    let packet: Packet = decode_stored(&raw)?;
                    packet.validate()?;
                    claim((&packet.request.task_id, &packet.request.cache_hash))?;
                } else {
                    let roster: Roster = decode_stored(&raw)?;
                    if roster.schema != ROSTER_SCHEMA {
                        return Err(fail("invalid sealed screening roster"));
                    }
                    claim((&roster.task_id, &roster.cache_hash))?;
                }
                files.insert(path, raw);
            }
        } else if kind.is_file() && name == "ledger.json" {
            let raw =
                read_optional(root, &path)?.ok_or_else(|| fail("screening ledger disappeared"))?;
            let ledger: Ledger = decode_stored(&raw)?;
            if ledger.schema != LEDGER_SCHEMA {
                return Err(fail("invalid screening ledger"));
            }
            claim((&ledger.task_id, &ledger.cache_hash))?;
            files.insert(path, raw);
        } else if kind.is_file() && name == "rejections.json" {
            let raw = read_optional(root, &path)?
                .ok_or_else(|| fail("screening rejection receipt disappeared"))?;
            claim(rejection::Rejections::decode(&raw)?.identity())?;
            files.insert(path, raw);
        } else {
            return Err(fail("unknown screening artifact prevents retention"));
        }
    }
    artifacts.files = files;
    Ok(artifacts)
}

pub(in crate::commands) struct PreparedScreeningPrune {
    pub removed: usize,
    pub directories: Vec<PathBuf>,
}

/// Remove the complete screening store of every task that is neither pending nor staged.
/// The caller holds the queue claim inside its mutation session. Unknown state refuses cleanup.
pub(in crate::commands::work) fn plan_prune(
    root: &Path,
    pending: &BTreeSet<(String, String)>,
    retained_pending: &BTreeSet<(String, String)>,
    changed_queue_files: &BTreeSet<PathBuf>,
    plan: &mut MutationPlan,
) -> miette::Result<PreparedScreeningPrune> {
    let store = Path::new(STORE);
    let full = lk_vault::page_path(root, store).into_diagnostic()?;
    let entries = match std::fs::read_dir(&full) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PreparedScreeningPrune {
                removed: 0,
                directories: Vec::new(),
            });
        }
        Err(error) => return Err(error).into_diagnostic(),
    };
    require_settled(root)?;
    if std::fs::symlink_metadata(&full)
        .into_diagnostic()?
        .file_type()
        .is_symlink()
    {
        return Err(fail("screening store must be a direct directory"));
    }
    let QueueReferences {
        identities: retained,
        snapshots,
    } = retained_queue_references(root, pending, retained_pending)?;
    let mut mutation = MutationPlan::default();
    for (path, raw) in snapshots {
        if !changed_queue_files.contains(&path) {
            mutation.require_unchanged(path, raw);
        }
    }
    let mut removed = 0;
    let mut directories = Vec::new();
    for entry in entries {
        let entry = entry.into_diagnostic()?;
        let name = entry.file_name();
        if !name.to_str().is_some_and(is_revision) || !entry.file_type().into_diagnostic()?.is_dir()
        {
            return Err(fail("unknown screening store entry prevents retention"));
        }
        let owner = store.join(&name);
        let artifacts = owned_artifacts(root, &owner)?;
        let Some((task_id, cache_hash)) = artifacts.identity else {
            directories.push(owner);
            continue;
        };
        if directory(&task_id, &cache_hash)? != owner {
            return Err(fail("screening store entry belongs to another task"));
        }
        if retained.contains(&(task_id, cache_hash)) {
            continue;
        }
        for (path, raw) in artifacts.files {
            mutation.remove(path, raw);
        }
        removed += 1;
        directories.push(owner);
    }
    if removed != 0 {
        plan.extend(mutation);
    }
    Ok(PreparedScreeningPrune {
        removed,
        directories,
    })
}

/// Remove the directories a published prune emptied. A retry after an interruption
/// finds them already empty and finishes the same way.
pub(in crate::commands::work) fn cleanup_directories(
    root: &Path,
    directories: &[PathBuf],
) -> miette::Result<()> {
    for owner in directories {
        for directory in [owner.join("packets"), owner.join("rosters"), owner.clone()] {
            match std::fs::remove_dir(root.join(directory)) {
                Ok(()) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                    ) => {}
                Err(error) => return Err(error).into_diagnostic(),
            }
        }
    }
    Ok(())
}
