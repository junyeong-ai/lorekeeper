use super::*;

#[test]
fn daily_review_reassessment_defers_without_pausing_and_invalid_reviews_remain_fatal() {
    for invalid in [false, true] {
        let fixture = Fixture::new("codex", 30, "exit 91");
        configure(&fixture, Some(20), 60);
        write_review_attention(&fixture, "wiki/documents/reviewed.md", invalid);
        let review = fixture.root().join("vault/wiki/explorations/review.md");
        let original_review = std::fs::read(&review).unwrap();
        let original_config = std::fs::read(&fixture.config).unwrap();
        let output = fixture
            .command()
            .args(["run", "daily", "--scheduled", "--json"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["ok"], false, "{report}");
        assert_eq!(report["halted"], invalid, "{report}");
        assert_eq!(report["semantic_calls"], 0, "{report}");
        assert_eq!(
            report["review_health"][if invalid {
                "invalid"
            } else {
                "needs_reassessment"
            }],
            1,
            "{report}"
        );
        assert_eq!(report["automation"]["reserved_calls"], 0);
        let usage_path = fixture
            .root()
            .join("vault/.lorekeeper/automation-usage.json");
        let before = if invalid {
            Some(std::fs::read(&usage_path).unwrap())
        } else {
            assert!(!usage_path.try_exists().unwrap());
            None
        };
        let repeated = run(&fixture, "daily", false);
        assert_eq!(repeated["ok"], false, "{repeated}");
        assert_eq!(repeated["semantic_calls"], 0, "{repeated}");
        assert_eq!(repeated["automation"]["reserved_calls"], 0);
        if let Some(before) = before {
            let state: serde_json::Value = serde_json::from_slice(&before).unwrap();
            assert_eq!(state["reserved_calls"], 0);
            assert!(state["paused"]["daily"]["at"].is_string());
            assert!(state["paused"]["daily"]["cause"]["stage"].is_string());
            assert_eq!(std::fs::read(usage_path).unwrap(), before);
        } else {
            assert!(!usage_path.try_exists().unwrap());
        }
        assert_eq!(std::fs::read(review).unwrap(), original_review);
        assert_eq!(std::fs::read(&fixture.config).unwrap(), original_config);
        assert!(!fixture.root().join("arguments").exists());
    }
}

#[test]
fn an_unrecordable_concept_defers_graph_stages_and_unreadable_pages_remain_fatal() {
    for unreadable in [None, Some("backlinks-sync"), Some("graph lint")] {
        let fixture = Fixture::new("codex", 30, "exit 91");
        configure(&fixture, Some(20), 60);
        let wiki = fixture.root().join("vault/wiki");
        std::fs::create_dir_all(wiki.join("concepts")).unwrap();
        std::fs::create_dir_all(wiki.join("documents")).unwrap();
        std::fs::write(
            wiki.join("concepts/owed.md"),
            "---\ntype: concept\ntitle: Owed\n---\n# Owed\n\n## Synthesis\n\n## Sources\n\n## Related\n",
        )
        .unwrap();
        std::fs::write(
            wiki.join("concepts/legacy.md"),
            "---\ntitle: Legacy\n---\n# Legacy\n\n## Synthesis\n",
        )
        .unwrap();
        let mut source = b"---\ntype: document\ntitle: Source\n---\nSee [Owed](../concepts/owed.md) and [Legacy](../concepts/legacy.md).\n".to_vec();
        match unreadable {
            Some("backlinks-sync") => source.push(0xff),
            Some(_) => {
                let notes = fixture.root().join("vault/notes");
                std::fs::create_dir_all(&notes).unwrap();
                std::fs::write(notes.join("private.md"), b"Personal note\n\xff").unwrap();
            }
            None => {}
        }
        std::fs::write(wiki.join("documents/source.md"), source).unwrap();

        let report = run(&fixture, "process", false);
        assert_eq!(report["ok"], false, "{report}");
        assert_eq!(
            report["runs"][0]["halted"],
            unreadable.is_some(),
            "{report}"
        );
        if let Some(stage) = unreadable {
            assert_eq!(
                usage(&fixture)["paused"]["process"]["cause"]["stage"],
                stage,
                "{report}"
            );
            let repeated = run(&fixture, "process", false);
            assert_eq!(
                repeated["runs"][0]["automation"]["stop"], "paused",
                "{repeated}"
            );
            continue;
        }
        assert_eq!(
            calls(&report),
            1,
            "independent synthesis must proceed: {report}"
        );
        let stages = report["runs"][0]["stages"].as_array().unwrap();
        for name in ["backlinks-sync", "graph lint"] {
            assert!(
                stages
                    .iter()
                    .any(|stage| stage["name"] == name && stage["state"] == "failed"),
                "{name} must defer: {report}"
            );
        }
        assert!(stages.iter().any(|stage| {
            stage["name"] == "backlinks-sync"
                && stage["detail"]
                    .as_str()
                    .is_some_and(|detail| detail.contains("wiki/concepts/legacy.md"))
        }));
        let owed = std::fs::read_to_string(wiki.join("concepts/owed.md")).unwrap();
        assert!(owed.contains("source_count: 1"), "{owed}");
        assert!(usage(&fixture)["paused"].as_object().unwrap().is_empty());
        let repeated = run(&fixture, "process", false);
        assert_eq!(repeated["runs"][0]["halted"], false, "{repeated}");
        assert!(usage(&fixture)["paused"].as_object().unwrap().is_empty());
    }
}

#[test]
fn inspection_timeouts_hold_until_the_host_answers_and_an_exact_explicit_retry_clears_them() {
    for engine in ["codex", "claude-code"] {
        let fixture = bounded_audit_fixture(engine, 1, "exec sleep 30");
        let config = std::fs::read_to_string(&fixture.config).unwrap().replace(
            "timeout_seconds: 30",
            &format!("timeout_seconds: {INVOCATION_TIMEOUT_SECONDS}"),
        );
        std::fs::write(&fixture.config, config).unwrap();
        configure(&fixture, Some(20), 60);
        let report = run(&fixture, "knowledge", false);
        assert_eq!(calls(&report), 1, "{report}");
        assert_eq!(report["runs"][0]["halted"], false, "{report}");
        assert_eq!(
            report["runs"][0]["work_steps"][0]["host_failure"]["phase"], "invocation",
            "{report}"
        );
        assert_eq!(
            report["runs"][0]["work_steps"][0]["host_failure"]["termination"], "timed-out",
            "{report}"
        );
        assert!(usage(&fixture)["paused"].as_object().unwrap().is_empty());
        for (resume, sent) in [(false, 1), (true, 0)] {
            let probed = run(&fixture, "knowledge", resume);
            assert_eq!(
                calls(&probed),
                sent,
                "a run with nothing admissible probes the held node once, and its failure spaces the next probe: {probed}"
            );
            assert_eq!(usage(&fixture)["reserved_calls"], 2);
        }
        let path = walkdir::WalkDir::new(fixture.root().join("vault/.lorekeeper/work"))
            .into_iter()
            .map(Result::unwrap)
            .find(|entry| entry.file_name() == "inspection-timeouts.json")
            .unwrap()
            .into_path();
        let before = std::fs::read(&path).unwrap();
        let held: serde_json::Value = serde_json::from_slice(&before).unwrap();
        let work = held["work_id"].as_str().unwrap();
        let node = held["entries"].as_object().unwrap().keys().next().unwrap();
        assert!(
            held["entries"][node.as_str()].get("exhausted").is_none(),
            "a probe that times out again renews the hold: {held}"
        );
        let plan = std::fs::read(path.parent().unwrap().join("plan.json")).unwrap();
        for dry in [true, false] {
            let mut command = fixture.command();
            command.args(["work", "retry", work, node]);
            if dry {
                command.arg("--dry-run");
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            if dry {
                assert_eq!(std::fs::read(&path).unwrap(), before);
            }
        }
        assert_eq!(
            std::fs::read(path.parent().unwrap().join("plan.json")).unwrap(),
            plan
        );
        assert_eq!(usage(&fixture)["reserved_calls"], 2);
        let agent = fixture.root().join("agent executable");
        let behavior = std::fs::read_to_string(&agent)
            .unwrap()
            .replace("exec sleep 30", RECORD_BOUNDED_AUDIT);
        std::fs::write(agent, behavior).unwrap();
        let report = run(&fixture, "knowledge", false);
        assert_eq!(calls(&report), 1, "{report}");
        assert_eq!(
            report["runs"][0]["retained_progress"]["evidence_nodes"], 1,
            "{report}"
        );
        assert_eq!(usage(&fixture)["reserved_calls"], 3);
        assert_eq!(
            std::fs::read(path.parent().unwrap().join("plan.json")).unwrap(),
            plan
        );
    }
}

#[test]
fn a_probe_wave_sends_one_call_until_its_host_gives_an_outcome() {
    // Each call leaves a marker named by its pid, which stays the host's pid after `exec`; a
    // call counts the markers whose process is still alive, so a finished call never counts.
    let tracked = r#"if [ -f "$TEST_EXECUTION_ROOT/fail" ]; then exit 7; fi
mkdir -p "$TEST_EXECUTION_ROOT/inflight"
touch "$TEST_EXECUTION_ROOT/inflight/$$"
sleep 1
alive=0
for marker in "$TEST_EXECUTION_ROOT"/inflight/*; do
  if kill -0 "${marker##*/}" 2>/dev/null; then alive=$((alive + 1)); fi
done
echo "$alive" >> "$TEST_EXECUTION_ROOT/concurrency"
"#;
    for (engine, inherited) in [("codex", false), ("claude-code", false), ("codex", true)] {
        let behavior = format!("{tracked}{RECORD_BOUNDED_AUDIT}");
        let fixture = bounded_audit_fixture(engine, 64, &behavior);
        let subject = fixture.root().join("vault/wiki/concepts/large.md");
        let body = std::fs::read_to_string(&subject).unwrap()
            + &"\nFurther evidence clause that widens the partitioned Work.\n".repeat(540);
        std::fs::write(subject, body).unwrap();
        record_discovery(&fixture, "wiki/concepts/large.md");
        let config = std::fs::read_to_string(&fixture.config)
            .unwrap()
            .replace("max_calls: 64}", "max_calls: 64, max_parallel: 4}");
        std::fs::write(&fixture.config, config).unwrap();
        configure(&fixture, Some(1), 60);
        std::fs::write(fixture.root().join("fail"), "").unwrap();
        let held = run(&fixture, "knowledge", false);
        assert_eq!(calls(&held), 1, "{held}");
        std::fs::remove_file(fixture.root().join("fail")).unwrap();
        let config = std::fs::read_to_string(&fixture.config)
            .unwrap()
            .replace("max_calls_per_day: 1,", "max_calls_per_day: 20,");
        std::fs::write(&fixture.config, config).unwrap();
        if inherited {
            release_held_node_as_probe(&fixture);
        }

        let probed = run(&fixture, "knowledge", false);
        assert_eq!(
            probed["runs"][0]["stages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|stage| stage["name"] == "host probe"),
            !inherited,
            "a probe an earlier run released needs no new one: {probed}"
        );
        let concurrency = std::fs::read_to_string(fixture.root().join("concurrency"))
            .unwrap()
            .lines()
            .map(|line| line.parse::<usize>().unwrap())
            .collect::<Vec<_>>();
        assert!(concurrency.len() >= 3, "{concurrency:?}: {probed}");
        assert_eq!(
            concurrency[0], 1,
            "the probe is sent alone until its host gives an outcome (inherited: {inherited}): {probed}"
        );
        assert!(
            concurrency[1..].iter().any(|count| *count > 1),
            "the host's answer restores parallel waves: {concurrency:?}"
        );
    }
}

/// Leaves the held inspection node released as the vault's probe, as a run stopped between
/// releasing it and sending it does.
fn release_held_node_as_probe(fixture: &Fixture) {
    let path = walkdir::WalkDir::new(fixture.root().join("vault/.lorekeeper/work"))
        .into_iter()
        .map(Result::unwrap)
        .find(|entry| entry.file_name() == "inspection-timeouts.json")
        .unwrap()
        .into_path();
    let held: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let (node, entry) = held["entries"].as_object().unwrap().iter().next().unwrap();
    let failure = &entry["failure"];
    let ledger = fixture
        .root()
        .join("vault/.lorekeeper/queue/host-answers.json");
    let mut answers: serde_json::Value = std::fs::read(&ledger)
        .map(|raw| serde_json::from_slice(&raw).unwrap())
        .unwrap_or_else(|_| serde_json::json!({"schema":"host-answers-v1","answers":{}}));
    answers["probe"] = serde_json::json!({
        "identity": {"store": "inspection", "work_id": held["work_id"], "node_id": node},
        "engine": failure["engine"],
        "host": {
            "profile": lk_core::rss_original::digest(
                failure["host"]["profile"].as_str().unwrap().as_bytes()
            ),
            "at": failure["host"]["at"],
        },
    });
    std::fs::create_dir_all(ledger.parent().unwrap()).unwrap();
    std::fs::write(ledger, serde_json::to_vec(&answers).unwrap()).unwrap();
}

#[test]
fn a_parallel_inspection_timeout_does_not_pause_or_discard_independent_audits() {
    for engine in ["codex", "claude-code"] {
        let behavior = RECORD_BOUNDED_AUDIT.replacen("if action=='inspect':", r#"if action=='inspect':
    subject=json.loads((root/'vault/.lorekeeper/work'/work_id/'plan.json').read_text())['context']['subject']['path']
    if subject=='wiki/concepts/large.md':
        import time
        time.sleep(30)
"#, 1);
        let fixture = bounded_audit_fixture(engine, 64, &behavior);
        let config = std::fs::read_to_string(&fixture.config)
            .unwrap()
            .replace(
                "timeout_seconds: 30",
                &format!("timeout_seconds: {INVOCATION_TIMEOUT_SECONDS}"),
            )
            .replace("max_calls: 64}", "max_calls: 64, max_parallel: 2}")
            .replace("batch_size: 1", "batch_size: 2");
        std::fs::write(&fixture.config, config).unwrap();
        write_audit_concept(&fixture, "healthy");
        record_discovery(&fixture, "wiki/concepts/healthy.md");
        configure(&fixture, Some(20), 60);
        let report = run(&fixture, "knowledge", false);
        let runs = report["runs"].as_array().unwrap();
        assert!(runs.iter().all(|run| run["halted"] == false), "{report}");
        assert!(
            runs.iter()
                .filter_map(|run| run["audit"].as_array())
                .flatten()
                .any(|audit| audit["assessed_path"] == "wiki/concepts/healthy.md"),
            "{report}"
        );
        assert!(usage(&fixture)["paused"].as_object().unwrap().is_empty());
        // With nothing else admissible, the next run sends only the timed-out node released
        // as a probe, although its sibling node of the same subject is still held.
        let reserved = usage(&fixture)["reserved_calls"].as_u64().unwrap();
        let held = run(&fixture, "knowledge", false);
        assert_eq!(calls(&held), 1, "{held}");
        assert!(
            held["runs"][0]["stages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|stage| stage["name"] == "host probe"),
            "{held}"
        );
        assert_eq!(usage(&fixture)["reserved_calls"], reserved + 1);
    }
}

#[test]
fn a_scheduled_run_checks_only_the_hosts_its_stages_use() {
    let fixture = bounded_audit_fixture("codex", 64, RECORD_BOUNDED_AUDIT);
    super::stage_profiles::route_profiles(
        &fixture,
        "codex",
        &[("audit", "auditor")],
        &[("auditor", "codex")],
    );
    configure(&fixture, Some(20), 60);
    std::fs::remove_file(fixture.root().join("auditor executable")).unwrap();
    let host_environment = |report: &serde_json::Value| {
        report["runs"][0]["stages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|stage| stage["name"] == "host environment")
            .map(|stage| stage["detail"].as_str().unwrap().to_owned())
    };
    let process = run(&fixture, "process", false);
    assert_eq!(
        host_environment(&process),
        None,
        "process never audits: {process}"
    );
    let knowledge = run(&fixture, "knowledge", false);
    assert!(
        host_environment(&knowledge).is_some_and(|detail| detail.contains("profile auditor")),
        "{knowledge}"
    );
}

fn usage_paused_is_empty(raw: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(raw).unwrap()["paused"]
        .as_object()
        .is_none_or(serde_json::Map::is_empty)
}

#[test]
fn a_scheduled_daily_run_without_semantic_hosts_checks_no_host() {
    let fixture = Fixture::new("codex", 30, "exit 91");
    let config = std::fs::read_to_string(&fixture.config).unwrap().replace(
        "execution:\n  default_profile: primary\n",
        "llm: {provider: noop}\nexecution:\n  operations: {process: primary}\n",
    );
    std::fs::write(&fixture.config, config).unwrap();
    configure(&fixture, Some(20), 60);
    let report = run(&fixture, "daily", false);
    assert!(
        !report["stages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|stage| stage["name"] == "host environment"),
        "{report}"
    );
    let usage = fixture
        .root()
        .join("vault/.lorekeeper/automation-usage.json");
    assert!(
        !usage.exists() || usage_paused_is_empty(&std::fs::read(&usage).unwrap()),
        "{report}"
    );
}

#[test]
fn a_scheduled_knowledge_run_without_semantic_hosts_skips_its_audit_without_pausing() {
    let fixture = Fixture::new("codex", 30, "exit 91");
    let config = std::fs::read_to_string(&fixture.config).unwrap().replace(
        "execution:\n  default_profile: primary\n",
        "llm: {provider: noop}\nexecution:\n  operations: {process: primary}\n",
    );
    std::fs::write(&fixture.config, config).unwrap();
    configure(&fixture, Some(20), 60);
    let directory = fixture.root().join("vault/wiki/concepts");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join("undiscovered.md"),
        include_str!("../behaviour/corpus/small/vault/wiki/concepts/unlinked-note.md"),
    )
    .unwrap();
    let report = run(&fixture, "knowledge", false);
    assert_eq!(calls(&report), 0, "{report}");
    let run = &report["runs"][0];
    assert_eq!(run["halted"], false, "{report}");
    let audit = run["stages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|stage| stage["name"] == "audit")
        .unwrap_or_else(|| panic!("{report}"));
    assert_eq!(audit["state"], "skipped", "{report}");
    let usage = fixture
        .root()
        .join("vault/.lorekeeper/automation-usage.json");
    assert!(
        !usage.exists() || usage_paused_is_empty(&std::fs::read(&usage).unwrap()),
        "{report}"
    );
}

#[test]
fn a_scheduled_run_whose_host_account_directory_is_gone_defers_its_work_without_a_pause() {
    let fixture = bounded_audit_fixture("codex", 64, RECORD_BOUNDED_AUDIT);
    configure(&fixture, Some(20), 60);
    let missing = fixture.root().join("deleted-account");
    let output = fixture
        .command()
        .env("CODEX_HOME", &missing)
        .args(["run", "knowledge", "--scheduled", "--json"])
        .output()
        .unwrap();
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(!output.status.success(), "the job is incomplete: {report}");
    assert_eq!(calls(&report), 0, "{report}");
    let first = &report["runs"][0];
    assert_eq!(first["halted"], false, "{report}");
    let stage = first["stages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|stage| stage["name"] == "host environment")
        .unwrap_or_else(|| panic!("{report}"));
    assert_eq!(stage["state"], "failed", "{report}");
    let detail = stage["detail"].as_str().unwrap();
    assert!(
        detail.contains("CODEX_HOME") && detail.contains("deleted-account"),
        "{detail}"
    );
    let usage_file = fixture
        .root()
        .join("vault/.lorekeeper/automation-usage.json");
    assert!(
        !usage_file.exists() || usage_paused_is_empty(&std::fs::read(&usage_file).unwrap()),
        "a host environment is a current condition, not a pause: {report}"
    );
    for store in [
        "vault/.lorekeeper/audit-preparation-attempts",
        "vault/.lorekeeper/queue/generation-rejections",
    ] {
        assert!(!fixture.root().join(store).exists(), "{store}: {report}");
    }
    assert!(
        walkdir::WalkDir::new(fixture.root().join("vault/.lorekeeper"))
            .into_iter()
            .map(Result::unwrap)
            .all(|entry| entry.file_name() != "inspection-timeouts.json"),
        "no item is held"
    );
    // The next scheduled run checks again and, with the directory back, proceeds.
    let recovered = run(&fixture, "knowledge", false);
    assert!(calls(&recovered) > 0, "{recovered}");
    assert!(
        recovered["runs"][0]["stages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|stage| stage["name"] != "host environment"),
        "{recovered}"
    );
}

#[test]
fn a_scheduled_run_defers_only_the_work_of_a_profile_that_cannot_start() {
    let fixture = bounded_audit_fixture("codex", 64, RECORD_BOUNDED_AUDIT);
    super::stage_profiles::route_profiles(
        &fixture,
        "codex",
        &[("reduce", "reducer")],
        &[("reducer", "codex")],
    );
    configure(&fixture, Some(64), 120);
    let reducer = fixture.root().join("reducer executable");
    let saved = std::fs::read(&reducer).unwrap();
    std::fs::remove_file(&reducer).unwrap();
    let report = run(&fixture, "knowledge", false);
    let runs = report["runs"].as_array().unwrap();
    assert!(runs.iter().all(|run| run["halted"] == false), "{report}");
    assert!(
        runs.iter().any(|run| run["stages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|stage| stage["name"] == "host environment"
                && stage["state"] == "failed"
                && stage["detail"]
                    .as_str()
                    .unwrap()
                    .contains("profile reducer"))),
        "{report}"
    );
    let inspected = calls(&report);
    assert!(inspected > 0, "the healthy profile keeps working: {report}");
    assert!(
        super::stage_profiles::routed_calls(&fixture)
            .iter()
            .all(|call| call["profile"] == "primary"),
        "{report}"
    );
    assert!(
        runs.iter()
            .all(|run| run["audit"].as_array().is_none_or(Vec::is_empty)),
        "the deferred reduction finishes no audit: {report}"
    );
    assert!(
        usage_paused_is_empty(
            &std::fs::read(
                fixture
                    .root()
                    .join("vault/.lorekeeper/automation-usage.json")
            )
            .unwrap()
        ),
        "{report}"
    );
    // Restored, the profile is checked again by the next run and its work completes.
    std::fs::write(&reducer, saved).unwrap();
    std::fs::set_permissions(&reducer, std::fs::Permissions::from_mode(0o755)).unwrap();
    let recovered = run(&fixture, "knowledge", false);
    assert!(
        recovered["runs"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|run| run["audit"].as_array())
            .flatten()
            .any(|audit| audit["assessed_path"] == "wiki/concepts/large.md"),
        "{recovered}"
    );
}

#[test]
fn an_assessment_whose_host_cannot_start_is_recorded_as_unsent() {
    let fixture = Fixture::new("codex", 30, RECORD_AUDIT);
    let config = std::fs::read_to_string(&fixture.config).unwrap()
        + "graph:\n  audit:\n    batch_size: 1\n    subjects: [concept, document]\n";
    std::fs::write(&fixture.config, config).unwrap();
    write_audit_concept(&fixture, "a-held");
    super::stage_profiles::route_profiles(
        &fixture,
        "codex",
        &[("audit", "auditor")],
        &[("auditor", "codex")],
    );
    configure(&fixture, Some(64), 120);
    std::fs::remove_file(fixture.root().join("auditor executable")).unwrap();
    let report = run(&fixture, "knowledge", false);
    let runs = report["runs"].as_array().unwrap();
    assert!(runs.iter().all(|run| run["halted"] == false), "{report}");
    assert!(
        runs.iter()
            .flat_map(|run| run["work_steps"].as_array().unwrap())
            .any(|step| step["role"] == "finalizer" && step["outcome"] == "unsent"),
        "the assessment that was never sent is in the report: {report}"
    );
}

#[test]
fn discovery_and_selected_requests_whose_host_cannot_start_are_recorded_as_unsent() {
    let unsent = |report: &serde_json::Value, role: &str| {
        report["runs"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|run| run["work_steps"].as_array().unwrap())
            .any(|step| step["role"] == role && step["outcome"] == "unsent")
    };
    // An audit subject without discovery.
    let fixture = Fixture::new("codex", 30, RECORD_AUDIT);
    let config = std::fs::read_to_string(&fixture.config).unwrap()
        + "graph:\n  audit:\n    batch_size: 1\n    subjects: [concept, document]\n";
    std::fs::write(&fixture.config, config).unwrap();
    let concepts = fixture.root().join("vault/wiki/concepts");
    std::fs::create_dir_all(&concepts).unwrap();
    std::fs::write(
        concepts.join("undiscovered.md"),
        include_str!("../behaviour/corpus/small/vault/wiki/concepts/unlinked-note.md"),
    )
    .unwrap();
    // An oversized extraction, which records discovery before planning its Work.
    let (extraction, _) = progressive_extraction_fixture("codex", RECORD_AUDIT, 2, 65536, 16384);
    let config = std::fs::read_to_string(&extraction.config)
        .unwrap()
        .replace(
            "max_input_bytes: 65536, max_result_bytes: 16384",
            "max_input_bytes: 65536, max_result_bytes: 1024",
        );
    std::fs::write(&extraction.config, config).unwrap();
    let source = "Transactional outbox commits an event with its state change.\n\n".repeat(2000);
    let vault = extraction.root().join("vault");
    let queue = vault.join(".lorekeeper/queue");
    let task = lk_queue::QueueLlmClient::new(queue.clone()).build_task(
        "ext",
        lk_queue::TaskKind::ExtractConcepts,
        lk_queue::ExtractConceptsRequest {
            concept_policy: lk_core::config::ConceptPolicy::SupportedNew,
            rss_original: None,
            text: source.clone(),
            source_id: "notes".into(),
            source_type: lk_core::config::SourceType::Manual,
            date: jiff::civil::date(2026, 10, 20),
            focus: None,
            locale: lk_core::i18n::Locale::En,
            categories: Vec::new(),
            target: lk_queue::TaskTarget {
                vault_path: "wiki/documents/source.md".into(),
                kind: lk_queue::TargetKind::DocumentConcepts,
                anchor: "## Related Concepts".into(),
            },
        },
    );
    std::fs::write(vault.join("wiki/documents/source.md"), format!("---\ntype: document\ntitle: Source\nllm_inputs:\n  concepts: {}\n---\n## Summary\nSource observation.\n\n## Content\n{source}\n\n## Related Concepts\n", task.cache_hash)).unwrap();
    std::fs::write(
        queue.join("fixture.jsonl"),
        serde_json::to_string(&task).unwrap() + "\n",
    )
    .unwrap();
    // A full-context extraction sent whole.
    let (selected, _) = progressive_extraction_fixture("codex", RECORD_AUDIT, 0, 65536, 16384);
    let config = std::fs::read_to_string(&selected.config).unwrap().replace(
        "work: {extraction_context: progressive}",
        "work: {extraction_context: full-context}",
    );
    std::fs::write(&selected.config, config).unwrap();
    for (fixture, operation, stage, role) in [
        (&fixture, "knowledge", "discovery", "discovery"),
        (&extraction, "process", "discovery", "discovery"),
        (&selected, "process", "extraction", "finalizer"),
    ] {
        super::stage_profiles::route_profiles(
            fixture,
            "codex",
            &[(stage, "routed")],
            &[("routed", "codex")],
        );
        configure(fixture, Some(64), 120);
        std::fs::remove_file(fixture.root().join("routed executable")).unwrap();
        let report = run(fixture, operation, false);
        assert!(
            report["runs"]
                .as_array()
                .unwrap()
                .iter()
                .all(|run| run["halted"] == false),
            "{operation} {stage}: {report}"
        );
        assert!(
            unsent(&report, role),
            "{operation} {stage}: the request that was never sent is in the report: {report}"
        );
    }
}

#[test]
fn a_scheduled_run_defers_a_profile_whose_host_cannot_start_at_its_first_call() {
    // Each passes the preflight's metadata check, which only finds the executable; the last
    // also answers `--version`, then cannot start for its first semantic call.
    for (form, broken) in [
        (
            "unsupported version",
            "#!/bin/sh\necho 'codex-cli 0.0.1'\n".to_owned(),
        ),
        (
            "missing interpreter",
            "#!/nonexistent/lorekeeper-test-interpreter\n".to_owned(),
        ),
        ("lost after its version", String::new()),
    ] {
        let fixture = bounded_audit_fixture("codex", 64, RECORD_BOUNDED_AUDIT);
        super::stage_profiles::route_profiles(
            &fixture,
            "codex",
            &[("reduce", "reducer")],
            &[("reducer", "codex")],
        );
        configure(&fixture, Some(64), 120);
        let reducer = fixture.root().join("reducer executable");
        let saved = std::fs::read(&reducer).unwrap();
        let broken = if broken.is_empty() {
            let script = String::from_utf8(saved.clone()).unwrap();
            let (shebang, rest) = script.split_once('\n').unwrap();
            format!("{shebang}\n[ \"$1\" = --version ] && chmod -x \"$0\"\n{rest}")
        } else {
            broken
        };
        std::fs::write(&reducer, broken).unwrap();
        let report = run(&fixture, "knowledge", false);
        let runs = report["runs"].as_array().unwrap();
        assert!(
            runs.iter().all(|run| run["halted"] == false),
            "{form}: {report}"
        );
        assert!(
            runs.iter()
                .any(|run| run["stages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|stage| stage["name"] == "host environment"
                        && stage["state"] == "failed"
                        && stage["detail"]
                            .as_str()
                            .unwrap()
                            .contains("profile reducer"))),
            "{form}: {report}"
        );
        assert!(
            calls(&report) > 0,
            "{form}: the healthy profile keeps working: {report}"
        );
        assert!(
            usage_paused_is_empty(
                &std::fs::read(
                    fixture
                        .root()
                        .join("vault/.lorekeeper/automation-usage.json")
                )
                .unwrap()
            ),
            "{form}: {report}"
        );
        // Replaced by a supported host, the profile is checked again and its work completes.
        std::fs::write(&reducer, saved).unwrap();
        let mut permissions = std::fs::metadata(&reducer).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        std::fs::set_permissions(&reducer, permissions).unwrap();
        let recovered = run(&fixture, "knowledge", false);
        assert!(
            recovered["runs"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|run| run["audit"].as_array())
                .flatten()
                .any(|audit| audit["assessed_path"] == "wiki/concepts/large.md"),
            "{form}: {recovered}"
        );
    }
}

#[test]
fn a_knowledge_hold_of_a_profile_no_longer_used_gets_a_turn_beside_new_subjects() {
    let behavior =
        format!("if [ -f \"$TEST_EXECUTION_ROOT/fail\" ]; then exit 7; fi\n{RECORD_BOUNDED_AUDIT}");
    let fixture = bounded_audit_fixture("codex", 1, &behavior);
    configure(&fixture, Some(20), 60);
    std::fs::write(fixture.root().join("fail"), "").unwrap();
    let first = run(&fixture, "knowledge", false);
    assert_eq!(calls(&first), 1, "{first}");
    std::fs::remove_file(fixture.root().join("fail")).unwrap();
    // The held request's host is a profile ordinary work no longer uses, so no answer of
    // ordinary work releases it: only a probe can.
    let path = walkdir::WalkDir::new(fixture.root().join("vault/.lorekeeper/work"))
        .into_iter()
        .map(Result::unwrap)
        .find(|entry| entry.file_name() == "inspection-timeouts.json")
        .unwrap()
        .into_path();
    let mut held: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let node = held["entries"]
        .as_object()
        .unwrap()
        .keys()
        .next()
        .unwrap()
        .clone();
    held["entries"][&node]["failure"]["host"]["profile"] = "retired".into();
    std::fs::write(&path, serde_json::to_vec(&held).unwrap()).unwrap();
    let work = held["work_id"].as_str().unwrap().to_owned();
    let probed = || {
        bounded_calls(&fixture)
            .iter()
            .any(|row| row["work_id"] == work.as_str() && row["node_id"] == node.as_str())
    };
    let mut runs = 0;
    while !probed() {
        runs += 1;
        assert!(
            runs <= 2,
            "the held request waits at most one run for its turn"
        );
        write_audit_concept(&fixture, &format!("fresh-{runs}"));
        let report = run(&fixture, "knowledge", false);
        assert_eq!(calls(&report), 1, "{report}");
    }
    // The probe answered through the current profile while the retired one never did: the
    // settled request is not inherited as a probe again.
    let ledger = fixture
        .root()
        .join("vault/.lorekeeper/queue/host-answers.json");
    write_audit_concept(&fixture, "after");
    run(&fixture, "knowledge", false);
    let answers: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&ledger).unwrap()).unwrap();
    assert!(answers.get("probe").is_none(), "{answers}");
    assert!(
        bounded_calls(&fixture)
            .iter()
            .any(|row| row["node_id"] != node.as_str()),
        "recovery does not take every turn from new subjects"
    );
}

#[test]
fn a_probe_reaches_its_node_while_a_sibling_of_the_same_subject_is_held() {
    for legacy in [false, true] {
        a_probe_reaches_its_node_while_a_sibling_of_the_same_subject_is_held_for(legacy);
    }
}

fn a_probe_reaches_its_node_while_a_sibling_of_the_same_subject_is_held_for(legacy: bool) {
    let behavior =
        format!("if [ -f \"$TEST_EXECUTION_ROOT/fail\" ]; then exit 7; fi\n{RECORD_BOUNDED_AUDIT}");
    let fixture = bounded_audit_fixture("codex", 64, &behavior);
    let config = std::fs::read_to_string(&fixture.config)
        .unwrap()
        .replace("max_calls: 64}", "max_calls: 64, max_parallel: 2}");
    std::fs::write(&fixture.config, config).unwrap();
    configure(&fixture, Some(20), 60);
    std::fs::write(fixture.root().join("fail"), "").unwrap();
    run(&fixture, "knowledge", false);
    std::fs::remove_file(fixture.root().join("fail")).unwrap();
    // Both nodes of one subject failed in one wave. Their host is a profile ordinary work no
    // longer uses, so only a probe releases either.
    let path = walkdir::WalkDir::new(fixture.root().join("vault/.lorekeeper/work"))
        .into_iter()
        .map(Result::unwrap)
        .find(|entry| entry.file_name() == "inspection-timeouts.json")
        .unwrap()
        .into_path();
    let mut held: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let nodes = held["entries"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    assert!(nodes.len() >= 2, "{held}");
    for node in &nodes {
        if legacy {
            // A timeout counted before timeouts became unanswered failures kept neither its
            // engine nor its time.
            let request_revision = held["entries"][node]["request_revision"].clone();
            held["entries"][node] = serde_json::json!({"kind": "timeout", "request_revision": request_revision, "count": 1});
        } else {
            held["entries"][node]["failure"]["host"]["profile"] = "retired".into();
        }
    }
    std::fs::write(&path, serde_json::to_vec(&held).unwrap()).unwrap();
    let work = held["work_id"].as_str().unwrap().to_owned();
    let before = bounded_calls(&fixture).len();
    for _ in 0..2 {
        run(&fixture, "knowledge", false);
    }
    assert!(
        bounded_calls(&fixture)[before..].iter().any(|row| {
            row["work_id"] == work.as_str()
                && nodes.iter().any(|node| row["node_id"] == node.as_str())
        }),
        "the released node is sent although its sibling is still held"
    );
}

#[test]
fn a_ledger_probe_whose_hold_was_renewed_is_retired() {
    let behavior =
        format!("if [ -f \"$TEST_EXECUTION_ROOT/fail\" ]; then exit 7; fi\n{RECORD_BOUNDED_AUDIT}");
    let fixture = bounded_audit_fixture("codex", 1, &behavior);
    configure(&fixture, Some(20), 60);
    std::fs::write(fixture.root().join("fail"), "").unwrap();
    run(&fixture, "knowledge", false);
    std::fs::remove_file(fixture.root().join("fail")).unwrap();
    let path = walkdir::WalkDir::new(fixture.root().join("vault/.lorekeeper/work"))
        .into_iter()
        .map(Result::unwrap)
        .find(|entry| entry.file_name() == "inspection-timeouts.json")
        .unwrap()
        .into_path();
    let held: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let (node, entry) = held["entries"].as_object().unwrap().iter().next().unwrap();
    let failed_at: jiff::Timestamp = entry["failure"]["host"]["at"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    // The ledger still names the hold as released at an earlier failure, which the hold has
    // since renewed: it releases no current request.
    let ledger = fixture
        .root()
        .join("vault/.lorekeeper/queue/host-answers.json");
    let mut answers: serde_json::Value = std::fs::read(&ledger).map_or_else(
        |_| serde_json::json!({"schema": "host-answers-v1", "answers": {}}),
        |raw| serde_json::from_slice(&raw).unwrap(),
    );
    answers["probe"] = serde_json::json!({
        "identity": {"store": "inspection", "work_id": held["work_id"], "node_id": node},
        "engine": "codex",
        "host": {"profile": "0".repeat(64), "at": (failed_at - jiff::SignedDuration::from_hours(1)).to_string()},
    });
    std::fs::write(&ledger, serde_json::to_vec(&answers).unwrap()).unwrap();
    write_audit_concept(&fixture, "fresh");
    let report = run(&fixture, "knowledge", false);
    let probe = serde_json::from_slice::<serde_json::Value>(&std::fs::read(&ledger).unwrap())
        .unwrap()
        .get("probe")
        .cloned();
    assert_ne!(
        probe.as_ref(),
        Some(&answers["probe"]),
        "a probe whose hold was renewed does not outlive it: {report}"
    );
}

#[test]
fn an_audit_probe_outside_the_current_audit_scope_is_withdrawn() {
    let behavior =
        format!("if [ -f \"$TEST_EXECUTION_ROOT/fail\" ]; then exit 7; fi\n{RECORD_BOUNDED_AUDIT}");
    let fixture = bounded_audit_fixture("codex", 1, &behavior);
    configure(&fixture, Some(20), 60);
    std::fs::write(fixture.root().join("fail"), "").unwrap();
    let first = run(&fixture, "knowledge", false);
    assert_eq!(calls(&first), 1, "{first}");
    std::fs::remove_file(fixture.root().join("fail")).unwrap();
    let path = walkdir::WalkDir::new(fixture.root().join("vault/.lorekeeper/work"))
        .into_iter()
        .map(Result::unwrap)
        .find(|entry| entry.file_name() == "inspection-timeouts.json")
        .unwrap()
        .into_path();
    let mut held: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let node = held["entries"]
        .as_object()
        .unwrap()
        .keys()
        .next()
        .unwrap()
        .clone();
    held["entries"][&node]["failure"]["host"]["profile"] = "retired".into();
    std::fs::write(&path, serde_json::to_vec(&held).unwrap()).unwrap();
    // The held concept subject is still current, but the audit no longer admits concepts.
    let config = std::fs::read_to_string(&fixture.config).unwrap();
    let scoped = config.replace("subjects: [concept, document]", "subjects: [document]");
    assert_ne!(scoped, config, "the fixture configures graph settings");
    std::fs::write(&fixture.config, scoped).unwrap();
    let ledger = fixture
        .root()
        .join("vault/.lorekeeper/queue/host-answers.json");
    let probe = || {
        std::fs::read(&ledger)
            .ok()
            .map(|raw| serde_json::from_slice::<serde_json::Value>(&raw).unwrap())
            .and_then(|ledger| ledger.get("probe").cloned())
    };
    let mut released = false;
    for _ in 0..2 {
        let report = run(&fixture, "knowledge", false);
        released |= report["runs"][0]["stages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|stage| stage["name"] == "host probe");
        assert_eq!(
            probe(),
            None,
            "a probe the audit cannot admit does not stay released: {report}"
        );
    }
    assert!(released, "the recovery turn released the hold");
}

fn configure(fixture: &Fixture, calls: Option<usize>, seconds: u64) {
    let calls = calls.map_or_else(|| "null".to_owned(), |calls| calls.to_string());
    let config = std::fs::read_to_string(&fixture.config).unwrap().replace(
        "execution:\n",
        &format!("execution:\n  automation: {{process_schedule: '0,15,30,45 * * * *', max_calls_per_day: {calls}, max_run_seconds: {seconds}}}\n"),
    );
    std::fs::write(&fixture.config, config).unwrap();
}

fn run(fixture: &Fixture, operation: &str, resume: bool) -> serde_json::Value {
    let mut command = fixture.command();
    command.args(["run", operation, "--scheduled", "--json"]);
    if resume {
        command.arg("--resume");
    }
    let output = command.output().unwrap();
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "{error}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn calls(report: &serde_json::Value) -> u64 {
    report["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|run| run["semantic_calls"].as_u64().unwrap())
        .sum()
}

fn usage(fixture: &Fixture) -> serde_json::Value {
    serde_json::from_slice(
        &std::fs::read(
            fixture
                .root()
                .join("vault/.lorekeeper/automation-usage.json"),
        )
        .unwrap(),
    )
    .unwrap()
}

fn status(fixture: &Fixture) -> serde_json::Value {
    let path = fixture
        .root()
        .join("vault/.lorekeeper/automation-usage.json");
    let before = std::fs::read(&path).unwrap();
    let output = fixture
        .command()
        .args(["status", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read(path).unwrap(), before);
    serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["data"]["automation"]
        .clone()
}

#[test]
fn scheduled_invocation_limit_survives_completed_audit_batch_boundaries() {
    for daily_limit in [Some(256), None] {
        let fixture = Fixture::new("codex", 30, RECORD_AUDIT);
        let config = std::fs::read_to_string(&fixture.config).unwrap().replace(
            "  default_profile: primary\n",
            "  default_profile: primary\n  work: {max_calls: 1}\n",
        ) + "graph:\n  audit:\n    batch_size: 1\n    subjects: [concept, document]\n";
        std::fs::write(&fixture.config, config).unwrap();
        for name in ["first", "second"] {
            write_audit_concept(&fixture, name);
            record_discovery(&fixture, &format!("wiki/concepts/{name}.md"));
        }
        configure(&fixture, daily_limit, 60);
        let report = run(&fixture, "knowledge", false);
        assert_eq!(calls(&report), 1, "{report}");
        assert_eq!(report["stop_reason"], "automation-limit", "{report}");
        let runs = report["runs"].as_array().unwrap();
        assert_eq!(runs.len(), 2, "{report}");
        assert_eq!(runs[0]["audit"].as_array().unwrap().len(), 1);
        assert_eq!(runs[1]["automation"]["stop"], "run-limit");
        assert_eq!(runs[1]["halted"], false);
        assert_eq!(usage(&fixture)["reserved_calls"], 1);
        assert!(usage(&fixture)["paused"].as_object().unwrap().is_empty());
        let snapshot = status(&fixture);
        assert_eq!(
            snapshot["usage"]["max_calls_per_day"],
            serde_json::json!(daily_limit)
        );
        assert_eq!(
            snapshot["usage"]["remaining_calls"],
            serde_json::json!(daily_limit.map(|limit| limit - 1))
        );
        assert_eq!(
            runs[0]["automation"]["max_calls_per_day"],
            serde_json::json!(daily_limit)
        );
    }
}

#[test]
fn scheduled_jobs_share_allowance_across_restarts_and_operations() {
    for engine in ["codex", "claude-code"] {
        let fixture = bounded_audit_fixture(engine, 64, RECORD_BOUNDED_AUDIT);
        configure(&fixture, Some(2), 60);
        let first = run(&fixture, "knowledge", false);
        assert_eq!(calls(&first), 2, "{first}");
        assert_eq!(usage(&fixture)["reserved_calls"], 2);
        let snapshot = status(&fixture);
        assert_eq!(snapshot["state"], "available");
        assert_eq!(snapshot["usage"]["reserved_calls"], 2);
        assert_eq!(snapshot["usage"]["remaining_calls"], 0);
        fixture.ingest();
        let second = run(&fixture, "process", false);
        assert_eq!(calls(&second), 0, "{second}");
        assert_eq!(second["stop_reason"], "automation-limit", "{second}");
        assert_eq!(second["runs"][0]["automation"]["stop"], "day-limit");
        assert_eq!(usage(&fixture)["reserved_calls"], 2);
    }
}

#[test]
fn scheduled_tool_enabled_failure_holds_its_request_without_pausing_or_refunding_calls() {
    for bounded in [false, true] {
        let (fixture, subject) = if bounded {
            let fixture = bounded_audit_fixture("codex", 64, RECORD_BOUNDED_AUDIT);
            std::fs::write(fixture.root().join("audit-response-mode"), "exit").unwrap();
            (fixture, "wiki/concepts/large.md")
        } else {
            let fixture = Fixture::new("codex", 30, "exit 7");
            write_audit_concept(&fixture, "stable");
            (fixture, "wiki/concepts/stable.md")
        };
        assert_tool_enabled_failure_hold(&fixture, subject);
    }
}

#[test]
fn a_subject_settled_during_another_subjects_discovery_is_not_assessed_again() {
    let settle = r#"if request.get('kind')=='audit-discovery':
    settle=root/'settle-during-discovery'
    if settle.exists():
        path=settle.read_text()
        settle.unlink()
        lore=os.environ['LORE_BIN']
        candidate=json.loads(subprocess.check_output([lore,'wiki','audit-plan','--path',path]))['candidates'][0]
        assessment=root/'settled.json'
        assessment.write_text(json.dumps({'assessments':[{'path':path,'file_revision':candidate['file_revision'],
            'input_revision':candidate['input_revision'],'disposition':'reviewed',
            'reason':'Settled by hand during discovery.','reviews':[]}]}))
        subprocess.check_call([lore,'wiki','audit-record','--file',str(assessment)])
"#;
    let host = RECORD_AUDIT.replacen(
        "if request.get('kind')=='audit-discovery':\n",
        &format!("{settle}if request.get('kind')=='audit-discovery':\n"),
        1,
    );
    assert_ne!(host, RECORD_AUDIT);
    let fixture = Fixture::new("codex", 30, &host);
    let config = std::fs::read_to_string(&fixture.config).unwrap()
        + "graph:\n  audit:\n    batch_size: 2\n    subjects: [concept, document]\n";
    std::fs::write(&fixture.config, config).unwrap();
    write_audit_concept(&fixture, "settled");
    let concepts = fixture.root().join("vault/wiki/concepts");
    std::fs::write(
        concepts.join("undiscovered.md"),
        include_str!("../behaviour/corpus/small/vault/wiki/concepts/unlinked-note.md")
            .replace("Unlinked note", "undiscovered")
            .replace("unlinked-note", "undiscovered"),
    )
    .unwrap();
    let settled = "wiki/concepts/settled.md";
    std::fs::write(fixture.root().join("settle-during-discovery"), settled).unwrap();

    let (_, report) = fixture.run("knowledge");
    assert_eq!(report["halted"], false, "{report}");
    assert!(
        !fixture.root().join("settle-during-discovery").exists(),
        "the discovery ran: {report}"
    );
    let assessed: Vec<String> =
        serde_json::from_slice(&std::fs::read(fixture.root().join("maintain-command")).unwrap())
            .unwrap();
    assert!(
        !assessed.iter().any(|path| path == settled),
        "a subject a writer settled is not sent again: {assessed:?} {report}"
    );
    let receipts: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            fixture
                .root()
                .join("vault/.lorekeeper/knowledge-audit.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        receipts["entries"][settled]["reason"], "Settled by hand during discovery.",
        "{receipts}"
    );
}

#[test]
fn a_legacy_subject_whose_stored_work_has_observations_continues_it_unadopted() {
    let fixture = bounded_audit_fixture("codex", 1, RECORD_BOUNDED_AUDIT);
    let subject = "wiki/concepts/large.md";
    let cli = |args: &[&str]| {
        let output = fixture.command().args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    // A recipe recorded before screening existed, which matches no candidate.
    let raw = std::fs::read_to_string(fixture.root().join("vault").join(subject)).unwrap();
    let discovery = fixture.root().join("legacy-discovery.json");
    std::fs::write(
        &discovery,
        serde_json::to_vec(&serde_json::json!({
            "path": subject, "file_revision": lk_core::concept::file_revision(&raw),
            "queries": [{"query":"no page carries this phrase", "phrase":true, "spacing":false}]
        }))
        .unwrap(),
    )
    .unwrap();
    cli(&[
        "wiki",
        "audit-discover",
        "--file",
        discovery.to_str().unwrap(),
    ]);
    let state = fixture
        .root()
        .join("vault/.lorekeeper/knowledge-audit.json");
    let mut receipts: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&state).unwrap()).unwrap();
    receipts["discovery"][subject]["selection"] = serde_json::Value::Null;
    std::fs::write(&state, serde_json::to_vec_pretty(&receipts).unwrap()).unwrap();
    // The Work 0.163 paid for under that derivation: one inspection observed.
    let plan = cli(&["work", "plan", "--path", subject]);
    let work = plan["work_id"].as_str().unwrap().to_owned();
    assert!(plan["ready"].as_array().unwrap().len() > 1, "{plan}");
    let observed = plan["ready"][0].as_str().unwrap().to_owned();
    let request = cli(&["work", "read", &work, &observed]);
    let observation = fixture.root().join("observation.json");
    std::fs::write(
        &observation,
        serde_json::to_vec(&serde_json::json!({
            "work_id":request["work_id"],"node_id":request["node_id"],
            "input_revision":request["input_revision"],
            "summary":"Recorded the assigned evidence.",
            "findings":[],"source_questions":[],"unresolved":[],"resolutions":[]
        }))
        .unwrap(),
    )
    .unwrap();
    cli(&["work", "record", "--file", observation.to_str().unwrap()]);

    configure(&fixture, Some(64), 120);
    let report = run(&fixture, "knowledge", false);
    assert_eq!(report["runs"][0]["halted"], false, "{report}");
    assert_eq!(report["runs"][0]["audit_adoptions"], 0, "{report}");
    let receipts: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&state).unwrap()).unwrap();
    assert!(
        receipts["discovery"][subject]["selection"].is_null(),
        "{receipts}"
    );
    let sent = bounded_calls(&fixture);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(
        sent[0]["work_id"],
        work.as_str(),
        "the paid Work continues: {sent:?}"
    );
    assert_ne!(
        sent[0]["node_id"],
        observed.as_str(),
        "no observed node is read again"
    );
}

#[test]
fn a_manual_audit_record_settles_the_attempts_a_native_failure_left() {
    let attempts = |fixture: &Fixture| -> Vec<serde_json::Value> {
        let directory = fixture
            .root()
            .join("vault/.lorekeeper/audit-preparation-attempts");
        std::fs::read_dir(directory)
            .map(|entries| {
                entries
                    .map(|entry| {
                        serde_json::from_slice(&std::fs::read(entry.unwrap().path()).unwrap())
                            .unwrap()
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let stages = |fixture: &Fixture| {
        attempts(fixture)
            .iter()
            .map(|attempt| attempt["key"]["stage"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    let fixture = Fixture::new("codex", 30, "exit 7");
    configure(&fixture, Some(64), 60);
    let subject = "wiki/concepts/stable.md";
    let concepts = fixture.root().join("vault/wiki/concepts");
    std::fs::create_dir_all(&concepts).unwrap();
    std::fs::write(
        concepts.join("stable.md"),
        include_str!("../behaviour/corpus/small/vault/wiki/concepts/unlinked-note.md")
            .replace("Unlinked note", "stable")
            .replace("unlinked-note", "stable"),
    )
    .unwrap();

    // The native discovery fails and holds the subject; recording its discovery by hand
    // settles that stage.
    run(&fixture, "knowledge", false);
    assert_eq!(stages(&fixture), ["audit-discovery"]);
    record_discovery(&fixture, subject);
    assert!(stages(&fixture).is_empty(), "{:?}", attempts(&fixture));

    // The native assessment fails and holds the subject; recording its assessment by hand
    // settles that stage, so the next run has no probe to release or withdraw.
    run(&fixture, "knowledge", false);
    assert_eq!(stages(&fixture), ["audit-assessment"]);
    let plan = fixture
        .command()
        .args(["wiki", "audit-plan", "--path", subject])
        .output()
        .unwrap();
    let candidate =
        serde_json::from_slice::<serde_json::Value>(&plan.stdout).unwrap()["candidates"][0].clone();
    let assessment = fixture.root().join("manual-assessment.json");
    std::fs::write(&assessment, serde_json::to_vec(&serde_json::json!({"assessments":[{
        "path":subject, "file_revision":candidate["file_revision"], "input_revision":candidate["input_revision"],
        "disposition":"reviewed", "reason":"The subject was read in full by hand.", "reviews":[]
    }]})).unwrap()).unwrap();
    let recorded = fixture
        .command()
        .args(["wiki", "audit-record", "--file"])
        .arg(&assessment)
        .output()
        .unwrap();
    assert!(
        recorded.status.success(),
        "{}",
        String::from_utf8_lossy(&recorded.stderr)
    );
    assert!(stages(&fixture).is_empty(), "{:?}", attempts(&fixture));
    let settled = run(&fixture, "knowledge", false);
    assert_eq!(calls(&settled), 0, "{settled}");
    assert!(
        settled["runs"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|run| run["stages"].as_array().unwrap())
            .all(|stage| stage["name"] != "host probe"),
        "{settled}"
    );
}

fn assert_tool_enabled_failure_hold(fixture: &Fixture, subject: &str) {
    configure(fixture, Some(64), 60);
    let first = run(fixture, "knowledge", false);
    assert!(calls(&first) >= 1, "{first}");
    assert_eq!(first["stop_reason"], "deferred", "{first}");
    assert_eq!(first["runs"][0]["halted"], false, "{first}");
    assert!(
        first["runs"][0]["stages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|stage| stage["name"] == "native result invocation held"),
        "the held request is reported as a deferred stage, not a halt: {first}"
    );
    assert!(usage(fixture)["paused"].as_object().unwrap().is_empty());
    let attempts = fixture
        .root()
        .join("vault/.lorekeeper/audit-preparation-attempts");
    let path = std::fs::read_dir(attempts)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let raw = std::fs::read(&path).unwrap();
    let attempt: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(attempt["key"]["stage"], "audit-assessment", "{attempt}");
    assert_eq!(attempt["subject"], subject);
    assert_eq!(attempt["disposition"]["failure"]["code"], 7);
    let before = usage(fixture);
    // A campaign's later batch may already have probed the hold; a failed probe spaces the
    // next, so only a hold no probe has failed yet is probed now, once.
    let already_probed = attempt["disposition"]["failure"]["host"]["probe_failures"]
        .as_u64()
        .is_some_and(|failures| failures > 0);
    let mut probes = 0;
    for (resume, sent) in [(false, u64::from(!already_probed)), (true, 0)] {
        probes += sent;
        let probed = run(fixture, "knowledge", resume);
        assert_eq!(
            calls(&probed),
            sent,
            "a run with nothing admissible probes the held request once, and its failure spaces the next probe: {probed}"
        );
        assert_eq!(probed["stop_reason"], "deferred", "{probed}");
        let renewed: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(renewed["input_revision"], attempt["input_revision"]);
        assert!(
            renewed["disposition"].get("exhausted").is_none(),
            "{renewed}"
        );
    }
    let mode = fixture.root().join("audit-response-mode");
    if mode.exists() {
        std::fs::remove_file(mode).unwrap();
    }
    for dry_run in [true, false] {
        let mut command = fixture.command();
        command.args([
            "work",
            "retry-preparation",
            "--stage",
            "audit-assessment",
            "--path",
            subject,
            "--input-revision",
            attempt["input_revision"].as_str().unwrap(),
        ]);
        if dry_run {
            command.arg("--dry-run");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(path.exists(), dry_run);
    }
    let retried = run(fixture, "knowledge", false);
    assert_eq!(calls(&retried), 1, "{retried}");
    assert_eq!(
        usage(fixture)["reserved_calls"],
        before["reserved_calls"].as_u64().unwrap() + probes + 1,
        "the probes sent, then the retried request"
    );
}

#[test]
fn scheduled_job_output_keeps_lore_stages_and_omits_host_transcripts() {
    // A scheduled job, a generated job without an automation budget (--unattended), and an
    // attended run.
    for (scheduled, unattended) in [(true, false), (false, true), (false, false)] {
        let token = ["ghp", "_0123456789abcdefghij", "klmnopqrstuvwxyz"].concat();
        let fixture = Fixture::new(
            "codex",
            30,
            &format!(
                "printf 'host-stderr-marker {token}\\n' >&2\nprintf 'host-stdout-marker\\n'\nexit 7"
            ),
        );
        write_audit_concept(&fixture, "stable");
        configure(&fixture, Some(4), 60);
        let mut command = fixture.command();
        command.args(["run", "knowledge", "--json"]);
        if scheduled {
            command.arg("--scheduled");
        }
        if unattended {
            command.arg("--unattended");
        }
        let output = command.output().unwrap();
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let invoked = if scheduled {
            calls(&report)
        } else {
            report["semantic_calls"].as_u64().unwrap()
        };
        assert!(invoked >= 1, "{report}");
        let diagnostics = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            diagnostics.contains("host-stdout-marker"),
            !scheduled && !unattended,
            "{diagnostics}"
        );
        if scheduled || unattended {
            // Only the bounded, sanitized stderr tail an answerless failure reports reaches
            // the job's log, with credentials masked; the host's streams are not forwarded.
            let marked = diagnostics
                .lines()
                .filter(|line| line.contains("host-stderr-marker"))
                .collect::<Vec<_>>();
            assert!(
                marked
                    .iter()
                    .any(|line| line.contains("host failure output")),
                "{diagnostics}"
            );
            assert!(
                marked
                    .iter()
                    .all(|line| line.contains("host failure output")
                        || line.contains(
                            r#""stderr_tail":"host-stderr-marker [GitHub access token]""#
                        )),
                "{diagnostics}"
            );
            assert!(!diagnostics.contains(&token), "{diagnostics}");
            assert!(
                diagnostics.contains("[GitHub access token]"),
                "{diagnostics}"
            );
        } else {
            assert!(diagnostics.contains("host-stderr-marker"), "{diagnostics}");
        }
        for stage_output in ["Prepared navigation snapshot", "graph lint: completed"] {
            assert!(diagnostics.contains(stage_output), "{diagnostics}");
        }
    }
}

#[test]
fn an_answerless_host_that_exits_zero_still_reports_its_stderr() {
    for engine in ["codex", "claude-code"] {
        let fixture = Fixture::new(
            engine,
            30,
            "printf 'host-stderr-marker no session\\n' >&2\nexit 0",
        );
        write_audit_concept(&fixture, "stable");
        configure(&fixture, Some(4), 60);
        let report = run(&fixture, "knowledge", false);
        assert!(calls(&report) >= 1, "{report}");
        let stage = report["runs"][0]["stages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|stage| stage["name"] == "host failure output")
            .unwrap_or_else(|| panic!("{engine}: {report}"));
        assert!(
            stage["detail"]
                .as_str()
                .unwrap()
                .ends_with("stderr: host-stderr-marker no session"),
            "{engine}: {stage}"
        );
    }
}

#[test]
fn scheduled_tool_free_failure_holds_its_input_without_pausing_or_refunding_calls() {
    for engine in ["codex", "claude-code"] {
        let fixture = bounded_audit_fixture(engine, 64, "exit 7");
        configure(&fixture, Some(4), 60);
        let first = run(&fixture, "knowledge", false);
        assert_eq!(calls(&first), 1, "{first}");
        assert_eq!(first["stop_reason"], "deferred");
        assert_eq!(usage(&fixture)["reserved_calls"], 1);
        assert!(usage(&fixture)["paused"].as_object().unwrap().is_empty());
        for (resume, sent) in [(false, 1), (true, 0)] {
            let probed = run(&fixture, "knowledge", resume);
            assert_eq!(
                calls(&probed),
                sent,
                "a run with nothing admissible probes the held input once, and its failure spaces the next probe: {probed}"
            );
            assert_eq!(probed["stop_reason"], "deferred");
            assert_eq!(usage(&fixture)["reserved_calls"], 2);
            assert!(usage(&fixture)["paused"].as_object().unwrap().is_empty());
        }
    }
}

#[test]
fn scheduled_parallel_observers_reserve_only_the_available_allowance() {
    for engine in ["codex", "claude-code"] {
        let fixture = bounded_audit_fixture(engine, 64, RECORD_BOUNDED_AUDIT);
        let config = std::fs::read_to_string(&fixture.config)
            .unwrap()
            .replace("max_calls: 64}", "max_calls: 64, max_parallel: 6}");
        std::fs::write(&fixture.config, config).unwrap();
        configure(&fixture, Some(3), 60);
        let report = run(&fixture, "knowledge", false);
        assert_eq!(calls(&report), 3, "{report}");
        assert_eq!(usage(&fixture)["reserved_calls"], 3);
        assert_eq!(bounded_calls(&fixture).len(), 3);
    }
}

#[test]
fn corrupt_automation_usage_stops_without_model_calls_or_replacement() {
    let fixture = bounded_audit_fixture("codex", 64, RECORD_BOUNDED_AUDIT);
    configure(&fixture, Some(4), 60);
    let path = fixture
        .root()
        .join("vault/.lorekeeper/automation-usage.json");
    let bytes = br#"{"schema":"unknown"}"#;
    std::fs::write(&path, bytes).unwrap();
    let report = run(&fixture, "knowledge", false);
    assert_eq!(calls(&report), 0, "{report}");
    assert_eq!(report["stop_reason"], "failed");
    assert!(report["runs"][0]["automation"]["reserved_calls"].is_null());
    assert_eq!(std::fs::read(path).unwrap(), bytes);
    let snapshot = status(&fixture);
    assert_eq!(snapshot["state"], "unavailable");
    assert!(snapshot["usage"].is_null());
}

#[test]
fn a_paused_scheduled_operation_reports_without_waiting_for_an_owned_workflow() {
    use std::io::{BufRead, Write};
    let fixture = bounded_audit_fixture("codex", 64, RECORD_BOUNDED_AUDIT);
    configure(&fixture, Some(4), 1);
    let path = fixture
        .root()
        .join("vault/.lorekeeper/automation-usage.json");
    let raw = serde_json::to_vec(&serde_json::json!({
        "schema":"automation-usage-v1",
        "utc_date":jiff::Timestamp::now().to_zoned_datetime(jiff::tz::TimeZone::UTC).date(),
        "reserved_calls":3,
        "paused":{"knowledge":"2026-11-16T23:28:26Z"}
    }))
    .unwrap();
    std::fs::write(&path, &raw).unwrap();
    let mut holder = fixture
        .command()
        .args([
            "workflow",
            "--",
            "/bin/sh",
            "-c",
            "printf 'ready\\n'; read task_release",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut ready = String::new();
    std::io::BufReader::new(holder.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    assert_eq!(ready, "ready\n");
    let output = fixture
        .command()
        .args(["run", "knowledge", "--scheduled", "--wait", "--json"])
        .output()
        .unwrap();
    holder
        .stdin
        .take()
        .unwrap()
        .write_all(b"release\n")
        .unwrap();
    assert!(holder.wait().unwrap().success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["stop_reason"], "blocked", "{report}");
    assert_eq!(
        report["runs"][0]["automation"]["stop"], "paused",
        "{report}"
    );
    assert_eq!(report["runs"][0]["automation"]["workflow_wait_ms"], 0);
    assert!(report["runs"][0]["workflow_id"].is_null());
    assert_eq!(calls(&report), 0);
    assert_eq!(std::fs::read(path).unwrap(), raw);
    assert!(!fixture.root().join("bounded-calls.jsonl").exists());
}

#[test]
fn a_waiting_workflow_gets_a_settled_parallel_wave_without_renewing_the_run_budget() {
    use std::io::Write;
    let behavior = RECORD_BOUNDED_AUDIT.replacen(
        "if action=='inspect':",
        r#"if action=='inspect':
    import time
    try:
        (root/'first-paid').open('x').close()
        first=True
    except FileExistsError:
        first=False
    if first:
        deadline=time.monotonic()+20
        while not (root/'contender-queued').exists():
            assert time.monotonic()<deadline, 'contender did not queue'
            time.sleep(.005)
"#,
        1,
    );
    let fixture = bounded_audit_fixture("codex", 4, &behavior);
    configure(&fixture, None, 60);
    let config = std::fs::read_to_string(&fixture.config)
        .unwrap()
        .replace("max_calls: 4}", "max_calls: 4, max_parallel: 2}")
        .replace(
            "max_run_seconds: 60}",
            "max_run_seconds: 60, yield_after_seconds: 0}",
        );
    std::fs::write(&fixture.config, config).unwrap();
    let owner = fixture
        .command()
        .args(["run", "knowledge", "--scheduled", "--wait", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let wait_for = |path: &Path| {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !path.exists() {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        true
    };
    let started = wait_for(&fixture.root().join("first-paid"));
    let mut contender = fixture
        .command()
        .args([
            "workflow",
            "--wait",
            "--",
            "/bin/sh",
            "-c",
            "printf entered > \"$TEST_EXECUTION_ROOT/contender-entered\"; read task_release",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let admission = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(
            fixture
                .root()
                .join("vault/.lorekeeper/workflow-admission.lock"),
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let queued = loop {
        match admission.try_lock() {
            Ok(()) => admission.unlock().unwrap(),
            Err(std::fs::TryLockError::WouldBlock) => break true,
            Err(error) => panic!("{error}"),
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    std::fs::write(fixture.root().join("contender-queued"), "ready").unwrap();
    let entered = wait_for(&fixture.root().join("contender-entered"));
    let at_checkpoint = usage(&fixture);
    let rows = bounded_calls(&fixture);
    let paid: Result<Vec<_>, std::io::Error> = rows
        .iter()
        .filter(|row| row["action"] == "inspect")
        .map(|row| {
            let path = fixture
                .root()
                .join("vault/.lorekeeper/work")
                .join(row["work_id"].as_str().unwrap())
                .join("nodes")
                .join(format!("{}.json", row["node_id"].as_str().unwrap()));
            std::fs::read(&path).map(|raw| (path, raw))
        })
        .collect();
    contender
        .stdin
        .take()
        .unwrap()
        .write_all(b"release\n")
        .unwrap();
    let contender = contender.wait_with_output().unwrap();
    let output = owner.wait_with_output().unwrap();
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(started && queued && entered, "{report}");
    assert!(
        contender.status.success(),
        "{}",
        String::from_utf8_lossy(&contender.stderr)
    );
    assert_eq!(at_checkpoint["reserved_calls"], 2, "{report}");
    assert_eq!(rows.len(), 2, "{rows:?}");
    for (path, raw) in paid.unwrap() {
        assert_eq!(std::fs::read(path).unwrap(), raw);
    }
    let runs = report["runs"].as_array().unwrap();
    assert_eq!(runs[0]["yield_reason"], "workflow-contention", "{report}");
    assert_eq!(runs[0]["ok"], false);
    assert_eq!(runs[0]["halted"], false);
    assert_eq!(runs[0]["retained_progress"]["evidence_nodes"], 2);
    assert!(
        !runs[0]["stages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|stage| stage["name"] == "queue completion")
    );
    assert_eq!(calls(&report), 4, "{report}");
    assert_eq!(usage(&fixture)["reserved_calls"], 4);
    assert_eq!(report["stop_reason"], "settled", "{report}");
    assert_eq!(
        runs.last().unwrap()["audit_progress"]["after"]["reviewed_current"],
        1
    );
    assert_eq!(
        runs.last().unwrap()["audit_progress"]["after"]["unreviewed"],
        0
    );
    assert!(
        runs.iter()
            .all(|run| run["automation"]["max_calls_per_run"] == 4)
    );
    assert!(usage(&fixture)["paused"].as_object().unwrap().is_empty());
}

#[test]
fn scheduled_workflow_wait_is_bounded_without_starting_a_model() {
    use std::io::{BufRead, Write};
    let fixture = bounded_audit_fixture("codex", 64, RECORD_BOUNDED_AUDIT);
    configure(&fixture, Some(4), 1);
    let mut holder = fixture
        .command()
        .args([
            "workflow",
            "--",
            "/bin/sh",
            "-c",
            "printf 'ready\\n'; read task_release",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut ready = String::new();
    std::io::BufReader::new(holder.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    assert_eq!(ready, "ready\n");
    let busy = run(&fixture, "knowledge", false);
    assert_eq!(calls(&busy), 0, "{busy}");
    assert_eq!(busy["stop_reason"], "deferred", "{busy}");
    assert!(
        !fixture
            .root()
            .join("vault/.lorekeeper/automation-usage.json")
            .exists()
    );
    let output = fixture
        .command()
        .args(["run", "knowledge", "--scheduled", "--wait", "--json"])
        .output()
        .unwrap();
    holder
        .stdin
        .take()
        .unwrap()
        .write_all(b"release\n")
        .unwrap();
    assert!(holder.wait().unwrap().success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(calls(&report), 0, "{report}");
    assert_eq!(report["stop_reason"], "automation-limit");
    assert_eq!(report["runs"][0]["automation"]["stop"], "wait-duration");
    assert!(report["runs"][0]["automation"]["reserved_calls"].is_null());
    assert!(!fixture.root().join("bounded-calls.jsonl").exists());
}

#[test]
fn scheduled_execution_accounts_for_workflow_wait_before_admitting_a_model() {
    use std::io::{BufRead, Write};
    use std::time::{Duration, Instant};

    let fixture = Fixture::new("codex", 30, RECORD_AUDIT);
    write_audit_concept(&fixture, "waiting");
    record_discovery(&fixture, "wiki/concepts/waiting.md");
    configure(&fixture, None, 60);
    let config = std::fs::read_to_string(&fixture.config).unwrap().replace(
        "max_run_seconds: 60",
        "max_run_seconds: 60, max_wait_seconds: 15",
    );
    std::fs::write(&fixture.config, config).unwrap();
    let mut holder = fixture
        .command()
        .args([
            "workflow",
            "--",
            "/bin/sh",
            "-c",
            "printf 'ready\\n'; read task_release",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut ready = String::new();
    std::io::BufReader::new(holder.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    assert_eq!(ready, "ready\n");
    let waiter = fixture
        .command()
        .args(["run", "knowledge", "--scheduled", "--wait", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let admission = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(
            fixture
                .root()
                .join("vault/.lorekeeper/workflow-admission.lock"),
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let waiting = loop {
        match admission.try_lock() {
            Ok(()) => admission.unlock().unwrap(),
            Err(std::fs::TryLockError::WouldBlock) => break true,
            Err(error) => panic!("{error}"),
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    holder
        .stdin
        .take()
        .unwrap()
        .write_all(b"release\n")
        .unwrap();
    assert!(holder.wait().unwrap().success());
    let output = waiter.wait_with_output().unwrap();
    assert!(waiting, "scheduled job did not reach workflow admission");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(calls(&report) > 0, "{report}");
    assert!(
        report["runs"][0]["automation"]["workflow_wait_ms"]
            .as_u64()
            .unwrap()
            > 0,
        "{report}"
    );
}
