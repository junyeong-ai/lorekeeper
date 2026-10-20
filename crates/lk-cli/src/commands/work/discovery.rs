use std::path::{Path, PathBuf};

use lk_core::discovery::DiscoveryQuery;
use miette::IntoDiagnostic;
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DiscoveryRequest {
    target_revision: String,
    queries: Vec<DiscoveryQuery>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiscoveryRecord {
    task_id: String,
    cache_hash: String,
    target_revision: String,
    queries: Vec<DiscoveryQuery>,
}

fn path(task_id: &str) -> PathBuf {
    PathBuf::from(".lorekeeper/queue/discovery").join(format!(
        "{}.json",
        lk_core::rss_original::digest(task_id.as_bytes())
    ))
}

pub(super) fn retire(
    root: &Path,
    task: &lk_queue::QueueTask,
    plan: &mut lk_vault::MutationPlan,
) -> miette::Result<()> {
    let path = path(&task.task_id);
    let raw = match std::fs::read_to_string(lk_vault::page_path(root, &path).into_diagnostic()?) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(miette::miette!("read extraction discovery: {error}")),
    };
    let record: DiscoveryRecord = serde_json::from_str(&raw).into_diagnostic()?;
    if record.task_id != task.task_id || record.cache_hash != task.cache_hash {
        return Err(miette::miette!(
            "extraction discovery belongs to a different retained task"
        ));
    }
    plan.remove(path, raw);
    Ok(())
}

pub(super) fn prune(
    session: &lk_vault::MutationSession,
    pending: &std::collections::BTreeSet<(String, String)>,
) -> miette::Result<usize> {
    let root = session.root();
    let queue = root.join(".lorekeeper/queue");
    let directory = queue.join("discovery");
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(miette::miette!(
                "read extraction discovery directory: {error}"
            ));
        }
    };
    let mut removed = 0;
    let mut plan = lk_vault::MutationPlan::default();
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
        let raw = std::fs::read_to_string(entry.path()).into_diagnostic()?;
        let Ok(record) = serde_json::from_str::<DiscoveryRecord>(&raw) else {
            eprintln!(
                "preserving malformed extraction discovery: {}",
                entry.path().display()
            );
            continue;
        };
        if entry.path() != root.join(path(&record.task_id))
            || validate_queries(&record.queries).is_err()
            || pending.contains(&(record.task_id.clone(), record.cache_hash.clone()))
        {
            continue;
        }
        plan.remove(path(&record.task_id), raw);
        removed += 1;
    }
    if removed != 0 {
        session.apply(plan).into_diagnostic()?;
    }
    Ok(removed)
}

pub(super) fn validate_queries(queries: &[DiscoveryQuery]) -> miette::Result<Vec<DiscoveryQuery>> {
    if queries.len() > 16 {
        return Err(miette::miette!(
            "extraction discovery accepts at most 16 queries"
        ));
    }
    for query in queries {
        query.validate().into_diagnostic()?;
    }
    let mut queries = queries.to_vec();
    queries.sort();
    queries.dedup();
    Ok(queries)
}

pub(super) fn current(
    root: &Path,
    task: &lk_queue::QueueTask,
) -> miette::Result<Option<Vec<DiscoveryQuery>>> {
    if task.kind != lk_queue::TaskKind::ExtractConcepts {
        return Ok(None);
    }
    let raw = match std::fs::read_to_string(
        lk_vault::page_path(root, &path(&task.task_id)).into_diagnostic()?,
    ) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(miette::miette!("read extraction discovery: {error}")),
    };
    let record: DiscoveryRecord = serde_json::from_str(&raw).into_diagnostic()?;
    let queries = validate_queries(&record.queries)?;
    let target = std::fs::read_to_string(
        lk_vault::page_path(root, Path::new(&task.target.vault_path)).into_diagnostic()?,
    )
    .into_diagnostic()?;
    Ok((record.task_id == task.task_id
        && record.cache_hash == task.cache_hash
        && record.target_revision == lk_core::concept::file_revision(&target))
    .then_some(queries))
}

pub(super) fn record(
    session: &lk_vault::MutationSession,
    task_id: &str,
    request: DiscoveryRequest,
) -> miette::Result<()> {
    let root = session.root();
    let _claim = match lk_queue::claim_queue(&root.join(".lorekeeper/queue")).into_diagnostic()? {
        lk_queue::QueueClaim::Held(claim) => claim,
        lk_queue::QueueClaim::Busy => return Err(miette::miette!("queue is busy")),
        lk_queue::QueueClaim::Unavailable(error) => {
            return Err(miette::miette!("queue claim unavailable: {error}"));
        }
    };
    let task = super::super::queue::select_task(
        root,
        task_id,
        super::super::queue::TaskSelection::CurrentOrDoneExtraction,
    )?;
    if task.kind != lk_queue::TaskKind::ExtractConcepts {
        return Err(miette::miette!(
            "discovery recipes require concept extraction"
        ));
    }
    let queries = validate_queries(&request.queries)?;
    let target_path = Path::new(&task.target.vault_path);
    let target = std::fs::read_to_string(lk_vault::page_path(root, target_path).into_diagnostic()?)
        .into_diagnostic()?;
    if lk_core::concept::file_revision(&target) != request.target_revision {
        return Err(miette::miette!(
            "extraction discovery target changed; reread before choosing queries"
        ));
    }
    let record = DiscoveryRecord {
        task_id: task.task_id,
        cache_hash: task.cache_hash,
        target_revision: request.target_revision,
        queries,
    };
    let after = serde_json::to_string_pretty(&record).into_diagnostic()?;
    let path = path(task_id);
    let mut plan = lk_vault::MutationPlan::default();
    plan.require_unchanged(target_path, target);
    match std::fs::read_to_string(lk_vault::page_path(root, &path).into_diagnostic()?) {
        Ok(before) => plan.edit(path, before, after),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => plan.create(path, after),
        Err(error) => return Err(miette::miette!("read extraction discovery: {error}")),
    }
    session.apply(plan).into_diagnostic()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pruning_discovery_keeps_pending_malformed_and_unowned_records() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let session = lk_vault::MutationSession::acquire(root).unwrap();
        let recipes = root.join(".lorekeeper/queue/discovery");
        std::fs::create_dir_all(&recipes).unwrap();
        let record = |id: &str| {
            serde_json::to_string(&DiscoveryRecord {
                task_id: id.into(),
                cache_hash: "input".into(),
                target_revision: "target".into(),
                queries: vec![DiscoveryQuery {
                    query: "subject".into(),
                    phrase: false,
                    spacing: false,
                }],
            })
            .unwrap()
        };
        for id in ["pending", "completed"] {
            std::fs::write(root.join(path(id)), record(id)).unwrap();
        }
        std::fs::write(root.join(path("malformed")), "{invalid").unwrap();
        std::fs::write(recipes.join("unowned.json"), record("unowned")).unwrap();
        let pending = [("pending".into(), "input".into())].into_iter().collect();
        assert_eq!(prune(&session, &pending).unwrap(), 1);
        assert!(!root.join(path("completed")).exists());
        assert_eq!(
            std::fs::read_to_string(root.join(path("pending"))).unwrap(),
            record("pending")
        );
        assert_eq!(
            std::fs::read_to_string(root.join(path("malformed"))).unwrap(),
            "{invalid"
        );
        assert!(recipes.join("unowned.json").exists());
        assert_eq!(prune(&session, &pending).unwrap(), 0);
    }
}
