use std::path::Path;
use std::process::{Command, Output};

fn run(config: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_lore"))
        .arg("--config")
        .arg(config)
        .args(args)
        .output()
        .unwrap()
}

fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("config.yaml");
    std::fs::write(&config, "vault:\n  root: vault\n  dirs:\n    wiki: knowledge\nidentity:\n  name: T\n  email: t@example.com\nsources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: inbox\n").unwrap();
    std::fs::create_dir_all(temp.path().join("vault/knowledge/documents")).unwrap();
    (temp, config)
}

#[test]
fn refresh_rebuilds_sqlite_discovery_without_materializing_markdown_lists() {
    let (temp, config) = fixture();
    let vault = temp.path().join("vault");
    let documents = vault.join("knowledge/documents");
    let page = |name: &str| {
        format!("---\ntype: document\ntitle: {name}\n---\n# {name}\n\nOriginal authored body.\n")
    };
    std::fs::write(documents.join("alpha.md"), page("Alpha")).unwrap();
    for name in ["log.md", "map.md", "catalog/authored.md"] {
        let path = vault.join("knowledge").join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "Preserve this existing user file.\n").unwrap();
    }
    let refresh = run(&config, &["wiki", "refresh"]);
    assert!(
        refresh.status.success(),
        "{}",
        String::from_utf8_lossy(&refresh.stderr)
    );
    let entry = std::fs::read_to_string(vault.join("knowledge/index.md")).unwrap();
    assert!(entry.contains("lore wiki browse"));
    assert!(!entry.contains("[Alpha]"));
    assert!(vault.join(".lorekeeper/search.sqlite").is_file());
    std::fs::write(documents.join("beta.md"), page("Beta")).unwrap();
    assert!(run(&config, &["wiki", "refresh"]).status.success());
    let found = run(&config, &["wiki", "search", "Beta", "--json"]);
    assert!(
        found.status.success(),
        "{}",
        String::from_utf8_lossy(&found.stderr)
    );
    let found: serde_json::Value = serde_json::from_slice(&found.stdout).unwrap();
    assert_eq!(found["hits"][0]["path"], "knowledge/documents/beta.md");
    let lint = run(&config, &["graph", "--json", "lint"]);
    assert!(
        lint.status.success(),
        "{}",
        String::from_utf8_lossy(&lint.stderr)
    );
    let lint: serde_json::Value = serde_json::from_slice(&lint.stdout).unwrap();
    assert!(lint["data"]["violations"].get("index").is_none());
    for name in ["log.md", "map.md", "catalog/authored.md"] {
        assert_eq!(
            std::fs::read_to_string(vault.join("knowledge").join(name)).unwrap(),
            "Preserve this existing user file.\n"
        );
    }
    assert_eq!(
        std::fs::read_to_string(documents.join("alpha.md")).unwrap(),
        page("Alpha")
    );
    assert_eq!(
        std::fs::read_to_string(vault.join("knowledge/index.md")).unwrap(),
        entry
    );
}

#[test]
fn failed_capture_preserves_the_existing_guide_and_snapshot() {
    let (temp, config) = fixture();
    let vault = temp.path().join("vault");
    assert!(run(&config, &["wiki", "refresh"]).status.success());
    let entry = std::fs::read(vault.join("knowledge/index.md")).unwrap();
    let snapshot = std::fs::read(vault.join(".lorekeeper/search.sqlite")).unwrap();
    std::fs::write(
        vault.join("knowledge/documents/unreadable.md"),
        [0xff, 0xfe],
    )
    .unwrap();
    assert!(!run(&config, &["wiki", "refresh"]).status.success());
    assert_eq!(
        std::fs::read(vault.join("knowledge/index.md")).unwrap(),
        entry
    );
    assert_eq!(
        std::fs::read(vault.join(".lorekeeper/search.sqlite")).unwrap(),
        snapshot
    );
    assert!(!vault.join("knowledge/log.md").exists());
    assert!(!vault.join("knowledge/map.md").exists());
}

#[test]
fn guide_collision_is_refused_without_touching_authored_files_or_creating_a_snapshot() {
    let (temp, config) = fixture();
    let vault = temp.path().join("vault");
    let original = "# Authored index\n\nKeep original prose.\n";
    std::fs::write(vault.join("knowledge/index.md"), original).unwrap();
    assert!(!run(&config, &["wiki", "refresh"]).status.success());
    assert_eq!(
        std::fs::read_to_string(vault.join("knowledge/index.md")).unwrap(),
        original
    );
    assert!(!vault.join(".lorekeeper/search.sqlite").exists());
    assert!(!vault.join(".lorekeeper/navigation-archives").exists());
}

#[test]
fn refresh_refuses_manual_inbox_overlap_before_capture() {
    let (temp, config) = fixture();
    let vault = temp.path().join("vault");
    let wiki = vault.join("knowledge");
    let inboxes = vec![wiki.clone()];
    #[cfg(unix)]
    let inboxes = {
        let mut inboxes = inboxes;
        let alias = temp.path().join("inbox-alias");
        std::os::unix::fs::symlink(&wiki, &alias).unwrap();
        inboxes.push(alias);
        inboxes
    };
    for inbox in inboxes {
        std::fs::write(&config, format!("vault:\n  root: vault\n  dirs:\n    wiki: knowledge\nidentity:\n  name: T\n  email: t@example.com\nsources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: {}\n", serde_json::to_string(&inbox.to_string_lossy()).unwrap())).unwrap();
        let output = run(&config, &["wiki", "refresh"]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("overlaps"));
        assert!(!vault.join(".lorekeeper/search.sqlite").exists());
        assert!(!wiki.join("index.md").exists());
    }
}
