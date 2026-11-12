use super::*;

#[test]
fn inspection_timeouts_retry_once_across_runs_and_require_an_exact_explicit_retry() {
    for engine in ["codex", "claude-code"] {
        let fixture = bounded_audit_fixture(engine, 1, "exec sleep 30");
        let config = std::fs::read_to_string(&fixture.config)
            .unwrap()
            .replace("timeout_seconds: 30", "timeout_seconds: 3");
        std::fs::write(&fixture.config, config).unwrap();
        configure(&fixture, Some(20), 60);
        for expected in [1, 2] {
            let report = run(&fixture, "knowledge", false);
            assert_eq!(calls(&report), 1, "{report}");
            assert_eq!(report["runs"][0]["halted"], false, "{report}");
            assert_eq!(
                report["runs"][0]["work_steps"][0]["host_failure"]["termination"], "timed-out",
                "{report}"
            );
            assert_eq!(usage(&fixture)["reserved_calls"], expected);
            assert!(usage(&fixture)["paused"].as_object().unwrap().is_empty());
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
        let plan = std::fs::read(path.parent().unwrap().join("plan.json")).unwrap();
        let report = run(&fixture, "knowledge", true);
        assert_eq!(
            calls(&report),
            0,
            "operation resume must not reset node holds: {report}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
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
fn a_parallel_inspection_timeout_does_not_pause_or_discard_independent_audits() {
    for engine in ["codex", "claude-code"] {
        let behavior = RECORD_BOUNDED_AUDIT.replacen("if action=='inspect':", r#"if action=='inspect':
    subject=json.loads((root/'vault/.lorekeeper/work'/work_id/'plan.json').read_text())['context']['subject']['path']
    if subject=='wiki/concepts/large.md':
        import time
        time.sleep(5)
"#, 1);
        let fixture = bounded_audit_fixture(engine, 64, &behavior);
        let config = std::fs::read_to_string(&fixture.config)
            .unwrap()
            .replace("timeout_seconds: 30", "timeout_seconds: 2")
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
        ) + "graph:\n  audit:\n    batch_size: 1\n";
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
fn scheduled_host_failure_requires_explicit_resume_without_refunding_calls() {
    let fixture = bounded_audit_fixture("codex", 64, "exit 7");
    configure(&fixture, Some(4), 60);
    let first = run(&fixture, "knowledge", false);
    assert_eq!(calls(&first), 1, "{first}");
    assert_eq!(first["stop_reason"], "failed");
    assert!(usage(&fixture)["paused"]["knowledge"].is_string());
    assert!(status(&fixture)["usage"]["paused"]["knowledge"].is_string());
    let before = usage(&fixture);
    let config = std::fs::read_to_string(&fixture.config)
        .unwrap()
        .replace("max_run_seconds: 60", "max_run_seconds: 90");
    std::fs::write(&fixture.config, config).unwrap();
    let held = run(&fixture, "knowledge", false);
    assert_eq!(calls(&held), 0, "{held}");
    assert_eq!(held["stop_reason"], "blocked");
    assert_eq!(usage(&fixture), before);
    let resumed = run(&fixture, "knowledge", true);
    assert_eq!(calls(&resumed), 1, "{resumed}");
    assert_eq!(usage(&fixture)["reserved_calls"], 2);
    assert!(usage(&fixture)["paused"]["knowledge"].is_string());
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
fn scheduled_execution_retains_its_duration_after_waiting_for_the_workflow() {
    use std::io::{BufRead, Write};
    use std::time::{Duration, Instant};

    let fixture = Fixture::new("codex", 30, RECORD_AUDIT);
    write_audit_concept(&fixture, "waiting");
    record_discovery(&fixture, "wiki/concepts/waiting.md");
    configure(&fixture, None, 3);
    let config = std::fs::read_to_string(&fixture.config).unwrap().replace(
        "max_run_seconds: 3",
        "max_run_seconds: 3, max_wait_seconds: 15",
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
    if waiting {
        std::thread::sleep(Duration::from_millis(3500));
    }
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
            >= 3000,
        "{report}"
    );
}
