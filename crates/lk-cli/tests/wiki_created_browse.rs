use std::process::Command;

#[test]
fn keyword_search_composes_exact_ownership_path_and_declared_created_bounds() {
    let root = tempfile::tempdir().unwrap();
    let vault = root.path().join("vault");
    let config = root.path().join("config.yaml");
    std::fs::write(&config, "vault:\n  root: vault\nidentity:\n  name: Test\n  email: test@example.com\nsources:\n  notes:\n    type: manual\n    enabled: false\n").unwrap();
    for (path, identity, created) in [
        (
            "team/a.md",
            "{repository: project-a, document: a}",
            "2026-10-01",
        ),
        (
            "team/old.md",
            "{repository: project-a, document: old}",
            "2026-09-30",
        ),
        (
            "teammate/decoy.md",
            "{repository: project-a, document: decoy}",
            "2026-10-01",
        ),
        (
            "team/drive.md",
            "{provider: google-drive, namespace: project-a, document: a}",
            "2026-10-01",
        ),
    ] {
        let path = vault.join("wiki/documents").join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("---\ntype: document\ntitle: Evidence\ncreated: {created}\nsource_identity: {identity}\ntags: [adr]\n---\n## Summary\nRetention decision.\n")).unwrap();
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
    let decode = |output: std::process::Output| {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    let base = [
        "wiki",
        "search",
        "Retention",
        "--repository",
        "project-a",
        "--path-prefix",
        "wiki/documents/team",
        "--from",
        "2026-10-01",
        "--to",
        "2026-10-31",
        "--tag",
        "ADR",
        "--json",
    ];
    let live = decode(run(&base));
    assert_eq!(live["total"], 1);
    assert_eq!(live["hits"][0]["path"], "wiki/documents/team/a.md");
    assert_eq!(live["repository"], "project-a");
    assert_eq!(live["date_basis"], "declared-created-not-ingestion");
    let mut body_args = base.to_vec();
    body_args.push("--body");
    assert_eq!(decode(run(&body_args))["total"], 1);
    let provider = decode(run(&[
        "wiki",
        "search",
        "Retention",
        "--provider",
        "google-drive",
        "--namespace",
        "project-a",
        "--json",
    ]));
    assert_eq!(provider["total"], 1);
    assert_eq!(provider["hits"][0]["path"], "wiki/documents/team/drive.md");
    for args in [
        vec!["wiki", "browse", "--provider", "google-drive"],
        vec!["wiki", "browse", "--namespace", "project-a"],
        vec![
            "wiki",
            "browse",
            "--provider",
            "made-up",
            "--namespace",
            "project-a",
        ],
        vec![
            "wiki",
            "browse",
            "--repository",
            "project-a",
            "--provider",
            "google-drive",
            "--namespace",
            "project-a",
        ],
        vec![
            "wiki",
            "search",
            "Retention",
            "--path-prefix",
            "wiki/../documents",
        ],
        vec![
            "wiki",
            "search",
            "Retention",
            "--from",
            "2026-10-02",
            "--to",
            "2026-10-01",
        ],
    ] {
        assert!(!run(&args).status.success(), "{args:?}");
    }
    assert!(!vault.join(".lorekeeper").exists());

    let capture = decode(run(&["wiki", "prepare"]));
    let snapshot = capture["snapshot"].as_str().unwrap();
    let mut captured_args = base.to_vec();
    captured_args.extend(["--snapshot", snapshot]);
    let captured = decode(run(&captured_args));
    assert_eq!(live["hits"], captured["hits"]);
    std::fs::write(vault.join("wiki/documents/team/a.md"), "---\ntype: document\ntitle: Moved ownership\ncreated: 2026-10-01\nsource_identity: {repository: other, document: a}\n---\n## Summary\nRetention decision.\n").unwrap();
    assert_eq!(decode(run(&base))["total"], 0);
    assert_eq!(decode(run(&captured_args))["total"], 1);
}

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
