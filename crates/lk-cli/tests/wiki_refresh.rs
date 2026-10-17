use std::process::Command;

#[test]
fn sqlite_refresh_exports_opt_in_and_preserves_originals_on_failure() {
    let temp = tempfile::tempdir().unwrap();
    let vault = temp.path().join("vault");
    let documents = vault.join("knowledge/documents");
    std::fs::create_dir_all(&documents).unwrap();
    let original = "---\ntype: document\ntitle: Original\ncreated: 2026-01-01\n---\n# Original\n\nSource body.\n";
    std::fs::write(documents.join("original.md"), original).unwrap();
    let config = temp.path().join("config.yaml");
    let configure = |exports: bool| {
        std::fs::write(&config, format!("vault:\n  root: vault\n  navigation:\n    markdown_exports: {exports}\n  dirs:\n    wiki: knowledge\nidentity:\n  name: T\n  email: t@example.com\nsources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: inbox\n")).unwrap();
    };
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_lore"))
            .arg("--config")
            .arg(&config)
            .args(args)
            .output()
            .unwrap()
    };
    configure(false);
    let refreshed = run(&["wiki", "refresh"]);
    assert!(
        refreshed.status.success(),
        "{}",
        String::from_utf8_lossy(&refreshed.stderr)
    );
    let entry = std::fs::read_to_string(vault.join("knowledge/index.md")).unwrap();
    assert!(entry.contains("lore wiki browse"));
    assert!(!vault.join("knowledge/log.md").exists());
    assert!(!vault.join("knowledge/map.md").exists());
    assert!(run(&["wiki", "index"]).status.success());
    let lint = run(&["graph", "--json", "lint"]);
    let lint: serde_json::Value = serde_json::from_slice(&lint.stdout).unwrap();
    assert!(lint["violations"]["index"].is_null());
    configure(true);
    let exported = run(&["wiki", "refresh"]);
    assert!(
        exported.status.success(),
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );
    let prior = ["index.md", "log.md", "map.md"]
        .map(|name| std::fs::read(vault.join("knowledge").join(name)).unwrap());
    configure(false);
    std::fs::write(documents.join("unreadable.md"), [0xff, 0xfe]).unwrap();
    assert!(!run(&["wiki", "refresh"]).status.success());
    for (name, before) in ["index.md", "log.md", "map.md"].into_iter().zip(prior) {
        assert_eq!(
            std::fs::read(vault.join("knowledge").join(name)).unwrap(),
            before
        );
    }
    std::fs::remove_file(documents.join("unreadable.md")).unwrap();
    // A file in the reserved export directory is not generated without its marker.
    std::fs::create_dir_all(vault.join("knowledge/catalog")).unwrap();
    std::fs::write(vault.join("knowledge/catalog/authored.md"), original).unwrap();
    assert!(run(&["wiki", "refresh"]).status.success());
    assert_eq!(
        std::fs::read_to_string(vault.join("knowledge/catalog/authored.md")).unwrap(),
        original
    );
    assert_eq!(
        std::fs::read_to_string(documents.join("original.md")).unwrap(),
        original
    );
    assert!(!vault.join("knowledge/log.md").exists());
    assert!(!vault.join("knowledge/map.md").exists());
}

#[test]
fn refresh_and_exports_refuse_manual_inbox_overlap_before_capture_or_retirement() {
    let temp = tempfile::tempdir().unwrap();
    let vault = temp.path().join("vault");
    let catalog = vault.join("knowledge/catalog");
    std::fs::create_dir_all(&catalog).unwrap();
    let original = "<!-- lorekeeper:catalog -->\nRaw user input, not an export.\n";
    std::fs::write(catalog.join("0-0.md"), original).unwrap();
    let config = temp.path().join("config.yaml");
    let inboxes = vec![catalog.clone()];
    #[cfg(unix)]
    let inboxes = {
        let mut inboxes = inboxes;
        let alias = temp.path().join("inbox-alias");
        std::os::unix::fs::symlink(&catalog, &alias).unwrap();
        inboxes.push(alias);
        inboxes
    };
    for inbox in inboxes {
        std::fs::write(&config, format!("vault:\n  root: vault\n  dirs:\n    wiki: knowledge\nidentity:\n  name: T\n  email: t@example.com\nsources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: {}\n", serde_json::to_string(&inbox.to_string_lossy()).unwrap())).unwrap();
        let approval = format!("knowledge/index.md={}", "0".repeat(64));
        for args in [
            vec!["wiki", "refresh"],
            vec!["wiki", "refresh", "--archive-replace", approval.as_str()],
            vec!["wiki", "index"],
            vec!["wiki", "log"],
            vec!["wiki", "map"],
            vec!["graph", "index-sync", "--fix"],
        ] {
            let output = Command::new(env!("CARGO_BIN_EXE_lore"))
                .arg("--config")
                .arg(&config)
                .args(&args)
                .output()
                .unwrap();
            assert!(!output.status.success(), "{args:?}");
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .contains("inbox overlaps generated navigation")
            );
            assert_eq!(
                std::fs::read_to_string(catalog.join("0-0.md")).unwrap(),
                original
            );
            assert!(!vault.join(".lorekeeper/search.sqlite").exists());
            assert!(!vault.join("knowledge/index.md").exists());
        }
    }
}

#[test]
fn explicit_revision_guarded_migration_preserves_edited_originals_and_authored_children() {
    let temp = tempfile::tempdir().unwrap();
    let vault = temp.path().join("vault");
    let wiki = vault.join("knowledge");
    std::fs::create_dir_all(wiki.join("catalog")).unwrap();
    let originals = [
        (
            "index.md",
            "# Old generated index\n\nA hand-written addition.\n",
        ),
        ("log.md", "# Old log\n\nUnmarked timeline.\n"),
        ("map.md", "---\ntype: map\n---\n# Old map\n"),
    ];
    for (name, body) in originals {
        std::fs::write(wiki.join(name), body).unwrap();
    }
    std::fs::write(wiki.join("catalog/authored.md"), "Keep authored child\n").unwrap();
    std::fs::write(
        wiki.join("catalog/0-0.md"),
        "<!-- lorekeeper:catalog -->\nOld child\n",
    )
    .unwrap();
    let config = temp.path().join("config.yaml");
    std::fs::write(&config, "vault:\n  root: vault\n  dirs:\n    wiki: knowledge\nidentity:\n  name: T\n  email: t@example.com\nsources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: inbox\n").unwrap();
    let run = |args: &[String]| {
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
    let mut args = vec!["wiki".to_string(), "refresh".to_string()];
    for (name, body) in originals {
        args.extend([
            "--archive-replace".to_string(),
            format!("knowledge/{name}={}", lk_core::concept::file_revision(body)),
        ]);
    }
    let mut stale = args.clone();
    stale[3] = format!("knowledge/index.md={}", "0".repeat(64));
    assert!(!run(&stale).status.success());
    assert!(!vault.join(".lorekeeper/search.sqlite").exists());
    assert!(!vault.join(".lorekeeper/navigation-archives").exists());
    for (name, body) in originals {
        assert_eq!(std::fs::read_to_string(wiki.join(name)).unwrap(), body);
    }
    let output = run(&args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let receipt = stderr
        .lines()
        .find_map(|line| line.strip_prefix("Preserved navigation originals; receipt "))
        .unwrap();
    let receipt: serde_json::Value =
        serde_json::from_slice(&std::fs::read(vault.join(receipt)).unwrap()).unwrap();
    assert_eq!(receipt["scheme"], "lore-navigation-archive-v1");
    assert_eq!(receipt["originals"].as_array().unwrap().len(), 3);
    for entry in receipt["originals"].as_array().unwrap() {
        let path = entry["original_path"].as_str().unwrap();
        let expected = originals
            .iter()
            .find(|(name, _)| path == format!("knowledge/{name}"))
            .unwrap()
            .1;
        assert_eq!(
            std::fs::read_to_string(vault.join(entry["archive_path"].as_str().unwrap())).unwrap(),
            expected
        );
        assert_eq!(
            entry["file_revision"],
            lk_core::concept::file_revision(expected)
        );
    }
    assert!(
        std::fs::read_to_string(wiki.join("index.md"))
            .unwrap()
            .contains("lore wiki browse")
    );
    assert!(!wiki.join("log.md").exists());
    assert!(!wiki.join("map.md").exists());
    assert!(!wiki.join("catalog/0-0.md").exists());
    assert_eq!(
        std::fs::read_to_string(wiki.join("catalog/authored.md")).unwrap(),
        "Keep authored child\n"
    );
    // Normal subsequent refresh requires no migration permission.
    assert!(run(&["wiki".into(), "refresh".into()]).status.success());
}
