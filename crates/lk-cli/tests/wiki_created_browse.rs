use std::process::Command;

#[test]
fn created_browse_cli_exposes_declared_dates_and_refuses_invalid_ranges() {
    let root = tempfile::tempdir().unwrap();
    let docs = root.path().join("vault/knowledge/documents");
    std::fs::create_dir_all(&docs).unwrap();
    let config = root.path().join("config.yaml");
    std::fs::write(&config, "vault:\n  root: vault\n  dirs:\n    wiki: knowledge\nidentity:\n  name: Test\n  email: test@example.com\nsources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: inbox\n").unwrap();
    for (path, created) in [
        ("z.md", "2026-01-01"),
        ("a.md", "2026-02-01"),
        ("bad.md", "2026-02-30"),
    ] {
        std::fs::write(
            docs.join(path),
            format!("---\ntitle: Entry\ntype: document\ncreated: {created}\n---\nBody.\n"),
        )
        .unwrap();
    }
    let run = |args: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_lore"));
        for (key, _) in std::env::vars() {
            if key.starts_with("LORE_") {
                command.env_remove(key);
            }
        }
        command
            .arg("--config")
            .arg(&config)
            .args(args)
            .output()
            .unwrap()
    };
    let output = run(&[
        "wiki", "browse", "--sort", "created", "--limit", "1", "--json",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["hits"][0]["path"], "knowledge/documents/a.md");
    assert_eq!(report["hits"][0]["created"], "2026-02-01");
    assert_eq!(report["hits"][0]["created_status"], "valid");
    assert_eq!(report["date_basis"], "declared-created-not-ingestion");
    assert_eq!(report["next_offset"], 1);
    let output = run(&[
        "wiki",
        "browse",
        "--from",
        "2026-02-01",
        "--to",
        "2026-02-01",
        "--json",
    ]);
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["total"], 1);
    for args in [
        vec!["wiki", "browse", "--from", "2026-02-30"],
        vec![
            "wiki",
            "browse",
            "--from",
            "2026-02-01",
            "--to",
            "2026-01-01",
        ],
        vec!["wiki", "search", "Entry", "--sort", "created"],
    ] {
        assert!(!run(&args).status.success());
    }
}
