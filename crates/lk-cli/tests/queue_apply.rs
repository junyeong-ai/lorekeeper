//! End-to-end contract of `lore queue apply`, exercised through the real binary.
//!
//! The case that matters is several results landing on ONE page in a single batch. It is
//! not exotic: `lore ingest` warns when pending queue files exist precisely because running
//! it twice before a drain enqueues the same target again, under the same input hash, and
//! both tasks classify current. Drain both and `queue apply` gets two results for one page.

use lk_queue::TaskRequest;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn extraction_request(seed: &str) -> lk_queue::ExtractConceptsRequest {
    lk_queue::ExtractConceptsRequest {
        concept_policy: lk_core::config::ConceptPolicy::SupportedNew,
        rss_original: None,
        text: seed.into(),
        source_id: "notes".into(),
        source_type: lk_core::config::SourceType::Manual,
        date: jiff::civil::date(2026, 5, 23),
        focus: None,
        locale: lk_core::i18n::Locale::En,
        categories: vec![],
        target: lk_queue::TaskTarget {
            vault_path: "unused.md".into(),
            kind: lk_queue::TargetKind::DailyConcepts,
            anchor: "## Related Concepts".into(),
        },
    }
}

struct Workspace {
    root: tempfile::TempDir,
}

impl Workspace {
    fn new() -> Self {
        let root = tempfile::TempDir::new().expect("tempdir");
        std::fs::create_dir_all(root.path().join("vault")).expect("vault dir");
        std::fs::write(
            root.path().join("config.yaml"),
            "vault:\n  root: vault\n  locale: en\n\
             identity:\n  name: Tester\n  email: tester@example.com\n\
             sources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: inbox\n",
        )
        .expect("config");
        Self { root }
    }

    fn vault(&self) -> PathBuf {
        self.root.path().join("vault")
    }

    fn write(&self, rel: &str, content: &str) {
        let path = self.vault().join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, content).expect("write");
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.vault().join(rel)).expect("read page")
    }

    /// One `extract-concepts` result naming a single concept, as a drain writes it.
    /// Serialized from the real type so the fixture cannot drift from the wire format.
    fn drop_result(&self, task_id: &str, target: &str, hash: &str, concept: &str) {
        self.drop_grounded_result(task_id, target, hash, concept, None);
    }

    /// The same result carrying the grounding sentence a drain writes when the concept is one
    /// the vault does not hold yet.
    fn drop_grounded_result(
        &self,
        task_id: &str,
        target: &str,
        hash: &str,
        concept: &str,
        synthesis: Option<&str>,
    ) {
        let result = lk_queue::TaskResult {
            concept_policy: lk_core::config::ConceptPolicy::SupportedNew,
            target_revision: std::fs::read_to_string(self.vault().join(target))
                .map(|content| blake3::hash(content.as_bytes()).to_hex().to_string())
                .unwrap_or_default(),
            task_id: task_id.into(),
            cache_hash: extraction_request(hash).cache_hash(),
            target: lk_queue::TaskTarget {
                vault_path: target.into(),
                kind: lk_queue::TargetKind::DailyConcepts,
                anchor: "## Related Concepts".into(),
            },
            date: jiff::civil::date(2026, 5, 23),
            concepts: vec![lk_queue::ReportedConcept {
                rss_evidence: Vec::new(),
                concept: lk_core::concept::ExtractedConcept {
                    name: concept.into(),
                    category: None,
                    aliases: Vec::new(),
                },
                synthesis: synthesis.map(str::to_string),
            }],
        };
        let body = serde_json::to_string(&result).expect("serialize result");
        let task = lk_queue::QueueTask {
            task_id: result.task_id.clone(),
            kind: lk_queue::TaskKind::ExtractConcepts,
            created_at: "2026-05-23T00:00:00Z".parse().unwrap(),
            cache_hash: result.cache_hash.clone(),
            input: {
                let mut input = extraction_request(hash).task_input();
                input.insert("locale".into(), "en".into());
                input
            },
            target: result.target.clone(),
        };
        self.write(
            &format!(".lorekeeper/queue/{task_id}.jsonl"),
            &serde_json::to_string(&task).unwrap(),
        );
        let dir = self
            .vault()
            .join(".lorekeeper")
            .join("queue")
            .join(lk_queue::RESULTS_SUBDIR);
        std::fs::create_dir_all(&dir).expect("results dir");
        std::fs::write(dir.join(format!("{task_id}.json")), body).expect("result");
    }

    /// Run `queue apply` and require it to have APPLIED `applied` results and dropped
    /// `dropped`.
    ///
    /// Asserting the resulting page alone cannot tell a correct re-apply from a silent
    /// drop — both leave it unchanged — so an idempotency test written that way passes
    /// against the very bug it is named after. What happened is part of the assertion.
    fn apply_expecting(&self, applied: usize, dropped: usize) {
        let out = self.run(&["queue", "apply"]);
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "queue apply failed: {err}");
        // Parsed, not substring-matched: `"1 applied"` is a substring of `"11 applied"`,
        // so a run that applied ten times too many would satisfy a `contains` check.
        let tally = err
            .lines()
            .find_map(|line| line.strip_prefix("queue apply: "))
            .unwrap_or_else(|| panic!("no summary line in: {err}"));
        let counted = |what: &str| -> usize {
            tally
                .split(", ")
                .find_map(|part| part.strip_suffix(&format!(" {what}")))
                .and_then(|n| n.parse().ok())
                .unwrap_or_else(|| panic!("no `{what}` count in: {tally}"))
        };
        assert_eq!(
            (counted("applied"), counted("dropped")),
            (applied, dropped),
            "in: {tally}"
        );
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_lore"));
        for (key, _) in std::env::vars() {
            if key.starts_with("LORE_") {
                cmd.env_remove(key);
            }
        }
        cmd.arg("--config")
            .arg(self.root.path().join("config.yaml"))
            .args(args)
            .output()
            .expect("spawn lore")
    }
}

const PAGE: &str = "daily/notes/2026-05-23.md";

fn daily_page(hash: &str) -> String {
    dated_daily_page("2026-05-23", hash)
}

fn dated_daily_page(date: &str, hash: &str) -> String {
    let hash = extraction_request(hash).cache_hash();
    format!(
        "---\nid: notes-{date}\ntype: daily\nllm_inputs:\n  concepts: \"{hash}\"\n---\n\n\
         ## Summary\n\nbody\n\n## Related Concepts\n\n## Key Events\n\n- a\n"
    )
}

fn results_dir(ws: &Workspace) -> PathBuf {
    ws.vault()
        .join(".lorekeeper")
        .join("queue")
        .join(lk_queue::RESULTS_SUBDIR)
}

fn json_files(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// Two results for one page in one batch must BOTH be cited. Every write lands after the
/// apply loop, so a second fold reading the page from disk would see a version predating
/// the first, and its write would drop the first's citation — the same silent loss the
/// accumulating render exists to prevent, reached through batching instead of re-extraction.
#[test]
fn two_results_for_one_page_both_land() {
    let ws = Workspace::new();
    ws.write(PAGE, &daily_page("h"));
    // Same target, same hash — what two ingests before one drain produce.
    ws.drop_result("ext-1", PAGE, "h", "Concept A");
    ws.drop_result("ext-2", PAGE, "h", "Concept B");

    let out = ws.run(&["queue", "apply"]);
    assert!(
        out.status.success(),
        "queue apply failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let page = ws.read(PAGE);
    for slug in ["concept-a", "concept-b"] {
        assert!(
            page.contains(&format!("wiki/concepts/{slug}.md")),
            "{slug} must be cited after applying both results:\n{page}"
        );
    }
    // Both concept pages exist, so both citations resolve.
    for slug in ["concept-a", "concept-b"] {
        assert!(
            ws.vault()
                .join("wiki/concepts")
                .join(format!("{slug}.md"))
                .exists(),
            "{slug} page must be written"
        );
    }
    assert!(
        json_files(&results_dir(&ws)).is_empty(),
        "every applied result must be consumed"
    );
}

#[test]
fn existing_only_policy_refuses_new_nodes_and_retains_work_until_an_owner_exists() {
    let ws = Workspace::new();
    let mut request = extraction_request("policy-proof");
    request.concept_policy = lk_core::config::ConceptPolicy::ExistingOnly;
    let hash = request.cache_hash();
    ws.write(
        PAGE,
        &daily_page("policy-proof")
            .replace(&extraction_request("policy-proof").cache_hash(), &hash),
    );
    ws.drop_result("policy-proof", PAGE, "policy-proof", "Known alias");
    let task_path = ".lorekeeper/queue/policy-proof.jsonl";
    let mut task: lk_queue::QueueTask = serde_json::from_str(&ws.read(task_path)).unwrap();
    task.input
        .insert("concept_policy".into(), serde_json::json!("existing-only"));
    task.cache_hash = hash.clone();
    ws.write(task_path, &serde_json::to_string(&task).unwrap());
    let result_path = ".lorekeeper/queue/results/policy-proof.json";
    let mut result: lk_queue::TaskResult = serde_json::from_str(&ws.read(result_path)).unwrap();
    result.concept_policy = lk_core::config::ConceptPolicy::ExistingOnly;
    result.cache_hash = hash;
    ws.write(result_path, &serde_json::to_string(&result).unwrap());

    let before = ws.read(PAGE);
    let rejected = ws.run(&["queue", "apply"]);
    assert!(!rejected.status.success());
    assert!(
        String::from_utf8_lossy(&rejected.stderr)
            .contains("existing-only concept policy refuses new concept")
    );
    assert_eq!(ws.read(PAGE), before);
    assert!(!ws.vault().join("wiki/concepts/known-alias.md").exists());
    assert!(ws.vault().join(result_path).exists());
    assert!(ws.vault().join(task_path).exists());

    ws.write("wiki/concepts/stable-address.md", "---\ntype: concept\ntitle: Existing subject\naliases: [Known alias]\n---\n## Synthesis\nA stable scoped definition.\n\n## Sources\n\n## Related\n");
    ws.apply_expecting(1, 0);
    assert!(ws.read(PAGE).contains("wiki/concepts/stable-address.md"));
    assert!(!ws.vault().join("wiki/concepts/known-alias.md").exists());
    assert!(
        ws.read("wiki/concepts/stable-address.md")
            .contains("A stable scoped definition.")
    );
}

/// A result whose target page will not parse is kept, and does not strand the batch.
///
/// The likeliest author of unparseable frontmatter is a drain stamping a marker, and the
/// repair is a line. So the page is a question nobody can answer yet rather than a dead
/// result: consuming it would spend the extraction on a page that could not receive it, with
/// nothing left to say what was lost. And every other page's citations must not wait for that
/// repair — aborting would hold them on this run and on every run after it, since nothing
/// prunes results.
#[test]
fn a_result_whose_page_will_not_parse_is_kept_and_holds_up_nothing() {
    const BROKEN: &str = "daily/notes/2026-05-24.md";
    let ws = Workspace::new();
    ws.write(PAGE, &daily_page("h"));
    ws.write(
        BROKEN,
        "---\nid: notes-2026-05-24\ntype: daily\nllm_inputs:\n   [[[\n---\n\n\
         ## Related Concepts\n\n",
    );
    ws.drop_result("ext-good", PAGE, "h", "Concept A");
    ws.drop_result("ext-broken", BROKEN, "h", "Concept B");

    let out = ws.run(&["queue", "apply"]);
    assert!(
        !out.status.success(),
        "a result nothing could apply must fail the run"
    );

    assert!(
        ws.read(PAGE).contains("wiki/concepts/concept-a.md"),
        "the page that could be written must be cited:\n{}",
        ws.read(PAGE)
    );
    assert_eq!(
        json_files(&results_dir(&ws))
            .iter()
            .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
            .map(str::to_owned)
            .collect::<Vec<_>>(),
        vec!["ext-broken.json".to_string()],
        "the unapplicable result is kept and the applied one consumed"
    );
}

/// `--dry-run` reports without touching the vault or consuming a result.
#[test]
fn dry_run_applies_nothing_and_consumes_nothing() {
    let ws = Workspace::new();
    ws.write(PAGE, &daily_page("h"));
    ws.drop_result("ext-1", PAGE, "h", "Concept A");
    let before = ws.read(PAGE);

    let out = ws.run(&["queue", "apply", "--dry-run"]);
    assert!(out.status.success(), "dry-run must succeed");
    assert_eq!(ws.read(PAGE), before, "dry-run must not rewrite the page");
    assert_eq!(
        json_files(&results_dir(&ws)).len(),
        1,
        "dry-run must leave the result for a real run"
    );
    assert!(
        !ws.vault().join("wiki/concepts/concept-a.md").exists(),
        "dry-run must not write concept pages"
    );
}

#[test]
fn extraction_refuses_a_manual_source_edit_without_a_cache_marker_change() {
    for recovery in [false, true] {
        let ws = Workspace::new();
        ws.write(PAGE, &daily_page("h"));
        ws.drop_grounded_result("ext-1", PAGE, "h", "Concept A", Some("Original claim."));
        let revised = format!("{}\nThe original claim was withdrawn.\n", ws.read(PAGE));
        ws.write(PAGE, &revised);

        let output = if recovery {
            std::fs::remove_file(ws.vault().join(".lorekeeper/queue/ext-1.jsonl")).unwrap();
            ws.run(&["queue", "apply", "--recover-extraction", "ext-1"])
        } else {
            ws.run(&["queue", "apply"])
        };
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("target changed"));
        assert_eq!(ws.read(PAGE), revised);
        assert!(!ws.vault().join("wiki/concepts/concept-a.md").exists());
        assert!(results_dir(&ws).join("ext-1.json").exists());
    }
}

#[test]
fn an_extraction_result_left_after_publication_is_retired_without_rewriting() {
    let ws = Workspace::new();
    ws.write(PAGE, &daily_page("h"));
    ws.drop_result("ext-1", PAGE, "h", "Concept A");
    let result_path = ".lorekeeper/queue/results/ext-1.json";
    let original = ws.read(result_path);
    ws.apply_expecting(1, 0);
    let page = ws.read(PAGE);
    let concept = ws.read("wiki/concepts/concept-a.md");
    ws.write(result_path, &original);
    ws.apply_expecting(0, 1);
    assert_eq!(ws.read(PAGE), page);
    assert_eq!(ws.read("wiki/concepts/concept-a.md"), concept);
    assert!(!ws.vault().join(result_path).exists());
}

#[test]
fn extraction_cannot_retarget_a_pending_task_or_apply_without_one() {
    for scenario in ["retargeted", "orphaned"] {
        let ws = Workspace::new();
        ws.write(PAGE, &daily_page("h"));
        ws.drop_result("ext-1", PAGE, "h", "Concept A");
        let task_path = ws.vault().join(".lorekeeper/queue/ext-1.jsonl");
        if scenario == "orphaned" {
            std::fs::remove_file(task_path).unwrap();
        } else {
            let mut task: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&task_path).unwrap()).unwrap();
            task["target"]["vault_path"] = "daily/another.md".into();
            std::fs::write(task_path, serde_json::to_vec(&task).unwrap()).unwrap();
        }
        let before = ws.read(PAGE);
        let output = ws.run(&["queue", "apply"]);
        assert!(!output.status.success(), "{scenario}");
        assert_eq!(ws.read(PAGE), before);
        assert!(!ws.vault().join("wiki/concepts/concept-a.md").exists());
        if scenario == "orphaned" {
            assert!(results_dir(&ws).join("orphaned/ext-1.json").exists());
        } else {
            assert!(results_dir(&ws).join("ext-1.json").exists());
        }
    }
}

#[test]
fn a_result_whose_page_moved_on_is_dropped() {
    let ws = Workspace::new();
    ws.write(PAGE, &daily_page("newer-hash"));
    ws.drop_result("ext-1", PAGE, "h", "Concept A");

    let out = ws.run(&["queue", "apply"]);
    assert!(out.status.success(), "a dropped result is not a failure");
    assert!(
        !ws.read(PAGE).contains("concept-a.md"),
        "a stale result must not edit the page"
    );
    assert!(json_files(&results_dir(&ws)).is_empty(), "and is consumed");
}

/// A `vault.locale` switch re-renders every heading but leaves the concept input hash
/// untouched, so a result drained before the switch stays hash-current while naming a
/// section that no longer exists. It used to fail `queue apply` — and so the whole
/// scheduled pipeline — on every run forever, since `queue prune` classifies tasks and
/// never results. It is dropped instead: the page carries no completion marker, so the
/// next ingest re-enqueues the work under the heading the page now has.
#[test]
fn a_result_whose_section_no_longer_exists_is_dropped_not_retried_forever() {
    let ws = Workspace::new();
    // Same input hash; the section is now named in another locale.
    ws.write(
        PAGE,
        "---\nid: notes-2026-05-23\ntype: daily\nllm_inputs:\n  concepts: \"h\"\n---\n\n\
         ## 관련 개념\n\n## Key Events\n\n- a\n",
    );
    ws.drop_result("ext-1", PAGE, "h", "Concept A");

    let out = ws.run(&["queue", "apply"]);
    assert!(
        out.status.success(),
        "a result with nowhere to land is dropped, not a failure: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        json_files(&results_dir(&ws)).is_empty(),
        "it must be consumed, or it blocks every later run"
    );
    assert!(
        !ws.vault().join("wiki/concepts/concept-a.md").exists(),
        "and nothing may be written from it"
    );
    assert_eq!(
        ws.read(PAGE).matches("## ").count(),
        2,
        "the page is untouched:\n{}",
        ws.read(PAGE)
    );
}

/// A result is a VALUE IN FLIGHT, not a request for work. The page already carrying the
/// completion marker for this input does not make the result redundant — for concepts the
/// marker is stamped by the very edit that writes them, and a drain following the skill's
/// own step-3c table stamps it before `queue apply` ever runs. Dropping such a result
/// discards the extraction permanently and silently: the marker keeps the empty section
/// looking cached, so nothing re-enqueues, and the command still exits 0.
#[test]
fn a_result_lands_even_when_its_page_is_already_marked_done() {
    let ws = Workspace::new();
    let hash = extraction_request("h").cache_hash();
    ws.write(PAGE, &format!("---\nid: notes-2026-05-23\ntype: daily\nllm_inputs:\n  concepts: {hash}\n  concepts_done: {hash}\n---\n\n## Related Concepts\n\n## Key Events\n\n- a\n"));
    ws.drop_result("ext-1", PAGE, "h", "Concept A");

    let out = ws.run(&["queue", "apply"]);
    assert!(out.status.success(), "queue apply must succeed");
    assert!(
        ws.read(PAGE).contains("wiki/concepts/concept-a.md"),
        "the citation must be written:\n{}",
        ws.read(PAGE)
    );
    assert!(
        ws.vault().join("wiki/concepts/concept-a.md").exists(),
        "and the concept page it points at must exist"
    );
}

/// Re-applying a result that already landed reproduces the page rather than doubling it —
/// which is what makes it safe for a result to ignore the completion marker.
#[test]
fn applying_the_same_result_twice_is_idempotent() {
    let ws = Workspace::new();
    ws.write(PAGE, &daily_page("h"));
    ws.drop_result("ext-1", PAGE, "h", "Concept A");
    ws.apply_expecting(1, 0);
    let once = ws.read(PAGE);

    ws.drop_result("ext-1", PAGE, "h", "Concept A");
    // APPLIED again, not dropped: the first apply stamped the completion marker, so a
    // result judged by that marker would be discarded here — leaving the page identical,
    // which is exactly what idempotence looks like from the bytes alone.
    ws.apply_expecting(1, 0);
    assert_eq!(
        ws.read(PAGE),
        once,
        "a re-applied result must change nothing"
    );
}

/// The safety argument for a result ignoring the completion marker is that re-applying is
/// idempotent — so it has to hold for the CONCEPT page too, not just the origin. A merge
/// that reset an established page's accumulated state would trade a silent loss for a
/// silent corruption, which is worse.
#[test]
fn re_applying_preserves_everything_an_established_concept_page_accumulated() {
    let ws = Workspace::new();
    ws.write(PAGE, &daily_page("h"));
    ws.drop_result("ext-1", PAGE, "h", "Concept A");
    ws.apply_expecting(1, 0);

    // Stand in for what the rest of the system writes onto an established page:
    // `backlinks-sync` owns the count, a human or a later audit owns the synthesis and
    // aliases, and `created` is the day it entered the vault.
    let concept = "wiki/concepts/concept-a.md";
    let enriched = ws
        .read(concept)
        .replace("source_count: 0", "source_count: 3")
        .replace("created: 2026-05-23", "created: 2020-01-01")
        .replace(
            "aliases: [\"Concept A\"]",
            "aliases: [\"Concept A\", \"CA\"]",
        )
        + "\nA human wrote this.\n";
    ws.write(concept, &enriched);

    ws.drop_result("ext-1", PAGE, "h", "Concept A");
    // Applied, not dropped — see `applying_the_same_result_twice_is_idempotent`.
    ws.apply_expecting(1, 0);

    let after = ws.read(concept);
    assert!(
        after.contains("source_count: 3"),
        "count preserved:\n{after}"
    );
    assert!(
        after.contains("created: 2020-01-01"),
        "origin date preserved:\n{after}"
    );
    assert!(after.contains("\"CA\""), "alias preserved:\n{after}");
    assert!(
        after.contains("A human wrote this."),
        "authored prose preserved:\n{after}"
    );
}

/// Explicit recovery uses the current stored page without re-fetching an expired source window.
#[test]
fn a_result_file_with_no_task_behind_it_recovers_a_lost_section() {
    let ws = Workspace::new();
    ws.write(PAGE, &daily_page("recorded-hash"));
    assert!(
        !ws.read(PAGE).contains("concepts_done"),
        "the fixture starts unanswered"
    );

    ws.drop_result("recovered", PAGE, "recorded-hash", "Recovered Concept");
    std::fs::remove_file(ws.vault().join(".lorekeeper/queue/recovered.jsonl")).unwrap();
    let evidence = ws.run(&["wiki", "evidence", PAGE]);
    assert!(evidence.status.success());
    let report: serde_json::Value = serde_json::from_slice(&evidence.stdout).unwrap();
    let result: serde_json::Value =
        serde_json::from_str(&ws.read(".lorekeeper/queue/results/recovered.json")).unwrap();
    assert_eq!(report["file_revisions"][PAGE], result["target_revision"]);
    let out = ws.run(&["queue", "apply", "--recover-extraction", "recovered"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains("original retained-input and part coverage cannot be established")
    );

    let page = ws.read(PAGE);
    assert!(
        page.contains("wiki/concepts/recovered-concept.md"),
        "{page}"
    );
    assert!(
        page.contains(&format!(
            "concepts_done: \"{}\"",
            extraction_request("recorded-hash").cache_hash()
        )),
        "the applier stamps the marker it owns\n{page}"
    );
}

fn bounded_config(ws: &Workspace) {
    let path = ws.root.path().join("config.yaml");
    let mut config = std::fs::read_to_string(&path).unwrap();
    config.push_str(
        "execution:\n  work: {max_input_bytes: 8192, max_result_bytes: 1024, max_calls: 100}\n",
    );
    std::fs::write(path, config).unwrap();
}

fn work_json(ws: &Workspace, args: &[&str]) -> serde_json::Value {
    let output = ws.run(args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn multipart_extraction_requires_completed_work_without_poisoning_valid_results() {
    for prepared in [false, true] {
        let ws = Workspace::new();
        bounded_config(&ws);
        let large = "Complete retained evidence.\n\n".repeat(2000);
        ws.write(PAGE, &daily_page(&large));
        ws.drop_result("a-large", PAGE, &large, "Blocked Subject");
        let small = "daily/notes/small.md";
        ws.write(small, &daily_page("small"));
        ws.drop_result("z-small", small, "small", "Published Subject");
        let original = ws.read(PAGE);
        if prepared {
            let plan = work_json(&ws, &["work", "plan", "--task-id", "a-large"]);
            assert!(plan["parts"].as_u64().unwrap() > 1);
            assert_eq!(plan["completed"], 0);
        }
        let output = ws.run(&["queue", "apply"]);
        assert!(!output.status.success());
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        assert!(
            diagnostic.contains(if prepared {
                "work reduction is incomplete"
            } else {
                "bounded evidence work is required"
            }),
            "{diagnostic}"
        );
        assert_eq!(ws.read(PAGE), original);
        assert!(!ws.vault().join("wiki/concepts/blocked-subject.md").exists());
        assert!(
            ws.vault()
                .join("wiki/concepts/published-subject.md")
                .exists()
        );
        assert!(ws.read(small).contains("concepts_done:"));
        assert!(results_dir(&ws).join("a-large.json").exists());
        assert!(!results_dir(&ws).join("z-small.json").exists());
    }
}

#[test]
fn oversized_orphan_recovery_preserves_source_and_requires_retained_task() {
    let ws = Workspace::new();
    bounded_config(&ws);
    let source = format!(
        "{}\n{}",
        daily_page("orphan"),
        "Full source evidence.\n".repeat(1000)
    );
    ws.write(PAGE, &source);
    ws.drop_result("orphan", PAGE, "orphan", "Unpublished Subject");
    std::fs::remove_file(ws.vault().join(".lorekeeper/queue/orphan.jsonl")).unwrap();
    let output = ws.run(&["queue", "apply", "--recover-extraction", "orphan"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("restore the retained task before bounded processing")
    );
    assert_eq!(ws.read(PAGE), source);
    assert!(
        !ws.vault()
            .join("wiki/concepts/unpublished-subject.md")
            .exists()
    );
    assert!(results_dir(&ws).join("orphan.json").exists());
    assert!(!ws.vault().join(".lorekeeper/work").exists());
}

#[test]
fn sealed_synthesis_requires_exact_read_source_bytes_even_when_authored_revision_is_unchanged() {
    let ws = Workspace::new();
    bounded_config(&ws);
    let source_path = "wiki/documents/source.md";
    let source = format!(
        "---\ntype: document\nupdated: 2026-01-01\n---\n{}",
        "Complete source evidence.\n\n".repeat(1000)
    );
    ws.write(source_path, &source);
    let parsed = lk_core::frontmatter::parse_page(&source).unwrap();
    let citation = lk_core::concept::Citation::new(Path::new(source_path), &parsed);
    let hash = lk_core::concept::citation_digest(std::slice::from_ref(&citation));
    let target = "wiki/concepts/topic.md";
    let body = format!(
        "---\ntype: concept\nllm_inputs:\n  synthesis: {hash}\n---\n## Synthesis\nPrior claim.\n\n## Related\n"
    );
    ws.write(target, &body);
    let task = serde_json::json!({"task_id":"syn-large", "kind":"synthesize-concept", "created_at":"2026-10-16T00:00:00Z", "cache_hash":hash,
        "input":{"locale":"en","citations":[citation],"related_anchor":"## Related"}, "target":{"vault_path":target,"kind":"concept-synthesis","anchor":"## Synthesis"}});
    ws.write(".lorekeeper/queue/syn-large.jsonl", &task.to_string());
    let planned = work_json(&ws, &["work", "plan", "--task-id", "syn-large"]);
    assert!(planned["parts"].as_u64().unwrap() > 1);
    let work_id = planned["work_id"].as_str().unwrap();
    loop {
        let progress = work_json(&ws, &["work", "status", work_id]);
        if progress["complete"] == true {
            break;
        }
        let nodes = progress["ready"].as_array().unwrap();
        assert!(!nodes.is_empty());
        for node in nodes {
            let node_id = node.as_str().unwrap();
            let input = work_json(&ws, &["work", "read", work_id, node_id]);
            let observation = serde_json::json!({"work_id":work_id,"node_id":node_id,
                "input_revision":input["input_revision"],"summary":"Deterministic fixture observation for the complete selected ranges.",
                "findings":[],"unresolved":[],"resolutions":[]});
            ws.write(
                ".lorekeeper/fixture-observation.json",
                &observation.to_string(),
            );
            let file = ws.vault().join(".lorekeeper/fixture-observation.json");
            work_json(&ws, &["work", "record", "--file", file.to_str().unwrap()]);
        }
    }
    let result = serde_json::json!({"task_id":"syn-large","cache_hash":hash,
        "target_revision":blake3::hash(body.as_bytes()).to_hex().to_string(),
        "synthesis":"The retained source supports the scoped claim.","related":""});
    ws.write(".lorekeeper/fixture-final.json", &result.to_string());
    let file = ws.vault().join(".lorekeeper/fixture-final.json");
    work_json(
        &ws,
        &["work", "finish", work_id, "--file", file.to_str().unwrap()],
    );
    let preview = ws.run(&["queue", "apply", "--dry-run"]);
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    let changed = source.replace("updated: 2026-01-01", "updated: 2026-01-02");
    assert_eq!(
        lk_core::concept::source_revision(&parsed),
        lk_core::concept::source_revision(&lk_core::frontmatter::parse_page(&changed).unwrap())
    );
    ws.write(source_path, &changed);
    let output = ws.run(&["queue", "apply"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("bounded evidence work is required"));
    assert_eq!(ws.read(target), body);
    assert!(!json_files(&results_dir(&ws)).is_empty());
    ws.write(source_path, &source);
    let applied = ws.run(&["queue", "apply"]);
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    assert!(
        ws.read(target)
            .contains("The retained source supports the scoped claim.")
    );
    assert!(ws.read(target).contains("synthesis_done:"));
    assert!(json_files(&results_dir(&ws)).is_empty());
}

/// `queue prune`'s `--json` envelope carries the same verdict as its exit code.
///
/// The janitor is the only place a blocked queue surfaces — `queue count` omits it so a
/// scheduled run never spends a session on work no session can do — so a caller that parses
/// `ok` is the one that has to be told, and telling it the opposite of the exit code is worse
/// than saying nothing.
#[test]
fn prune_reports_the_same_verdict_in_json_as_in_its_exit_code() {
    let ws = Workspace::new();
    let queue = ws.vault().join(".lorekeeper/queue");
    std::fs::create_dir_all(&queue).expect("queue dir");

    let verdict = |args: &[&str]| {
        let out = ws.run(args);
        let parsed: serde_json::Value =
            serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("valid JSON");
        (
            parsed["ok"].as_bool().expect("ok is a bool"),
            out.status.code() == Some(0),
            parsed,
        )
    };

    // Nothing to do: green, and it says so.
    let (ok, exited_zero, _) = verdict(&["queue", "prune", "--json"]);
    assert!(ok && exited_zero, "an empty queue is not a finding");

    // One line nobody can parse: the file is kept whole, and so is any task beside it.
    std::fs::write(queue.join("run-1.jsonl"), "{not json\n").expect("queue file");
    let (ok, exited_zero, parsed) = verdict(&["queue", "prune", "--json", "--dry-run"]);
    assert_eq!(
        ok, exited_zero,
        "the parsed verdict and the exit code are one answer\n{parsed}"
    );
    assert!(!ok, "a queue nothing can drain is a finding\n{parsed}");
    assert_eq!(parsed["data"]["kept_unparseable"].as_u64(), Some(1));
}

/// A grounding sentence answers the ONE page whose material produced it, and `queue apply`
/// records that page as the concept's synthesis input. Without the record the sentence is
/// indistinguishable from an authored synthesis, and `backlinks-sync` adopts it as the answer
/// for every page citing the concept by the time it first runs — which in a backfill is the
/// whole citation set, permanently, since adoption also stamps the answer.
#[test]
fn a_grounding_answers_one_page_and_a_second_citation_owes_a_rewrite() {
    const SECOND: &str = "daily/notes/2026-05-24.md";
    let ws = Workspace::new();
    ws.write(PAGE, &daily_page("h"));
    ws.write(SECOND, &dated_daily_page("2026-05-24", "h"));
    ws.drop_grounded_result("ext-1", PAGE, "h", "Concept A", Some("What Concept A is."));
    ws.drop_result("ext-2", SECOND, "h", "Concept A");
    ws.apply_expecting(2, 0);

    let concept = ws.read("wiki/concepts/concept-a.md");
    let grounded = lk_core::concept::citation_digest(&[lk_core::concept::Citation::new(
        std::path::Path::new(PAGE),
        &lk_core::frontmatter::parse_page(&ws.read(PAGE)).unwrap(),
    )]);
    assert!(
        concept.contains(&format!("synthesis_done: \"{grounded}\"")),
        "the seeded sentence must answer the page it was written from:\n{concept}"
    );

    let out = ws.run(&["graph", "backlinks-sync"]);
    assert!(
        out.status.success(),
        "backlinks-sync failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = ws.run(&["queue", "count"]);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "1",
        "two pages cite a concept whose synthesis answers one, so the rewrite is owed"
    );
}

#[test]
fn source_revisions_drive_synthesis_without_reprocessing_bookkeeping() {
    let ws = Workspace::new();
    ws.write(PAGE, &daily_page("h"));
    ws.drop_grounded_result(
        "ext-1",
        PAGE,
        "h",
        "Concept A",
        Some("Timeout is 30 seconds."),
    );
    ws.apply_expecting(1, 0);

    let sync = || {
        let out = ws.run(&["graph", "--json", "backlinks-sync"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["data"].clone()
    };
    assert!(ws.run(&["queue", "prune"]).status.success());
    assert_eq!(sync()["queued"], 0);
    let page = ws.read(PAGE);
    let changed_metadata = page.replacen("---\n", "---\nupdated: 2026-10-15\n", 1);
    ws.write(PAGE, &changed_metadata);
    assert_eq!(sync()["queued"], 0);

    ws.write(
        PAGE,
        &format!("{changed_metadata}\nTimeout is 5 seconds.\n"),
    );
    assert_eq!(sync()["queued"], 1);
    assert_eq!(sync()["queued"], 0, "a pending revision is queued once");
    let status = || {
        let out = ws.run(&["queue", "status", "--json"]);
        assert!(out.status.success());
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["data"].clone()
    };
    assert_eq!(status()["current"], 1);

    let latest = ws.read(PAGE);
    ws.write(PAGE, &latest.replacen("---\n", "---\nbroken: [\n", 1));
    assert_eq!(status()["unreadable"], 1);
    let before = ws.read("wiki/concepts/concept-a.md");
    let out = ws.run(&["graph", "--json", "backlinks-sync"]);
    assert!(!out.status.success());
    assert_eq!(ws.read("wiki/concepts/concept-a.md"), before);

    ws.write(PAGE, &latest);
    assert_eq!(status()["current"], 1, "repair restores the pending task");
    ws.write(PAGE, &latest.replace("5 seconds", "10 seconds"));
    assert_eq!(
        status()["stale"],
        1,
        "a queued draft cannot use changed evidence"
    );
    assert_eq!(sync()["queued"], 1);
    assert_eq!(status()["current"], 1);
}

#[test]
fn synthesis_results_preserve_edits_and_apply_both_sections_only_to_current_inputs() {
    for scenario in [
        "current",
        "source-changed",
        "target-changed",
        "missing-related",
        "missing-link",
        "existing-link",
    ] {
        let ws = Workspace::new();
        let source = "---\ntype: document\n---\nTimeout is 30 seconds.\n";
        ws.write("wiki/documents/spec.md", source);
        let citation = lk_core::concept::Citation::new(
            Path::new("wiki/documents/spec.md"),
            &lk_core::frontmatter::parse_page(source).unwrap(),
        );
        let hash = lk_core::concept::citation_digest(std::slice::from_ref(&citation));
        let concept = format!(
            "---\ntype: concept\nllm_inputs:\n  synthesis: {hash}\n---\n## Synthesis\nOld synthesis.\n\n## Related\nOld relation.\n\n## Notes\nKeep this.\n"
        );
        ws.write("wiki/concepts/timeout.md", &concept);
        let task = serde_json::json!({"task_id":"syn-1", "kind":"synthesize-concept", "created_at":"2026-10-16T00:00:00Z", "cache_hash":hash,
            "input":{"locale":"en","citations":[citation], "related_anchor":"## Related"}, "target":{"vault_path":"wiki/concepts/timeout.md","kind":"concept-synthesis","anchor":"## Synthesis"}});
        ws.write(".lorekeeper/queue/tasks.jsonl", &task.to_string());
        let status = ws.run(&["queue", "status", "--json"]);
        assert!(status.status.success());
        let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
        let mut result = serde_json::json!({"task_id":"syn-1", "cache_hash":hash, "target_revision":status["data"]["tasks"][0]["target_revision"], "synthesis":"Timeout is 30 seconds.", "related":"A sourced relationship."});
        match scenario {
            "source-changed" => ws.write("wiki/documents/spec.md", &source.replace("30", "5")),
            "target-changed" => ws.write(
                "wiki/concepts/timeout.md",
                &concept.replace("Keep this.", "New human decision."),
            ),
            "missing-related" => {
                result.as_object_mut().unwrap().remove("related");
            }
            "missing-link" => {
                result["synthesis"] =
                    serde_json::json!("Conflict: [review](../explorations/missing.md).");
            }
            "existing-link" => {
                ws.write(
                    "wiki/explorations/review.md",
                    "---\ntype: exploration\n---\nReview\n",
                );
                result["synthesis"] = serde_json::json!(
                    "Timeout is 30 seconds. [Review](../explorations/review.md#claims). [External](https://example.com) [Here](#local) ` [example](missing.md)`"
                );
            }
            _ => {}
        }
        let before = ws.read("wiki/concepts/timeout.md");
        ws.write(
            ".lorekeeper/queue/results/syn-1.synthesis.json",
            &result.to_string(),
        );
        if matches!(scenario, "missing-link" | "existing-link") {
            let dry_run = ws.run(&["queue", "apply", "--dry-run"]);
            assert_eq!(dry_run.status.success(), scenario == "existing-link");
            assert_eq!(ws.read("wiki/concepts/timeout.md"), before);
            assert!(
                ws.vault()
                    .join(".lorekeeper/queue/results/syn-1.synthesis.json")
                    .exists()
            );
        }
        let out = ws.run(&["queue", "apply"]);
        let after = ws.read("wiki/concepts/timeout.md");
        if matches!(scenario, "current" | "existing-link") {
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(after.contains("Timeout is 30 seconds."));
            assert!(after.contains("A sourced relationship."));
            assert!(after.contains("Keep this."));
            assert!(after.contains("synthesis_done:"));
            assert!(ws.run(&["queue", "apply"]).status.success());
            assert_eq!(after, ws.read("wiki/concepts/timeout.md"));
        } else {
            assert_eq!(after, before, "{scenario}");
            assert_eq!(
                out.status.success(),
                scenario == "source-changed",
                "{scenario}"
            );
        }
    }
}

#[test]
fn orphan_synthesis_is_preserved_with_a_reason_and_does_not_fail_forever() {
    let ws = Workspace::new();
    let result = serde_json::json!({"task_id":"retired", "cache_hash":"old", "target_revision":"old", "synthesis":"Evidence.", "related":null}).to_string();
    let path = ".lorekeeper/queue/results/retired.synthesis.json";
    ws.write(path, &result);
    let out = ws.run(&["queue", "apply", "--dry-run"]);
    assert!(!out.status.success());
    assert_eq!(ws.read(path), result);
    let out = ws.run(&["queue", "apply"]);
    assert!(!out.status.success());
    assert!(!ws.vault().join(path).exists());
    assert_eq!(
        ws.read(".lorekeeper/queue/results/orphaned/retired.synthesis.json"),
        result
    );
    assert!(
        ws.read(".lorekeeper/queue/results/orphaned/retired.synthesis.json.reason.txt")
            .contains("no matching pending task")
    );
    assert!(ws.run(&["queue", "apply"]).status.success());
}

#[test]
fn malformed_pending_task_prevents_orphan_disposition() {
    let ws = Workspace::new();
    ws.write(".lorekeeper/queue/tasks.jsonl", "{truncated task");
    let path = ".lorekeeper/queue/results/unknown.synthesis.json";
    let result = serde_json::json!({"task_id":"unknown", "cache_hash":"old", "target_revision":"old", "synthesis":"Evidence.", "related":null}).to_string();
    ws.write(path, &result);
    let out = ws.run(&["queue", "apply"]);
    assert!(!out.status.success());
    assert_eq!(ws.read(path), result);
    assert!(String::from_utf8_lossy(&out.stderr).contains("unparseable queue lines"));
}

#[test]
fn corrupt_result_payloads_are_preserved_once_without_blocking_valid_synthesis() {
    for suffix in ["json", "synthesis.json", "section.json"] {
        for bytes in [
            b"{\"task_id\":".as_slice(),
            b"{\"task_id\":\"\xea\xb0".as_slice(),
        ] {
            let ws = Workspace::new();
            let source = "---\ntype: document\n---\nEvidence.\n";
            ws.write("wiki/documents/spec.md", source);
            let citation = lk_core::concept::Citation::new(
                Path::new("wiki/documents/spec.md"),
                &lk_core::frontmatter::parse_page(source).unwrap(),
            );
            let hash = lk_core::concept::citation_digest(std::slice::from_ref(&citation));
            ws.write("wiki/concepts/example.md", &format!("---\ntype: concept\nllm_inputs:\n  synthesis: {hash}\n---\n## Synthesis\nOld.\n\n## Related\n\n"));
            let task = serde_json::json!({"task_id":"good", "kind":"synthesize-concept", "created_at":"2026-10-16T00:00:00Z", "cache_hash":hash,
                "input":{"locale":"en","citations":[citation],"related_anchor":"## Related"}, "target":{"vault_path":"wiki/concepts/example.md","kind":"concept-synthesis","anchor":"## Synthesis"}});
            ws.write(".lorekeeper/queue/tasks.jsonl", &task.to_string());
            let status: serde_json::Value =
                serde_json::from_slice(&ws.run(&["queue", "status", "--json"]).stdout).unwrap();
            let result = serde_json::json!({"task_id":"good","cache_hash":hash,"target_revision":status["data"]["tasks"][0]["target_revision"],"synthesis":"Grounded evidence.","related":""});
            ws.write(
                ".lorekeeper/queue/results/good.synthesis.json",
                &result.to_string(),
            );
            let bad = ws
                .vault()
                .join(format!(".lorekeeper/queue/results/bad.{suffix}"));
            std::fs::write(&bad, bytes).unwrap();
            let dry = ws.run(&["queue", "apply", "--dry-run"]);
            assert!(!dry.status.success());
            assert_eq!(std::fs::read(&bad).unwrap(), bytes);
            assert!(ws.read("wiki/concepts/example.md").contains("Old."));
            assert!(!ws.run(&["queue", "apply"]).status.success());
            assert!(
                ws.read("wiki/concepts/example.md")
                    .contains("Grounded evidence.")
            );
            assert!(!bad.exists());
            let preserved = ws
                .vault()
                .join(format!(".lorekeeper/queue/results/corrupt/bad.{suffix}"));
            assert_eq!(std::fs::read(&preserved).unwrap(), bytes);
            assert!(
                preserved
                    .with_file_name(format!("bad.{suffix}.reason.txt"))
                    .is_file()
            );
            assert!(ws.run(&["queue", "apply"]).status.success());
        }
    }
}

#[test]
fn shortened_synthesis_citations_cannot_bypass_source_validation_or_be_pruned() {
    for malformed in [
        serde_json::json!([]),
        serde_json::json!(null),
        serde_json::json!({"bad":"shape"}),
    ] {
        let ws = Workspace::new();
        ws.write("wiki/concepts/example.md", "---\ntype: concept\nllm_inputs:\n  synthesis: \"00000000000000000000000000000001\"\n---\n## Synthesis\nKeep.\n\n## Related\n\n");
        let mut task = serde_json::json!({"task_id":"bad-input", "kind":"synthesize-concept", "created_at":"2026-10-16T00:00:00Z", "cache_hash":"00000000000000000000000000000001",
        "input":{"locale":"en","citations":[],"related_anchor":"## Related"}, "target":{"vault_path":"wiki/concepts/example.md","kind":"concept-synthesis","anchor":"## Synthesis"}});
        task["input"]["citations"] = malformed;
        ws.write(".lorekeeper/queue/tasks.jsonl", &task.to_string());
        let status: serde_json::Value =
            serde_json::from_slice(&ws.run(&["queue", "status", "--json"]).stdout).unwrap();
        assert_eq!(status["data"]["unreadable"], 1);
        let result = serde_json::json!({"task_id":"bad-input","cache_hash":task["cache_hash"],"target_revision":status["data"]["tasks"][0]["target_revision"],"synthesis":"Unsupported.","related":""});
        ws.write(
            ".lorekeeper/queue/results/bad-input.synthesis.json",
            &result.to_string(),
        );
        assert!(!ws.run(&["queue", "apply"]).status.success());
        assert!(ws.read("wiki/concepts/example.md").contains("Keep."));
        assert!(!ws.run(&["queue", "prune"]).status.success());
        assert!(ws.vault().join(".lorekeeper/queue/tasks.jsonl").exists());
    }
}

#[test]
fn staged_sections_are_snapshot_guarded_and_only_the_applier_stamps_completion() {
    for scenario in [
        "summary",
        "empty",
        "theme",
        "refine",
        "changed-target",
        "stale",
        "wrong-kind",
        "missing-anchor",
        "changed-event-headings",
        "sibling-heading",
        "unknown-policy",
        "mutated-input",
        "missing-input",
    ] {
        let ws = Workspace::new();
        let (kind, target_kind, key) = match scenario {
            "theme" => ("identify-themes", "weekly-synthesis-themes", "themes"),
            "refine" | "changed-event-headings" => {
                ("refine-events", "daily-refine-events", "refine_events")
            }
            _ => ("summarize", "daily-summary", "summary"),
        };
        let input = if scenario == "theme" {
            serde_json::json!({"text":"Source material", "max_themes":3, "locale":"en"})
        } else {
            serde_json::json!({"text":"Source material", "max_sentences":3, "locale":"en"})
        };
        let hash = lk_queue::cache_hash(input.as_object().unwrap());
        let page = format!(
            "---\ntype: daily\nllm_inputs:\n  {key}: {hash}\n---\n## Target\n### Original Event\nRaw source.\n\n## Notes\nPreserve.\n"
        );
        ws.write("daily/source/page.md", &page);
        let mut task = serde_json::json!({"task_id":"section-1","kind":kind,"created_at":"2026-10-16T00:00:00Z","cache_hash":hash,"input":input,"target":{"vault_path":"daily/source/page.md","kind":target_kind,"anchor":"## Target"}});
        if scenario == "mutated-input" {
            task["input"]["text"] = "Substituted source".into();
        }
        if scenario == "missing-input" {
            task["input"].as_object_mut().unwrap().remove("text");
        }
        if scenario == "wrong-kind" {
            task["kind"] = serde_json::json!("extract-concepts");
        }
        if scenario == "unknown-policy" {
            task["input"]["generation_policy"] = serde_json::json!("future-policy");
        }
        ws.write(".lorekeeper/queue/tasks.jsonl", &task.to_string());
        let status: serde_json::Value =
            serde_json::from_slice(&ws.run(&["queue", "status", "--json"]).stdout).unwrap();
        assert!(status["data"]["tasks"][0]["target_revision"].is_string());
        let body = match scenario {
            "empty" => "",
            "refine" => "### Original Event\nRefined evidence.",
            "changed-event-headings" => "### Different Event\nWrong.",
            "sibling-heading" => "## Notes\nOverwrite.",
            "theme" => "### 1. Theme\nGrounded theme.",
            _ => "Grounded summary.",
        };
        let result = serde_json::json!({"task_id":"section-1","cache_hash":hash,"target_revision":status["data"]["tasks"][0]["target_revision"],"body":body});
        ws.write(
            ".lorekeeper/queue/results/section-1.section.json",
            &result.to_string(),
        );
        match scenario {
            "changed-target" => ws.write(
                "daily/source/page.md",
                &page.replace("Preserve.", "Human edit."),
            ),
            "stale" => ws.write("daily/source/page.md", &page.replace(&hash, "newer")),
            "missing-anchor" => ws.write(
                "daily/source/page.md",
                &page.replace("## Target", "## Renamed"),
            ),
            _ => {}
        }
        let before = ws.read("daily/source/page.md");
        let accepted = matches!(scenario, "summary" | "empty" | "theme" | "refine");
        let dropped = matches!(scenario, "stale" | "missing-anchor");
        let dry = ws.run(&["queue", "apply", "--dry-run"]);
        assert_eq!(
            dry.status.success(),
            accepted || dropped,
            "{scenario}: {}",
            String::from_utf8_lossy(&dry.stderr)
        );
        assert_eq!(ws.read("daily/source/page.md"), before);
        let applied = ws.run(&["queue", "apply"]);
        assert_eq!(
            applied.status.success(),
            accepted || dropped,
            "{scenario}: {}",
            String::from_utf8_lossy(&applied.stderr)
        );
        if accepted {
            let after = ws.read("daily/source/page.md");
            assert!(after.contains(&format!("{key}_done: {hash}")));
            assert!(after.contains("Preserve."));
            assert!(after.contains(body));
            assert!(ws.run(&["queue", "apply"]).status.success());
            assert_eq!(ws.read("daily/source/page.md"), after);
        } else {
            assert_eq!(ws.read("daily/source/page.md"), before);
        }
        assert_eq!(
            ws.vault()
                .join(".lorekeeper/queue/results/section-1.section.json")
                .exists(),
            !accepted && !dropped
        );
        if matches!(scenario, "mutated-input" | "missing-input") {
            assert_eq!(status["data"]["tasks"][0]["status"], "unreadable");
            assert!(!ws.run(&["queue", "prune"]).status.success());
            assert!(ws.vault().join(".lorekeeper/queue/tasks.jsonl").exists());
        }
    }
}

#[test]
fn queue_apply_refuses_a_busy_vault_mutation_session_before_consuming_results() {
    let ws = Workspace::new();
    ws.write(".lorekeeper/queue/results/bad.section.json", "{truncated");
    let _held = lk_vault::MutationSession::acquire(&ws.vault()).unwrap();
    let out = ws.run(&["queue", "apply"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("vault mutation"));
    assert!(
        ws.vault()
            .join(".lorekeeper/queue/results/bad.section.json")
            .exists()
    );
    assert!(
        !ws.vault()
            .join(".lorekeeper/queue/results/corrupt")
            .exists()
    );
}

#[test]
fn explicit_regeneration_reuses_retained_input_and_stamps_policy_without_global_invalidation() {
    use lk_queue::TaskRequest;
    for scenario in [
        "processed",
        "historical-policy",
        "pending",
        "changed-input",
        "missing",
        "ambiguous",
        "missing-input",
    ] {
        let ws = Workspace::new();
        let req = lk_queue::SummarizeRequest {
            text: "Retained complete source text.".into(),
            max_sentences: 5,
            focus: None,
            locale: lk_core::i18n::Locale::En,
            source_type: None,
            target: lk_queue::TaskTarget {
                vault_path: "daily/source/day.md".into(),
                kind: lk_queue::TargetKind::DailySummary,
                anchor: "## Summary".into(),
            },
        };
        let hash = req.cache_hash();
        let mut input = req.task_input();
        input.insert("locale".into(), "en".into());
        let mut task = serde_json::json!({"task_id":"retained","kind":"summarize","created_at":"2026-10-16T00:00:00Z","cache_hash":hash,"input":input,"target":req.target});
        if scenario == "historical-policy" {
            task["input"]["generation_policy"] = serde_json::json!("previous-policy-v0");
        }
        if scenario == "missing-input" {
            task["input"].as_object_mut().unwrap().remove("text");
        }
        let page = format!(
            "---\ntype: daily\nllm_inputs:\n  summary: \"{hash}\"\n  summary_done: \"{hash}\"\n---\n## Summary\nPreserved authored summary.\n"
        );
        ws.write("daily/source/day.md", &page);
        let retained = if scenario == "pending" {
            ".lorekeeper/queue/retained.jsonl"
        } else {
            ".lorekeeper/queue/processed/retained.jsonl"
        };
        if scenario != "missing" {
            ws.write(retained, &task.to_string());
        }
        if scenario == "ambiguous" {
            ws.write(".lorekeeper/queue/duplicate.jsonl", &task.to_string());
        }
        if scenario == "changed-input" {
            ws.write("daily/source/day.md", &page.replace(&hash, "changed"));
        }
        let before = ws.read("daily/source/day.md");
        let args = ["queue", "regenerate", "--task-id", "retained"];
        let preview = ws.run(&[args.as_slice(), &["--dry-run"]].concat());
        let accepted = matches!(scenario, "processed" | "pending" | "historical-policy");
        assert_eq!(
            preview.status.success(),
            accepted,
            "{scenario}: {}",
            String::from_utf8_lossy(&preview.stderr)
        );
        assert_eq!(ws.read("daily/source/day.md"), before);
        let out = ws.run(&args);
        assert_eq!(
            out.status.success(),
            accepted,
            "{scenario}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        if !accepted {
            assert_eq!(ws.read("daily/source/day.md"), before);
            continue;
        }
        let selected: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        let new_id = selected["data"]["task_id"].as_str().unwrap();
        assert!(
            ws.read("daily/source/day.md")
                .contains("Preserved authored summary.")
        );
        let status: serde_json::Value =
            serde_json::from_slice(&ws.run(&["queue", "status", "--json"]).stdout).unwrap();
        let row = status["data"]["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["task_id"] == new_id)
            .unwrap();
        assert_eq!(row["status"], "current");
        if scenario == "pending" {
            let old = status["data"]["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["task_id"] == "retained")
                .unwrap();
            assert_eq!(
                old["status"], "stale",
                "legacy task cannot satisfy explicit new-policy request"
            );
        }
        let result = serde_json::json!({"task_id":new_id,"cache_hash":hash,"target_revision":row["target_revision"],"body":"Newly reviewed summary."});
        ws.write(
            &format!(".lorekeeper/queue/results/{new_id}.section.json"),
            &result.to_string(),
        );
        let applied = ws.run(&["queue", "apply"]);
        assert!(
            applied.status.success(),
            "{}",
            String::from_utf8_lossy(&applied.stderr)
        );
        let after = ws.read("daily/source/day.md");
        assert!(
            after.contains("summary_policy_done: \"summary-v1\"")
                || after.contains("summary_policy_done: summary-v1")
        );
        assert!(after.contains("Newly reviewed summary."));
    }
}

#[test]
fn regeneration_of_answered_synthesis_still_checks_current_source_revisions() {
    use lk_queue::TaskRequest;
    for changed in [false, true] {
        let ws = Workspace::new();
        let source = "---\ntype: document\n---\nOriginal evidence.\n";
        ws.write("wiki/documents/source.md", source);
        let citation = lk_core::concept::Citation::new(
            Path::new("wiki/documents/source.md"),
            &lk_core::frontmatter::parse_page(source).unwrap(),
        );
        let req = lk_queue::ConceptSynthesisRequest {
            citations: vec![citation],
            locale: lk_core::i18n::Locale::En,
            related_anchor: Some("## Related".into()),
            target: lk_queue::TaskTarget {
                vault_path: "wiki/concepts/example.md".into(),
                kind: lk_queue::TargetKind::ConceptSynthesis,
                anchor: "## Synthesis".into(),
            },
        };
        let hash = req.cache_hash();
        let mut input = req.task_input();
        input.insert("locale".into(), "en".into());
        let task = serde_json::json!({"task_id":"retained-synthesis","kind":"synthesize-concept","created_at":"2026-10-16T00:00:00Z","cache_hash":hash,"input":input,"target":req.target});
        ws.write(
            ".lorekeeper/queue/processed/retained.jsonl",
            &task.to_string(),
        );
        let page = format!(
            "---\ntype: concept\nllm_inputs:\n  synthesis: \"{hash}\"\n  synthesis_done: \"{hash}\"\n---\n## Synthesis\nPreserve authored knowledge.\n\n## Related\n\n"
        );
        ws.write("wiki/concepts/example.md", &page);
        if changed {
            ws.write(
                "wiki/documents/source.md",
                &source.replace("Original", "Changed"),
            );
        }
        let result = ws.run(&["queue", "regenerate", "--task-id", "retained-synthesis"]);
        assert_eq!(
            result.status.success(),
            !changed,
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(
            ws.read("wiki/concepts/example.md")
                .contains("Preserve authored knowledge.")
        );
        if changed {
            assert_eq!(ws.read("wiki/concepts/example.md"), page);
        }
    }
}

#[cfg(unix)]
#[test]
fn a_section_target_outside_the_vault_is_unreadable_and_never_published() {
    let ws = Workspace::new();
    let external = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(external.path(), ws.vault().join("alias")).unwrap();
    let req = lk_queue::SummarizeRequest {
        text: "Original evidence".into(),
        max_sentences: 3,
        focus: None,
        locale: lk_core::i18n::Locale::En,
        source_type: None,
        target: lk_queue::TaskTarget {
            vault_path: "alias/page.md".into(),
            kind: lk_queue::TargetKind::DailySummary,
            anchor: "## Summary".into(),
        },
    };
    let hash = req.cache_hash();
    let page =
        format!("---\ntype: daily\nllm_inputs:\n  summary: {hash}\n---\n## Summary\nOriginal.\n");
    std::fs::write(external.path().join("page.md"), &page).unwrap();
    let task = serde_json::json!({"task_id":"outside", "kind":"summarize", "created_at":"2026-10-16T00:00:00Z", "cache_hash":hash, "input":req.task_input(), "target":req.target});
    ws.write(".lorekeeper/queue/tasks.jsonl", &task.to_string());
    let status: serde_json::Value =
        serde_json::from_slice(&ws.run(&["queue", "status", "--json"]).stdout).unwrap();
    assert_eq!(status["data"]["tasks"][0]["status"], "unreadable");
    assert!(status["data"]["tasks"][0]["target_revision"].is_null());
    let result = serde_json::json!({"task_id":"outside", "cache_hash":hash, "target_revision":lk_core::concept::file_revision(&page), "body":"Overwrite outside."});
    let path = ".lorekeeper/queue/results/outside.section.json";
    ws.write(path, &result.to_string());
    assert!(!ws.run(&["queue", "apply", "--dry-run"]).status.success());
    assert!(!ws.run(&["queue", "apply"]).status.success());
    assert_eq!(
        std::fs::read_to_string(external.path().join("page.md")).unwrap(),
        page
    );
    assert!(ws.vault().join(path).exists());
    let pruned = ws.run(&["queue", "prune"]);
    assert!(!pruned.status.success());
    assert!(ws.vault().join(".lorekeeper/queue/tasks.jsonl").exists());
}
