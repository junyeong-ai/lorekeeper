use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use lk_core::concept::file_revision;
use lk_core::config::Config;
use lk_core::discovery::DiscoveryQuery;
use miette::IntoDiagnostic;
use serde_json::json;

use super::{Prepared, discovery, resources, sealed_task_controlled, selection};
use crate::commands::queue::application::PublicationRejection;

pub(in crate::commands) struct PublicationScope {
    pub revision: String,
    pub owners: BTreeSet<PathBuf>,
    pub pages: BTreeMap<PathBuf, String>,
}

#[cfg(test)]
pub(in crate::commands) fn publication_scope(
    root: &Path,
    config: &Config,
    config_revision: &str,
    task_id: &str,
    queries: &[DiscoveryQuery],
    selection: Option<&lk_queue::ExtractionSelection>,
) -> miette::Result<PublicationScope> {
    publication_scope_controlled(
        root,
        config,
        config_revision,
        task_id,
        queries,
        selection,
        &lk_vault::ReadControl::default(),
    )
}

struct Evidence {
    prepared: Prepared,
    owners: BTreeSet<PathBuf>,
    pages: BTreeMap<PathBuf, String>,
    phase: serde_json::Value,
}

/// What deterministic application reads for a result bound to a sealed roster: the source,
/// the selected owners and the stored screening state. A typed refusal keeps the source
/// and that stored state, so the proposal stays held until one of them changes.
fn sealed_evidence(
    root: &Path,
    config: &Config,
    config_revision: &str,
    task: &lk_queue::QueueTask,
    queries: &[DiscoveryQuery],
    binding: &lk_queue::ExtractionSelection,
    read: &lk_vault::ReadControl,
) -> miette::Result<Evidence> {
    let source = resources::task_for_publication_controlled(root, config, &task.task_id, read)?;
    let stored = selection::sealed_inputs(root, &task.task_id, &task.cache_hash, binding)?;
    let sealed = sealed_task_controlled(
        root,
        config,
        config_revision,
        &source,
        queries,
        binding,
        None,
        read,
    );
    let (prepared, observation_guards, refusal) = match sealed {
        Ok(sealed) => (sealed.prepared, sealed.observation_guards, None),
        Err(error) => match error.downcast_ref::<PublicationRejection>() {
            Some(rejection) => (source, BTreeMap::new(), Some(rejection.kind)),
            None => return Err(error),
        },
    };
    let owners = prepared.pages.keys().cloned().collect();
    let mut pages = prepared.pages.clone();
    merge_pages(&mut pages, observation_guards)?;
    let mut state = Vec::with_capacity(stored.len());
    for (path, raw) in stored {
        state.push((path.clone(), raw.as_deref().map(file_revision)));
        if let Some(raw) = raw {
            merge_pages(&mut pages, BTreeMap::from([(path, raw)]))?;
        }
    }
    Ok(Evidence {
        prepared,
        owners,
        pages,
        phase: json!({"selection":binding, "stored":state, "refusal":refusal}),
    })
}

/// Current inputs for retry admission: everything deterministic application would read
/// for a proposal carrying these recipes and this selection. This observation never
/// authorizes publication or replaces deterministic application.
pub(in crate::commands) fn publication_scope_controlled(
    root: &Path,
    config: &Config,
    config_revision: &str,
    task_id: &str,
    queries: &[DiscoveryQuery],
    selection: Option<&lk_queue::ExtractionSelection>,
    read: &lk_vault::ReadControl,
) -> miette::Result<PublicationScope> {
    read.check().into_diagnostic()?;
    let root = root.canonicalize().into_diagnostic()?;
    let task = super::work_task_controlled(&root, task_id, read)?;
    let supplied = discovery::validate_queries(queries)?;
    let extraction = task.kind == lk_queue::TaskKind::ExtractConcepts;
    let recorded = if extraction {
        discovery::current(&root, &task)?
    } else {
        None
    };
    let Evidence {
        prepared,
        owners,
        pages,
        phase,
    } = match selection.filter(|_| extraction) {
        Some(binding) => sealed_evidence(
            &root,
            config,
            config_revision,
            &task,
            &supplied,
            binding,
            read,
        )?,
        None => {
            let prepared = if extraction {
                resources::task_with_discovery_controlled(&root, config, task_id, &supplied, read)?
            } else {
                resources::task_for_publication_controlled(&root, config, task_id, read)?
            };
            let mut owners = prepared.pages.keys().cloned().collect::<BTreeSet<_>>();
            let mut pages = prepared.pages.clone();
            // A synthesis publishes only for the citation set it answers, and an alias can move
            // that set without changing any captured byte, so the scope carries the set too.
            let graph_citations = prepared.graph_citations.clone();
            let recorded_revision = match &recorded {
                Some(queries) if *queries != supplied => {
                    let current = resources::task_with_discovery_controlled(
                        &root, config, task_id, queries, read,
                    )?;
                    owners.extend(current.pages.keys().cloned());
                    let revision = current.revision(config_revision)?;
                    merge_pages(&mut pages, current.pages)?;
                    Some(revision)
                }
                _ => None,
            };
            Evidence {
                prepared,
                owners,
                pages,
                phase: match graph_citations {
                    Some(citations) => json!({
                        "recorded_context_revision": recorded_revision,
                        "graph_citations": citations,
                    }),
                    None => json!({"recorded_context_revision": recorded_revision}),
                },
            }
        }
    };
    read.check().into_diagnostic()?;
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
    read.check().into_diagnostic()?;
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
            self.scope_of(None).unwrap()
        }

        fn scope_of(
            &self,
            selection: Option<&lk_queue::ExtractionSelection>,
        ) -> miette::Result<PublicationScope> {
            publication_scope(
                &self.root,
                &self.config,
                &self.config_revision,
                &self.task.task_id,
                &self.queries,
                selection,
            )
        }

        /// Screen every candidate, inspecting only `selected`, and seal that roster.
        fn seal(&self, selected: &str) -> lk_queue::ExtractionSelection {
            let screen = || {
                super::super::screening(
                    &self.root,
                    &self.config,
                    &self.config_revision,
                    &self.task.task_id,
                    &self.queries,
                )
                .unwrap()
                .into_candidates()
                .unwrap()
            };
            let request = screen().selection.next_request(false).unwrap().unwrap();
            let value = serde_json::to_value(&request).unwrap();
            let output = serde_json::from_value(json!({
                "task_id":value["task_id"],
                "request_revision":value["request_revision"],
                "decisions":value["candidates"].as_array().unwrap().iter().map(|candidate| json!({
                    "candidate_key":candidate["candidate_key"],
                    "decision":if candidate["owner"] == selected { "inspect" } else { "not-selected" },
                    "reason":"Card-scoped assessment.",
                })).collect::<Vec<_>>(),
            }))
            .unwrap();
            let session = lk_vault::MutationSession::acquire(&self.root).unwrap();
            session
                .apply(selection::plan_record(&self.root, &request, output).unwrap())
                .unwrap();
            let screening = screen();
            let mut plan = lk_vault::MutationPlan::default();
            screening
                .selection
                .seal()
                .unwrap()
                .plan(&self.root, &mut plan)
                .unwrap();
            session.apply(plan).unwrap();
            screening.selection.binding().unwrap()
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
    fn controlled_publication_scope_preserves_current_inputs_and_cancellation() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };

        let fixture = Fixture::build("progressive");
        let binding = fixture.seal("wiki/documents/candidate.md");
        for selection in [None, Some(&binding)] {
            let expected = fixture.scope_of(selection).unwrap();
            let cancelled = Arc::new(AtomicBool::new(false));
            let read = lk_vault::ReadControl::new(cancelled.clone());
            let scope = || {
                publication_scope_controlled(
                    &fixture.root,
                    &fixture.config,
                    &fixture.config_revision,
                    &fixture.task.task_id,
                    &fixture.queries,
                    selection,
                    &read,
                )
            };
            let current = scope().unwrap();
            assert_eq!(current.revision, expected.revision);
            assert_eq!(current.owners, expected.owners);
            assert_eq!(current.pages, expected.pages);
            cancelled.store(true, Ordering::Relaxed);
            assert!(scope().is_err());
            assert_eq!(fixture.scope_of(selection).unwrap().pages, expected.pages);
        }
    }

    #[test]
    fn unbound_scope_tracks_membership_reviews_and_configuration() {
        let fixture = Fixture::build("full-context");
        let initial = fixture.scope();
        assert_eq!(initial.revision, fixture.scope().revision);
        assert!(
            initial
                .owners
                .contains(Path::new("wiki/documents/candidate.md"))
        );
        let changed_config = publication_scope(
            &fixture.root,
            &fixture.config,
            "different-config-revision",
            &fixture.task.task_id,
            &fixture.queries,
            None,
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
        std::fs::write(fixture.root.join("wiki/documents/candidate.md"), [0xff]).unwrap();
        let error = fixture.scope_of(None).err().unwrap();
        assert!(error.downcast_ref::<PublicationRejection>().is_none());
    }

    #[test]
    fn sealed_scope_tracks_the_source_selected_owners_and_stored_decisions() {
        let fixture = Fixture::build("progressive");
        let (selected, excluded) = ("wiki/documents/candidate.md", "wiki/documents/excluded.md");
        let excluded_raw =
            "---\ntype: document\ntitle: Excluded observation\n---\nCandidate excluded context.\n";
        write(&fixture.root, excluded, excluded_raw);
        let binding = fixture.seal(selected);
        let scope = || fixture.scope_of(Some(&binding));
        let sealed = scope().unwrap();
        assert_eq!(
            sealed.owners,
            BTreeSet::from([
                PathBuf::from(&fixture.task.target.vault_path),
                selected.into()
            ])
        );
        let stored = sealed
            .pages
            .keys()
            .filter(|path| path.starts_with(".lorekeeper/queue/screening"))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(stored.len(), 2, "the ledger and the sealed roster");

        write(
            &fixture.root,
            excluded,
            &format!("{excluded_raw}\nEdited afterwards.\n"),
        );
        write(
            &fixture.root,
            "wiki/documents/new.md",
            "---\ntype: document\n---\nCandidate added afterwards.\n",
        );
        assert_eq!(scope().unwrap().revision, sealed.revision);

        let selected_raw = std::fs::read_to_string(fixture.root.join(selected)).unwrap();
        write(
            &fixture.root,
            selected,
            &format!("{selected_raw}\nEdited afterwards.\n"),
        );
        assert_ne!(scope().unwrap().revision, sealed.revision);
        write(&fixture.root, selected, &selected_raw);
        assert_eq!(scope().unwrap().revision, sealed.revision);

        let unsealed = lk_queue::ExtractionSelection {
            inventory_revision: "e".repeat(64),
            ..binding.clone()
        };
        let refused = fixture.scope_of(Some(&unsealed)).unwrap();
        assert_ne!(refused.revision, sealed.revision);
        assert_eq!(
            refused.owners,
            BTreeSet::from([PathBuf::from(&fixture.task.target.vault_path)])
        );
        assert_eq!(
            refused.revision,
            fixture.scope_of(Some(&unsealed)).unwrap().revision
        );

        for path in stored {
            let raw = std::fs::read_to_string(fixture.root.join(&path)).unwrap();
            std::fs::write(fixture.root.join(&path), "{").unwrap();
            let error = scope()
                .err()
                .expect("corrupt stored screening state is fatal");
            assert!(error.downcast_ref::<PublicationRejection>().is_none());
            std::fs::write(fixture.root.join(&path), raw).unwrap();
        }
        assert_eq!(scope().unwrap().revision, sealed.revision);
    }

    #[test]
    fn unbound_scope_keeps_supplied_and_recorded_recipe_inputs_distinct() {
        let fixture = Fixture::build("full-context");
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
                None,
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
}
