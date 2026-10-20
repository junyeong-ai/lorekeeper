use std::process::{Command, Output};

fn invoke(config: &std::path::Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lore"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("LORE_") {
            command.env_remove(key);
        }
    }
    command.arg("--config").arg(config).arg("ingest").args(args);
    command.output().unwrap()
}

#[test]
fn source_recovery_resumes_bounded_days_without_reprocessing_the_manual_inbox() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("vault");
    std::fs::create_dir_all(root.join("inbox")).unwrap();
    std::fs::write(
        root.join("inbox/note.md"),
        "# Retained note\n\nNot a historical snapshot.\n",
    )
    .unwrap();
    let today = jiff::Timestamp::now()
        .to_zoned(jiff::tz::TimeZone::UTC)
        .date();
    let start = today.checked_sub(jiff::Span::new().days(3)).unwrap();
    let config = temp.path().join("config.yaml");
    std::fs::write(&config, format!("vault: {{root: vault, timezone: UTC}}\nidentity: {{name: Test, email: test@example.invalid}}\nllm: {{provider: noop}}\npersonal: {{tasks: {{}}}}\ningest:\n  recovery: {{start_date: '{start}', max_days_per_source_per_run: 2}}\nsources:\n  tasks: {{type: tasks}}\n  notes: {{type: manual, params: {{inbox_dir: inbox}}}}\n")).unwrap();
    let preview = invoke(&config, &["--recover", "--dry-run"]);
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    assert!(String::from_utf8_lossy(&preview.stderr).contains("2 source-day selections"));
    assert!(!root.join(".lorekeeper").exists());

    let first = invoke(&config, &["--recover"]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(String::from_utf8_lossy(&first.stderr).contains("2 source-day attempts"));
    let state = root.join(".lorekeeper/periods/state.json");
    let receipts: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&state).unwrap()).unwrap();
    assert_eq!(receipts["batches"].as_array().unwrap().len(), 2);
    assert!(root.join("inbox/note.md").exists());
    assert!(!root.join("wiki/documents").exists());

    let second = invoke(&config, &["--recover"]);
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(String::from_utf8_lossy(&second.stderr).contains("1 source-day attempts"));
    let before = std::fs::read(&state).unwrap();
    let third = invoke(&config, &["--recover"]);
    assert!(
        third.status.success(),
        "{}",
        String::from_utf8_lossy(&third.stderr)
    );
    assert!(String::from_utf8_lossy(&third.stderr).contains("0 source-day attempts"));
    assert_eq!(std::fs::read(&state).unwrap(), before);

    for args in [
        ["--recover", "--date", "2026-01-01"].as_slice(),
        ["--recover", "tasks"].as_slice(),
    ] {
        assert!(!invoke(&config, args).status.success());
    }
}

#[test]
fn failed_source_does_not_prevent_independent_coverage_and_unsupported_sources_are_not_replayed() {
    let temp = tempfile::tempdir().unwrap();
    let today = jiff::Timestamp::now()
        .to_zoned(jiff::tz::TimeZone::UTC)
        .date();
    let start = today.yesterday().unwrap();
    let config = temp.path().join("config.yaml");
    std::fs::write(&config, format!("vault: {{root: vault, timezone: UTC}}\nidentity: {{name: Test, email: test@example.invalid}}\nllm: {{provider: noop}}\npersonal: {{tasks: {{}}}}\ningest:\n  recovery: {{start_date: '{start}', max_days_per_source_per_run: 1}}\nsources:\n  mail: {{type: gmail, params: {{queries: ['from:test@example.invalid']}}}}\n  tasks: {{type: tasks}}\n  news: {{type: rss, params: {{feeds: [{{id: example, url: 'https://example.invalid/feed'}}]}}}}\n")).unwrap();
    let output = invoke(&config, &["--recover"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("historical coverage unverified"),
        "{stderr}"
    );
    let state: serde_json::Value = serde_json::from_slice(
        &std::fs::read(temp.path().join("vault/.lorekeeper/periods/state.json")).unwrap(),
    )
    .unwrap();
    let batches = state["batches"].as_array().unwrap();
    assert!(
        batches
            .iter()
            .any(|batch| batch["sources"][0]["source_id"] == "tasks"
                && batch["sources"][0]["outcome"] == "observed")
    );
    assert!(
        batches
            .iter()
            .all(|batch| batch["sources"][0]["source_id"] != "news")
    );
}

#[test]
fn historical_ingest_does_not_write_current_intent_snapshots_but_today_still_does() {
    let temp = tempfile::tempdir().unwrap();
    let today = jiff::Timestamp::now()
        .to_zoned(jiff::tz::TimeZone::UTC)
        .date();
    let past = today.yesterday().unwrap().to_string();
    let config = temp.path().join("config.yaml");
    std::fs::write(&config, "vault: {root: vault, timezone: UTC}\nidentity: {name: Test, email: test@example.invalid}\nllm: {provider: noop}\npersonal:\n  tasks: {}\nsources:\n  tasks: {type: tasks}\n").unwrap();
    let snapshot = temp.path().join("vault/.lorekeeper/proposals/tasks.jsonl");
    let historical = invoke(&config, &["tasks", "--date", &past]);
    assert!(
        historical.status.success(),
        "{}",
        String::from_utf8_lossy(&historical.stderr)
    );
    assert!(
        !snapshot.exists(),
        "historical collection must not publish current intent state"
    );
    let current = invoke(&config, &["tasks", "--date", &today.to_string()]);
    assert!(
        current.status.success(),
        "{}",
        String::from_utf8_lossy(&current.stderr)
    );
    assert!(
        snapshot.exists(),
        "explicit today remains a current observation"
    );
}
