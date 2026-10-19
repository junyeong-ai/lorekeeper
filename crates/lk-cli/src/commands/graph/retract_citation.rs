use std::collections::BTreeSet;
use std::path::{Component, Path};

use super::ResolvedConfig;

/// A narrow operation on generated citation-list items, never arbitrary prose links.
#[cfg(test)]
pub(super) fn request(
    config: &ResolvedConfig,
    pages: &[lk_graph::scan::ScannedPage],
    origin: &Path,
    concept: &Path,
    revision: &str,
    dry_run: bool,
    session: Option<&lk_vault::MutationSession>,
) -> Result<serde_json::Value, String> {
    request_selected(
        config, pages, origin, concept, revision, dry_run, session, None,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn request_selected(
    config: &ResolvedConfig,
    pages: &[lk_graph::scan::ScannedPage],
    origin: &Path,
    concept: &Path,
    revision: &str,
    dry_run: bool,
    session: Option<&lk_vault::MutationSession>,
    selected: Option<&lk_core::rss_evidence::RssEventSelection>,
) -> Result<serde_json::Value, String> {
    for path in [origin, concept] {
        if path.extension().is_none_or(|ext| ext != "md")
            || path
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err("expected exact vault-relative Markdown paths".into());
        }
    }
    if dry_run
        && config
            .root
            .join(".lorekeeper/mutation/pending.json")
            .try_exists()
            .map_err(|e| e.to_string())?
    {
        return Err("pending mutation must be recovered before citation retraction preview".into());
    }
    let _claim =
        crate::commands::queue::claim_writer(&config.root.join(".lorekeeper/queue"), dry_run)
            .map_err(|e| e.to_string())?;
    crate::commands::queue::require_citation_origin_quiescent(&config.root, origin)
        .map_err(|e| e.to_string())?;
    let source = pages
        .iter()
        .find(|page| page.path == origin)
        .ok_or("origin must be an exact scanned source path")?;
    let existence =
        lk_graph::scan::VaultExistence::build_at(&config.root, pages, &config.vault_dirs)
            .map_err(|error| error.to_string())?;
    if !lk_graph::scan::is_citation_origin(source, &config.vault_dirs, &existence)
        || source.format.as_deref() == Some("concept")
    {
        return Err("origin must be real citation evidence, not a concept or task board".into());
    }
    let target = pages
        .iter()
        .find(|page| page.path == concept)
        .ok_or("concept must be an exact scanned path")?;
    if target.format.as_deref() != Some("concept")
        || !lk_graph::scan::is_concept_page(concept, &config.vault_dirs)
    {
        return Err(
            "target must be a declared concept under the configured concepts directory".into(),
        );
    }
    // Both endpoints must pass the vault's symlink/root checks, not just the scan.
    let full = lk_vault::page_path(&config.root, origin).map_err(|e| e.to_string())?;
    let target_full = lk_vault::page_path(&config.root, concept).map_err(|e| e.to_string())?;
    let target_raw = std::fs::read_to_string(target_full).map_err(|e| e.to_string())?;
    let target_page = lk_vault::parse_page(&target_raw).map_err(|e| e.to_string())?;
    if target_page
        .frontmatter
        .get("type")
        .and_then(|value| value.as_str())
        != Some("concept")
    {
        return Err("target no longer declares a concept".into());
    }
    let before = std::fs::read_to_string(full).map_err(|e| e.to_string())?;
    if lk_core::concept::file_revision(&before) != revision {
        return Err(
            "origin revision changed; read the current source and reconsider the citation".into(),
        );
    }
    let page = lk_vault::parse_page(&before).map_err(|e| e.to_string())?;
    if page
        .frontmatter
        .get("type")
        .and_then(|value| value.as_str())
        == Some(lk_core::vault_path::TASK_BOARD_FORMAT)
        || page
            .frontmatter
            .get("type")
            .is_some_and(|value| !value.is_string() || value.as_str() == Some("concept"))
    {
        return Err(
            "concepts, task boards and malformed page types cannot be citation origins".into(),
        );
    }
    lk_vault::rss_concept_citations(&config.root, origin, &before).map_err(|e| e.to_string())?;
    let mut declarations = lk_core::rss_evidence::bindings(&page)?;
    let mut matching = std::collections::BTreeSet::new();
    for binding in &declarations {
        if Path::new(&binding.concept) == concept
            || lk_vault::same_existing_file(&config.root, Path::new(&binding.concept), concept)
                .map_err(|e| e.to_string())?
        {
            matching.insert(binding.concept.clone());
        }
    }
    let mut removed = 0;
    for binding in &mut declarations {
        if matching.contains(&binding.concept) {
            let count = binding.events.len();
            binding
                .events
                .retain(|event| selected.is_some_and(|selected| event != selected));
            removed += count - binding.events.len();
        }
    }
    if selected.is_some() && removed == 0 {
        return Err("selected RSS concept evidence is absent".into());
    }
    declarations.retain(|binding| !binding.events.is_empty());
    let remains = declarations
        .iter()
        .any(|binding| matching.contains(&binding.concept));
    let after = if selected.is_some() && remains {
        remove_generated_links(
            &config.root,
            origin,
            concept,
            &before,
            RetractionScope::GeneratedOnly,
        )?;
        before.clone()
    } else if selected.is_some() {
        remove_generated_links(
            &config.root,
            origin,
            concept,
            &before,
            RetractionScope::GeneratedOnly,
        )?
    } else {
        retract(&config.root, origin, concept, &before)?
    };
    let after = if page
        .frontmatter
        .get(lk_core::rss_evidence::CONCEPT_EVIDENCE_FIELD)
        .is_some()
    {
        lk_vault::write_bindings(&after, &declarations).map_err(|e| e.to_string())?
    } else {
        after
    };
    let archived_revision = lk_core::concept::source_revision(&page);
    if !dry_run {
        // This existing publisher archives the exact inspected evidence before its
        // journaled edit, checks archive/current byte identity and refuses stale writes.
        let mut plan = lk_vault::MutationPlan::default();
        lk_vault::guard_rss_evidence(&mut plan, &config.root, origin, &before)
            .map_err(|e| e.to_string())?;
        lk_vault::publish_document_expected_with_plan(
            session.ok_or("citation retraction requires a vault mutation session")?,
            origin,
            &after,
            Some(&Some(before.clone())),
            plan,
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(serde_json::json!({"ok":true,"data":{
        "origin":origin,"concept":concept,"revision":revision,
        "after_revision":lk_core::concept::file_revision(&after),
        "archived_revision":archived_revision,"dry_run":dry_run,
        "follow_up":"Finalize: graph backlinks-sync; process resulting synthesis tasks; reconcile backlinks again; wiki refresh; graph lint; queue check; wiki reviews --check. Report unrelated findings separately."
    }}))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RetractionScope {
    Strict,
    GeneratedOnly,
}

fn retract(root: &Path, origin: &Path, concept: &Path, before: &str) -> Result<String, String> {
    remove_generated_links(root, origin, concept, before, RetractionScope::Strict)
}

fn remove_generated_links(
    root: &Path,
    origin: &Path,
    concept: &Path,
    before: &str,
    scope: RetractionScope,
) -> Result<String, String> {
    let headings: Vec<_> = lk_vault::section_headings(|s| s.related_concepts).collect();
    let mut fence = lk_core::markdown::FenceState::new();
    let count = before
        .lines()
        .filter(|line| {
            !fence.apply(line)
                && fence.is_closed()
                && headings
                    .iter()
                    .any(|heading| line.trim_end() == format!("## {heading}"))
        })
        .count();
    if count != 1 {
        return Err("origin must have exactly one existing related-concepts section".into());
    }
    let section = lk_vault::resolve_section(before, |s| s.related_concepts)
        .ok_or("missing related-concepts section")?;
    let start = section.body.as_ptr() as usize - before.as_ptr() as usize;
    let end = start + section.body.len();
    let lines: Vec<_> = before.split_inclusive('\n').collect();
    let mut offsets = Vec::with_capacity(lines.len());
    let mut offset = 0;
    for line in &lines {
        offsets.push(offset);
        offset += line.len();
    }
    let mut removed = BTreeSet::new();
    for occurrence in lk_core::link::extract_page_link_occurrences(before) {
        let Some(dest) = lk_core::link::resolve_dest(origin, &occurrence.link.dest) else {
            continue;
        };
        // Filesystem identity permits real filesystem aliases but never slug/name guesses.
        // page_path also refuses an outside-root symlink destination.
        let addresses_target = dest == concept
            || (lk_vault::page_path(root, &dest).is_ok()
                && lk_vault::same_existing_file(root, &dest, concept)
                    .map_err(|e| e.to_string())?);
        if !addresses_target {
            continue;
        }
        let index = occurrence.line - 1;
        let line = lines[index];
        if offsets[index] < start || offsets[index] + line.len() > end {
            if scope == RetractionScope::GeneratedOnly {
                continue;
            }
            return Err(
                "concept is also cited outside the related-concepts section; no edit performed"
                    .into(),
            );
        }
        let expected = format!(
            "- {}",
            lk_core::link::md_link(
                &occurrence.link.text,
                &lk_core::link::encode_dest(&occurrence.link.dest)
            )
        );
        if line.trim_end_matches(['\r', '\n']) != expected {
            if scope == RetractionScope::GeneratedOnly {
                continue;
            }
            return Err("citation must be a standalone generated list item; prose and mixed items are not edited".into());
        }
        removed.insert(index);
    }
    if removed.is_empty() {
        return Err("no matching citation in the existing related-concepts section".into());
    }
    Ok(lines
        .into_iter()
        .enumerate()
        .filter_map(|(i, line)| (!removed.contains(&i)).then_some(line))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    const ORIGIN: &str = "wiki/documents/evidence.md";
    const CONCEPT: &str = "wiki/concepts/retry.md";
    const RAW: &str = "---\ntype: document\ntitle: Evidence\nllm_inputs:\n  concepts: abc\n  concepts_done: abc\n---\n# Evidence\n\nOriginal claim.\n\n## Related Concepts\n\n- [Retry](../concepts/retry.md)\n- [Other](../concepts/other.md)\n\n## Notes\n\nKeep this verbatim.\n";
    fn fixture(raw: &str) -> (tempfile::TempDir, ResolvedConfig) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("wiki/concepts")).unwrap();
        std::fs::create_dir_all(dir.path().join("wiki/documents")).unwrap();
        std::fs::write(dir.path().join(ORIGIN), raw).unwrap();
        std::fs::write(dir.path().join(CONCEPT), "---\ntype: concept\nsource_count: 1\n---\n# Retry\n\n## Synthesis\n\nKeep the concept.\n\n## Sources\n\nKeep machine sources.\n").unwrap();
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
    fn run(
        config: &ResolvedConfig,
        raw: &str,
        dry: bool,
        session: Option<&lk_vault::MutationSession>,
    ) -> Result<serde_json::Value, String> {
        let pages =
            lk_graph::scan::VaultViews::resolve(&config.root, &config.graph, &config.vault_dirs)
                .map_err(|e| e.to_string())?
                .scanned;
        request(
            config,
            &pages,
            Path::new(ORIGIN),
            Path::new(CONCEPT),
            &lk_core::concept::file_revision(raw),
            dry,
            session,
        )
    }
    #[test]
    fn archives_original_exactly_and_only_removes_list_item() {
        let (_dir, config) = fixture(RAW);
        let concept = std::fs::read(config.root.join(CONCEPT)).unwrap();
        let preview = run(&config, RAW, true, None).unwrap();
        assert!(!config.root.join(".lorekeeper").exists());
        let session = lk_vault::MutationSession::acquire(&config.root).unwrap();
        let report = run(&config, RAW, false, Some(&session)).unwrap();
        assert_eq!(
            report["data"]["after_revision"],
            preview["data"]["after_revision"]
        );
        assert_eq!(
            std::fs::read_to_string(config.root.join(ORIGIN)).unwrap(),
            RAW.replace("- [Retry](../concepts/retry.md)\n", "")
        );
        assert_eq!(std::fs::read(config.root.join(CONCEPT)).unwrap(), concept);
        let archived = lk_vault::load_archived_evidence(
            &config.root,
            report["data"]["archived_revision"].as_str().unwrap(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(archived.original.as_deref(), Some(RAW));
        assert!(
            run(&config, RAW, false, Some(&session))
                .unwrap_err()
                .contains("revision changed")
        );
        assert!(
            !config
                .root
                .join(".lorekeeper/mutation/pending.json")
                .exists()
        );
    }
    #[test]
    fn refuses_prose_outside_citations_duplicate_sections_and_preserves_code() {
        for raw in [
            RAW.replace(
                "Original claim.",
                "Original [Retry](../concepts/retry.md) claim.",
            ),
            RAW.replace(
                "- [Retry](../concepts/retry.md)",
                "- [Retry](../concepts/retry.md) — prose",
            ),
            format!("{RAW}\n## 관련 개념\n\n"),
        ] {
            let (_dir, config) = fixture(&raw);
            assert!(run(&config, &raw, true, None).is_err());
            assert_eq!(
                std::fs::read_to_string(config.root.join(ORIGIN)).unwrap(),
                raw
            );
            assert!(!config.root.join(".lorekeeper").exists());
        }
        let raw = RAW.replace(
            "Original claim.",
            "```md\n## Related Concepts\n- [Retry](../concepts/retry.md)\n```\nOriginal claim.",
        );
        let (_dir, config) = fixture(&raw);
        let after = retract(&config.root, Path::new(ORIGIN), Path::new(CONCEPT), &raw).unwrap();
        assert!(after.contains("```md\n## Related Concepts\n- [Retry](../concepts/retry.md)\n```"));
        let ko = RAW.replace("Related Concepts", "관련 개념");
        assert!(retract(&config.root, Path::new(ORIGIN), Path::new(CONCEPT), &ko).is_ok());
    }
    #[test]
    fn queue_collision_and_unpublished_result_refuse_without_edit() {
        let (_dir, config) = fixture(RAW);
        let session = lk_vault::MutationSession::acquire(&config.root).unwrap();
        let queue = config.root.join(".lorekeeper/queue");
        let held = crate::commands::queue::claim_writer(&queue, false).unwrap();
        assert!(run(&config, RAW, false, Some(&session)).is_err());
        drop(held);
        std::fs::create_dir_all(queue.join("results")).unwrap();
        std::fs::write(
            queue.join("results/orphan.section.json"),
            serde_json::to_vec(&lk_queue::SectionResult {
                task_id: "orphan".into(),
                cache_hash: "abc".into(),
                target_revision: lk_core::concept::file_revision(RAW),
                body: "draft".into(),
            })
            .unwrap(),
        )
        .unwrap();
        assert!(
            run(&config, RAW, false, Some(&session))
                .unwrap_err()
                .contains("unmatched staged result")
        );
        assert_eq!(
            std::fs::read_to_string(config.root.join(ORIGIN)).unwrap(),
            RAW
        );
        assert!(!config.root.join(".lorekeeper/evidence").exists());
    }
    #[test]
    fn current_and_blocked_work_refuse_but_done_and_unrelated_work_do_not() {
        let (_dir, config) = fixture(RAW);
        let queue = config.root.join(".lorekeeper/queue");
        std::fs::create_dir_all(&queue).unwrap();
        let client = lk_queue::QueueLlmClient::new(queue.clone());
        let task = client.build_task(
            "sum",
            lk_queue::TaskKind::Summarize,
            lk_queue::SummarizeRequest {
                text: "Original claim.".into(),
                max_sentences: 5,
                focus: None,
                locale: lk_core::i18n::Locale::En,
                source_type: None,
                target: lk_queue::TaskTarget {
                    vault_path: ORIGIN.into(),
                    kind: lk_queue::TargetKind::DocumentSummary,
                    anchor: "## Summary".into(),
                },
            },
        );
        task.validate_retained_input().unwrap();
        std::fs::write(
            queue.join("work.jsonl"),
            serde_json::to_string(&task).unwrap() + "\n",
        )
        .unwrap();
        let raw = lk_vault::record_llm_input(
            RAW,
            "summary",
            &serde_json::to_string(&task.cache_hash).unwrap(),
        )
        .unwrap();
        // Matching work with its anchor missing is blocked, not permission to edit.
        std::fs::write(config.root.join(ORIGIN), &raw).unwrap();
        assert!(
            run(&config, &raw, true, None)
                .unwrap_err()
                .contains("pending task")
        );
        let raw = format!("{raw}\n## Summary\n\n");
        std::fs::write(config.root.join(ORIGIN), &raw).unwrap();
        assert!(
            run(&config, &raw, true, None)
                .unwrap_err()
                .contains("pending task")
        );
        let done = lk_vault::record_llm_input(
            &raw,
            "summary_done",
            &serde_json::to_string(&task.cache_hash).unwrap(),
        )
        .unwrap();
        std::fs::write(config.root.join(ORIGIN), &done).unwrap();
        assert!(run(&config, &done, true, None).is_ok());
        std::fs::create_dir_all(queue.join("results")).unwrap();
        let staged = queue.join("results/work.section.json");
        std::fs::write(
            &staged,
            serde_json::to_vec(&lk_queue::SectionResult {
                task_id: task.task_id.clone(),
                cache_hash: task.cache_hash.clone(),
                target_revision: lk_core::concept::file_revision(&done),
                body: "unpublished".into(),
            })
            .unwrap(),
        )
        .unwrap();
        assert!(
            run(&config, &done, true, None)
                .unwrap_err()
                .contains("staged result")
        );
        std::fs::remove_file(staged).unwrap();
        let mut unrelated = task.clone();
        unrelated.target.vault_path = "wiki/documents/other.md".into();
        unrelated.input.insert(
            "target".into(),
            serde_json::to_value(&unrelated.target).unwrap(),
        );
        std::fs::write(
            queue.join("work.jsonl"),
            serde_json::to_string(&unrelated).unwrap() + "\n",
        )
        .unwrap();
        std::fs::write(config.root.join("wiki/documents/other.md"), &raw).unwrap();
        assert!(run(&config, &done, true, None).is_ok());
    }

    #[test]
    fn generated_percent_encoded_destinations_work_with_configured_paths() {
        let (_dir, mut config) = fixture(RAW);
        std::fs::rename(config.root.join("wiki"), config.root.join("knowledge base")).unwrap();
        config.vault_dirs.wiki = "knowledge base".into();
        let origin = Path::new("knowledge base/documents/evidence.md");
        let concept = Path::new("knowledge base/concepts/retry.md");
        // A daily source must traverse the configured wiki directory (including its space).
        let daily = Path::new("daily/source/2026-10-17.md");
        std::fs::create_dir_all(config.root.join("daily/source")).unwrap();
        let raw = RAW.replace(
            "../concepts/retry.md",
            "../../knowledge%20base/concepts/retry.md",
        );
        std::fs::write(config.root.join(daily), &raw).unwrap();
        let pages =
            lk_graph::scan::VaultViews::resolve(&config.root, &config.graph, &config.vault_dirs)
                .unwrap()
                .scanned;
        let session = lk_vault::MutationSession::acquire(&config.root).unwrap();
        request(
            &config,
            &pages,
            daily,
            concept,
            &lk_core::concept::file_revision(&raw),
            false,
            Some(&session),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(config.root.join(daily)).unwrap(),
            raw.replace("- [Retry](../../knowledge%20base/concepts/retry.md)\n", "")
        );
        assert_eq!(
            std::fs::read_to_string(config.root.join(origin)).unwrap(),
            RAW
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn exact_scanned_paths_allow_real_filesystem_directory_case_aliases() {
        let (_dir, config) = fixture(RAW);
        std::fs::rename(config.root.join("wiki"), config.root.join("Wiki")).unwrap();
        if !config.root.join("wiki").exists() {
            return;
        } // case-sensitive volume
        let pages =
            lk_graph::scan::VaultViews::resolve(&config.root, &config.graph, &config.vault_dirs)
                .unwrap()
                .scanned;
        assert!(
            request(
                &config,
                &pages,
                Path::new("Wiki/documents/evidence.md"),
                Path::new("Wiki/concepts/retry.md"),
                &lk_core::concept::file_revision(RAW),
                true,
                None
            )
            .is_ok()
        );
        assert!(!config.root.join(".lorekeeper").exists());
    }

    #[test]
    fn pending_synthesis_using_origin_evidence_blocks_retraction() {
        let (_dir, config) = fixture(RAW);
        let queue = config.root.join(".lorekeeper/queue");
        std::fs::create_dir_all(&queue).unwrap();
        let client = lk_queue::QueueLlmClient::new(queue.clone());
        let task = client.build_task(
            "syn",
            lk_queue::TaskKind::SynthesizeConcept,
            lk_queue::ConceptSynthesisRequest {
                citations: vec![lk_core::concept::Citation::at_revision(
                    Path::new(ORIGIN),
                    lk_core::concept::source_revision(&lk_vault::parse_page(RAW).unwrap()),
                )],
                locale: lk_core::i18n::Locale::En,
                related_anchor: Some("## Related".into()),
                target: lk_queue::TaskTarget {
                    vault_path: CONCEPT.into(),
                    kind: lk_queue::TargetKind::ConceptSynthesis,
                    anchor: "## Synthesis".into(),
                },
            },
        );
        task.validate_retained_input().unwrap();
        let concept =
            std::fs::read_to_string(config.root.join(CONCEPT)).unwrap() + "\n## Related\n\n";
        let concept = lk_vault::record_llm_input(
            &concept,
            "synthesis",
            &serde_json::to_string(&task.cache_hash).unwrap(),
        )
        .unwrap();
        std::fs::write(config.root.join(CONCEPT), &concept).unwrap();
        std::fs::write(
            queue.join("work.jsonl"),
            serde_json::to_string(&task).unwrap() + "\n",
        )
        .unwrap();
        assert!(
            run(&config, RAW, true, None)
                .unwrap_err()
                .contains("pending task")
        );
        assert_eq!(
            std::fs::read_to_string(config.root.join(ORIGIN)).unwrap(),
            RAW
        );
    }

    #[cfg(unix)]
    #[test]
    fn stale_scan_does_not_authorize_an_escaping_source_symlink() {
        let (_dir, config) = fixture(RAW);
        let pages =
            lk_graph::scan::VaultViews::resolve(&config.root, &config.graph, &config.vault_dirs)
                .unwrap()
                .scanned;
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("outside.md"), RAW).unwrap();
        std::fs::remove_file(config.root.join(ORIGIN)).unwrap();
        std::os::unix::fs::symlink(outside.path().join("outside.md"), config.root.join(ORIGIN))
            .unwrap();
        assert!(
            request(
                &config,
                &pages,
                Path::new(ORIGIN),
                Path::new(CONCEPT),
                &lk_core::concept::file_revision(RAW),
                true,
                None
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(outside.path().join("outside.md")).unwrap(),
            RAW
        );
        assert!(!config.root.join(".lorekeeper").exists());
    }

    #[test]
    fn fresh_type_checks_refuse_concept_ownership_even_after_scan() {
        let (_dir, config) = fixture(RAW);
        let pages =
            lk_graph::scan::VaultViews::resolve(&config.root, &config.graph, &config.vault_dirs)
                .unwrap()
                .scanned;
        for kind in [
            "concept",
            lk_core::vault_path::TASK_BOARD_FORMAT,
            "[malformed]",
        ] {
            let raw = RAW.replace("type: document", &format!("type: {kind}"));
            std::fs::write(config.root.join(ORIGIN), &raw).unwrap();
            let error = request(
                &config,
                &pages,
                Path::new(ORIGIN),
                Path::new(CONCEPT),
                &lk_core::concept::file_revision(&raw),
                true,
                None,
            )
            .unwrap_err();
            assert!(error.contains("cannot be citation origins"), "{error}");
            assert_eq!(
                std::fs::read_to_string(config.root.join(ORIGIN)).unwrap(),
                raw
            );
        }
        assert!(!config.root.join(".lorekeeper").exists());
    }

    #[test]
    fn refuses_corrupt_archive_and_unparseable_queue_without_losing_source() {
        let (_dir, config) = fixture(RAW);
        let session = lk_vault::MutationSession::acquire(&config.root).unwrap();
        let revision = lk_core::concept::source_revision(&lk_vault::parse_page(RAW).unwrap());
        let evidence = config.root.join(".lorekeeper/evidence");
        std::fs::create_dir_all(&evidence).unwrap();
        std::fs::write(evidence.join(format!("{revision}.md")), "corrupt").unwrap();
        assert!(run(&config, RAW, false, Some(&session)).is_err());
        assert_eq!(
            std::fs::read_to_string(config.root.join(ORIGIN)).unwrap(),
            RAW
        );
        std::fs::create_dir_all(config.root.join(".lorekeeper/queue")).unwrap();
        std::fs::write(config.root.join(".lorekeeper/queue/bad.jsonl"), "broken").unwrap();
        assert!(
            run(&config, RAW, true, None)
                .unwrap_err()
                .contains("unparseable")
        );
    }
}
