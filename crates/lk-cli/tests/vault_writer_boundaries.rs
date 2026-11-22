//! Cooperating CLI writers share one vault lease; observations and dry runs don't recover journals.
use std::path::Path;
use std::process::{Command, Output};

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("vault/inbox")).unwrap();
    std::fs::create_dir_all(dir.path().join("vault/wiki/concepts")).unwrap();
    std::fs::write(dir.path().join("config.yaml"), "vault:\n  root: vault\n  locale: en\nidentity:\n  name: Test\n  email: test@example.com\nsources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: inbox\n").unwrap();
    std::fs::write(
        dir.path().join("vault/wiki/concepts/example.md"),
        "---\nid: example\ntype: concept\ntitle: Example\n---\n\n## Synthesis\n\nAn example.\n",
    )
    .unwrap();
    dir
}

fn run(root: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_lore"));
    for (key, _) in std::env::vars() {
        if key.starts_with("LORE_") {
            cmd.env_remove(key);
        }
    }
    cmd.arg("--config")
        .arg(root.join("config.yaml"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn all_page_writer_entrypoints_refuse_another_processes_vault_lease() {
    let dir = fixture();
    let vault = dir.path().join("vault");
    let held = lk_vault::MutationSession::acquire(&vault).unwrap();
    for args in [
        vec!["ingest"],
        vec!["synthesis", "weekly"],
        vec!["maintenance", "--prune-queue"],
        vec!["schema"],
        vec!["wiki", "refresh"],
        vec!["wiki", "evidence", "wiki/concepts/example.md", "--archive"],
    ] {
        let out = run(dir.path(), &args);
        assert!(!out.status.success(), "{args:?} ignored vault lease");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("vault mutation is busy"),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert!(!vault.join("wiki/index.md").exists());
    assert!(!vault.join("wiki/AGENTS.md").exists());
    assert!(!vault.join(".lorekeeper/queue").exists());
    drop(held);
    // Refresh owns one lease across all views, rather than deadlocking on nested acquisition.
    let out = run(dir.path(), &["wiki", "refresh"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(vault.join("wiki/index.md").is_file());
    assert!(vault.join(".lorekeeper/search.sqlite").is_file());
    for page in ["log.md", "map.md"] {
        assert!(!vault.join("wiki").join(page).exists());
    }
}

#[test]
fn observations_and_dry_run_neither_create_a_lease_nor_recover_a_journal() {
    let dir = fixture();
    let vault = dir.path().join("vault");
    for args in [
        vec!["wiki", "reviews"],
        vec!["wiki", "evidence", "wiki/concepts/example.md"],
        vec!["ingest", "--dry-run"],
        vec![
            "maintenance",
            "--prune-queue",
            "--dry-run",
            "--wait-for-vault",
        ],
    ] {
        let out = run(dir.path(), &args);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!vault.join(".lorekeeper/mutation").exists());
    }
    let state = vault.join(".lorekeeper/mutation");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join("pending.json"), "unreadable historical journal").unwrap();
    for args in [
        vec!["wiki", "reviews"],
        vec!["wiki", "evidence", "wiki/concepts/example.md"],
        vec!["ingest", "--dry-run"],
        vec![
            "maintenance",
            "--prune-queue",
            "--dry-run",
            "--wait-for-vault",
        ],
    ] {
        let out = run(dir.path(), &args);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(state.join("pending.json")).unwrap(),
            "unreadable historical journal"
        );
        assert!(!state.join("lock").exists());
    }
}

#[test]
fn scheduled_writers_wait_then_read_after_the_previous_writer_finishes() {
    use std::io::{BufRead, BufReader, Read};
    use std::process::Stdio;
    for args in [
        vec!["synthesis", "weekly", "--wait-for-vault"],
        vec!["maintenance", "--prune-queue", "--wait-for-vault"],
    ] {
        let dir = fixture();
        let vault = dir.path().join("vault");
        let held = lk_vault::MutationSession::acquire(&vault).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_lore"));
        for (key, _) in std::env::vars() {
            if key.starts_with("LORE_") {
                command.env_remove(key);
            }
        }
        let mut child = command
            .arg("--config")
            .arg(dir.path().join("config.yaml"))
            .args(&args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stderr = BufReader::new(child.stderr.take().unwrap());
        let mut line = String::new();
        stderr.read_line(&mut line).unwrap();
        assert!(
            line.contains("Waiting for exclusive vault access"),
            "{line}"
        );
        assert!(child.try_wait().unwrap().is_none());
        drop(held);
        let mut remaining = String::new();
        stderr.read_to_string(&mut remaining).unwrap();
        assert!(child.wait().unwrap().success(), "{args:?}: {remaining}");
    }
}

#[test]
fn maintenance_retires_unconfigured_intent_only_under_the_configuration_it_read() {
    use std::io::Read;
    use std::process::Stdio;
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path().join("vault");
    let judged = vault.join(".lorekeeper/proposals/judged/2026-11-20.jsonl");
    let snapshot = vault.join(".lorekeeper/proposals/mail.jsonl");
    std::fs::create_dir_all(judged.parent().unwrap()).unwrap();
    let kept =
        "{\"source_id\":\"tasks\",\"summary\":\"Plan\",\"url\":\"https://example.invalid/0\"}\n";
    let judgments = format!(
        "{kept}{{\"source_id\":\"mail\",\"summary\":\"Reply\",\"url\":\"https://example.invalid/1\"}}\n"
    );
    let snapshotted =
        "{\"source_id\":\"mail\",\"summary\":\"Reply\",\"url\":\"https://example.invalid/1\"}\n";
    std::fs::write(&judged, &judgments).unwrap();
    std::fs::write(&snapshot, snapshotted).unwrap();
    let config = dir.path().join("config.yaml");
    let base = "vault: {root: vault, timezone: UTC}\nidentity: {name: Test, email: test@example.invalid}\npersonal:\n  tasks: {}\nsources:\n  tasks: {type: tasks}\n";
    std::fs::write(&config, base).unwrap();

    let plane = lk_task::PlaneLock::hold(&vault).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_lore"));
    for (key, _) in std::env::vars() {
        if key.starts_with("LORE_") {
            command.env_remove(key);
        }
    }
    let mut child = command
        .arg("--config")
        .arg(&config)
        .args(["maintenance", "--wait-for-vault"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while lk_vault::MutationClaim::acquire(&vault).is_ok() {
        assert!(
            std::time::Instant::now() < deadline,
            "maintenance never held the vault"
        );
        assert!(child.try_wait().unwrap().is_none());
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    std::fs::write(
        &config,
        format!("{base}  mail: {{type: gmail, params: {{include_queries: ['from:test@example.invalid']}}}}\n"),
    )
    .unwrap();
    drop(plane);
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(!child.wait().unwrap().success(), "{stderr}");
    assert!(
        stderr.contains("selected configuration changed during the job"),
        "{stderr}"
    );
    assert_eq!(std::fs::read_to_string(&judged).unwrap(), judgments);
    assert_eq!(std::fs::read_to_string(&snapshot).unwrap(), snapshotted);
    assert!(!vault.join(".lorekeeper/mutation/pending.json").exists());

    std::fs::write(&config, base).unwrap();
    let out = run(dir.path(), &["maintenance"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(std::fs::read_to_string(&judged).unwrap(), kept);
    assert!(!snapshot.exists());
}

#[test]
fn review_check_preserves_open_and_historical_judgments_but_refuses_stale_or_invalid() {
    let dir = fixture();
    let vault = dir.path().join("vault");
    let source = "wiki/concepts/example.md";
    let evidence = lk_vault::archive_evidence(&vault, &[source.into()]).unwrap();
    let review_dir = vault.join("wiki/explorations");
    std::fs::create_dir_all(&review_dir).unwrap();
    let review_path = review_dir.join("assessment.md");
    let review = serde_json::json!({"status":"open", "evidence":evidence.citations});
    let original = format!("---\ntype: exploration\nreview: {review}\n---\nAuthored assessment.\n");
    std::fs::write(&review_path, &original).unwrap();
    let output = run(dir.path(), &["wiki", "reviews", "--check"]);
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report[0]["effective_state"], "open");

    let source_path = vault.join(source);
    let source_before = std::fs::read_to_string(&source_path).unwrap();
    std::fs::write(
        &source_path,
        format!("{source_before}\nChanged evidence.\n"),
    )
    .unwrap();
    let output = run(dir.path(), &["wiki", "reviews", "--check"]);
    assert!(!output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report[0]["effective_state"], "needs-reassessment");
    assert!(run(dir.path(), &["wiki", "reviews"]).status.success());
    assert_eq!(std::fs::read_to_string(&review_path).unwrap(), original);

    // A verified archive retains the recorded historical evidence after deletion.
    std::fs::remove_file(&source_path).unwrap();
    let output = run(dir.path(), &["wiki", "reviews", "--check"]);
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report[0]["effective_state"], "open");
    assert_eq!(report[0]["missing_evidence"], serde_json::json!([source]));

    std::fs::write(&review_path, "---\ntype: exploration\nreview: [\n---\n").unwrap();
    let output = run(dir.path(), &["wiki", "reviews", "--check"]);
    assert!(!output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report[0]["effective_state"], "invalid");
    assert!(run(dir.path(), &["wiki", "reviews"]).status.success());
}
