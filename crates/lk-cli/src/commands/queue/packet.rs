//! Read-only, revision-checked source packets. Export is not a semantic read receipt.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use lk_core::concept::{Citation, file_revision, source_revision};
use lk_queue::{QueueTask, TargetKind, TaskKind};
use serde::Serialize;

use super::{
    TaskStatus, classify_task, read_pending_tasks, resolve_target_path, resolve_vault_root,
};

#[derive(Serialize)]
struct Packet {
    /// Packet schema only, independent of the generation policy of each task.
    version: u8,
    vault_root: PathBuf,
    semantic_validation: &'static str,
    tasks: Vec<PacketTask>,
    sources: Vec<PacketSource>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    event_evidence: Vec<PacketEventEvidence>,
}

#[derive(Serialize)]
struct PacketTask {
    task: QueueTask,
    target_revision: String,
    /// None means legacy/unknown, never an inferred current policy.
    generation_policy: Option<String>,
    source_indices: Vec<usize>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    event_evidence_indices: Vec<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_content: Option<String>,
}

#[derive(Serialize)]
struct PacketSource {
    path: String,
    authored_revision: String,
    /// BLAKE3 of exact bytes, unlike the authored revision that ignores bookkeeping.
    file_revision: String,
    source_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    original: Option<PacketOriginal>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
}

#[derive(Serialize)]
struct PacketEventEvidence {
    citation: Citation,
    concept: String,
    authority: &'static str,
    selected_event_command: Vec<String>,
}

#[derive(Serialize)]
struct PacketOriginal {
    /// Discovery is verified, but is not a semantic read receipt.
    discovery: lk_vault::RssOriginalView,
    discovery_command: Vec<String>,
    selected_event_command: Vec<String>,
}

fn original(root: &Path, path: &str) -> miette::Result<Option<PacketOriginal>> {
    let options = lk_vault::RssOriginalOptions::default();
    let view = lk_vault::inspect_rss_original(root, Path::new(path), &options)
        .map_err(|e| miette::miette!("{path} original: {e}"))?
        .map(|discovery| {
            let base = vec![
                "lore".into(),
                "wiki".into(),
                "original".into(),
                path.into(),
                "--root".into(),
                root.display().to_string(),
            ];
            let mut selected = base.clone();
            selected.extend([
                "--revision".into(),
                discovery.revision.clone(),
                "--event".into(),
                "<event_id from discovery>".into(),
                "--text-offset".into(),
                "0".into(),
                "--text-limit".into(),
                "4096".into(),
            ]);
            PacketOriginal {
                discovery,
                discovery_command: base,
                selected_event_command: selected,
            }
        });
    Ok(view)
}

pub(super) fn run(
    opts: &crate::commands::GlobalOptions,
    root: Option<PathBuf>,
    task_ids: Vec<String>,
    include_content: bool,
) -> miette::Result<()> {
    let root = resolve_vault_root(opts, root)?;
    let packet = build(&root, &task_ids, include_content)?;
    // Emit nothing until all source, target and pending-task guards have passed.
    println!(
        "{}",
        serde_json::to_string_pretty(&packet).map_err(|e| miette::miette!("packet JSON: {e}"))?
    );
    Ok(())
}

fn require_settled(root: &Path) -> miette::Result<()> {
    match std::fs::symlink_metadata(root.join(".lorekeeper/mutation/pending.json")) {
        Ok(_) => Err(miette::miette!(
            "pending vault mutation journal: finish recovery with a cooperating writer before exporting a packet; packet export never recovers it"
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(miette::miette!("inspect mutation journal: {e}")),
    }
}

fn read_snapshot(root: &Path, path: &str) -> miette::Result<String> {
    let relative =
        resolve_target_path(path).ok_or_else(|| miette::miette!("invalid packet path: {path}"))?;
    super::read_page(root, &relative).map_err(|e| miette::miette!("read {path}: {e}"))
}

fn selected_tasks(root: &Path, ids: &BTreeSet<String>) -> miette::Result<Vec<QueueTask>> {
    let pending = read_pending_tasks(&root.join(".lorekeeper/queue"))?;
    if pending.unparseable != 0 {
        return Err(miette::miette!(
            "cannot export a complete task selection while pending queue lines are unparseable"
        ));
    }
    let mut selected = BTreeMap::new();
    let mut revisions = BTreeMap::new();
    for task in pending
        .tasks
        .into_values()
        .filter(|t| ids.contains(&t.task_id))
    {
        if selected.contains_key(&task.task_id) {
            return Err(miette::miette!(
                "ambiguous pending task id: {}",
                task.task_id
            ));
        }
        if task.kind != TaskKind::SynthesizeConcept
            || task.target.kind != TargetKind::ConceptSynthesis
        {
            return Err(miette::miette!(
                "packet accepts only synthesis tasks: {}",
                task.task_id
            ));
        }
        let status = classify_task(root, &task, &mut revisions)?;
        if status != TaskStatus::Current {
            return Err(miette::miette!(
                "task {} is {}, not current",
                task.task_id,
                status.as_str()
            ));
        }
        task.validate_retained_input()
            .map_err(|e| miette::miette!("{}: {e}", task.task_id))?;
        // Validate policy even for a legacy task whose generation stamp is absent.
        task.generation_policy()
            .map_err(|e| miette::miette!("{}: {e}", task.task_id))?;
        selected.insert(task.task_id.clone(), task);
    }
    for id in ids {
        if !selected.contains_key(id) {
            return Err(miette::miette!("pending synthesis task not found: {id}"));
        }
    }
    Ok(selected.into_values().collect())
}

fn build(root: &Path, task_ids: &[String], include_content: bool) -> miette::Result<Packet> {
    let root = root
        .canonicalize()
        .map_err(|e| miette::miette!("vault root: {e}"))?;
    require_settled(&root)?;
    let ids: BTreeSet<_> = task_ids.iter().cloned().collect();
    if ids.is_empty() {
        return Err(miette::miette!("select at least one synthesis task id"));
    }
    let tasks = selected_tasks(&root, &ids)?;
    let mut snapshots = BTreeMap::<String, String>::new();
    let mut source_keys = BTreeMap::new();
    let mut sources: Vec<PacketSource> = Vec::new();
    let mut source_index_by_path = BTreeMap::<String, usize>::new();
    let mut exported = Vec::new();
    let mut event_evidence = Vec::new();
    for task in &tasks {
        let target = if let Some(raw) = snapshots.get(&task.target.vault_path) {
            raw.clone()
        } else {
            let raw = read_snapshot(&root, &task.target.vault_path)?;
            snapshots.insert(task.target.vault_path.clone(), raw.clone());
            raw
        };
        let citations: Vec<Citation> = serde_json::from_value(
            task.input
                .get("citations")
                .cloned()
                .ok_or_else(|| miette::miette!("{} missing citations", task.task_id))?,
        )
        .map_err(|e| miette::miette!("{} citations: {e}", task.task_id))?;
        let mut source_indices = BTreeSet::new();
        let mut event_evidence_indices = Vec::new();
        for citation in citations {
            if let Some(selector) = &citation.rss_event {
                lk_vault::validate_rss_citation(
                    &root,
                    Path::new(&task.target.vault_path),
                    &citation,
                )
                .map_err(|e| miette::miette!("RSS evidence: {e}"))?;
                event_evidence_indices.push(event_evidence.len());
                event_evidence.push(PacketEventEvidence {
                    citation: citation.clone(),
                    concept: task.target.vault_path.clone(),
                    authority: "explicit_immutable_event_selection_not_daily_interpretation",
                    selected_event_command: vec![
                        "lore".into(),
                        "wiki".into(),
                        "original".into(),
                        citation.path.clone(),
                        "--root".into(),
                        root.display().to_string(),
                        "--evidence-concept".into(),
                        task.target.vault_path.clone(),
                        "--event".into(),
                        selector.event_id.clone(),
                        "--revision".into(),
                        citation.revision.clone(),
                        "--text-offset".into(),
                        "0".into(),
                        "--text-limit".into(),
                        "4096".into(),
                    ],
                });
            }
            if let Some(index) = source_index_by_path.get(&citation.path) {
                if citation.rss_event.is_none()
                    && sources[*index].authored_revision != citation.revision
                {
                    return Err(miette::miette!(
                        "selected tasks disagree on source revision: {}",
                        citation.path
                    ));
                }
                source_indices.insert(*index);
                continue;
            }
            let source_path = citation.path.clone();
            let raw = if let Some(raw) = snapshots.get(&citation.path) {
                raw.clone()
            } else {
                let raw = read_snapshot(&root, &citation.path)?;
                snapshots.insert(citation.path.clone(), raw.clone());
                raw
            };
            let page = lk_core::frontmatter::parse_page(&raw)
                .map_err(|e| miette::miette!("{}: {e}", citation.path))?;
            let authored_revision = source_revision(&page);
            if citation.rss_event.is_none() && authored_revision != citation.revision {
                return Err(miette::miette!(
                    "source changed while exporting packet: {}",
                    citation.path
                ));
            }
            let exact = file_revision(&raw);
            let key = (
                citation.path.clone(),
                authored_revision.clone(),
                exact.clone(),
            );
            let original = original(&root, &citation.path)?;
            if original
                .as_ref()
                .is_some_and(|o| o.discovery.page_revision != exact)
            {
                return Err(miette::miette!(
                    "page changed before original discovery: {}",
                    citation.path
                ));
            }
            let index = *source_keys.entry(key).or_insert_with(|| {
                let index = sources.len();
                sources.push(PacketSource {
                    path: citation.path,
                    authored_revision,
                    file_revision: exact,
                    source_url: page
                        .frontmatter
                        .get("source_url")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned),
                    original,
                    content: include_content.then_some(raw),
                });
                index
            });
            source_index_by_path.insert(source_path, index);
            source_indices.insert(index);
        }
        exported.push(PacketTask {
            task: task.clone(),
            target_revision: file_revision(&target),
            generation_policy: task
                .generation_policy()
                .map_err(|e| miette::miette!("{e}"))?
                .map(str::to_owned),
            source_indices: source_indices.into_iter().collect(),
            event_evidence_indices,
            target_content: include_content.then_some(target),
        });
    }
    // No writer lease is acquired: read-only export never mutates or recovers state.
    // Re-read exact files and task identities, refusing drift rather than retagging it.
    verify_snapshots(&root, &snapshots)?;
    for source in &sources {
        let fresh = original(&root, &source.path)?;
        match (&source.original, &fresh) {
            (Some(prior), Some(fresh))
                if prior.discovery.revision == fresh.discovery.revision
                    && prior.discovery.page_revision == fresh.discovery.page_revision
                    && fresh.discovery.page_revision == source.file_revision => {}
            (None, None) => {}
            _ => return Err(miette::miette!("original or page changed: {}", source.path)),
        }
    }
    verify_snapshots(&root, &snapshots)?;
    require_settled(&root)?;
    let current_tasks = selected_tasks(&root, &ids)?;
    if serde_json::to_value(&current_tasks).map_err(|e| miette::miette!("{e}"))?
        != serde_json::to_value(&tasks).map_err(|e| miette::miette!("{e}"))?
    {
        return Err(miette::miette!(
            "pending tasks changed while exporting packet"
        ));
    }
    Ok(Packet {
        version: if event_evidence.is_empty() { 1 } else { 2 },
        vault_root: root,
        semantic_validation: "not performed: this packet is input evidence, not a read receipt or a completed per-target judgment",
        tasks: exported,
        sources,
        event_evidence,
    })
}

fn verify_snapshots(root: &Path, snapshots: &BTreeMap<String, String>) -> miette::Result<()> {
    for (path, expected) in snapshots {
        if read_snapshot(root, path)? != *expected {
            return Err(miette::miette!(
                "page changed while exporting packet: {path}"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn final_check_rejects_exact_byte_drift_even_when_authored_content_is_unchanged() {
        let root = tempfile::tempdir().unwrap();
        let raw = "---\ntitle: Source\nupdated: 2026-10-15\n---\n\nEvidence.\n";
        std::fs::write(root.path().join("source.md"), raw).unwrap();
        let snapshots = BTreeMap::from([("source.md".into(), raw.into())]);
        let canonical = root.path().canonicalize().unwrap();
        assert!(verify_snapshots(&canonical, &snapshots).is_ok());
        let changed = raw.replace("2026-10-15", "2026-10-16");
        assert_eq!(
            source_revision(&lk_core::frontmatter::parse_page(raw).unwrap()),
            source_revision(&lk_core::frontmatter::parse_page(&changed).unwrap())
        );
        std::fs::write(root.path().join("source.md"), changed).unwrap();
        assert!(verify_snapshots(&canonical, &snapshots).is_err());
    }
}

#[cfg(test)]
mod original_tests {
    use super::*;
    #[test]
    fn packet_original_discovery_does_not_embed_collection_body_or_bind_legacy() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(
            root.join("daily.md"),
            "---\ntype: daily\nsource: rss\ncreated: 2026-10-18\n---\nAbridged.\n",
        )
        .unwrap();
        let event:lk_core::event::Event=serde_json::from_value(serde_json::json!({"id":"rss:2026-10-18:0000000000000001","source_id":"rss","source_type":"rss","timestamp":"2026-10-18T00:00:00Z","date":"2026-10-18","title":"Title","body":"large original not copied into the packet","url":null,"author":null,"labels":[],"category":null,"performance_category":null,"is_self":false,"is_personal":false,"metadata":{}})).unwrap();
        let dir = root.join(".lorekeeper/events/rss");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("2026-10-18.jsonl"),
            serde_json::to_string(&event).unwrap(),
        )
        .unwrap();
        let discovered = original(root, "daily.md").unwrap().unwrap();
        assert_eq!(discovered.discovery.authority, "legacy_unbound");
        let json = serde_json::to_string(&discovered).unwrap();
        assert!(!json.contains(&event.body));
        assert!(
            discovered
                .selected_event_command
                .contains(&"--revision".to_owned())
        );
        assert!(!root.join(".lorekeeper/evidence").exists());
    }
}
