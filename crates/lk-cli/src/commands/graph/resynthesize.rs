use std::path::{Component, Path, PathBuf};

use lk_queue::{QueueLlmClient, TargetKind, TaskKind};

use super::ResolvedConfig;

pub(super) fn request(
    config: &ResolvedConfig,
    pages: &[lk_graph::scan::ScannedPage],
    path: &Path,
    dry_run: bool,
    session: Option<&lk_vault::MutationSession>,
) -> Result<serde_json::Value, String> {
    if path.extension().is_none_or(|ext| ext != "md")
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err("expected an exact vault-relative concept .md path".into());
    }
    if config.llm.provider != lk_core::config::LlmProvider::Queue {
        return Err("resynthesis requires llm.provider: queue".into());
    }
    if dry_run
        && config
            .root
            .join(".lorekeeper/mutation/pending.json")
            .try_exists()
            .map_err(|e| e.to_string())?
    {
        return Err("pending mutation must be recovered before resynthesis preview".into());
    }
    let queue = config.root.join(".lorekeeper/queue");
    let _claim =
        crate::commands::queue::claim_writer(&queue, dry_run).map_err(|e| e.to_string())?;
    let evidence =
        lk_graph::backlinks::concept_resynthesis(pages, &config.root, &config.vault_dirs, path)
            .map_err(|e| e.to_string())?;
    let target = path.to_str().ok_or("concept path must be UTF-8")?;
    let hash = lk_core::concept::citation_digest(&evidence.citations);
    let policy = if evidence.citations.iter().any(|c| c.rss_event.is_some()) {
        lk_core::rss_evidence::SYNTHESIS_POLICY
    } else {
        TaskKind::SynthesizeConcept.generation_policy()
    };
    if let Some(task) =
        crate::commands::queue::current_synthesis_request(&config.root, target, &hash)
            .map_err(|e| e.to_string())?
    {
        return Ok(
            serde_json::json!({"ok":true,"data":{"task_id":task.task_id,"already_pending":true,"dry_run":dry_run,"evidence":evidence}}),
        );
    }
    let full_path = lk_vault::page_path(&config.root, path).map_err(|e| e.to_string())?;
    let before = std::fs::read_to_string(full_path).map_err(|e| e.to_string())?;
    let stamp = serde_json::to_string(&hash).map_err(|e| e.to_string())?;
    let mut after = lk_vault::record_llm_input(&before, "synthesis", &stamp)
        .ok_or("cannot record synthesis input")?;
    for (key, value) in [
        ("synthesis_done", "null".to_owned()),
        ("synthesis_policy_done", "null".to_owned()),
        (
            "synthesis_policy_requested",
            serde_json::to_string(policy).unwrap(),
        ),
        ("synthesis_policy_requested_input", stamp),
    ] {
        after = lk_vault::record_llm_input(&after, key, &value)
            .ok_or("cannot record synthesis policy request")?;
    }
    let client = QueueLlmClient::new(queue);
    let task = client.build_task(
        "syn",
        TaskKind::SynthesizeConcept,
        lk_queue::ConceptSynthesisRequest {
            citations: evidence.citations.clone(),
            locale: config.locale,
            related_anchor: evidence.related_anchor.clone(),
            target: lk_queue::TaskTarget {
                vault_path: target.into(),
                kind: TargetKind::ConceptSynthesis,
                anchor: evidence.anchor.clone(),
            },
        },
    );
    task.validate_retained_input().map_err(|e| e.to_string())?;
    if !dry_run {
        let mut plan = lk_vault::MutationPlan::default();
        plan.edit(path.to_path_buf(), before, after);
        let queue_path = PathBuf::from(".lorekeeper/queue")
            .join(format!("{}.resynthesize.jsonl", client.run_id()));
        plan.create(
            queue_path,
            serde_json::to_string(&task).map_err(|e| e.to_string())? + "\n",
        );
        session
            .ok_or("resynthesis requires a vault mutation session")?
            .apply(plan)
            .map_err(|e| e.to_string())?;
    }
    Ok(
        serde_json::json!({"ok":true,"data":{"task_id":task.task_id,"already_pending":false,"generation_policy":policy,"dry_run":dry_run,"evidence":evidence}}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, ResolvedConfig) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("wiki/concepts")).unwrap();
        std::fs::create_dir_all(dir.path().join("wiki/documents")).unwrap();
        std::fs::write(dir.path().join("wiki/concepts/retry.md"), "---\ntype: concept\ntitle: Retry\nsource_count: 1\nllm_inputs:\n  synthesis: old\n  synthesis_done: old\n---\n# Retry\n\n## Synthesis\n\nPreserved authored answer.\n\n## Sources\n\n## Related\n").unwrap();
        std::fs::write(dir.path().join("wiki/documents/evidence.md"), "---\ntype: document\ntitle: Evidence\n---\n# Evidence\n\nA bounded retry stops after three failed attempts.\n\n[Retry](../concepts/retry.md)\n").unwrap();
        let config = ResolvedConfig {
            root: dir.path().into(),
            graph: Default::default(),
            locale: lk_core::i18n::Locale::En,
            vault_dirs: Default::default(),
            concept_categories: vec![],
            llm: Default::default(),
        };
        (dir, config)
    }

    fn pages(config: &ResolvedConfig) -> Vec<lk_graph::scan::ScannedPage> {
        lk_graph::scan::VaultViews::resolve(&config.root, &config.graph, &config.vault_dirs)
            .unwrap()
            .scanned
    }

    #[test]
    fn publishes_current_evidence_atomically_and_reuses_pending_request() {
        let (_dir, config) = fixture();
        let path = Path::new("wiki/concepts/retry.md");
        let before = std::fs::read_to_string(config.root.join(path)).unwrap();
        let preview = request(&config, &pages(&config), path, true, None).unwrap();
        assert!(!preview["data"]["already_pending"].as_bool().unwrap());
        assert_eq!(
            std::fs::read_to_string(config.root.join(path)).unwrap(),
            before
        );
        assert!(!config.root.join(".lorekeeper").exists());
        let session = lk_vault::MutationSession::acquire(&config.root).unwrap();
        let report = request(&config, &pages(&config), path, false, Some(&session)).unwrap();
        let hash = lk_core::concept::citation_digest(
            &lk_graph::backlinks::concept_resynthesis(
                &pages(&config),
                &config.root,
                &config.vault_dirs,
                path,
            )
            .unwrap()
            .citations,
        );
        let pending = crate::commands::queue::current_synthesis_request(
            &config.root,
            path.to_str().unwrap(),
            &hash,
        )
        .unwrap()
        .unwrap();
        pending.validate_retained_input().unwrap();
        assert_eq!(pending.task_id, report["data"]["task_id"]);
        assert_eq!(
            pending.input["generation_policy"],
            TaskKind::SynthesizeConcept.generation_policy()
        );
        let after = std::fs::read_to_string(config.root.join(path)).unwrap();
        assert!(after.contains("Preserved authored answer."));
        assert!(after.contains("synthesis_done: null"));
        let second = request(&config, &pages(&config), path, false, Some(&session)).unwrap();
        assert_eq!(second["data"]["task_id"], report["data"]["task_id"]);
        assert_eq!(second["data"]["already_pending"], true);
        assert_eq!(
            std::fs::read_to_string(config.root.join(path)).unwrap(),
            after
        );
        assert!(
            !config
                .root
                .join(".lorekeeper/mutation/pending.json")
                .exists()
        );
        // The old request must not suppress a fresh request after its evidence changes.
        std::fs::write(config.root.join("wiki/documents/evidence.md"), "---\ntype: document\n---\nUpdated finding: stop after two failures. [Retry](../concepts/retry.md)\n").unwrap();
        let third = request(&config, &pages(&config), path, false, Some(&session)).unwrap();
        assert_ne!(third["data"]["task_id"], report["data"]["task_id"]);
        assert_eq!(third["data"]["already_pending"], false);
    }

    #[test]
    fn queue_lock_refusal_preserves_answer_and_preview_never_recovers() {
        let (_dir, config) = fixture();
        let path = Path::new("wiki/concepts/retry.md");
        let before = std::fs::read_to_string(config.root.join(path)).unwrap();
        let session = lk_vault::MutationSession::acquire(&config.root).unwrap();
        let queue = config.root.join(".lorekeeper/queue");
        let _held = crate::commands::queue::claim_writer(&queue, false).unwrap();
        assert!(request(&config, &pages(&config), path, false, Some(&session)).is_err());
        assert_eq!(
            std::fs::read_to_string(config.root.join(path)).unwrap(),
            before
        );
        let journal = config.root.join(".lorekeeper/mutation/pending.json");
        std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
        std::fs::write(&journal, "interrupted journal sentinel").unwrap();
        assert!(request(&config, &pages(&config), path, true, None).is_err());
        assert_eq!(
            std::fs::read_to_string(journal).unwrap(),
            "interrupted journal sentinel"
        );
    }

    #[test]
    fn refuses_empty_malformed_and_inexact_targets_without_writes() {
        let (_dir, config) = fixture();
        let target = config.root.join("wiki/concepts/retry.md");
        let before = std::fs::read_to_string(&target).unwrap();
        for path in [
            "retry",
            "../wiki/concepts/retry.md",
            "wiki/documents/evidence.md",
            "wiki/concepts/Retry.md",
        ] {
            assert!(request(&config, &pages(&config), Path::new(path), true, None).is_err());
        }
        std::fs::write(
            config.root.join("wiki/documents/evidence.md"),
            "---\ntype: [broken\n---\n[Retry](../concepts/retry.md)",
        )
        .unwrap();
        assert!(
            request(
                &config,
                &pages(&config),
                Path::new("wiki/concepts/retry.md"),
                true,
                None
            )
            .is_err()
        );
        std::fs::write(
            config.root.join("wiki/documents/evidence.md"),
            "---\ntype: document\n---\nNo concept citation.",
        )
        .unwrap();
        assert!(
            request(
                &config,
                &pages(&config),
                Path::new("wiki/concepts/retry.md"),
                true,
                None
            )
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), before);
        std::fs::write(&target, "---\ntype: [broken\n---\n## Synthesis\n").unwrap();
        assert!(
            request(
                &config,
                &pages(&config),
                Path::new("wiki/concepts/retry.md"),
                true,
                None
            )
            .is_err()
        );
        assert!(!config.root.join(".lorekeeper").exists());
    }
}
