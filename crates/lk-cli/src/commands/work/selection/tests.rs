use super::*;
use lk_vault::MutationSession;
use serde_json::json;

const CONTRACT: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
const LIMITS: ScreeningLimits = ScreeningLimits {
    max_input_bytes: 65536,
    max_result_bytes: 16384,
};

struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    task_id: String,
    cache_hash: String,
}

fn source() -> Value {
    json!({"role":"complete-retained-input","text":"A complete retained observation."})
}

fn owner(index: usize) -> String {
    format!("wiki/documents/candidate-{index}.md")
}

impl Fixture {
    fn new(candidates: usize) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let fixture = Self {
            root: directory.path().canonicalize().unwrap(),
            _directory: directory,
            task_id: "retained-task".into(),
            cache_hash: "input-hash".into(),
        };
        std::fs::create_dir_all(fixture.root.join("wiki/documents")).unwrap();
        for index in 0..candidates {
            fixture.write(index, "", "has its own declared scope.");
        }
        fixture
    }

    fn write(&self, index: usize, metadata: &str, opening: &str) {
        std::fs::write(
            self.root.join(owner(index)),
            format!(
                "---\ntype: document\ntitle: Subject {index}\n{metadata}---\n## Summary\nCandidate {index} {opening}\n"
            ),
        )
        .unwrap();
    }

    fn capture(&self) -> lk_vault::DiscoveryCapture {
        lk_vault::capture_discovery(
            &self.root,
            &Default::default(),
            &[DiscoveryQuery {
                query: "Candidate".into(),
                phrase: true,
                spacing: false,
            }],
        )
        .unwrap()
    }

    fn build(
        &self,
        contract: &str,
        source: Value,
        limits: ScreeningLimits,
    ) -> miette::Result<Selection> {
        let capture = self.capture();
        Selection::build(
            &self.root,
            &SelectionIdentity {
                task_id: &self.task_id,
                cache_hash: &self.cache_hash,
                contract_revision: contract,
            },
            source,
            &capture.queries,
            Rows::build(&capture)?,
            limits,
        )
    }

    fn selection(&self) -> Selection {
        self.build(CONTRACT, source(), LIMITS).unwrap()
    }

    fn apply(&self, plan: MutationPlan) {
        MutationSession::acquire(&self.root)
            .unwrap()
            .apply(plan)
            .unwrap();
    }

    fn record(&self, request: &Request, decide: impl Fn(&str) -> ScreeningDecision) {
        self.apply(plan_record(&self.root, request, answer(request, decide)).unwrap());
    }

    fn seal(&self, selection: &Selection) {
        let mut plan = MutationPlan::default();
        selection
            .seal()
            .unwrap()
            .plan(&self.root, &mut plan)
            .unwrap();
        self.apply(plan);
    }

    fn ledger(&self) -> PathBuf {
        self.root
            .join(directory(&self.task_id, &self.cache_hash).unwrap())
            .join("ledger.json")
    }
}

fn answer(request: &Request, decide: impl Fn(&str) -> ScreeningDecision) -> ScreeningOutput {
    ScreeningOutput {
        task_id: request.task_id.clone(),
        request_revision: request.request_revision.clone(),
        decisions: request
            .candidates
            .iter()
            .rev()
            .map(|candidate| CandidateDecision {
                candidate_key: candidate.candidate_key.clone(),
                decision: decide(&candidate.owner),
                reason: "Card-scoped assessment.".into(),
            })
            .collect(),
    }
}

fn owners(request: &Request) -> Vec<&str> {
    request
        .candidates
        .iter()
        .map(|candidate| candidate.owner.as_str())
        .collect()
}

fn keys(selection: &Selection) -> BTreeMap<String, String> {
    selection
        .members
        .iter()
        .map(|member| (member.owner.clone(), member.key.clone()))
        .collect()
}

fn inspect_first(owner: &str) -> ScreeningDecision {
    if owner == self::owner(0) {
        ScreeningDecision::Inspect
    } else {
        ScreeningDecision::NotSelected
    }
}

#[test]
fn only_changed_and_added_cards_return_for_screening() {
    let fixture = Fixture::new(4);
    let selection = fixture.selection();
    assert!(!fixture.root.join(".lorekeeper").exists());
    let request = selection.next_request(false).unwrap().unwrap();
    assert_eq!(request.candidates.len(), 4);
    fixture.record(&request, inspect_first);
    let complete = fixture.selection();
    assert!(complete.progress().complete);
    assert!(complete.next_request(false).unwrap().is_none());
    assert_eq!(complete.selected_owners(), BTreeSet::from([owner(0)]));
    let ledger = std::fs::read(fixture.ledger()).unwrap();

    fixture.write(1, "", "now describes another declared scope.");
    fixture.write(
        2,
        "source_count: 9\nupdated: 2026-11-17\n",
        "has its own declared scope.",
    );
    fixture.write(4, "", "has its own declared scope.");
    let current = fixture.selection();
    let progress = current.progress();
    assert_eq!(
        (
            progress.inspect,
            progress.card_not_selected,
            progress.unknown
        ),
        (1, 2, 2)
    );
    let request = current.next_request(false).unwrap().unwrap();
    assert_eq!(owners(&request), [owner(1), owner(4)]);
    assert_eq!(std::fs::read(fixture.ledger()).unwrap(), ledger);

    fixture.write(1, "", "has its own declared scope.");
    std::fs::remove_file(fixture.root.join(owner(4))).unwrap();
    assert!(fixture.selection().progress().complete);
}

#[test]
fn keys_bind_card_source_and_contract_but_not_packing() {
    let fixture = Fixture::new(3);
    let selection = fixture.selection();
    let before = keys(&selection);
    let whole = selection.next_request(false).unwrap().unwrap();
    assert_eq!(whole.candidates.len(), 3);

    let narrow = fixture
        .build(
            CONTRACT,
            source(),
            ScreeningLimits {
                max_input_bytes: encoded(&whole).unwrap().len() - 1,
                max_result_bytes: LIMITS.max_result_bytes,
            },
        )
        .unwrap();
    let packet = narrow.next_request(false).unwrap().unwrap();
    assert!(packet.candidates.len() < 3);
    assert_ne!(packet.request_revision, whole.request_revision);
    assert_eq!(keys(&narrow), before);
    fixture.record(&packet, inspect_first);
    let resumed = fixture.selection();
    assert_eq!(resumed.progress().unknown, 3 - packet.candidates.len());
    assert_eq!(
        owners(&resumed.next_request(false).unwrap().unwrap()),
        owners(&whole)[packet.candidates.len()..]
    );

    for changed in [
        fixture.build(&"d".repeat(64), source(), LIMITS).unwrap(),
        fixture
            .build(
                CONTRACT,
                json!({"role":"complete-retained-input","text":"Another observation."}),
                LIMITS,
            )
            .unwrap(),
    ] {
        assert_eq!(changed.progress().unknown, 3);
        assert!(
            keys(&changed)
                .iter()
                .all(|(owner, key)| before[owner] != *key)
        );
    }
    fixture.write(1, "", "now describes another declared scope.");
    let edited = keys(&fixture.selection());
    assert_ne!(edited[&owner(1)], before[&owner(1)]);
    assert_eq!(edited[&owner(0)], before[&owner(0)]);
    assert_eq!(edited[&owner(2)], before[&owner(2)]);
}

#[test]
fn deferred_decisions_hold_until_explicit_reassessment() {
    let fixture = Fixture::new(2);
    let request = fixture.selection().next_request(false).unwrap().unwrap();
    let defer_first = |owner: &str| {
        if owner == self::owner(0) {
            ScreeningDecision::Defer
        } else {
            ScreeningDecision::NotSelected
        }
    };
    fixture.record(&request, defer_first);
    let held = fixture.selection();
    let progress = held.progress();
    assert_eq!((progress.deferred, progress.unknown), (1, 0));
    assert!(!progress.complete);
    assert!(held.next_request(false).unwrap().is_none());
    assert!(held.binding().is_err());

    let ledger = std::fs::read(fixture.ledger()).unwrap();
    fixture.record(&request, defer_first);
    assert_eq!(std::fs::read(fixture.ledger()).unwrap(), ledger);
    assert!(plan_record(&fixture.root, &request, answer(&request, inspect_first)).is_err());

    let reassessment = held.next_request(true).unwrap().unwrap();
    assert_eq!(owners(&reassessment), [owner(0)]);
    assert!(reassessment.reassess && reassessment.candidates[0].previous.is_some());
    let output = answer(&reassessment, inspect_first);
    assert_eq!(
        held.request_for(&output).unwrap().request_revision,
        reassessment.request_revision
    );
    fixture.record(&reassessment, inspect_first);
    let complete = fixture.selection();
    assert!(complete.progress().complete);
    assert_eq!(complete.selected_owners(), BTreeSet::from([owner(0)]));
    assert!(complete.next_request(true).unwrap().is_none());
    assert!(complete.request_for(&output).is_err());
    assert!(
        plan_record(
            &fixture.root,
            &reassessment,
            answer(&reassessment, |_| { ScreeningDecision::NotSelected })
        )
        .is_err()
    );
}

#[test]
fn standalone_outputs_must_answer_a_current_request_exactly() {
    let fixture = Fixture::new(2);
    let selection = fixture.selection();
    let request = selection.next_request(false).unwrap().unwrap();
    let output = answer(&request, inspect_first);
    assert_eq!(
        selection.request_for(&output).unwrap().request_revision,
        request.request_revision
    );
    let mut partial = output.clone();
    partial.decisions.pop();
    assert!(selection.request_for(&partial).is_err());
    assert!(plan_record(&fixture.root, &request, partial).is_err());
    let mut foreign = output.clone();
    foreign.decisions[0].candidate_key = "e".repeat(64);
    assert!(selection.request_for(&foreign).is_err());
    for reason in [" ".to_owned(), "x".repeat(MAX_REASON_JSON_BYTES)] {
        let mut invalid = output.clone();
        invalid.decisions[0].reason = reason;
        assert!(plan_record(&fixture.root, &request, invalid).is_err());
    }
    let mut duplicate = output.clone();
    duplicate.decisions[1] = duplicate.decisions[0].clone();
    assert!(plan_record(&fixture.root, &request, duplicate).is_err());
    assert!(!fixture.root.join(".lorekeeper").exists());
}

#[test]
fn a_sealed_roster_binds_its_decisions_without_rereading_unselected_pages() {
    let fixture = Fixture::new(3);
    let request = fixture.selection().next_request(false).unwrap().unwrap();
    fixture.record(&request, inspect_first);
    let selection = fixture.selection();
    let binding = selection.binding().unwrap();
    let guard = |binding: &lk_queue::ExtractionSelection| {
        load_sealed(
            &fixture.root,
            &fixture.task_id,
            &fixture.cache_hash,
            binding,
            None,
        )
    };
    let kind = |error: miette::Report| {
        error
            .downcast_ref::<crate::commands::queue::application::PublicationRejection>()
            .map(|rejection| rejection.kind)
    };
    assert_eq!(
        kind(guard(&binding).err().unwrap()),
        Some(crate::commands::work::RejectionKind::SelectionInventoryChanged)
    );
    let seal = selection.seal().unwrap();
    let sealing = load_sealed(
        &fixture.root,
        &fixture.task_id,
        &fixture.cache_hash,
        &binding,
        Some(&seal),
    )
    .unwrap();
    assert_eq!(sealing.selected, BTreeSet::from([owner(0)]));
    assert!(
        sealing.guards.iter().all(|(path, _)| *path != seal.path),
        "the plan that stores the roster does not also require it unchanged"
    );
    fixture.seal(&selection);
    let sealed = guard(&binding).unwrap();
    assert_eq!(sealed.selected, BTreeSet::from([owner(0)]));
    assert_eq!(sealed.queries, selection.queries);
    let inputs = sealed_inputs(
        &fixture.root,
        &fixture.task_id,
        &fixture.cache_hash,
        &binding,
    )
    .unwrap();
    assert!(inputs.iter().all(|(_, raw)| raw.is_some()));
    for (path, raw) in &inputs {
        assert!(sealed.guards.contains(&(path.clone(), raw.clone())));
    }

    fixture.write(1, "", "now describes another declared scope.");
    fixture.write(3, "", "has its own declared scope.");
    assert_eq!(
        guard(&binding).unwrap().selected,
        BTreeSet::from([owner(0)])
    );
    assert_ne!(
        fixture.selection().roster_revision().unwrap(),
        binding.inventory_revision
    );

    let mut changed = binding.clone();
    changed.decisions_revision = "e".repeat(64);
    assert_eq!(
        kind(guard(&changed).err().unwrap()),
        Some(crate::commands::work::RejectionKind::SelectionDecisionsChanged)
    );
    let mut retired = binding.clone();
    retired.policy = lk_queue::ExtractionSelectionPolicy::ProgressiveV1;
    assert_eq!(
        kind(guard(&retired).err().unwrap()),
        Some(crate::commands::work::RejectionKind::SelectionDecisionsChanged)
    );
}

#[test]
fn an_empty_roster_is_complete_without_any_stored_state() {
    let fixture = Fixture::new(0);
    let selection = fixture.selection();
    assert!(selection.progress().complete);
    assert!(selection.next_request(false).unwrap().is_none());
    assert!(selection.selected_owners().is_empty());
    let binding = selection.binding().unwrap();
    assert!(!fixture.root.join(".lorekeeper").exists());
    fixture.seal(&selection);
    assert!(
        load_sealed(
            &fixture.root,
            &fixture.task_id,
            &fixture.cache_hash,
            &binding,
            None
        )
        .unwrap()
        .selected
        .is_empty()
    );
}

#[test]
fn indivisible_candidates_report_typed_capacity_instead_of_clipping() {
    let fixture = Fixture::new(1);
    let capacity = |limits| {
        fixture
            .build(CONTRACT, source(), limits)
            .err()
            .and_then(|error| error.downcast_ref::<ScreeningCapacity>().copied())
    };
    assert!(matches!(
        capacity(ScreeningLimits {
            max_input_bytes: 512,
            max_result_bytes: LIMITS.max_result_bytes,
        }),
        Some(ScreeningCapacity::Input { max_bytes: 512, .. })
    ));
    assert!(matches!(
        capacity(ScreeningLimits {
            max_input_bytes: LIMITS.max_input_bytes,
            max_result_bytes: 128,
        }),
        Some(ScreeningCapacity::Result { max_bytes: 128, .. })
    ));
    let rows = Rows::build(&fixture.capture()).unwrap();
    let identity = SelectionIdentity {
        task_id: &fixture.task_id,
        cache_hash: &fixture.cache_hash,
        contract_revision: CONTRACT,
    };
    assert!(Selection::source_fits(&identity, &source(), &rows, LIMITS).unwrap());
    assert!(
        !Selection::source_fits(
            &identity,
            &json!({"role":"complete-retained-input","text":"x".repeat(LIMITS.max_input_bytes)}),
            &rows,
            LIMITS
        )
        .unwrap()
    );
}

#[test]
fn stored_state_refuses_ambiguous_or_foreign_bytes() {
    let fixture = Fixture::new(2);
    let request = fixture.selection().next_request(false).unwrap().unwrap();
    fixture.record(&request, inspect_first);
    let path = fixture.ledger();
    let original = std::fs::read_to_string(&path).unwrap();
    for corrupt in [
        original.replacen("\"task_id\":", "\"task_id\":\"other\",\"task_id\":", 1),
        original.replacen("{\"schema\"", "{ \"schema\"", 1),
        original.replace(&fixture.task_id, "another-task"),
        original.replace("\"row\":0", "\"row\":1"),
    ] {
        assert_ne!(corrupt, original);
        std::fs::write(&path, &corrupt).unwrap();
        assert!(fixture.build(CONTRACT, source(), LIMITS).is_err());
        assert!(plan_record(&fixture.root, &request, answer(&request, inspect_first)).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), corrupt);
    }
    std::fs::write(&path, &original).unwrap();
    let packets = path.parent().unwrap().join("packets");
    let packet = std::fs::read_dir(&packets)
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    let raw = std::fs::read_to_string(packet.path()).unwrap();
    std::fs::write(packet.path(), raw.replace("inspect", "defer")).unwrap();
    assert!(fixture.build(CONTRACT, source(), LIMITS).is_err());
    std::fs::write(packet.path(), raw).unwrap();
    assert!(fixture.selection().progress().complete);
}

#[cfg(unix)]
#[test]
fn holds_name_exact_requests_and_clear_only_by_explicit_retry() {
    let fixture = Fixture::new(2);
    let selection = fixture.selection();
    let request = selection.next_request(false).unwrap().unwrap();
    assert!(selection.hold_reason(&request).unwrap().is_none());
    assert!(selection.plan_retry(&fixture.root).is_err());
    fixture.apply(
        plan_rejection(
            &fixture.root,
            &request,
            ScreeningRejection::ReferenceCoverage {
                missing: 1,
                duplicate: 0,
                unknown: 1,
                response_revision: "d".repeat(64),
            },
        )
        .unwrap(),
    );
    let failure = || crate::commands::work::state::InvocationFailure::Exited {
        engine: lk_core::execution::AgentEngine::Codex,
        code: 97,
        host: Some(crate::commands::work::state::HostEnd {
            profile: "codex".into(),
            at: jiff::Timestamp::now(),
        }),
    };
    fixture.apply(plan_failed_attempt(&fixture.root, &request, failure()).unwrap());
    assert!(plan_failed_attempt(&fixture.root, &request, failure()).is_err());
    let held = fixture.selection();
    assert!(
        held.hold_reason(&request)
            .unwrap()
            .unwrap()
            .contains("1 missing")
    );

    let other_contract = fixture.build(&"d".repeat(64), source(), LIMITS).unwrap();
    let reissued = other_contract.next_request(false).unwrap().unwrap();
    assert_ne!(reissued.request_revision, request.request_revision);
    assert!(
        other_contract
            .hold_reason(&reissued)
            .unwrap()
            .unwrap()
            .contains("select-retry --task-id retained-task")
    );
    std::thread::sleep(std::time::Duration::from_millis(2));
    crate::commands::work::host::record_answer(
        &fixture.root,
        lk_core::execution::AgentEngine::Codex,
        "codex",
        jiff::Timestamp::now(),
        &["codex"],
    )
    .unwrap();
    let answered = fixture.build(&"d".repeat(64), source(), LIMITS).unwrap();
    assert!(answered.hold_reason(&reissued).unwrap().is_none());
    fixture.apply(plan_failed_attempt(&fixture.root, &reissued, failure()).unwrap());
    assert!(
        fixture
            .build(&"d".repeat(64), source(), LIMITS)
            .unwrap()
            .hold_reason(&reissued)
            .unwrap()
            .unwrap()
            .contains("retried automatically"),
        "a changed request is a fresh judgment even on the same packet input"
    );
    std::thread::sleep(std::time::Duration::from_millis(2));
    crate::commands::work::host::record_answer(
        &fixture.root,
        lk_core::execution::AgentEngine::Codex,
        "codex",
        jiff::Timestamp::now(),
        &["codex"],
    )
    .unwrap();
    fixture.apply(plan_failed_attempt(&fixture.root, &reissued, failure()).unwrap());
    let exhausted = fixture.build(&"d".repeat(64), source(), LIMITS).unwrap();
    assert!(
        exhausted
            .hold_reason(&reissued)
            .unwrap()
            .unwrap()
            .contains("again after the host answered")
    );
    fixture.apply(exhausted.plan_retry(&fixture.root).unwrap());
    assert!(
        fixture
            .build(&"d".repeat(64), source(), LIMITS)
            .unwrap()
            .hold_reason(&reissued)
            .unwrap()
            .is_none()
    );
    assert!(fixture.selection().hold_reason(&request).unwrap().is_some());

    fixture.write(2, "", "has its own declared scope.");
    let grown = fixture.selection();
    let next = grown.next_request(false).unwrap().unwrap();
    assert!(grown.hold_reason(&next).unwrap().is_none());
    std::fs::remove_file(fixture.root.join(owner(2))).unwrap();

    fixture.record(&request, inspect_first);
    assert!(fixture.selection().progress().complete);
    assert!(
        plan_rejection(
            &fixture.root,
            &request,
            ScreeningRejection::InvalidReasons {
                blank: 1,
                oversized: 0,
                max_reason_json_bytes: 256,
                largest_oversized_json_bytes: 0,
                response_revision: "d".repeat(64),
            },
        )
        .is_err()
    );
}

#[test]
fn retention_removes_whole_stores_of_retired_tasks_and_keeps_pending_and_staged_ones() {
    let mut fixture = Fixture::new(2);
    let queue = fixture.root.join(".lorekeeper/queue");
    let task = lk_queue::QueueLlmClient::new(queue.clone()).build_task(
        "summary",
        lk_queue::TaskKind::Summarize,
        lk_queue::SummarizeRequest {
            text: "Retained source.".into(),
            max_sentences: 3,
            locale: lk_core::i18n::Locale::En,
            source_type: None,
            focus: None,
            target: lk_queue::TaskTarget {
                vault_path: "wiki/documents/candidate-0.md".into(),
                kind: lk_queue::TargetKind::DocumentSummary,
                anchor: "## Summary".into(),
            },
        },
    );
    fixture.task_id.clone_from(&task.task_id);
    fixture.cache_hash.clone_from(&task.cache_hash);
    let request = fixture.selection().next_request(false).unwrap().unwrap();
    fixture.record(&request, inspect_first);
    let selection = fixture.selection();
    fixture.seal(&selection);
    let store = fixture.root.join(&selection.directory);
    let prune = |pending: &BTreeSet<(String, String)>| {
        let mut plan = MutationPlan::default();
        let prepared = plan_prune(&fixture.root, pending, pending, &BTreeSet::new(), &mut plan)?;
        if prepared.removed != 0 {
            fixture.apply(plan);
        }
        cleanup_directories(&fixture.root, &prepared.directories)?;
        Ok::<_, miette::Report>(prepared.removed)
    };
    let ledger = std::fs::read(fixture.ledger()).unwrap();
    lk_queue::write_tasks_atomic(&queue.join("pending.jsonl"), std::slice::from_ref(&task))
        .unwrap();
    let pending = BTreeSet::from([(task.task_id.clone(), task.cache_hash.clone())]);
    assert_eq!(prune(&pending).unwrap(), 0);
    std::fs::remove_file(queue.join("pending.jsonl")).unwrap();
    std::fs::create_dir_all(queue.join("results")).unwrap();
    let staged = queue.join("results/pending.section.json");
    std::fs::write(
        &staged,
        encoded(&lk_queue::SectionResult {
            context_revision: None,
            task_id: task.task_id.clone(),
            cache_hash: task.cache_hash.clone(),
            target_revision: "captured".into(),
            body: "Staged interpretation.".into(),
        })
        .unwrap(),
    )
    .unwrap();
    assert_eq!(prune(&BTreeSet::new()).unwrap(), 0);
    assert_eq!(std::fs::read(fixture.ledger()).unwrap(), ledger);
    std::fs::remove_file(staged).unwrap();

    std::fs::write(store.join("unknown.json"), "{}").unwrap();
    assert!(prune(&BTreeSet::new()).is_err());
    std::fs::remove_file(store.join("unknown.json")).unwrap();
    assert_eq!(std::fs::read(fixture.ledger()).unwrap(), ledger);

    assert_eq!(prune(&BTreeSet::new()).unwrap(), 1);
    assert!(!store.exists());
    assert_eq!(prune(&BTreeSet::new()).unwrap(), 0);
}
