use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use strum::IntoEnumIterator;

use super::{QueueTask, TaskStatus, classify_task, read_pending_tasks, resolve_target_path};

pub(crate) struct InputReadiness {
    tasks: BTreeMap<PathBuf, Vec<QueueTask>>,
    staged: BTreeSet<(String, String)>,
    pages: BTreeMap<PathBuf, ApprovedPage>,
    revisions: BTreeMap<PathBuf, Result<String, TaskStatus>>,
}

struct ApprovedPage {
    revision: String,
    reasons: Vec<String>,
}

pub(crate) fn unfinished_inputs(page: &lk_vault::VaultPage) -> Result<Vec<&'static str>, String> {
    let Some(inputs) = page
        .frontmatter
        .get(lk_core::frontmatter::field::LLM_INPUTS)
        .filter(|inputs| !inputs.is_null())
    else {
        return Ok(Vec::new());
    };
    let inputs = inputs
        .as_object()
        .ok_or_else(|| "llm_inputs must be an object or null".to_owned())?;
    let keys = lk_queue::TargetKind::iter()
        .map(|kind| kind.llm_inputs_key())
        .collect::<BTreeSet<_>>();
    Ok(keys
        .into_iter()
        .filter(|key| {
            inputs.get(*key).is_some_and(|input| !input.is_null())
                && !lk_queue::TargetKind::iter()
                    .filter(|kind| kind.llm_inputs_key() == *key)
                    .any(|kind| lk_pipeline::completed_input(page, kind).is_some())
        })
        .collect())
}

impl InputReadiness {
    pub fn load(root: &Path) -> miette::Result<Self> {
        let pending = read_pending_tasks(&root.join(".lorekeeper/queue"))?;
        if pending.unparseable != 0 {
            return Err(miette::miette!(
                "unparseable pending tasks have unknown targets; repair them before assessing knowledge"
            ));
        }
        let queue = root.join(".lorekeeper/queue");
        let batch = lk_queue::read_results(&queue)
            .map_err(|error| miette::miette!("semantic staged results: {error}"))?;
        if !batch.unreadable.is_empty() {
            return Err(miette::miette!(
                "unreadable staged results have unknown targets; repair them before assessing knowledge"
            ));
        }
        let mut staged = BTreeSet::new();
        for (_, result) in batch.ready {
            staged.insert((result.task_id, result.cache_hash));
        }
        let entries = match std::fs::read_dir(queue.join(lk_queue::RESULTS_SUBDIR)) {
            Ok(entries) => Some(entries),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(miette::miette!("semantic staged results: {error}")),
        };
        for entry in entries.into_iter().flatten() {
            let path = entry.map_err(|error| miette::miette!("{error}"))?.path();
            let name = path.to_string_lossy();
            let section = name.ends_with(lk_queue::SECTION_RESULT_SUFFIX);
            if !section && !name.ends_with(lk_queue::SYNTHESIS_RESULT_SUFFIX) {
                continue;
            }
            let raw = std::fs::read(&path).map_err(|error| miette::miette!("{error}"))?;
            let identity = if section {
                serde_json::from_slice::<lk_queue::SectionResult>(&raw)
                    .map(|result| (result.task_id, result.cache_hash))
            } else {
                serde_json::from_slice::<lk_queue::SynthesisResult>(&raw)
                    .map(|result| (result.task_id, result.cache_hash))
            }
            .map_err(|error| {
                miette::miette!("unreadable staged result {}: {error}", path.display())
            })?;
            staged.insert(identity);
        }
        if staged
            .iter()
            .any(|identity| !pending.tasks.contains_key(identity))
        {
            return Err(miette::miette!(
                "unmatched staged results have unknown targets; repair them before assessing knowledge"
            ));
        }
        let mut tasks: BTreeMap<PathBuf, Vec<QueueTask>> = BTreeMap::new();
        for task in pending.tasks.into_values() {
            let Some(target) = resolve_target_path(&task.target.vault_path) else {
                continue;
            };
            let Ok(full) = lk_vault::page_path(root, &target) else {
                continue;
            };
            let owner = match full.canonicalize() {
                Ok(owner) => owner,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(miette::miette!(
                        "audit task target {}: {error}",
                        target.display()
                    ));
                }
            };
            tasks.entry(owner).or_default().push(task);
        }
        Ok(Self {
            tasks,
            staged,
            pages: BTreeMap::new(),
            revisions: BTreeMap::new(),
        })
    }

    fn page(&mut self, root: &Path, path: &str) -> miette::Result<Vec<String>> {
        let full = lk_vault::page_path(root, Path::new(path))
            .map_err(|error| miette::miette!("{error}"))?;
        let owner = full
            .canonicalize()
            .map_err(|error| miette::miette!("{path}: {error}"))?;
        if let Some(page) = self.pages.get(&owner) {
            return Ok(page.reasons.clone());
        }
        let raw =
            std::fs::read_to_string(full).map_err(|error| miette::miette!("{path}: {error}"))?;
        let page =
            lk_vault::parse_page(&raw).map_err(|error| miette::miette!("{path}: {error}"))?;
        let mut reasons = BTreeSet::new();
        for key in unfinished_inputs(&page).map_err(|error| miette::miette!("{path}: {error}"))? {
            reasons.insert(format!("{path}: unfinished {key}"));
        }
        for task in self.tasks.get(&owner).into_iter().flatten() {
            let status = classify_task(root, task, &mut self.revisions)?;
            if self
                .staged
                .contains(&(task.task_id.clone(), task.cache_hash.clone()))
                && (matches!(status, TaskStatus::Current | TaskStatus::Unreadable)
                    || status == TaskStatus::Done
                        && task.kind == lk_queue::TaskKind::ExtractConcepts)
            {
                reasons.insert(format!("{path}: unpublished result {}", task.task_id));
            }
            if matches!(status, TaskStatus::Current | TaskStatus::Unreadable) {
                reasons.insert(format!("{path}: {} task {}", status.as_str(), task.task_id));
            }
        }
        let reasons = reasons.into_iter().collect::<Vec<_>>();
        self.pages.insert(
            owner,
            ApprovedPage {
                revision: lk_core::concept::file_revision(&raw),
                reasons: reasons.clone(),
            },
        );
        Ok(reasons)
    }

    pub fn guard(&self, root: &Path, plan: &mut lk_vault::MutationPlan) -> miette::Result<()> {
        let root = root
            .canonicalize()
            .map_err(|error| miette::miette!("{error}"))?;
        for (owner, approved) in &self.pages {
            if !approved.reasons.is_empty() {
                return Err(miette::miette!(
                    "cannot publish an assessment with unfinished inputs"
                ));
            }
            let relative = owner
                .strip_prefix(&root)
                .map_err(|error| miette::miette!("{error}"))?;
            let raw = std::fs::read_to_string(
                lk_vault::page_path(&root, relative).map_err(|error| miette::miette!("{error}"))?,
            )
            .map_err(|error| miette::miette!("{error}"))?;
            if lk_core::concept::file_revision(&raw) != approved.revision {
                return Err(miette::miette!(
                    "audit readiness input changed: {}; reread before assessing",
                    relative.display()
                ));
            }
            plan.require_unchanged(relative, raw);
        }
        Ok(())
    }

    pub fn unfinished_synthesis(
        &mut self,
        root: &Path,
        task: &QueueTask,
    ) -> miette::Result<Vec<String>> {
        let citations: Vec<lk_core::concept::Citation> = serde_json::from_value(
            task.input
                .get("citations")
                .cloned()
                .ok_or_else(|| miette::miette!("synthesis input has no citations"))?,
        )
        .map_err(|error| miette::miette!("synthesis citations: {error}"))?;
        let target = lk_vault::page_path(root, Path::new(&task.target.vault_path))
            .and_then(|path| path.canonicalize())
            .map_err(|error| miette::miette!("synthesis target: {error}"))?;
        let mut reasons = BTreeSet::new();
        for citation in citations {
            let owner = lk_vault::page_path(root, Path::new(&citation.path))
                .and_then(|path| path.canonicalize())
                .map_err(|error| miette::miette!("{}: {error}", citation.path))?;
            if owner != target {
                reasons.extend(self.page(root, &citation.path)?);
            }
        }
        Ok(reasons.into_iter().collect())
    }

    pub fn unfinished(
        &mut self,
        root: &Path,
        candidate: &lk_graph::audit::AuditCandidate,
    ) -> miette::Result<Vec<String>> {
        let mut paths = BTreeSet::from([candidate.path.as_str()]);
        paths.extend(
            candidate
                .evidence
                .iter()
                .chain(&candidate.related_pages)
                .map(|citation| citation.path.as_str()),
        );
        for review in &candidate.reviews {
            if review.file_revision.is_some() {
                paths.insert(&review.path);
            }
            for evidence in &review.evidence {
                if let Some(current) = &evidence.current {
                    paths.insert(&current.path);
                }
            }
        }
        let mut reasons = BTreeSet::new();
        for path in paths {
            reasons.extend(self.page(root, path)?);
        }
        Ok(reasons.into_iter().collect())
    }

    pub fn require_ready(
        &mut self,
        root: &Path,
        candidate: &lk_graph::audit::AuditCandidate,
    ) -> miette::Result<()> {
        let unfinished = self.unfinished(root, candidate)?;
        if unfinished.is_empty() {
            Ok(())
        } else {
            Err(miette::miette!(
                "audit inputs have unfinished semantic work: {}",
                unfinished.join("; ")
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(root: &Path) -> lk_graph::audit::AuditCandidate {
        let pages = lk_graph::scan::scan_vault(root, false).unwrap();
        lk_graph::audit::plan_selected_partitioned(
            root,
            &Default::default(),
            &pages,
            1,
            &["wiki/documents/subject.md".into()],
            u64::MAX,
        )
        .unwrap()
        .candidates
        .remove(0)
    }

    fn write(root: &Path, path: &str, body: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn synthesis(citations: Vec<lk_core::concept::Citation>) -> QueueTask {
        lk_queue::QueueLlmClient::new(PathBuf::new()).build_task(
            "synthesis",
            lk_queue::TaskKind::SynthesizeConcept,
            lk_queue::ConceptSynthesisRequest {
                citations,
                locale: Default::default(),
                related_anchor: Some("## Related Concepts".into()),
                target: lk_queue::TaskTarget {
                    vault_path: "wiki/concepts/topic.md".into(),
                    kind: lk_queue::TargetKind::ConceptSynthesis,
                    anchor: "## Synthesis".into(),
                },
            },
        )
    }

    #[test]
    fn synthesis_waits_for_its_mutable_origins_without_waiting_for_unrelated_inputs() {
        let root = tempfile::tempdir().unwrap();
        write(
            root.path(),
            "wiki/concepts/topic.md",
            "---\ntype: concept\nllm_inputs: {synthesis: pending}\n---\n## Synthesis\n\n",
        );
        let pending = "---\ntype: document\nllm_inputs: {summary: pending}\n---\n## Summary\n\n";
        write(root.path(), "wiki/documents/source.md", pending);
        write(root.path(), "wiki/documents/unrelated.md", pending);
        let citation = lk_core::concept::Citation::new(
            Path::new("wiki/documents/source.md"),
            &lk_vault::parse_page(pending).unwrap(),
        );
        let task = synthesis(vec![citation.clone()]);
        let blocked = InputReadiness::load(root.path())
            .unwrap()
            .unfinished_synthesis(root.path(), &task)
            .unwrap();
        assert_eq!(blocked, ["wiki/documents/source.md: unfinished summary"]);
        write(
            root.path(),
            "wiki/documents/source.md",
            &pending.replace(
                "summary: pending",
                "summary: pending, summary_done: pending",
            ),
        );
        assert!(
            InputReadiness::load(root.path())
                .unwrap()
                .unfinished_synthesis(root.path(), &task)
                .unwrap()
                .is_empty()
        );
        let own = synthesis(vec![lk_core::concept::Citation::at_revision(
            Path::new("wiki/concepts/topic.md"),
            "captured".into(),
        )]);
        assert!(
            InputReadiness::load(root.path())
                .unwrap()
                .unfinished_synthesis(root.path(), &own)
                .unwrap()
                .is_empty()
        );
        write(root.path(), "wiki/documents/source.md", pending);
        let selected = synthesis(vec![lk_core::concept::Citation {
            rss_event: Some(lk_core::rss_evidence::RssEventSelector {
                schema: lk_core::rss_evidence::RSS_EVIDENCE_SCHEMA.into(),
                event_id: "news:2026-10-22:0000000000000000".into(),
            }),
            revision: "a".repeat(64),
            ..citation
        }]);
        assert_eq!(
            InputReadiness::load(root.path())
                .unwrap()
                .unfinished_synthesis(root.path(), &selected)
                .unwrap(),
            blocked
        );
    }

    #[test]
    fn authored_pages_need_no_markers_but_exact_context_requires_completed_owned_inputs() {
        let root = tempfile::tempdir().unwrap();
        write(
            root.path(),
            "wiki/documents/subject.md",
            "---\ntype: document\n---\n[Context](context.md)\n",
        );
        write(
            root.path(),
            "wiki/documents/context.md",
            "---\ntype: document\nllm_inputs: {summary: pending}\n---\n## Summary\nDraft.\n",
        );
        let selected = candidate(root.path());
        let mut readiness = InputReadiness::load(root.path()).unwrap();
        let error = readiness.require_ready(root.path(), &selected).unwrap_err();
        assert!(error.to_string().contains("context.md: unfinished summary"));
        write(
            root.path(),
            "wiki/documents/context.md",
            "---\ntype: document\nllm_inputs: {summary: pending, summary_done: pending}\n---\n## Summary\n\n",
        );
        InputReadiness::load(root.path())
            .unwrap()
            .require_ready(root.path(), &candidate(root.path()))
            .unwrap();
        assert!(!root.path().join(".lorekeeper").exists());
    }

    #[test]
    fn completed_annual_narrative_does_not_require_another_periods_heading() {
        let root = tempfile::tempdir().unwrap();
        write(
            root.path(),
            "wiki/documents/subject.md",
            "---\ntype: document\n---\nAuthored source.\n",
        );
        let heading = lk_core::i18n::Locale::default().strings().overall_summary;
        write(
            root.path(),
            "annual/2025.md",
            &format!(
                "---\ntype: annual-review\nllm_inputs: {{narrative: assessed, narrative_done: assessed}}\n---\n## {heading}\n\n"
            ),
        );
        let mut subject = candidate(root.path());
        subject
            .evidence
            .push(lk_core::concept::Citation::at_revision(
                Path::new("annual/2025.md"),
                "captured".into(),
            ));
        InputReadiness::load(root.path())
            .unwrap()
            .require_ready(root.path(), &subject)
            .unwrap();
    }

    #[test]
    fn null_inputs_leave_untracked_subjects_and_later_ready_work_eligible() {
        let root = tempfile::tempdir().unwrap();
        for (name, inputs) in [
            ("a", "llm_inputs: null\n"),
            ("b", "llm_inputs: {summary: null}\n"),
            ("c", ""),
        ] {
            write(
                root.path(),
                &format!("wiki/documents/{name}.md"),
                &format!("---\ntype: document\n{inputs}---\nAuthored source.\n"),
            );
        }
        let pages = lk_graph::scan::scan_vault(root.path(), false).unwrap();
        let mut readiness = InputReadiness::load(root.path()).unwrap();
        let plan = lk_graph::audit::plan_available_partitioned(
            root.path(),
            &Default::default(),
            &pages,
            3,
            u64::MAX,
            &mut |candidate| {
                readiness
                    .unfinished(root.path(), candidate)
                    .map(|reasons| reasons.is_empty())
                    .map_err(|error| lk_graph::GraphError::Io(error.to_string()))
            },
        )
        .unwrap();
        assert_eq!(plan.candidates.len(), 3);
        assert_eq!(plan.candidates[0].path, "wiki/documents/a.md");
        assert_eq!(plan.candidates[1].path, "wiki/documents/b.md");
        assert_eq!(plan.candidates[2].path, "wiki/documents/c.md");
        write(
            root.path(),
            "wiki/documents/subject.md",
            "---\ntype: document\nllm_inputs: unstructured\n---\nAuthored source.\n",
        );
        assert!(
            InputReadiness::load(root.path())
                .unwrap()
                .require_ready(root.path(), &candidate(root.path()))
                .is_err()
        );
    }

    #[test]
    fn readiness_guards_refuse_marker_only_changes_before_planning_and_publication() {
        let root = tempfile::tempdir().unwrap();
        let ready = "---\ntype: document\nllm_inputs: {summary: captured, summary_done: captured}\n---\n## Summary\nA completed interpretation.\n";
        let changed = ready.replace(", summary_done: captured", "");
        write(root.path(), "wiki/documents/subject.md", ready);
        let selected = candidate(root.path());
        let mut readiness = InputReadiness::load(root.path()).unwrap();
        readiness.require_ready(root.path(), &selected).unwrap();
        write(root.path(), "wiki/documents/subject.md", &changed);
        assert!(
            readiness
                .guard(root.path(), &mut Default::default())
                .unwrap_err()
                .to_string()
                .contains("readiness input changed")
        );
        write(root.path(), "wiki/documents/subject.md", ready);
        let mut plan = lk_vault::MutationPlan::default();
        readiness.guard(root.path(), &mut plan).unwrap();
        plan.create("assessment.json", "completed".into());
        write(root.path(), "wiki/documents/subject.md", &changed);
        let session = lk_vault::MutationSession::acquire(root.path()).unwrap();
        assert!(session.apply(plan).is_err());
        assert!(!root.path().join("assessment.json").exists());
        assert_eq!(
            std::fs::read_to_string(root.path().join("wiki/documents/subject.md")).unwrap(),
            changed
        );
    }

    #[test]
    fn unreadable_unrelated_task_is_ignored_but_unknown_target_lines_block() {
        let root = tempfile::tempdir().unwrap();
        write(
            root.path(),
            "wiki/documents/subject.md",
            "---\ntype: document\n---\nAuthored source.\n",
        );
        write(
            root.path(),
            "wiki/documents/unrelated.md",
            "---\ntype: document\n---\nSeparate source.\n",
        );
        let request = lk_queue::SummarizeRequest {
            text: "Separate source".into(),
            max_sentences: 1,
            focus: None,
            locale: Default::default(),
            source_type: None,
            target: lk_queue::TaskTarget {
                vault_path: "wiki/documents/unrelated.md".into(),
                kind: lk_queue::TargetKind::DocumentSummary,
                anchor: "## Summary".into(),
            },
        };
        let client = lk_queue::QueueLlmClient::new(root.path().join(".lorekeeper/queue"));
        let mut task = client.build_task("pending", lk_queue::TaskKind::Summarize, request);
        task.input
            .insert("max_sentences".into(), serde_json::json!("invalid"));
        write(
            root.path(),
            ".lorekeeper/queue/pending.jsonl",
            &(serde_json::to_string(&task).unwrap() + "\n"),
        );
        InputReadiness::load(root.path())
            .unwrap()
            .require_ready(root.path(), &candidate(root.path()))
            .unwrap();
        write(root.path(), ".lorekeeper/queue/pending.jsonl", "not-json\n");
        assert!(
            InputReadiness::load(root.path())
                .err()
                .unwrap()
                .to_string()
                .contains("unknown targets")
        );
    }

    #[test]
    fn completed_section_and_stale_proposals_do_not_block_assessment() {
        let root = tempfile::tempdir().unwrap();
        let client = lk_queue::QueueLlmClient::new(root.path().join(".lorekeeper/queue"));
        let task = client.build_task(
            "pending",
            lk_queue::TaskKind::Summarize,
            lk_queue::SummarizeRequest {
                text: "Source".into(),
                max_sentences: 1,
                focus: None,
                locale: Default::default(),
                source_type: None,
                target: lk_queue::TaskTarget {
                    vault_path: "wiki/documents/subject.md".into(),
                    kind: lk_queue::TargetKind::DocumentSummary,
                    anchor: "## Summary".into(),
                },
            },
        );
        write(
            root.path(),
            "wiki/documents/subject.md",
            &format!(
                "---\ntype: document\nllm_inputs: {{summary: {}, summary_done: {}}}\n---\n## Summary\nAnswered.\n",
                task.cache_hash, task.cache_hash
            ),
        );
        write(
            root.path(),
            ".lorekeeper/queue/pending.jsonl",
            &(serde_json::to_string(&task).unwrap() + "\n"),
        );
        InputReadiness::load(root.path())
            .unwrap()
            .require_ready(root.path(), &candidate(root.path()))
            .unwrap();
        let result = lk_queue::SectionResult {
            task_id: task.task_id,
            cache_hash: task.cache_hash,
            target_revision: "captured".into(),
            body: "Staged answer".into(),
        };
        write(
            root.path(),
            ".lorekeeper/queue/results/pending.section.json",
            &serde_json::to_string(&result).unwrap(),
        );
        InputReadiness::load(root.path())
            .unwrap()
            .require_ready(root.path(), &candidate(root.path()))
            .unwrap();
        write(
            root.path(),
            "wiki/documents/subject.md",
            "---\ntype: document\nllm_inputs: {summary: newer, summary_done: newer}\n---\n## Summary\nA completed newer interpretation.\n",
        );
        InputReadiness::load(root.path())
            .unwrap()
            .require_ready(root.path(), &candidate(root.path()))
            .unwrap();
    }

    #[test]
    fn answered_extraction_with_a_publishable_result_blocks_but_superseded_result_does_not() {
        let root = tempfile::tempdir().unwrap();
        let client = lk_queue::QueueLlmClient::new(root.path().join(".lorekeeper/queue"));
        let heading = lk_core::i18n::Locale::default().strings().related_concepts;
        let date = jiff::civil::date(2026, 10, 20);
        let task = client.build_task(
            "pending",
            lk_queue::TaskKind::ExtractConcepts,
            lk_queue::ExtractConceptsRequest {
                text: "Source".into(),
                source_id: "manual".into(),
                source_type: lk_core::config::SourceType::Manual,
                date,
                focus: None,
                locale: Default::default(),
                categories: vec![],
                rss_original: None,
                concept_policy: lk_core::config::ConceptPolicy::SupportedNew,
                target: lk_queue::TaskTarget {
                    vault_path: "wiki/documents/subject.md".into(),
                    kind: lk_queue::TargetKind::DocumentConcepts,
                    anchor: format!("## {heading}"),
                },
            },
        );
        let page = format!(
            "---\ntype: document\nllm_inputs: {{concepts: {}, concepts_done: {}}}\n---\n## {heading}\n\n",
            task.cache_hash, task.cache_hash
        );
        write(root.path(), "wiki/documents/subject.md", &page);
        write(
            root.path(),
            "wiki/concepts/topic.md",
            "---\ntype: concept\n---\n## Synthesis\n\n",
        );
        let dependent = synthesis(vec![lk_core::concept::Citation::new(
            Path::new("wiki/documents/subject.md"),
            &lk_vault::parse_page(&page).unwrap(),
        )]);
        write(
            root.path(),
            ".lorekeeper/queue/pending.jsonl",
            &(serde_json::to_string(&task).unwrap() + "\n"),
        );
        InputReadiness::load(root.path())
            .unwrap()
            .require_ready(root.path(), &candidate(root.path()))
            .unwrap();
        assert!(
            InputReadiness::load(root.path())
                .unwrap()
                .unfinished_synthesis(root.path(), &dependent)
                .unwrap()
                .is_empty()
        );
        let result = lk_queue::TaskResult {
            selection: None,
            task_id: task.task_id,
            cache_hash: task.cache_hash,
            target_revision: lk_core::concept::file_revision(&page),
            context_revision: "captured".into(),
            discovery: vec![],
            concept_policy: lk_core::config::ConceptPolicy::SupportedNew,
            target: task.target,
            date,
            concepts: vec![],
        };
        write(
            root.path(),
            ".lorekeeper/queue/results/pending.json",
            &serde_json::to_string(&result).unwrap(),
        );
        assert!(
            InputReadiness::load(root.path())
                .unwrap()
                .require_ready(root.path(), &candidate(root.path()))
                .unwrap_err()
                .to_string()
                .contains("unpublished result")
        );
        assert!(
            InputReadiness::load(root.path())
                .unwrap()
                .unfinished_synthesis(root.path(), &dependent)
                .unwrap()
                .iter()
                .any(|reason| reason.contains("unpublished result"))
        );
        write(
            root.path(),
            "wiki/documents/subject.md",
            &format!(
                "---\ntype: document\nllm_inputs: {{concepts: newer, concepts_done: newer}}\n---\n## {heading}\n\n"
            ),
        );
        InputReadiness::load(root.path())
            .unwrap()
            .require_ready(root.path(), &candidate(root.path()))
            .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn directory_aliases_share_task_readiness_but_hardlink_entries_stay_distinct() {
        let root = tempfile::tempdir().unwrap();
        write(
            root.path(),
            "wiki/documents/subject.md",
            "---\ntype: document\nllm_inputs: {summary: pending, summary_done: pending}\n---\n## Summary\nAnswered.\n",
        );
        std::os::unix::fs::symlink("documents", root.path().join("wiki/alias")).unwrap();
        let client = lk_queue::QueueLlmClient::new(root.path().join(".lorekeeper/queue"));
        let task = client.build_task(
            "pending",
            lk_queue::TaskKind::Summarize,
            lk_queue::SummarizeRequest {
                text: "Source".into(),
                max_sentences: 1,
                focus: None,
                locale: Default::default(),
                source_type: None,
                target: lk_queue::TaskTarget {
                    vault_path: "wiki/alias/subject.md".into(),
                    kind: lk_queue::TargetKind::DocumentSummary,
                    anchor: "## Summary".into(),
                },
            },
        );
        let raw = serde_json::to_value(&task).unwrap();
        let mut invalid = raw.clone();
        invalid["input"]["max_sentences"] = serde_json::json!("invalid");
        write(
            root.path(),
            ".lorekeeper/queue/pending.jsonl",
            &(serde_json::to_string(&invalid).unwrap() + "\n"),
        );
        assert!(
            InputReadiness::load(root.path())
                .unwrap()
                .require_ready(root.path(), &candidate(root.path()))
                .is_err()
        );
        std::fs::hard_link(
            root.path().join("wiki/documents/subject.md"),
            root.path().join("wiki/documents/hard.md"),
        )
        .unwrap();
        invalid["target"]["vault_path"] = serde_json::json!("wiki/documents/hard.md");
        write(
            root.path(),
            ".lorekeeper/queue/pending.jsonl",
            &(serde_json::to_string(&invalid).unwrap() + "\n"),
        );
        InputReadiness::load(root.path())
            .unwrap()
            .require_ready(root.path(), &candidate(root.path()))
            .unwrap();
    }
}
