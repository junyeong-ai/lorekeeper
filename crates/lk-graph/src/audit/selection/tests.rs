use super::*;

fn retained_generations() -> (Fixture, StateReference, StateReference, StateReference) {
    let fixture = Fixture::new(1);
    fixture
        .record(output(
            &fixture.packet(20, false),
            ScreeningDecision::NotSelected,
        ))
        .unwrap();
    let first = state_reference(&fixture.candidate()).unwrap().unwrap();
    fixture.write(
        "wiki/documents/added.md",
        "---\ntype: document\ntitle: Added\n---\nScreen protocol has another scoped claim.\n",
    );
    fixture
        .record(output(
            &fixture.packet(20, false),
            ScreeningDecision::NotSelected,
        ))
        .unwrap();
    let second = state_reference(&fixture.candidate()).unwrap().unwrap();
    let path = "wiki/documents/candidate-000.md";
    let raw = std::fs::read_to_string(fixture.root.join(path)).unwrap();
    fixture.write(path, &(raw + "\nAn authored scope change.\n"));
    fixture
        .record(output(
            &fixture.packet(20, false),
            ScreeningDecision::NotSelected,
        ))
        .unwrap();
    let current = state_reference(&fixture.candidate()).unwrap().unwrap();
    (fixture, first, second, current)
}

#[test]
fn selection_retention_keeps_current_assessed_and_work_references() {
    let (fixture, first, second, current) = retained_generations();
    let cutoff = jiff::Timestamp::now().as_second() + 3600;
    let (_, mut receipts) = crate::audit::load(&fixture.root).unwrap();
    receipts
        .selection_assessments
        .insert(SUBJECT.into(), second.clone());
    fixture.write(
        crate::audit::STATE,
        &serde_json::to_string(&receipts).unwrap(),
    );
    let session = MutationSession::acquire(&fixture.root).unwrap();
    let protected = BTreeSet::from([first.clone()]);
    let report = prune(
        &fixture.root,
        Some(&session),
        &protected,
        MutationPlan::default(),
        cutoff,
    )
    .unwrap();
    assert_eq!((report.states, report.packets), (0, 0));
    assert!(fixture.root.join(first.path().unwrap()).exists());
    let before = fixture.snapshot();
    let dry = prune(
        &fixture.root,
        None,
        &BTreeSet::new(),
        MutationPlan::default(),
        cutoff,
    )
    .unwrap();
    assert_eq!((dry.states, dry.packets), (1, 0));
    assert_eq!(fixture.snapshot(), before);
    let report = prune(
        &fixture.root,
        Some(&session),
        &BTreeSet::new(),
        MutationPlan::default(),
        cutoff,
    )
    .unwrap();
    assert_eq!((report.states, report.packets), (1, 0));
    assert!(!fixture.root.join(first.path().unwrap()).exists());
    assert!(fixture.root.join(second.path().unwrap()).exists());
    let (_, mut receipts) = crate::audit::load(&fixture.root).unwrap();
    receipts.selection_assessments.clear();
    fixture.write(
        crate::audit::STATE,
        &serde_json::to_string(&receipts).unwrap(),
    );
    let report = prune(
        &fixture.root,
        Some(&session),
        &BTreeSet::new(),
        MutationPlan::default(),
        cutoff,
    )
    .unwrap();
    assert_eq!((report.states, report.packets), (1, 1));
    assert!(fixture.root.join(current.path().unwrap()).exists());
    let (_, mut receipts) = crate::audit::load(&fixture.root).unwrap();
    receipts.discovery.clear();
    fixture.write(
        crate::audit::STATE,
        &serde_json::to_string(&receipts).unwrap(),
    );
    let young = prune(
        &fixture.root,
        Some(&session),
        &BTreeSet::new(),
        MutationPlan::default(),
        jiff::Timestamp::now().as_second() - 3600,
    )
    .unwrap();
    assert_eq!((young.states, young.packets), (0, 0));
    let report = prune(
        &fixture.root,
        Some(&session),
        &BTreeSet::new(),
        MutationPlan::default(),
        cutoff,
    )
    .unwrap();
    assert_eq!((report.states, report.packets), (1, 2));
    assert!(!fixture.root.join(DIRECTORY).exists());
}

#[test]
fn unknown_or_missing_selection_references_refuse_before_retention_deletes() {
    for defect in [
        "missing-reference",
        "unknown-artifact",
        "changed-artifact",
        "pending-journal",
    ] {
        let (fixture, first, _, _) = retained_generations();
        let mut references = BTreeSet::new();
        match defect {
            "missing-reference" => {
                references.insert(StateReference {
                    subject: SUBJECT.into(),
                    revision: "0".repeat(64),
                });
            }
            "unknown-artifact" => {
                fixture.write(&format!("{DIRECTORY}/unknown.txt"), "Unknown owner.")
            }
            "changed-artifact" => fixture.write(first.path().unwrap().to_str().unwrap(), "{}"),
            "pending-journal" => fixture.write(".lorekeeper/mutation/pending.json", "{}"),
            _ => unreachable!(),
        }
        let before = fixture.snapshot();
        assert!(
            prune(
                &fixture.root,
                None,
                &references,
                MutationPlan::default(),
                jiff::Timestamp::now().as_second() + 3600
            )
            .is_err(),
            "{defect}"
        );
        assert_eq!(fixture.snapshot(), before);
    }
}

#[test]
fn retained_work_manifest_and_audit_root_guards_precede_retention_replay() {
    for changed in ["work", "audit"] {
        let (fixture, _, _, _) = retained_generations();
        fixture.write("work-reference.json", "Original retained manifest.");
        let mut guards = MutationPlan::default();
        guards.require_unchanged("work-reference.json", "Original retained manifest.".into());
        let planned = super::retention::plan_prune(
            &fixture.root,
            &BTreeSet::new(),
            guards,
            jiff::Timestamp::now().as_second() + 3600,
        )
        .unwrap();
        match changed {
            "work" => fixture.write("work-reference.json", "Externally changed manifest."),
            "audit" => {
                let (_, mut receipts) = crate::audit::load(&fixture.root).unwrap();
                receipts.discovery.clear();
                fixture.write(
                    crate::audit::STATE,
                    &serde_json::to_string(&receipts).unwrap(),
                );
            }
            _ => unreachable!(),
        }
        let artifacts = fixture
            .snapshot()
            .into_iter()
            .filter(|(path, _)| path.starts_with(DIRECTORY))
            .collect::<BTreeMap<_, _>>();
        let journal = serde_json::json!({"version":1,"plan":planned.mutation}).to_string();
        fixture.write(".lorekeeper/mutation/pending.json", &journal);
        assert!(
            MutationSession::acquire(&fixture.root).is_err(),
            "{changed}"
        );
        for (path, raw) in artifacts {
            assert_eq!(std::fs::read(fixture.root.join(path)).unwrap(), raw);
        }
    }
}

use crate::audit::{AuditAssessment, AuditDisposition, AuditResults, DiscoveryPlan, Receipts};

const SUBJECT: &str = "wiki/documents/subject.md";

struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    dirs: lk_core::config::VaultDirs,
}
impl Fixture {
    fn new(count: usize) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let fixture = Self {
            _directory: directory,
            root,
            dirs: Default::default(),
        };
        fixture.write(
            SUBJECT,
            "---\ntype: document\ntitle: Subject\n---\nAudit the deployment proposal.\n",
        );
        for index in 0..count {
            fixture.write(&format!("wiki/documents/candidate-{index:03}.md"),
                &format!("---\ntype: document\ntitle: Candidate {index}\n---\nScreen protocol {index} has a declared scope.\n"));
        }
        let raw = std::fs::read_to_string(fixture.root.join(SUBJECT)).unwrap();
        let mut receipts = Receipts::default();
        receipts.discovery.insert(
            SUBJECT.into(),
            DiscoveryPlan {
                subject_revision: lk_core::concept::source_revision(
                    &lk_vault::parse_page(&raw).unwrap(),
                ),
                queries: vec![DiscoveryQuery {
                    query: "screen protocol".into(),
                    phrase: false,
                    spacing: false,
                }],
                selection: Some(SelectionPlan::default()),
            },
        );
        fixture.write(
            crate::audit::STATE,
            &serde_json::to_string(&receipts).unwrap(),
        );
        fixture
    }
    fn write(&self, path: &str, raw: &str) {
        let full = self.root.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, raw).unwrap();
    }
    fn pages(&self) -> Vec<crate::scan::ScannedPage> {
        crate::scan::scan_vault(&self.root, true).unwrap()
    }
    fn limits(&self) -> ScreeningLimits {
        ScreeningLimits {
            max_input_bytes: 65536,
            max_result_bytes: 32768,
            max_candidates: 20,
        }
    }
    fn packet(&self, limit: usize, reassess: bool) -> CandidatePacket {
        let mut limits = self.limits();
        limits.max_candidates = limit;
        candidates(
            &self.root,
            &self.dirs,
            &self.pages(),
            SUBJECT,
            limits,
            reassess,
            &ReadControl::default(),
        )
        .unwrap()
    }
    fn candidate(&self) -> AuditCandidate {
        crate::audit::load_selected_inputs(&self.root, &self.dirs, &self.pages(), &[SUBJECT.into()])
            .unwrap()
            .remove(0)
    }
    fn record(&self, output: ScreeningOutput) -> Result<bool, GraphError> {
        record(
            &MutationSession::acquire(&self.root).unwrap(),
            &self.dirs,
            &self.pages(),
            output,
            self.limits(),
            &ReadControl::default(),
        )
    }
    fn snapshot(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        walkdir::WalkDir::new(&self.root)
            .into_iter()
            .map(Result::unwrap)
            .filter(|entry| entry.file_type().is_file())
            .map(|entry| {
                (
                    entry.path().strip_prefix(&self.root).unwrap().into(),
                    std::fs::read(entry.path()).unwrap(),
                )
            })
            .collect()
    }
}
fn output(packet: &CandidatePacket, decision: ScreeningDecision) -> ScreeningOutput {
    let request = packet.request.as_ref().expect("unrecorded packet");
    ScreeningOutput::Record {
        path: request.subject.clone(),
        purpose_revision: request.purpose_revision.clone(),
        packet_revision: packet.request_revision.clone().unwrap(),
        reassess: request.reassess,
        decisions: request
            .candidates
            .iter()
            .map(|card| CandidateDecision {
                candidate_key: card.candidate_key.clone(),
                decision,
                reason: "The assigned card has an independently assessed scope.".into(),
            })
            .collect(),
    }
}

#[test]
fn candidate_decisions_resume_without_repeating_unchanged_inputs() {
    let fixture = Fixture::new(6);
    let before = fixture.snapshot();
    let first = fixture.packet(2, false);
    assert_eq!(
        fixture.snapshot(),
        before,
        "read-only candidate inspection writes no sidecars"
    );
    assert_eq!(first.progress.unknown, 6);
    let decision = output(&first, ScreeningDecision::NotSelected);
    assert!(fixture.record(decision.clone()).unwrap());
    assert!(
        !fixture.record(decision).unwrap(),
        "an exact repeated submission acknowledges its existing receipt"
    );
    let prior = fixture
        .snapshot()
        .into_iter()
        .filter(|(path, _)| path.starts_with(DIRECTORY))
        .collect::<BTreeMap<_, _>>();
    fixture.write(
        "wiki/documents/candidate-001-new.md",
        "---\ntype: document\ntitle: Another candidate\n---\nScreen protocol has another scope.\n",
    );
    let next = fixture.packet(20, false);
    assert_eq!(next.progress.total, 7);
    assert_eq!(next.progress.not_selected, 2);
    assert_eq!(next.progress.unknown, 5);
    assert_eq!(next.request.as_ref().unwrap().candidates.len(), 5);
    assert!(
        fixture
            .record(output(&next, ScreeningDecision::Inspect))
            .unwrap()
    );
    let current = fixture.candidate();
    assert!(
        current
            .discovery
            .as_ref()
            .unwrap()
            .selection
            .as_ref()
            .unwrap()
            .is_ready()
    );
    assert_eq!(current.discovery.as_ref().unwrap().matches.len(), 7);
    assert_eq!(
        current.related_pages.len(),
        5,
        "unselected discovery bodies are outside the full read scope"
    );
    for (path, raw) in prior {
        assert_eq!(std::fs::read(fixture.root.join(path)).unwrap(), raw);
    }
    let path = "wiki/documents/candidate-003.md";
    let raw = std::fs::read_to_string(fixture.root.join(path)).unwrap();
    fixture.write(path, &(raw + "\nThe authored scope changed.\n"));
    let changed = fixture.packet(20, false);
    assert_eq!(changed.progress.unknown, 1);
    assert_eq!(changed.request.as_ref().unwrap().candidates.len(), 1);
}

#[test]
fn deferred_cards_do_not_repeat_or_hide_later_unknown_candidates() {
    let fixture = Fixture::new(3);
    let first = fixture.packet(1, false);
    fixture
        .record(output(&first, ScreeningDecision::Defer))
        .unwrap();
    let next = fixture.packet(20, false);
    assert_eq!(next.request.as_ref().unwrap().candidates.len(), 2);
    fixture
        .record(output(&next, ScreeningDecision::NotSelected))
        .unwrap();
    let held = fixture.packet(20, false);
    assert!(held.request.is_none());
    assert_eq!(held.progress.unknown, 0);
    assert_eq!(held.progress.deferred, 1);
    assert!(!held.progress.complete);
    let candidate = fixture.candidate();
    let before = std::fs::read(fixture.root.join(crate::audit::STATE)).unwrap();
    assert!(
        crate::audit::record(
            &MutationSession::acquire(&fixture.root).unwrap(),
            &fixture.dirs,
            &fixture.pages(),
            AuditResults {
                assessments: vec![AuditAssessment {
                    path: SUBJECT.into(),
                    file_revision: candidate.file_revision,
                    input_revision: candidate.input_revision,
                    disposition: AuditDisposition::Reviewed,
                    reason: "A deferred candidate remains unresolved.".into(),
                    reviews: vec![]
                }]
            },
            u64::MAX
        )
        .is_err()
    );
    assert_eq!(
        std::fs::read(fixture.root.join(crate::audit::STATE)).unwrap(),
        before
    );
    let explicit = fixture.packet(20, true);
    assert_eq!(explicit.request.as_ref().unwrap().candidates.len(), 1);
    fixture
        .record(output(&explicit, ScreeningDecision::Inspect))
        .unwrap();
    assert!(fixture.packet(20, false).progress.is_ready());
}

#[test]
fn candidate_recording_rejects_partial_duplicate_unknown_and_stale_packets() {
    for defect in [
        "partial",
        "duplicate",
        "unknown",
        "escaped-reason",
        "changed-page",
    ] {
        let fixture = Fixture::new(2);
        let packet = fixture.packet(20, false);
        let mut response = output(&packet, ScreeningDecision::NotSelected);
        if let ScreeningOutput::Record { decisions, .. } = &mut response {
            match defect {
                "partial" => {
                    decisions.pop();
                }
                "duplicate" => decisions.push(decisions[0].clone()),
                "unknown" => decisions[0].candidate_key = "0".repeat(64),
                "escaped-reason" => decisions[0].reason = "\u{1}".repeat(60),
                "changed-page" => {
                    let path = &packet.request.as_ref().unwrap().candidates[0]
                        .candidate
                        .path;
                    let raw = std::fs::read_to_string(fixture.root.join(path)).unwrap();
                    fixture.write(path, &(raw + "\nA changed claim.\n"));
                }
                _ => unreachable!(),
            }
        }
        let before = fixture.snapshot();
        assert!(fixture.record(response).is_err(), "{defect}");
        for (path, raw) in before {
            assert_eq!(std::fs::read(fixture.root.join(path)).unwrap(), raw);
        }
        assert!(!fixture.root.join(DIRECTORY).exists());
    }
}

#[test]
fn rejected_candidate_bytes_remain_publication_guards() {
    for recover in [false, true] {
        let fixture = Fixture::new(1);
        fixture
            .record(output(
                &fixture.packet(20, false),
                ScreeningDecision::NotSelected,
            ))
            .unwrap();
        let candidate = fixture.candidate();
        assert!(candidate.related_pages.is_empty());
        let mut mutation = MutationPlan::default();
        guard_ready(
            &fixture.root,
            &fixture.dirs,
            &candidate,
            &mut mutation,
            &ReadControl::default(),
        )
        .unwrap();
        mutation.create("sentinel.md", "This publication must refuse.".into());
        fixture.write(
            "wiki/documents/candidate-000.md",
            "---\ntype: document\n---\nAn externally changed page.\n",
        );
        if recover {
            let pending = serde_json::json!({"version":1,"plan":mutation}).to_string();
            fixture.write(".lorekeeper/mutation/pending.json", &pending);
            assert!(MutationSession::acquire(&fixture.root).is_err());
        } else {
            assert!(
                MutationSession::acquire(&fixture.root)
                    .unwrap()
                    .apply(mutation)
                    .is_err()
            );
        }
        assert!(!fixture.root.join("sentinel.md").exists());
    }
}

#[test]
fn both_packet_bounds_keep_an_available_fitting_prefix() {
    let fixture = Fixture::new(2);
    let one = fixture.packet(1, false);
    let input = serde_json::to_vec(one.request.as_ref().unwrap())
        .unwrap()
        .len();
    let mut response = output(&one, ScreeningDecision::NotSelected);
    if let ScreeningOutput::Record { decisions, .. } = &mut response {
        decisions[0].reason = "x".into();
    }
    let result = serde_json::to_vec(&response).unwrap().len();
    for boundary in [CapacityBoundary::Input, CapacityBoundary::Result] {
        let mut limits = fixture.limits();
        match boundary {
            CapacityBoundary::Input => limits.max_input_bytes = input,
            CapacityBoundary::Result => limits.max_result_bytes = result,
        }
        let packet = candidates(
            &fixture.root,
            &fixture.dirs,
            &fixture.pages(),
            SUBJECT,
            limits,
            false,
            &ReadControl::default(),
        )
        .unwrap();
        assert_eq!(packet.request.as_ref().unwrap().candidates.len(), 1);
        match boundary {
            CapacityBoundary::Input => limits.max_input_bytes -= 1,
            CapacityBoundary::Result => limits.max_result_bytes -= 1,
        }
        assert!(
            matches!(candidates(&fixture.root, &fixture.dirs, &fixture.pages(), SUBJECT, limits, false, &ReadControl::default()),
            Err(GraphError::AuditScreeningCapacity { boundary: actual, .. }) if actual == boundary)
        );
    }
}

#[test]
fn changed_effective_query_spellings_reopen_only_affected_candidate_inputs() {
    let fixture = Fixture::new(2);
    let target = "wiki/documents/candidate-000.md";
    fixture.write(target, "---\ntype: document\ntitle: Candidate\n---\nOpening statement.\n\nOrbit has an earlier scope.\n\nHelios has a later scope.\n");
    fixture.write(
        "wiki/concepts/helios.md",
        "---\ntype: concept\ntitle: Helios\naliases: []\n---\nA declared identity.\n",
    );
    let (before, mut receipts) = crate::audit::load(&fixture.root).unwrap();
    let discovery = receipts.discovery.get_mut(SUBJECT).unwrap();
    discovery.queries.push(DiscoveryQuery {
        query: "Helios".into(),
        phrase: false,
        spacing: false,
    });
    discovery.queries.sort();
    fixture.write(
        crate::audit::STATE,
        &serde_json::to_string(&receipts).unwrap(),
    );
    assert!(before.is_some());
    let original = fixture.packet(20, false);
    let original_card = original
        .request
        .as_ref()
        .unwrap()
        .candidates
        .iter()
        .find(|card| card.candidate.path == target)
        .unwrap();
    let key = original_card.candidate_key.clone();
    let preview = serde_json::to_string(&original_card.candidate.card.matches).unwrap();
    fixture
        .record(output(&original, ScreeningDecision::NotSelected))
        .unwrap();
    fixture.write(
        "wiki/concepts/helios.md",
        "---\ntype: concept\ntitle: Helios\naliases: [Orbit]\n---\nA declared identity.\n",
    );
    let changed = fixture.packet(20, false);
    let changed_card = changed
        .request
        .as_ref()
        .unwrap()
        .candidates
        .iter()
        .find(|card| card.candidate.path == target)
        .unwrap();
    assert_ne!(changed_card.candidate_key, key);
    assert_ne!(
        serde_json::to_string(&changed_card.candidate.card.matches).unwrap(),
        preview
    );
    assert_eq!(
        changed.progress.not_selected, 1,
        "the separate unchanged query keeps its decision"
    );
}

#[cfg(unix)]
#[test]
fn subject_aliases_exclude_their_owner_but_keep_distinct_hard_links() {
    let fixture = Fixture::new(1);
    fixture.write(
        SUBJECT,
        "---\ntype: document\ntitle: Subject\n---\nScreen protocol subject.\n",
    );
    let (_, mut receipts) = crate::audit::load(&fixture.root).unwrap();
    receipts
        .discovery
        .get_mut(SUBJECT)
        .unwrap()
        .subject_revision = lk_core::concept::source_revision(
        &lk_vault::parse_page(&std::fs::read_to_string(fixture.root.join(SUBJECT)).unwrap())
            .unwrap(),
    );
    fixture.write(
        crate::audit::STATE,
        &serde_json::to_string(&receipts).unwrap(),
    );
    let owner = fixture.root.join("wiki/documents/z-owner.md");
    std::fs::rename(fixture.root.join(SUBJECT), &owner).unwrap();
    std::os::unix::fs::symlink("z-owner.md", fixture.root.join(SUBJECT)).unwrap();
    assert_eq!(fixture.packet(20, false).progress.total, 1);
    std::fs::hard_link(owner, fixture.root.join("wiki/documents/hard.md")).unwrap();
    let packet = fixture.packet(20, false);
    assert_eq!(packet.progress.total, 2);
    fixture
        .record(output(&packet, ScreeningDecision::Inspect))
        .unwrap();
    assert_eq!(fixture.candidate().related_pages.len(), 2);
}
