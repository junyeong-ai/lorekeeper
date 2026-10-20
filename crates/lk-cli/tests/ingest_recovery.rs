use std::process::{Command, Output};

fn invoke(config: &std::path::Path, args: &[&str]) -> Output {
    let mut full_args = vec!["ingest"];
    full_args.extend_from_slice(args);
    invoke_command(config, &full_args)
}

fn invoke_command(config: &std::path::Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lore"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("LORE_") {
            command.env_remove(key);
        }
    }
    command.arg("--config").arg(config).args(args);
    command.output().unwrap()
}

fn complete_inputs(path: &std::path::Path) {
    let raw = std::fs::read_to_string(path).unwrap();
    let mut page = lk_core::frontmatter::parse_page(&raw).unwrap();
    let inputs = page
        .frontmatter
        .fields
        .get_mut(lk_core::frontmatter::field::LLM_INPUTS)
        .unwrap()
        .as_object_mut()
        .unwrap();
    for kind in [
        lk_queue::TargetKind::DailySummary,
        lk_queue::TargetKind::DailyRefineEvents,
        lk_queue::TargetKind::WorkLogSynthesis,
    ] {
        if let Some(value) = inputs.get(kind.llm_inputs_key()).cloned() {
            inputs.insert(kind.completion_key(), value);
        }
    }
    let updated = lk_vault::set_frontmatter_field(
        &raw,
        lk_core::frontmatter::field::LLM_INPUTS,
        &serde_json::to_string(inputs).unwrap(),
    )
    .unwrap();
    std::fs::write(path, updated).unwrap();
}

fn resume_personal(config: &std::path::Path) -> serde_json::Value {
    let output = invoke_command(
        config,
        &["synthesis", "pending", "--stage", "base", "--json"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let state: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    state.as_object().unwrap().values().next().unwrap().clone()
}

#[test]
fn personal_period_refuses_changed_filtered_inputs_and_accepts_unchanged_refinement_and_full_refresh()
 {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("config.yaml");
    std::fs::write(&config, "vault: {root: vault, timezone: UTC, locale: en}\nidentity: {name: Test, email: test@example.invalid}\npersonal:\n  tracked_sources: [tasks]\n  tasks: {}\nsources:\n  tasks: {type: tasks, concept_policy: disabled}\n").unwrap();
    let root = temp.path().join("vault");
    let day = jiff::civil::date(2026, 10, 5);
    let close = jiff::civil::date(2026, 10, 11);
    let zone = jiff::tz::TimeZone::UTC;
    let at = day
        .at(12, 0, 0, 0)
        .to_zoned(zone.clone())
        .unwrap()
        .timestamp();
    let log = lk_task::TransitionLog::new(&root);
    log.record(
        &[lk_task::Transition::new(
            "7k2p".parse().unwrap(),
            lk_task::TransitionKind::Done,
            "Baseline work",
            at,
        )
        .with_note(Some("Original contribution".into()))],
        &zone,
    )
    .unwrap();
    for date in [day, close] {
        let output = invoke(&config, &["--date", &date.to_string()]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let daily = root.join(format!("daily/tasks/{day}.md"));
    let work_log = root.join(format!("me/work-log/{day}.md"));
    complete_inputs(&daily);
    complete_inputs(&work_log);
    let deferred = invoke_command(
        &config,
        &["synthesis", "weekly", "--date", &day.to_string(), "--defer"],
    );
    assert!(
        deferred.status.success(),
        "{}",
        String::from_utf8_lossy(&deferred.stderr)
    );
    assert_eq!(resume_personal(&config)["status"], "awaiting-semantics");

    let unchanged = invoke(&config, &["tasks", "--date", &day.to_string()]);
    assert!(unchanged.status.success());
    assert_ne!(resume_personal(&config)["status"], "blocked");
    let body = std::fs::read_to_string(&daily).unwrap();
    let polished = lk_vault::replace_section(&body, "Key Events", "Polished baseline narrative.");
    std::fs::write(&daily, polished).unwrap();
    let body = std::fs::read_to_string(&work_log).unwrap();
    std::fs::write(
        &work_log,
        lk_vault::replace_section(&body, "Topic Summary", "Completed baseline work."),
    )
    .unwrap();
    assert_ne!(resume_personal(&config)["status"], "blocked");
    let retained = std::fs::read(&work_log).unwrap();

    log.record(
        &[lk_task::Transition::new(
            "3b8q".parse().unwrap(),
            lk_task::TransitionKind::Done,
            "Added work",
            at,
        )
        .with_note(Some("Additional contribution".into()))],
        &zone,
    )
    .unwrap();
    let changed = invoke(&config, &["tasks", "--date", &day.to_string()]);
    assert!(changed.status.success());
    complete_inputs(&daily);
    assert!(
        std::fs::read_to_string(&daily)
            .unwrap()
            .contains("Added work")
    );
    assert_eq!(std::fs::read(&work_log).unwrap(), retained);
    let blocked = resume_personal(&config);
    assert_eq!(blocked["status"], "blocked");
    assert!(
        blocked["reason"]
            .as_str()
            .unwrap()
            .contains("personal contributions changed")
    );

    let refreshed = invoke(&config, &["--date", &day.to_string()]);
    assert!(
        refreshed.status.success(),
        "{}",
        String::from_utf8_lossy(&refreshed.stderr)
    );
    complete_inputs(&daily);
    complete_inputs(&work_log);
    assert_eq!(resume_personal(&config)["status"], "awaiting-semantics");
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

#[test]
fn personal_period_requires_worklog_for_known_filtered_contributions_on_an_earlier_day() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("config.yaml");
    std::fs::write(&config, "vault: {root: vault, timezone: UTC, locale: en}\nidentity: {name: Test, email: test@example.invalid}\npersonal:\n  tracked_sources: [tasks]\n  tasks: {}\nsources:\n  tasks: {type: tasks, concept_policy: disabled}\n").unwrap();
    let root = temp.path().join("vault");
    let day = jiff::civil::date(2026, 10, 5);
    let close = jiff::civil::date(2026, 10, 11);
    let zone = jiff::tz::TimeZone::UTC;
    let at = day
        .at(12, 0, 0, 0)
        .to_zoned(zone.clone())
        .unwrap()
        .timestamp();
    lk_task::TransitionLog::new(&root)
        .record(
            &[lk_task::Transition::new(
                "7k2p".parse().unwrap(),
                lk_task::TransitionKind::Done,
                "Known contribution",
                at,
            )],
            &zone,
        )
        .unwrap();
    let closing = invoke(&config, &["--date", &close.to_string()]);
    assert!(closing.status.success());
    let partial = invoke(&config, &["tasks", "--date", &day.to_string()]);
    assert!(partial.status.success());
    complete_inputs(&root.join(format!("daily/tasks/{day}.md")));
    assert!(!root.join(format!("me/work-log/{day}.md")).exists());
    let deferred = invoke_command(
        &config,
        &["synthesis", "weekly", "--date", &day.to_string(), "--defer"],
    );
    assert!(deferred.status.success());
    let state = resume_personal(&config);
    assert_eq!(state["status"], "blocked");
    assert!(
        state["reason"]
            .as_str()
            .unwrap()
            .contains("known personal contributions have no published work-log")
    );
}
