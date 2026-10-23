use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use lk_core::concept::file_revision;
use lk_core::config::Config;
use lk_core::discovery::DiscoveryQuery;
use miette::IntoDiagnostic;
use serde_json::json;

use super::{
    ScreeningPreparation, admitted_task, discovery, is_progressive, resources,
    screening_with_inventory,
};

pub(in crate::commands) struct PublicationScope {
    pub revision: String,
    pub owners: BTreeSet<PathBuf>,
    pub pages: BTreeMap<PathBuf, String>,
}

/// Current inputs for retry admission, including an unplanned replacement selection.
/// This observation never authorizes publication or replaces deterministic application.
pub(in crate::commands) fn publication_scope(
    root: &Path,
    config: &Config,
    config_revision: &str,
    task_id: &str,
    queries: &[DiscoveryQuery],
) -> miette::Result<PublicationScope> {
    let root = root.canonicalize().into_diagnostic()?;
    let task = super::super::queue::select_task(
        &root,
        task_id,
        super::super::queue::TaskSelection::CurrentOrDoneExtraction,
    )?;
    let supplied = discovery::validate_queries(queries)?;
    let recorded = if task.kind == lk_queue::TaskKind::ExtractConcepts {
        discovery::current(&root, &task)?
    } else {
        None
    };
    let (prepared, owners, pages, phase) = if task.kind == lk_queue::TaskKind::ExtractConcepts
        && is_progressive(config)?
    {
        let inventory = resources::selection_inventory(
            &root,
            config,
            task_id,
            recorded.as_deref().unwrap_or(&supplied),
        )?;
        let inventory_revision = inventory.revision.clone();
        let mut owners = inventory.pages.keys().cloned().collect::<BTreeSet<_>>();
        let mut pages = inventory.pages.clone();
        let supplied_revision = if recorded
            .as_ref()
            .is_some_and(|queries| *queries != supplied)
        {
            let supplied_inventory =
                resources::selection_inventory(&root, config, task_id, &supplied)?;
            owners.extend(supplied_inventory.pages.keys().cloned());
            merge_pages(&mut pages, supplied_inventory.pages)?;
            Some(supplied_inventory.revision)
        } else {
            None
        };
        match screening_with_inventory(&root, config, config_revision, inventory)? {
            ScreeningPreparation::SourceBlocked { inventory, work_id } => {
                merge_pages(&mut pages, inventory.pages.clone())?;
                let phase = json!({
                    "inventory_revision":inventory_revision,
                    "supplied_inventory_revision":supplied_revision,
                    "source_work":work_id,
                    "source_blocked":true,
                });
                (inventory.source, owners, pages, phase)
            }
            ScreeningPreparation::Capacity {
                inventory,
                capacity,
            } => {
                merge_pages(&mut pages, inventory.pages.clone())?;
                let capacity = match capacity {
                    super::selection::ScreeningCapacity::Input {
                        required_bytes,
                        max_bytes,
                    } => {
                        json!({"kind":"input","required_bytes":required_bytes,"max_bytes":max_bytes})
                    }
                    super::selection::ScreeningCapacity::Result {
                        required_bytes,
                        max_bytes,
                    } => {
                        json!({"kind":"result","required_bytes":required_bytes,"max_bytes":max_bytes})
                    }
                };
                let phase = json!({
                    "inventory_revision":inventory_revision,
                    "supplied_inventory_revision":supplied_revision,
                    "capacity":capacity,
                });
                (inventory.source, owners, pages, phase)
            }
            ScreeningPreparation::Source { work, prepared } => {
                let planned = work.is_planned(&root)?;
                let progress = if planned {
                    let (progress, guards) = work.capture_progress(&root, &prepared.resources)?;
                    merge_pages(&mut pages, guards)?;
                    Some(progress)
                } else {
                    None
                };
                let phase = json!({
                    "inventory_revision":inventory_revision,
                    "supplied_inventory_revision":supplied_revision,
                    "source_work":work.plan.id,
                    "planned":planned,
                    "progress":progress,
                });
                (*prepared, owners, pages, phase)
            }
            ScreeningPreparation::Candidates(screening) => {
                merge_pages(&mut pages, screening.inventory.pages.clone())?;
                let state = screening.selection.publication_state(&root)?;
                let ledger = state.as_ref().map(|(ledger, _)| ledger);
                let prepared = match ledger.filter(|ledger| ledger.progress.complete) {
                    Some(ledger) => {
                        admitted_task(&screening, &ledger.binding()?, &ledger.selected_owners)?
                    }
                    None => screening.inventory.source.clone(),
                };
                let phase = json!({
                    "inventory_revision":inventory_revision,
                    "supplied_inventory_revision":supplied_revision,
                    "selection_id":screening.selection.id,
                    "ledger":ledger,
                });
                if let Some((_, guards)) = state {
                    merge_pages(&mut pages, guards)?;
                }
                (prepared, owners, pages, phase)
            }
        }
    } else {
        let prepared = if task.kind == lk_queue::TaskKind::ExtractConcepts {
            resources::task_with_discovery(&root, config, task_id, &supplied)?
        } else {
            resources::task_for_publication(&root, config, task_id)?
        };
        let mut owners = prepared.pages.keys().cloned().collect::<BTreeSet<_>>();
        let mut pages = prepared.pages.clone();
        let recorded_revision = match &recorded {
            Some(queries) if *queries != supplied => {
                let current = resources::task_with_discovery(&root, config, task_id, queries)?;
                owners.extend(current.pages.keys().cloned());
                let revision = current.revision(config_revision)?;
                merge_pages(&mut pages, current.pages)?;
                Some(revision)
            }
            _ => None,
        };
        (
            prepared,
            owners,
            pages,
            json!({"recorded_context_revision":recorded_revision}),
        )
    };
    let guards = pages
        .iter()
        .map(|(path, raw)| (path, file_revision(raw)))
        .collect::<Vec<_>>();
    let revision = lk_core::rss_original::digest(
        &serde_json::to_vec(&(
            "queue-publication-scope-v1",
            config_revision,
            &task,
            supplied,
            recorded,
            prepared.revision(config_revision)?,
            phase,
            guards,
        ))
        .into_diagnostic()?,
    );
    Ok(PublicationScope {
        revision,
        owners,
        pages,
    })
}

fn merge_pages(
    pages: &mut BTreeMap<PathBuf, String>,
    additional: BTreeMap<PathBuf, String>,
) -> miette::Result<()> {
    for (path, raw) in additional {
        if pages
            .insert(path, raw.clone())
            .is_some_and(|prior| prior != raw)
        {
            return Err(miette::miette!(
                "publication scope captured conflicting input bytes"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::queue::application::PublicationRejection;

    struct Fixture {
        _directory: tempfile::TempDir,
        root: PathBuf,
        config: Config,
        config_revision: String,
        task: lk_queue::QueueTask,
        queries: Vec<DiscoveryQuery>,
    }

    fn write(root: &Path, path: &str, raw: &str) {
        let full = root.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, raw).unwrap();
    }

    impl Fixture {
        fn build(mode: &str) -> Self {
            Self::build_with_text(mode, "Candidate source evidence.")
        }

        fn build_with_text(mode: &str, text: &str) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap();
            let config_path = root.join("config.yaml");
            std::fs::write(&config_path, format!("vault:\n  root: .\n  locale: en\nidentity: {{name: Tester, email: tester@example.com}}\nsources:\n  notes:\n    type: manual\n    params: {{inbox_dir: inbox}}\n    concept_policy: supported-new\nexecution:\n  work: {{extraction_context: {mode}, max_input_bytes: 16384, max_result_bytes: 4096}}\n")).unwrap();
            let (config, config_revision) = Config::load_with_revision(&config_path).unwrap();
            let task = lk_queue::QueueLlmClient::new(root.join(".lorekeeper/queue")).build_task(
                "ext",
                lk_queue::TaskKind::ExtractConcepts,
                lk_queue::ExtractConceptsRequest {
                    text: text.into(),
                    source_id: "notes".into(),
                    source_type: lk_core::config::SourceType::Manual,
                    date: jiff::civil::date(2026, 10, 22),
                    focus: None,
                    locale: lk_core::i18n::Locale::En,
                    categories: vec![],
                    rss_original: None,
                    concept_policy: lk_core::config::ConceptPolicy::SupportedNew,
                    target: lk_queue::TaskTarget {
                        vault_path: "wiki/documents/source.md".into(),
                        kind: lk_queue::TargetKind::DocumentConcepts,
                        anchor: "## Related Concepts".into(),
                    },
                },
            );
            write(
                &root,
                &task.target.vault_path,
                &format!(
                    "---\ntype: document\nllm_inputs: {{concepts: {}}}\n---\n## Content\nCandidate source evidence.\n\n## Related Concepts\n",
                    task.cache_hash
                ),
            );
            write(
                &root,
                "wiki/documents/candidate.md",
                "---\ntype: document\ntitle: Candidate observation\n---\nCandidate context.\n",
            );
            write(
                &root,
                ".lorekeeper/queue/pending.jsonl",
                &serde_json::to_string(&task).unwrap(),
            );
            Self {
                _directory: directory,
                root,
                config,
                config_revision,
                task,
                queries: vec![DiscoveryQuery {
                    query: "Candidate".into(),
                    phrase: true,
                    spacing: false,
                }],
            }
        }

        fn scope(&self) -> PublicationScope {
            publication_scope(
                &self.root,
                &self.config,
                &self.config_revision,
                &self.task.task_id,
                &self.queries,
            )
            .unwrap()
        }

        fn record_queries(&self, queries: &[DiscoveryQuery]) {
            let raw =
                std::fs::read_to_string(self.root.join(&self.task.target.vault_path)).unwrap();
            let session = lk_vault::MutationSession::acquire(&self.root).unwrap();
            discovery::record(
                &session,
                &self.task.task_id,
                serde_json::from_value(
                    json!({"target_revision":file_revision(&raw),"queries":queries}),
                )
                .unwrap(),
            )
            .unwrap();
        }
    }

    #[test]
    fn publication_scope_retains_readable_capacity_inputs_and_refuses_unreadable_pages() {
        let mut fixture = Fixture::build("progressive");
        let config_path = fixture.root.join("config.yaml");
        let config = std::fs::read_to_string(&config_path)
            .unwrap()
            .replace("max_result_bytes: 4096", "max_result_bytes: 1024");
        std::fs::write(&config_path, config).unwrap();
        (fixture.config, fixture.config_revision) =
            Config::load_with_revision(&config_path).unwrap();
        fixture.record_queries(&fixture.queries);
        let before = fixture.scope();
        let address = format!(
            "wiki/concepts/{}candidate.md",
            format!("{}/", "owner".repeat(40)).repeat(4)
        );
        let original =
            "---\ntype: concept\ntitle: Candidate\n---\nA scoped Candidate observation.\n";
        write(&fixture.root, &address, original);
        let capacity = fixture.scope();
        assert_ne!(capacity.revision, before.revision);
        assert!(capacity.owners.contains(Path::new(&address)));
        assert_eq!(capacity.pages[Path::new(&address)], original);
        assert_eq!(capacity.revision, fixture.scope().revision);

        let changed = format!("{original}\nChanged scoped evidence.\n");
        write(&fixture.root, &address, &changed);
        assert_ne!(capacity.revision, fixture.scope().revision);
        std::fs::write(fixture.root.join(&address), [0xff]).unwrap();
        let error = publication_scope(
            &fixture.root,
            &fixture.config,
            &fixture.config_revision,
            &fixture.task.task_id,
            &fixture.queries,
        )
        .err()
        .expect("unreadable current evidence must remain fatal");
        assert!(error.downcast_ref::<PublicationRejection>().is_none());
        assert!(
            error
                .downcast_ref::<super::super::selection::ScreeningCapacity>()
                .is_none()
        );
        std::fs::remove_file(fixture.root.join(&address)).unwrap();
        assert_eq!(before.revision, fixture.scope().revision);
    }

    #[test]
    fn source_observation_scope_preserves_guards_when_capacity_or_blockers_prevent_screening() {
        let mut fixture = Fixture::build_with_text(
            "progressive",
            &"Candidate attributed source observation.\n\n".repeat(2000),
        );
        fixture
            .queries
            .extend((b'A'..=b'C').map(|character| DiscoveryQuery {
                query: char::from(character).to_string().repeat(4096),
                phrase: true,
                spacing: false,
            }));
        fixture.record_queries(&fixture.queries);
        let ScreeningPreparation::Source { work, prepared } = super::super::screening(
            &fixture.root,
            &fixture.config,
            &fixture.config_revision,
            &fixture.task.task_id,
            &fixture.queries,
        )
        .unwrap() else {
            panic!("large retained source requires bounded observations");
        };
        {
            let session = lk_vault::MutationSession::acquire(&fixture.root).unwrap();
            super::super::persist_prepared(
                &session,
                &fixture.config,
                &fixture.config_revision,
                &prepared,
            )
            .unwrap();
            loop {
                let (progress, request) = work
                    .prepare_inspection(&fixture.root, &prepared.resources)
                    .unwrap();
                if progress.complete {
                    break;
                }
                let request = request.unwrap();
                work.record(
                    &session,
                    super::super::WorkOutput {
                        work_id: work.plan.id.clone(),
                        node_id: request["node_id"].as_str().unwrap().into(),
                        input_revision: request["input_revision"].as_str().unwrap().into(),
                        summary: "S".repeat(2500),
                        findings: vec![],
                        source_questions: vec![],
                        unresolved: vec![],
                        resolutions: vec![],
                    },
                    &prepared.resources,
                )
                .unwrap();
            }
        }
        let inventory = resources::selection_inventory(
            &fixture.root,
            &fixture.config,
            &fixture.task.task_id,
            &fixture.queries,
        )
        .unwrap();
        assert!(matches!(
            screening_with_inventory(
                &fixture.root,
                &fixture.config,
                &fixture.config_revision,
                inventory,
            )
            .unwrap(),
            ScreeningPreparation::Capacity {
                capacity: super::super::selection::ScreeningCapacity::Input { .. },
                ..
            }
        ));
        let (result, guards) = work
            .result_with_guards(&fixture.root, &prepared.resources)
            .unwrap();
        assert!(guards.len() > 1);
        let before = fixture.scope();
        for (path, raw) in &guards {
            assert_eq!(before.pages.get(path), Some(raw));
            assert!(!before.owners.contains(path));
        }
        let mut output: super::super::WorkOutput =
            serde_json::from_value(result["output"].clone()).unwrap();
        output.summary = "T".repeat(2500);
        let session = lk_vault::MutationSession::acquire(&fixture.root).unwrap();
        work.record(&session, output.clone(), &prepared.resources)
            .unwrap();
        drop(session);
        assert_ne!(before.revision, fixture.scope().revision);

        output.unresolved.push(super::super::state::WorkQuestion {
            id: "source:missing-required-read".into(),
            question: "A required supporting read remains unavailable.".into(),
            evidence: vec![work.plan.parts[0].ranges[0].clone()],
        });
        let session = lk_vault::MutationSession::acquire(&fixture.root).unwrap();
        work.record(&session, output, &prepared.resources).unwrap();
        drop(session);
        assert!(
            work.progress(&fixture.root, &prepared.resources)
                .unwrap()
                .complete
        );
        let (blocked_result, guards) = work
            .result_with_guards(&fixture.root, &prepared.resources)
            .unwrap();
        assert_eq!(
            blocked_result["output"]["unresolved"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let blocked = fixture.scope();
        for (path, raw) in guards {
            assert_eq!(blocked.pages.get(&path), Some(&raw));
            assert!(!blocked.owners.contains(&path));
        }
    }

    #[test]
    fn publication_scope_tracks_unplanned_membership_reviews_and_configuration() {
        let fixture = Fixture::build("progressive");
        let initial = fixture.scope();
        assert_eq!(initial.revision, fixture.scope().revision);
        assert!(
            initial
                .owners
                .contains(Path::new("wiki/documents/candidate.md"))
        );
        assert!(!fixture.root.join(".lorekeeper/queue/selection").exists());
        let changed_config = publication_scope(
            &fixture.root,
            &fixture.config,
            "different-config-revision",
            &fixture.task.task_id,
            &fixture.queries,
        )
        .unwrap();
        assert_ne!(initial.revision, changed_config.revision);
        write(
            &fixture.root,
            "wiki/documents/new.md",
            "---\ntype: document\n---\nCandidate added.\n",
        );
        let expanded = fixture.scope();
        assert_ne!(initial.revision, expanded.revision);
        assert!(expanded.owners.contains(Path::new("wiki/documents/new.md")));
        let raw =
            std::fs::read_to_string(fixture.root.join(&fixture.task.target.vault_path)).unwrap();
        let revision = lk_core::concept::source_revision(&lk_vault::parse_page(&raw).unwrap());
        write(
            &fixture.root,
            "wiki/explorations/review.md",
            &format!(
                "---\ntype: exploration\nreview: {{status: open, subjects: [wiki/documents/candidate.md], evidence: [{{path: {}, revision: {revision}}}]}}\n---\nA scoped assessment.\n",
                fixture.task.target.vault_path
            ),
        );
        let reviewed = fixture.scope();
        assert_ne!(expanded.revision, reviewed.revision);
        assert!(
            reviewed
                .owners
                .contains(Path::new("wiki/explorations/review.md"))
        );
        write(
            &fixture.root,
            "wiki/documents/unrelated.md",
            "---\ntype: document\n---\nA separate observation.\n",
        );
        assert_eq!(reviewed.revision, fixture.scope().revision);
        assert!(!fixture.root.join(".lorekeeper/queue/selection").exists());
        std::fs::write(fixture.root.join("wiki/documents/candidate.md"), [0xff]).unwrap();
        let error = publication_scope(
            &fixture.root,
            &fixture.config,
            &fixture.config_revision,
            &fixture.task.task_id,
            &fixture.queries,
        )
        .err()
        .unwrap();
        assert!(error.downcast_ref::<PublicationRejection>().is_none());
    }

    #[test]
    fn publication_scope_tracks_current_decisions_and_refuses_corrupt_receipts() {
        let fixture = Fixture::build("progressive");
        fixture.record_queries(&fixture.queries);
        let unplanned = fixture.scope();
        let ScreeningPreparation::Candidates(screening) = super::super::screening(
            &fixture.root,
            &fixture.config,
            &fixture.config_revision,
            &fixture.task.task_id,
            &fixture.queries,
        )
        .unwrap() else {
            panic!("small fixture requires candidate screening")
        };
        let session = lk_vault::MutationSession::acquire(&fixture.root).unwrap();
        screening
            .selection
            .create(&session, &screening.inventory.pages)
            .unwrap();
        let planned = fixture.scope();
        assert_ne!(unplanned.revision, planned.revision);
        let (_, request) = screening.selection.prepare(&fixture.root).unwrap();
        let request = request.unwrap();
        let output = super::super::selection::ScreeningOutput {
            selection_id: screening.selection.id.clone(),
            batch_id: request["batch_id"].as_str().unwrap().into(),
            input_revision: request["input_revision"].as_str().unwrap().into(),
            decisions: screening
                .inventory
                .capture
                .candidates
                .iter()
                .map(|candidate| super::super::selection::CandidateDecision {
                    candidate_key: candidate.owner.clone(),
                    decision: super::super::selection::ScreeningDecision::NotSelected,
                    reason: "The candidate card is unrelated to this source scope.".into(),
                })
                .collect(),
        };
        screening
            .selection
            .record(&session, &screening.inventory.pages, output, None)
            .unwrap();
        let completed = fixture.scope();
        assert_ne!(planned.revision, completed.revision);
        assert!(
            completed
                .pages
                .keys()
                .any(|path| path.starts_with(".lorekeeper/queue/selection"))
        );
        assert!(
            completed
                .owners
                .iter()
                .all(|path| !path.starts_with(".lorekeeper"))
        );
        let receipt = screening
            .selection
            .ledger(&fixture.root)
            .unwrap()
            .progress
            .receipts
            .remove(0)
            .path;
        std::fs::write(fixture.root.join(receipt), "{").unwrap();
        let error = publication_scope(
            &fixture.root,
            &fixture.config,
            &fixture.config_revision,
            &fixture.task.task_id,
            &fixture.queries,
        )
        .err()
        .unwrap();
        assert!(error.downcast_ref::<PublicationRejection>().is_none());
    }

    #[test]
    fn publication_scope_keeps_supplied_and_recorded_recipe_inputs_distinct() {
        for mode in ["progressive", "full-context"] {
            let fixture = Fixture::build(mode);
            let initial = fixture.scope();
            fixture.record_queries(&[DiscoveryQuery {
                query: "Other".into(),
                phrase: true,
                spacing: false,
            }]);
            let recorded = fixture.scope();
            assert_ne!(initial.revision, recorded.revision);
            assert!(
                recorded
                    .owners
                    .contains(Path::new("wiki/documents/candidate.md"))
            );
            write(
                &fixture.root,
                "wiki/documents/candidate.md",
                "---\ntype: document\n---\nCandidate context changed.\n",
            );
            assert_ne!(recorded.revision, fixture.scope().revision);
        }
    }

    #[test]
    fn ordinary_section_scope_retains_only_its_target_and_request() {
        let fixture = Fixture::build("progressive");
        let task = lk_queue::QueueLlmClient::new(fixture.root.join(".lorekeeper/queue"))
            .build_task(
                "summary",
                lk_queue::TaskKind::Summarize,
                lk_queue::SummarizeRequest {
                    text: "Retained source text.".into(),
                    max_sentences: 2,
                    focus: None,
                    locale: lk_core::i18n::Locale::En,
                    source_type: None,
                    target: lk_queue::TaskTarget {
                        vault_path: fixture.task.target.vault_path.clone(),
                        kind: lk_queue::TargetKind::DocumentSummary,
                        anchor: "## Summary".into(),
                    },
                },
            );
        write(
            &fixture.root,
            &task.target.vault_path,
            &format!(
                "---\ntype: document\nllm_inputs: {{summary: {}}}\n---\n## Summary\n",
                task.cache_hash
            ),
        );
        write(
            &fixture.root,
            ".lorekeeper/queue/pending.jsonl",
            &serde_json::to_string(&task).unwrap(),
        );
        let scope = || {
            publication_scope(
                &fixture.root,
                &fixture.config,
                &fixture.config_revision,
                &task.task_id,
                &[],
            )
            .unwrap()
        };
        let before = scope();
        assert_eq!(
            before.owners,
            BTreeSet::from([PathBuf::from(&task.target.vault_path)])
        );
        assert_eq!(before.pages.len(), 1);
        write(
            &fixture.root,
            "wiki/documents/candidate.md",
            "---\ntype: document\n---\nAn unrelated changed observation.\n",
        );
        assert_eq!(before.revision, scope().revision);
    }

    #[test]
    fn absent_proposed_reuse_owner_is_local_but_unknown_scope_io_is_fatal() {
        let fixture = Fixture::build("full-context");
        let before = fixture.scope();
        let prepared = resources::task_with_discovery(
            &fixture.root,
            &fixture.config,
            &fixture.task.task_id,
            &fixture.queries,
        )
        .unwrap();
        let result = lk_queue::TaskResult {
            task_id: fixture.task.task_id.clone(),
            cache_hash: fixture.task.cache_hash.clone(),
            target_revision: file_revision(
                &before.pages[Path::new(&fixture.task.target.vault_path)],
            ),
            context_revision: prepared.revision(&fixture.config_revision).unwrap(),
            selection: None,
            discovery: fixture.queries.clone(),
            concept_policy: lk_core::config::ConceptPolicy::SupportedNew,
            target: fixture.task.target.clone(),
            date: jiff::civil::date(2026, 10, 22),
            concepts: vec![lk_queue::ReportedConcept {
                admission: lk_queue::ConceptAdmission::Reuse {
                    owner: lk_queue::ConceptOwner {
                        path: "wiki/concepts/missing.md".into(),
                        revision: "0".repeat(32),
                    },
                },
                concept: lk_core::concept::ExtractedConcept {
                    name: "Candidate owner".into(),
                    category: None,
                    aliases: vec![],
                },
                synthesis: None,
                rss_evidence: vec![],
            }],
        };
        let apply = || {
            super::super::prepare_queue_publication(
                &fixture.root,
                &fixture.config,
                &fixture.config_revision,
                &fixture.task.task_id,
                serde_json::to_value(&result).unwrap(),
            )
        };
        let error = apply().err().unwrap();
        assert_eq!(
            error
                .downcast_ref::<PublicationRejection>()
                .map(|error| error.kind),
            Some(super::super::RejectionKind::EvidenceScopeChanged),
        );
        assert_eq!(before.revision, fixture.scope().revision);
        write(
            &fixture.root,
            "wiki/concepts/missing.md",
            "---\ntype: concept\ntitle: Candidate owner\n---\nA newly available Candidate definition.\n",
        );
        assert_ne!(before.revision, fixture.scope().revision);
        std::fs::remove_file(fixture.root.join("wiki/concepts/missing.md")).unwrap();
        std::fs::remove_dir(fixture.root.join("wiki/concepts")).unwrap();
        std::fs::write(fixture.root.join("wiki/concepts"), "not a directory").unwrap();
        assert!(
            apply()
                .err()
                .unwrap()
                .downcast_ref::<PublicationRejection>()
                .is_none()
        );
    }

    #[test]
    fn partial_source_observation_changes_scope_without_changing_progress() {
        let fixture = Fixture::build_with_text(
            "progressive",
            &"Candidate attributed source observation.\n\n".repeat(2000),
        );
        fixture.record_queries(&fixture.queries);
        let unplanned = fixture.scope();
        let ScreeningPreparation::Source { work, prepared } = super::super::screening(
            &fixture.root,
            &fixture.config,
            &fixture.config_revision,
            &fixture.task.task_id,
            &fixture.queries,
        )
        .unwrap() else {
            panic!("large source requires observations")
        };
        let session = lk_vault::MutationSession::acquire(&fixture.root).unwrap();
        super::super::state::Work::create(
            &session,
            &prepared.resources,
            &prepared.revision(&fixture.config_revision).unwrap(),
            prepared.context.clone(),
            super::super::limits(&fixture.config).unwrap(),
        )
        .unwrap();
        assert_ne!(unplanned.revision, fixture.scope().revision);
        let (_, request) = work
            .prepare_inspection(&fixture.root, &prepared.resources)
            .unwrap();
        let request = request.unwrap();
        let mut output = super::super::state::WorkOutput {
            work_id: work.plan.id.clone(),
            node_id: request["node_id"].as_str().unwrap().into(),
            input_revision: request["input_revision"].as_str().unwrap().into(),
            summary: "First attributed source observation.".into(),
            findings: vec![],
            source_questions: vec![],
            unresolved: vec![],
            resolutions: vec![],
        };
        work.record(&session, output.clone(), &prepared.resources)
            .unwrap();
        let before = fixture.scope();
        let progress =
            serde_json::to_value(work.progress(&fixture.root, &prepared.resources).unwrap())
                .unwrap();
        output.summary = "Corrected attributed source observation.".into();
        work.record(&session, output, &prepared.resources).unwrap();
        assert_eq!(
            progress,
            serde_json::to_value(work.progress(&fixture.root, &prepared.resources).unwrap())
                .unwrap()
        );
        let after = fixture.scope();
        assert_ne!(before.revision, after.revision);
        assert!(
            after
                .pages
                .keys()
                .any(|path| path.starts_with(".lorekeeper/work"))
        );
        assert!(
            after
                .owners
                .iter()
                .all(|path| !path.starts_with(".lorekeeper"))
        );
    }
}
