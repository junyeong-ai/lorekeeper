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
        for (resume, reserved) in [(false, 2), (true, 3)] {
            let probed = run(&fixture, "knowledge", resume);
            assert_eq!(
                calls(&probed),
                1,
                "each run with nothing admissible probes the held node once: {probed}"
            );
            assert_eq!(usage(&fixture)["reserved_calls"], reserved);
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
        assert_eq!(usage(&fixture)["reserved_calls"], 3);
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
        assert_eq!(usage(&fixture)["reserved_calls"], 4);
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
    for engine in ["codex", "claude-code"] {
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

        let probed = run(&fixture, "knowledge", false);
        assert!(
            probed["runs"][0]["stages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|stage| stage["name"] == "host probe"),
            "{probed}"
        );
        let concurrency = std::fs::read_to_string(fixture.root().join("concurrency"))
            .unwrap()
            .lines()
            .map(|line| line.parse::<usize>().unwrap())
            .collect::<Vec<_>>();
        assert!(concurrency.len() >= 3, "{concurrency:?}: {probed}");
        assert_eq!(
            concurrency[0], 1,
            "the probe is sent alone until its host gives an outcome: {probed}"
        );
        assert!(
            concurrency[1..].iter().any(|count| *count > 1),
            "the host's answer restores parallel waves: {concurrency:?}"
        );
    }
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
        let reserved = usage(&fixture)["reserved_calls"].clone();
        let held = run(&fixture, "knowledge", false);
        assert_eq!(calls(&held), 0, "{held}");
        assert_eq!(usage(&fixture)["reserved_calls"], reserved);
    }
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
    for resume in [false, true] {
        let probed = run(fixture, "knowledge", resume);
        assert_eq!(
            calls(&probed),
            1,
            "a run with nothing admissible probes the held request once: {probed}"
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
        before["reserved_calls"].as_u64().unwrap() + 3,
        "two probes, then the retried request"
    );
}

#[test]
fn scheduled_job_output_keeps_lore_stages_and_omits_host_transcripts() {
    for scheduled in [true, false] {
        let fixture = Fixture::new(
            "codex",
            30,
            "printf 'host-stderr-marker\\n' >&2\nprintf 'host-stdout-marker\\n'\nexit 7",
        );
        write_audit_concept(&fixture, "stable");
        configure(&fixture, Some(4), 60);
        let mut command = fixture.command();
        command.args(["run", "knowledge", "--json"]);
        if scheduled {
            command.arg("--scheduled");
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
        for marker in ["host-stderr-marker", "host-stdout-marker"] {
            assert_eq!(diagnostics.contains(marker), !scheduled, "{diagnostics}");
        }
        for stage_output in ["Prepared navigation snapshot", "graph lint: completed"] {
            assert!(diagnostics.contains(stage_output), "{diagnostics}");
        }
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
        for (resume, reserved) in [(false, 2), (true, 3)] {
            let probed = run(&fixture, "knowledge", resume);
            assert_eq!(
                calls(&probed),
                1,
                "a run with nothing admissible probes the held input once: {probed}"
            );
            assert_eq!(probed["stop_reason"], "deferred");
            assert_eq!(usage(&fixture)["reserved_calls"], reserved);
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
