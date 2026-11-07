use super::*;

fn configure(fixture: &Fixture, calls: usize, seconds: u64) {
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

#[test]
fn scheduled_invocation_limit_survives_completed_audit_batch_boundaries() {
    let fixture = Fixture::new("codex", 30, RECORD_FIRST_AUDIT);
    let config = std::fs::read_to_string(&fixture.config).unwrap().replace(
        "  default_profile: primary\n",
        "  default_profile: primary\n  work: {max_calls: 1}\n",
    ) + "graph:\n  audit:\n    batch_size: 1\n";
    std::fs::write(&fixture.config, config).unwrap();
    for name in ["first", "second"] {
        write_audit_concept(&fixture, name);
        record_discovery(&fixture, &format!("wiki/concepts/{name}.md"));
    }
    configure(&fixture, 256, 60);
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
}

#[test]
fn scheduled_jobs_share_allowance_across_restarts_and_operations() {
    for engine in ["codex", "claude-code"] {
        let fixture = bounded_audit_fixture(engine, 64, RECORD_BOUNDED_AUDIT);
        configure(&fixture, 2, 60);
        let first = run(&fixture, "knowledge", false);
        assert_eq!(calls(&first), 2, "{first}");
        assert_eq!(usage(&fixture)["reserved_calls"], 2);
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
    configure(&fixture, 4, 60);
    let first = run(&fixture, "knowledge", false);
    assert_eq!(calls(&first), 1, "{first}");
    assert_eq!(first["stop_reason"], "failed");
    assert!(usage(&fixture)["paused"]["knowledge"].is_string());
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
        configure(&fixture, 3, 60);
        let report = run(&fixture, "knowledge", false);
        assert_eq!(calls(&report), 3, "{report}");
        assert_eq!(usage(&fixture)["reserved_calls"], 3);
        assert_eq!(bounded_calls(&fixture).len(), 3);
    }
}

#[test]
fn corrupt_automation_usage_stops_without_model_calls_or_replacement() {
    let fixture = bounded_audit_fixture("codex", 64, RECORD_BOUNDED_AUDIT);
    configure(&fixture, 4, 60);
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
}

#[test]
fn scheduled_workflow_wait_is_bounded_without_starting_a_model() {
    use std::io::{BufRead, Write};
    let fixture = bounded_audit_fixture("codex", 64, RECORD_BOUNDED_AUDIT);
    configure(&fixture, 4, 1);
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
    assert_eq!(report["runs"][0]["automation"]["stop"], "run-duration");
    assert!(report["runs"][0]["automation"]["reserved_calls"].is_null());
    assert!(!fixture.root().join("bounded-calls.jsonl").exists());
}
