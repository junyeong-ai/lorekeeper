//! Semantic dependencies are explicit selections; unrelated daily interpretation is not input.
use lk_core::{
    concept::{Citation, citation_digest, file_revision},
    rss_evidence::RssEventSelection,
};
use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

struct Fixture {
    tmp: tempfile::TempDir,
    config: PathBuf,
    capture: lk_core::rss_original::RssOriginalCapture,
    origin: PathBuf,
}

impl Fixture {
    fn build() -> Self {
        let fixture = Self::pending();
        fixture.ok(&["queue", "apply"]);
        fixture
    }
    fn pending() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let config = root.join("config.yaml");
        std::fs::write(&config, format!("vault:\n  root: {}\n  locale: en\nidentity:\n  name: Test\n  email: test@example.com\nsources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: inbox\n", root.display())).unwrap();
        let events = (1..=2).map(|n| serde_json::from_value::<lk_core::event::Event>(serde_json::json!({"id":format!("feed:2026-10-18:{n:016x}"),"source_id":"feed","source_type":"rss","timestamp":"2026-10-18T00:00:00Z","date":"2026-10-18","title":format!("Article {n}"),"body":format!("Independent mechanism {n}; historical observation, not a current guarantee."),"url":format!("https://example.com/{n}"),"author":null,"labels":[],"category":null,"performance_category":null,"is_self":false,"is_personal":false,"metadata":{}})).unwrap()).collect::<Vec<_>>();
        let capture = lk_core::rss_original::RssOriginalCapture::from_events(
            &events.iter().collect::<Vec<_>>(),
        )
        .unwrap();
        let session = lk_vault::MutationSession::acquire(root).unwrap();
        let reference = lk_vault::publish_rss_original(&session, &capture).unwrap();
        drop(session);
        let origin = PathBuf::from("daily/feed/2026-10-18.md");
        std::fs::create_dir_all(root.join(origin.parent().unwrap())).unwrap();
        let request = lk_queue::ExtractConceptsRequest {
            concept_policy: lk_core::config::ConceptPolicy::SupportedNew,
            rss_original: Some(capture.manifest.clone()),
            text: "Two independent original articles".into(),
            source_id: "feed".into(),
            source_type: lk_core::config::SourceType::Rss,
            date: capture.manifest.date,
            focus: None,
            locale: lk_core::i18n::Locale::En,
            categories: vec![],
            target: lk_queue::TaskTarget {
                vault_path: origin.to_string_lossy().into(),
                kind: lk_queue::TargetKind::DailyConcepts,
                anchor: "## Related Concepts".into(),
            },
        };
        let client = lk_queue::QueueLlmClient::new(root.join(".lorekeeper/queue"));
        let task = client.build_task("ext", lk_queue::TaskKind::ExtractConcepts, request);
        assert_eq!(
            task.generation_policy().unwrap(),
            Some(lk_core::rss_evidence::EXTRACTION_POLICY)
        );
        task.validate_retained_input().unwrap();
        let raw = format!(
            "---\ntype: daily\nsource: feed\ncreated: 2026-10-18\nsource_record: {}\nllm_inputs:\n  concepts: {}\n---\n# Feed\n\n## Summary\n\nUnrelated interpretation.\n\n## Key Events\n\nTwo articles.\n\n## Related Concepts\n\n",
            serde_json::json!({"rss_original":reference}),
            task.cache_hash
        );
        std::fs::write(root.join(&origin), &raw).unwrap();
        let queue = root.join(".lorekeeper/queue");
        std::fs::create_dir_all(queue.join("results")).unwrap();
        std::fs::write(
            queue.join("fixture.jsonl"),
            serde_json::to_string(&task).unwrap() + "\n",
        )
        .unwrap();
        let discovery = vec![lk_core::discovery::DiscoveryQuery {
            query: "Independent mechanism".into(),
            phrase: true,
            spacing: false,
        }];
        let discovery_file = root.join("discovery-request.json");
        std::fs::write(
            &discovery_file,
            serde_json::to_vec(&serde_json::json!({
                "target_revision":file_revision(&raw), "queries":discovery,
            }))
            .unwrap(),
        )
        .unwrap();
        let cli = |args: &[&str]| {
            let mut command = Command::new(env!("CARGO_BIN_EXE_lore"));
            for (key, _) in std::env::vars() {
                if key.starts_with("LORE_") {
                    command.env_remove(key);
                }
            }
            let output = command
                .arg("--config")
                .arg(&config)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
        };
        cli(&[
            "work",
            "discover",
            "--task-id",
            &task.task_id,
            "--file",
            discovery_file.to_str().unwrap(),
        ]);
        let context = cli(&["work", "context", "--task-id", &task.task_id]);
        let result = lk_queue::TaskResult {
            context_revision: context["context_revision"].as_str().unwrap().into(),
            discovery,
            concept_policy: lk_core::config::ConceptPolicy::SupportedNew,
            task_id: task.task_id.clone(),
            cache_hash: task.cache_hash.clone(),
            target_revision: file_revision(&raw),
            target: task.target.clone(),
            date: capture.manifest.date,
            concepts: ["Alpha", "Beta"]
                .iter()
                .zip(&capture.manifest.events)
                .map(|(name, event)| lk_queue::ReportedConcept {
                    admission: lk_queue::ConceptAdmission::New,
                    concept: lk_core::concept::ExtractedConcept {
                        name: (*name).into(),
                        ..Default::default()
                    },
                    synthesis: Some(format!("{name} is supported by its selected original.")),
                    rss_evidence: vec![RssEventSelection {
                        event_id: event.event_id.clone(),
                        revision: event.revision.clone(),
                    }],
                })
                .collect(),
        };
        std::fs::write(
            queue.join("results").join(format!("{}.json", task.task_id)),
            serde_json::to_vec(&result).unwrap(),
        )
        .unwrap();
        Self {
            tmp,
            config,
            capture,
            origin,
        }
    }
    fn root(&self) -> &Path {
        self.tmp.path()
    }
    fn run(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_lore"));
        for (key, _) in std::env::vars() {
            if key.starts_with("LORE_") {
                cmd.env_remove(key);
            }
        }
        cmd.arg("--config")
            .arg(&self.config)
            .args(args)
            .output()
            .unwrap()
    }
    fn ok(&self, args: &[&str]) -> Output {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
    fn raw(&self) -> String {
        std::fs::read_to_string(self.root().join(&self.origin)).unwrap()
    }
    fn citations(&self, name: &str) -> Vec<Citation> {
        let dirs = lk_core::config::VaultDirs::default();
        let pages = lk_graph::scan::scan_vault(self.root(), false).unwrap();
        lk_graph::backlinks::concept_resynthesis(
            &pages,
            self.root(),
            &dirs,
            Path::new(&format!("wiki/concepts/{name}.md")),
        )
        .unwrap()
        .citations
    }
    fn settle(&self) {
        let dirs = lk_core::config::VaultDirs::default();
        let pages = lk_graph::scan::scan_vault(self.root(), false).unwrap();
        lk_graph::backlinks::sync_concept_backlinks(
            &pages,
            self.root(),
            false,
            &dirs,
            lk_graph::backlinks::SynthesisPolicy::Record,
        )
        .unwrap();
        for name in ["alpha", "beta"] {
            let hash = citation_digest(&self.citations(name));
            let path = self.root().join(format!("wiki/concepts/{name}.md"));
            let raw = std::fs::read_to_string(&path).unwrap();
            std::fs::write(
                path,
                lk_vault::set_llm_input(&raw, "synthesis_done", &format!("\"{hash}\"")).unwrap(),
            )
            .unwrap();
        }
    }
}

#[test]
fn bound_rss_concepts_require_article_selection_but_empty_extraction_is_valid() {
    for empty in [false, true] {
        let fixture = Fixture::pending();
        let before = fixture.raw();
        let results = fixture.root().join(".lorekeeper/queue/results");
        let path = std::fs::read_dir(&results)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let mut result: lk_queue::TaskResult =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        if empty {
            result.concepts.clear();
        } else {
            result.concepts[0].rss_evidence.clear();
        }
        std::fs::write(&path, serde_json::to_vec(&result).unwrap()).unwrap();
        let output = fixture.run(&["queue", "apply"]);
        assert_eq!(
            output.status.success(),
            empty,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!fixture.root().join("wiki/concepts").exists());
        if !empty {
            assert_eq!(fixture.raw(), before);
        }
    }
}

#[test]
fn two_articles_invalidate_only_the_explicitly_changed_selection() {
    let f = Fixture::build();
    f.settle();
    let before = [
        citation_digest(&f.citations("alpha")),
        citation_digest(&f.citations("beta")),
    ];
    let raw = f.raw().replace(
        "Unrelated interpretation.",
        "Unrelated article summary corrected; [page](../../wiki/concepts/beta.md#unrelated). ",
    );
    // A prose link to Beta conservatively adds page evidence, while Alpha is unaffected.
    std::fs::write(f.root().join(&f.origin), raw).unwrap();
    assert_eq!(before[0], citation_digest(&f.citations("alpha")));
    assert_ne!(before[1], citation_digest(&f.citations("beta")));
    std::fs::write(
        f.root().join(&f.origin),
        f.raw().replace(
            "Unrelated article summary corrected; [page](../../wiki/concepts/beta.md#unrelated). ",
            "Other article summary corrected; an external [link](https://example.com/changed). ",
        ),
    )
    .unwrap();
    let pages = lk_graph::scan::scan_vault(f.root(), false).unwrap();
    let report = lk_graph::backlinks::sync_concept_backlinks(
        &pages,
        f.root(),
        true,
        &Default::default(),
        lk_graph::backlinks::SynthesisPolicy::Record,
    )
    .unwrap();
    assert!(
        report.resynthesize.is_empty(),
        "unrelated interpretation/link edits owe zero rewrites"
    );
    let mut updated = f.capture.clone();
    let mut payload: lk_core::rss_original::RssEventPayload =
        serde_json::from_slice(&updated.payloads[0]).unwrap();
    payload.body = "Corrected original mechanism Alpha".into();
    updated.payloads[0] = lk_core::rss_original::canonical_bytes(&payload).unwrap();
    updated.manifest.events[0].revision = lk_core::rss_original::digest(&updated.payloads[0]);
    updated.manifest.events[0].bytes = updated.payloads[0].len();
    let session = lk_vault::MutationSession::acquire(f.root()).unwrap();
    let reference = lk_vault::publish_rss_original(&session, &updated).unwrap();
    drop(session);
    let raw = lk_vault::set_frontmatter_field(
        &f.raw(),
        "source_record",
        &serde_json::json!({"rss_original":reference}).to_string(),
    )
    .unwrap();
    std::fs::write(f.root().join(&f.origin), &raw).unwrap();
    assert_eq!(
        before[0],
        citation_digest(&f.citations("alpha")),
        "new manifest alone does not reinterpret the declared historical pair"
    );
    let event = &f.capture.manifest.events[0];
    f.ok(&[
        "wiki",
        "original",
        f.origin.to_str().unwrap(),
        "--evidence-concept",
        "wiki/concepts/alpha.md",
        "--event",
        &event.event_id,
        "--revision",
        &event.revision,
        "--text-limit",
        "8",
    ]);
    let selection = RssEventSelection {
        event_id: updated.manifest.events[0].event_id.clone(),
        revision: updated.manifest.events[0].revision.clone(),
    };
    let raw = lk_vault::accumulate_rss_evidence(
        f.root(),
        &f.origin,
        &raw,
        Some(&updated.manifest),
        &[("wiki/concepts/alpha.md".into(), vec![selection])],
    )
    .unwrap();
    std::fs::write(f.root().join(&f.origin), raw).unwrap();
    assert_eq!(
        f.citations("alpha").len(),
        2,
        "re-extraction accumulates, not implicitly replaces"
    );
    let revision = file_revision(&f.raw());
    f.ok(&[
        "graph",
        "retract-citation",
        f.origin.to_str().unwrap(),
        "wiki/concepts/alpha.md",
        "--revision",
        &revision,
        "--event",
        &event.event_id,
        "--event-revision",
        &event.revision,
    ]);
    assert_eq!(f.citations("alpha").len(), 1);
    assert_eq!(before[1], citation_digest(&f.citations("beta")));
    let pages = lk_graph::scan::scan_vault(f.root(), false).unwrap();
    let report = lk_graph::backlinks::sync_concept_backlinks(
        &pages,
        f.root(),
        true,
        &Default::default(),
        lk_graph::backlinks::SynthesisPolicy::Record,
    )
    .unwrap();
    assert_eq!(report.resynthesize.len(), 1);
    assert_eq!(
        report.resynthesize[0].path,
        Path::new("wiki/concepts/alpha.md")
    );
}

#[test]
fn final_pair_retraction_and_merge_keep_links_and_typed_declarations_consistent() {
    let f = Fixture::build();
    let event = &f.capture.manifest.events[1];
    let before = f.raw();
    let revision = file_revision(&before);
    f.ok(&[
        "graph",
        "retract-citation",
        f.origin.to_str().unwrap(),
        "wiki/concepts/beta.md",
        "--revision",
        &revision,
        "--event",
        &event.event_id,
        "--event-revision",
        &event.revision,
        "--dry-run",
    ]);
    assert_eq!(f.raw(), before);
    f.ok(&[
        "graph",
        "retract-citation",
        f.origin.to_str().unwrap(),
        "wiki/concepts/beta.md",
        "--revision",
        &revision,
        "--event",
        &event.event_id,
        "--event-revision",
        &event.revision,
    ]);
    assert!(!f.raw().contains("[Beta]"));
    assert!(
        lk_core::rss_evidence::bindings(&lk_vault::parse_page(&f.raw()).unwrap())
            .unwrap()
            .iter()
            .all(|b| b.concept != "wiki/concepts/beta.md")
    );
    let f = Fixture::build();
    f.ok(&["graph", "merge", "alpha", "beta", "--force"]);
    assert!(!f.root().join("wiki/concepts/alpha.md").exists());
    let citations = f.citations("beta");
    assert_eq!(citations.len(), 2);
    assert!(citations.iter().all(|c| c.rss_event.is_some()));
    let pages = lk_graph::scan::scan_vault(f.root(), false).unwrap();
    lk_graph::backlinks::sync_concept_backlinks(
        &pages,
        f.root(),
        false,
        &Default::default(),
        lk_graph::backlinks::SynthesisPolicy::Record,
    )
    .unwrap();
    assert_eq!(
        lk_vault::parse_page(
            &std::fs::read_to_string(f.root().join("wiki/concepts/beta.md")).unwrap()
        )
        .unwrap()
        .frontmatter
        .source_count(),
        Some(1)
    );
}

#[test]
fn packet_and_sqlite_links_expose_selected_evidence_and_corruption_is_not_empty_success() {
    let f = Fixture::build();
    f.ok(&["graph", "resynthesize", "wiki/concepts/alpha.md"]);
    let status: serde_json::Value =
        serde_json::from_slice(&f.ok(&["queue", "status", "--json"]).stdout).unwrap();
    let task = status["data"]["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["kind"] == "synthesize-concept" && t["status"] == "current")
        .unwrap();
    let packet: serde_json::Value = serde_json::from_slice(
        &f.ok(&[
            "queue",
            "packet",
            "--task-id",
            task["task_id"].as_str().unwrap(),
        ])
        .stdout,
    )
    .unwrap();
    assert_eq!(packet["version"], 2);
    assert_eq!(packet["event_evidence"].as_array().unwrap().len(), 1);
    let argv = packet["event_evidence"][0]["selected_event_command"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect::<Vec<_>>();
    f.ok(&argv[1..]);
    let snapshot: serde_json::Value =
        serde_json::from_slice(&f.ok(&["wiki", "prepare"]).stdout).unwrap();
    let links: serde_json::Value = serde_json::from_slice(
        &f.ok(&[
            "wiki",
            "links",
            "wiki/concepts/alpha.md",
            "--direction",
            "incoming",
            "--snapshot",
            snapshot["snapshot"].as_str().unwrap(),
        ])
        .stdout,
    )
    .unwrap();
    assert_eq!(links["links"][0]["rss_evidence"]["total"], 1);
    let object = f.root().join(format!(
        "{}/events/{}.json",
        lk_core::rss_original::RSS_ORIGINAL_STORE,
        f.capture.manifest.events[0].revision
    ));
    std::fs::write(object, "corrupt").unwrap();
    let status: serde_json::Value =
        serde_json::from_slice(&f.ok(&["queue", "status", "--json"]).stdout).unwrap();
    assert!(
        status["data"]["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["task_id"] == task["task_id"] && t["status"] == "unreadable")
    );
    assert!(
        !f.run(&[
            "queue",
            "packet",
            "--task-id",
            task["task_id"].as_str().unwrap()
        ])
        .status
        .success()
    );
}

#[test]
fn explicit_bindings_refuse_scope_changes_and_legacy_or_unprovided_selections() {
    let f = Fixture::build();
    let raw = f.raw();
    for changed in [
        raw.replace("type: daily", "type: document"),
        raw.replace("source: feed", "source: other"),
        raw.replace("created: 2026-10-18", "created: 2026-10-19"),
        raw.replace("- [Alpha](../../wiki/concepts/alpha.md)", ""),
        lk_vault::set_frontmatter_field(&raw, "source_record", "{}").unwrap(),
    ] {
        assert!(lk_vault::rss_concept_citations(f.root(), &f.origin, &changed).is_err());
    }
    let bad = RssEventSelection {
        event_id: f.capture.manifest.events[0].event_id.clone(),
        revision: "f".repeat(64),
    };
    assert!(
        lk_vault::accumulate_rss_evidence(
            f.root(),
            &f.origin,
            &raw,
            Some(&f.capture.manifest),
            &[("wiki/concepts/alpha.md".into(), vec![bad])]
        )
        .is_err()
    );
    let selection = RssEventSelection {
        event_id: f.capture.manifest.events[0].event_id.clone(),
        revision: f.capture.manifest.events[0].revision.clone(),
    };
    let legacy = "---\ntype: daily\nsource: feed\ncreated: 2026-10-18\n---\n## Related Concepts\n\n- [Alpha](../../wiki/concepts/alpha.md)\n";
    assert!(
        lk_vault::accumulate_rss_evidence(
            f.root(),
            &f.origin,
            legacy,
            Some(&f.capture.manifest),
            &[("wiki/concepts/alpha.md".into(), vec![selection])]
        )
        .is_err()
    );
    assert_eq!(f.raw(), raw);
    assert_eq!(
        lk_vault::accumulate_rss_evidence(f.root(), &f.origin, &raw, None, &[]).unwrap(),
        raw,
        "omission preserves carried declarations exactly"
    );
}

#[test]
fn selected_inventory_and_snapshot_link_projection_stay_bounded() {
    let f = Fixture::build();
    let mut capture = f.capture.clone();
    let template: lk_core::rss_original::RssEventPayload =
        serde_json::from_slice(&capture.payloads[0]).unwrap();
    for n in 3..=27 {
        let mut payload = template.clone();
        payload.event_id = format!("feed:2026-10-18:{n:016x}");
        payload.title = format!("Additional {n}");
        let bytes = lk_core::rss_original::canonical_bytes(&payload).unwrap();
        capture
            .manifest
            .events
            .push(lk_core::rss_original::RssEventEntry {
                event_id: payload.event_id,
                revision: lk_core::rss_original::digest(&bytes),
                bytes: bytes.len(),
                event_timestamp: payload.event_timestamp,
                title: payload.title,
                url: payload.url,
            });
        capture.payloads.push(bytes);
    }
    let session = lk_vault::MutationSession::acquire(f.root()).unwrap();
    let reference = lk_vault::publish_rss_original(&session, &capture).unwrap();
    drop(session);
    let raw = lk_vault::set_frontmatter_field(
        &f.raw(),
        "source_record",
        &serde_json::json!({"rss_original":reference}).to_string(),
    )
    .unwrap();
    let selections = capture
        .manifest
        .events
        .iter()
        .skip(2)
        .map(|e| RssEventSelection {
            event_id: e.event_id.clone(),
            revision: e.revision.clone(),
        })
        .collect();
    let raw = lk_vault::accumulate_rss_evidence(
        f.root(),
        &f.origin,
        &raw,
        Some(&capture.manifest),
        &[("wiki/concepts/alpha.md".into(), selections)],
    )
    .unwrap();
    std::fs::write(f.root().join(&f.origin), &raw).unwrap();
    let first = lk_vault::inspect_rss_evidence_inventory(
        f.root(),
        &f.origin,
        Path::new("wiki/concepts/alpha.md"),
        None,
        0,
        5,
    )
    .unwrap();
    assert_eq!(first.total, 26);
    assert_eq!(first.events.len(), 5);
    assert_eq!(first.next_offset, Some(5));
    std::fs::write(
        f.root().join(&f.origin),
        raw.replace("Unrelated interpretation", "Other interpretation"),
    )
    .unwrap();
    let second = lk_vault::inspect_rss_evidence_inventory(
        f.root(),
        &f.origin,
        Path::new("wiki/concepts/alpha.md"),
        Some(&first.revision),
        5,
        5,
    )
    .unwrap();
    assert_eq!(second.revision, first.revision);
    assert_eq!(second.events.len(), 5);
    let snapshot: serde_json::Value =
        serde_json::from_slice(&f.ok(&["wiki", "prepare"]).stdout).unwrap();
    let links: serde_json::Value = serde_json::from_slice(
        &f.ok(&[
            "wiki",
            "links",
            "wiki/concepts/alpha.md",
            "--direction",
            "incoming",
            "--snapshot",
            snapshot["snapshot"].as_str().unwrap(),
        ])
        .stdout,
    )
    .unwrap();
    assert_eq!(links["links"][0]["rss_evidence"]["total"], 26);
    assert_eq!(
        links["links"][0]["rss_evidence"]["events"]
            .as_array()
            .unwrap()
            .len(),
        20
    );
    assert_eq!(links["links"][0]["rss_evidence"]["truncated"], true);
}

#[cfg(unix)]
#[test]
fn filesystem_alias_evidence_is_shared_by_backlinks_original_and_retraction() {
    let f = Fixture::build();
    std::fs::create_dir_all(f.root().join("aliases")).unwrap();
    std::os::unix::fs::symlink(
        "../wiki/concepts/alpha.md",
        f.root().join("aliases/alpha.md"),
    )
    .unwrap();
    let raw = f.raw();
    let page = lk_vault::parse_page(&raw).unwrap();
    let mut declarations = lk_core::rss_evidence::bindings(&page).unwrap();
    let canonical = declarations
        .iter()
        .find(|binding| binding.concept == "wiki/concepts/alpha.md")
        .unwrap()
        .clone();
    for binding in &mut declarations {
        if binding.concept == "wiki/concepts/alpha.md" {
            binding.concept = "aliases/alpha.md".into();
        }
    }
    declarations.push(canonical);
    let raw = lk_vault::write_bindings(&raw, &declarations).unwrap();
    std::fs::write(f.root().join(&f.origin), &raw).unwrap();
    let citations = f.citations("alpha");
    assert_eq!(citations.len(), 1);
    assert!(citations[0].rss_event.is_some());
    assert!(
        lk_vault::has_rss_citation(f.root(), Path::new("wiki/concepts/alpha.md"), &citations[0])
            .unwrap()
    );
    let previews = lk_vault::rss_link_previews(f.root(), &f.origin, &raw).unwrap();
    assert_eq!(previews.values().map(|p| p.total).sum::<usize>(), 2);
    let inventory: serde_json::Value = serde_json::from_slice(
        &f.ok(&[
            "wiki",
            "original",
            f.origin.to_str().unwrap(),
            "--evidence-concept",
            "wiki/concepts/alpha.md",
        ])
        .stdout,
    )
    .unwrap();
    assert_eq!(inventory["total"], 1);
    let selected = &citations[0];
    f.ok(&[
        "graph",
        "retract-citation",
        "daily/feed/2026-10-18.md",
        "wiki/concepts/alpha.md",
        "--revision",
        &file_revision(&raw),
        "--event",
        &selected.rss_event.as_ref().unwrap().event_id,
        "--event-revision",
        &selected.revision,
    ]);
    assert!(!f.raw().contains("aliases/alpha.md"));
}

#[test]
fn normalization_rewrites_explicit_evidence_and_generated_link_in_one_plan() {
    let f = Fixture::build();
    let old = "wiki/concepts/alpha.md";
    let renamed = "wiki/concepts/Alpha_Name.md";
    std::fs::rename(f.root().join(old), f.root().join(renamed)).unwrap();
    std::fs::write(f.root().join(&f.origin), f.raw().replace(old, renamed)).unwrap();
    f.ok(&["graph", "normalize", "--fix"]);
    assert!(f.root().join("wiki/concepts/alpha-name.md").is_file());
    assert!(!f.raw().contains("Alpha_Name"));
    let citations = f.citations("alpha-name");
    assert_eq!(citations.len(), 1);
    assert!(citations[0].rss_event.is_some());
}

#[test]
fn authored_prose_links_never_receive_generated_event_preview() {
    let f = Fixture::build();
    let raw = format!(
        "{}\nAuthored explanation [Alpha](../../wiki/concepts/alpha.md).\n",
        f.raw()
    );
    std::fs::write(f.root().join(&f.origin), raw).unwrap();
    assert_eq!(
        f.citations("alpha").len(),
        2,
        "mixed authorship keeps conservative page evidence"
    );
    let snapshot: serde_json::Value =
        serde_json::from_slice(&f.ok(&["wiki", "prepare"]).stdout).unwrap();
    for argv in [
        vec![
            "wiki",
            "links",
            "daily/feed/2026-10-18.md",
            "--target",
            "wiki/concepts/alpha.md",
        ],
        vec![
            "wiki",
            "links",
            "wiki/concepts/alpha.md",
            "--direction",
            "incoming",
            "--snapshot",
            snapshot["snapshot"].as_str().unwrap(),
        ],
    ] {
        let links: serde_json::Value = serde_json::from_slice(&f.ok(&argv).stdout).unwrap();
        let rows = links["links"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows.iter()
                .filter(|r| r.get("rss_evidence").is_some())
                .count(),
            1
        );
        let prose = rows
            .iter()
            .find(|r| {
                r["context"]
                    .as_str()
                    .unwrap()
                    .contains("Authored explanation")
            })
            .unwrap();
        assert!(prose.get("rss_evidence").is_none());
    }
}

#[test]
fn selected_pair_retraction_preserves_authored_page_evidence_and_strict_whole_refusal() {
    fn snapshot(root: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
        fn visit(
            root: &Path,
            directory: &Path,
            files: &mut std::collections::BTreeMap<PathBuf, Vec<u8>>,
        ) {
            for entry in std::fs::read_dir(directory).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    visit(root, &path, files);
                } else {
                    files.insert(
                        path.strip_prefix(root).unwrap().to_owned(),
                        std::fs::read(path).unwrap(),
                    );
                }
            }
        }
        let mut files = std::collections::BTreeMap::new();
        visit(root, root, &mut files);
        files
    }
    let f = Fixture::build();
    let pairs = f
        .capture
        .manifest
        .events
        .iter()
        .map(|e| RssEventSelection {
            event_id: e.event_id.clone(),
            revision: e.revision.clone(),
        })
        .collect::<Vec<_>>();
    let raw = lk_vault::accumulate_rss_evidence(
        f.root(),
        &f.origin,
        &f.raw(),
        Some(&f.capture.manifest),
        &[("wiki/concepts/alpha.md".into(), pairs.clone())],
    )
    .unwrap();
    let prose = "Authored explanation [Alpha](../../wiki/concepts/alpha.md).";
    let outside = "Outside [Alpha](../../wiki/concepts/alpha.md) stays.";
    let raw = format!("{raw}\n{prose}\n\n## Notes\n\n{outside}\n");
    std::fs::write(f.root().join(&f.origin), &raw).unwrap();
    let base = [
        "graph",
        "retract-citation",
        f.origin.to_str().unwrap(),
        "wiki/concepts/alpha.md",
    ];
    let before = snapshot(f.root());
    let revision = file_revision(&raw);
    let mut whole = base.to_vec();
    whole.extend(["--revision", &revision, "--dry-run"]);
    assert!(!f.run(&whole).status.success());
    assert_eq!(snapshot(f.root()), before);
    for (i, pair) in pairs.iter().enumerate() {
        let current = f.raw();
        let revision = file_revision(&current);
        let mut selected = base.to_vec();
        selected.extend([
            "--revision",
            &revision,
            "--event",
            &pair.event_id,
            "--event-revision",
            &pair.revision,
        ]);
        let mut stale = selected.clone();
        stale[5] = "0000000000000000000000000000000000000000000000000000000000000000";
        assert!(!f.run(&stale).status.success());
        let before = snapshot(f.root());
        let mut dry = selected.clone();
        dry.push("--dry-run");
        f.ok(&dry);
        assert_eq!(
            snapshot(f.root()),
            before,
            "dry-run must not archive or write"
        );
        let output = f.ok(&selected);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let archived = lk_vault::load_archived_evidence(
            f.root(),
            report["data"]["archived_revision"].as_str().unwrap(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(archived.original.as_deref(), Some(current.as_str()));
        let after = f.raw();
        assert!(after.contains(prose) && after.contains(outside));
        assert_eq!(
            after.contains("- [Alpha](../../wiki/concepts/alpha.md)"),
            i == 0
        );
        let citations = f.citations("alpha");
        assert_eq!(
            citations.iter().filter(|c| c.rss_event.is_some()).count(),
            1 - i
        );
        assert_eq!(
            citations.iter().filter(|c| c.rss_event.is_none()).count(),
            1
        );
        let page = lk_vault::parse_page(&after).unwrap();
        assert!(citations.contains(&Citation::new(&f.origin, &page)));
    }
    let pages = lk_graph::scan::scan_vault(f.root(), false).unwrap();
    lk_graph::backlinks::sync_concept_backlinks(
        &pages,
        f.root(),
        false,
        &Default::default(),
        lk_graph::backlinks::SynthesisPolicy::Record,
    )
    .unwrap();
    let concept = std::fs::read_to_string(f.root().join("wiki/concepts/alpha.md")).unwrap();
    assert!(concept.contains("source_count: 1"));
}
