use std::path::Path;
use std::process::{Command, Output};

fn run(root: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lore"));
    for (key, _) in std::env::vars() {
        if key.starts_with("LORE_") {
            command.env_remove(key);
        }
    }
    command
        .arg("--config")
        .arg(root.join("config.yaml"))
        .args(args)
        .output()
        .unwrap()
}

fn report(output: Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn snapshot_metadata_remains_captured_after_external_edits_without_writes() {
    let root = tempfile::tempdir().unwrap();
    let vault = root.path().join("vault");
    std::fs::create_dir_all(vault.join("wiki/documents")).unwrap();
    std::fs::write(root.path().join("config.yaml"), "vault:\n  root: vault\nidentity:\n  name: T\n  email: t@example.com\nsources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: inbox\n").unwrap();
    assert!(!run(root.path(), &["wiki", "snapshot"]).status.success());
    assert!(!vault.join(".lorekeeper").exists());
    let page = vault.join("wiki/documents/example.md");
    std::fs::write(
        &page,
        "---\ntype: document\ntitle: Example\n---\nOriginal.\n",
    )
    .unwrap();
    let prepared = report(run(root.path(), &["wiki", "prepare"]));
    let index = vault.join(".lorekeeper/search.sqlite");
    let before = std::fs::read(&index).unwrap();
    std::fs::write(&page, "---\ninvalid: [\n---\nExternal edit.\n").unwrap();
    let captured = report(run(root.path(), &["wiki", "snapshot"]));
    assert_eq!(captured, prepared);
    assert_eq!(std::fs::read(index).unwrap(), before);
    assert!(!run(root.path(), &["wiki", "prepare"]).status.success());
    assert_eq!(report(run(root.path(), &["wiki", "snapshot"])), prepared);
}

#[test]
fn cli_preparation_and_refresh_publish_exact_citations_without_rewriting_concepts() {
    let root = tempfile::tempdir().unwrap();
    let vault = root.path().join("vault");
    std::fs::create_dir_all(vault.join("knowledge/concepts")).unwrap();
    std::fs::create_dir_all(vault.join("observations")).unwrap();
    std::fs::write(root.path().join("config.yaml"), "vault:\n  root: vault\n  dirs:\n    wiki: knowledge\n    daily: observations\nidentity:\n  name: T\n  email: t@example.com\nsources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: inbox\n").unwrap();
    let concept = "knowledge/concepts/topic.md";
    let authored = "---\ntype: concept\ntitle: Topic\nsource_count: 99\n---\n## Synthesis\nAuthored prose.\n## Sources\nKeep this exact existing projection.\n## Related\nAuthored relations.\n";
    std::fs::write(vault.join(concept), authored).unwrap();
    for name in ["a", "b"] {
        std::fs::write(vault.join(format!("observations/{name}.md")), "---\ntype: daily\n---\n[Topic](../knowledge/concepts/topic.md)\n[Again](../knowledge/concepts/topic.md)\n").unwrap();
    }
    std::fs::write(
        vault.join("observations/board.md"),
        "---\ntype: task-board\n---\n[Topic](../knowledge/concepts/topic.md)\n",
    )
    .unwrap();
    let prepared = report(run(root.path(), &["wiki", "prepare"]));
    assert_eq!(prepared["citations_available"], true);
    let snapshot = prepared["snapshot"].as_str().unwrap();
    let first = report(run(
        root.path(),
        &[
            "wiki",
            "citations",
            concept,
            "--snapshot",
            snapshot,
            "--limit",
            "1",
        ],
    ));
    assert_eq!(first["source_count"], 2);
    assert_eq!(first["total"], 2);
    assert_eq!(first["citations"][0]["path"], "observations/a.md");
    assert_eq!(first["next_offset"], 1);
    std::fs::remove_file(vault.join("observations/b.md")).unwrap();
    let second = report(run(
        root.path(),
        &[
            "wiki",
            "citations",
            concept,
            "--snapshot",
            snapshot,
            "--limit",
            "1",
            "--offset",
            "1",
        ],
    ));
    assert_eq!(second["citations"][0]["path"], "observations/b.md");
    assert_eq!(second["digest"], first["digest"]);
    assert_eq!(second["next_offset"], serde_json::Value::Null);
    assert!(
        !run(
            root.path(),
            &[
                "wiki",
                "citations",
                concept,
                "--snapshot",
                snapshot,
                "--limit",
                "101"
            ]
        )
        .status
        .success()
    );
    let refreshed = run(root.path(), &["wiki", "refresh"]);
    assert!(
        refreshed.status.success(),
        "{}",
        String::from_utf8_lossy(&refreshed.stderr)
    );
    assert!(
        !run(
            root.path(),
            &["wiki", "citations", concept, "--snapshot", snapshot]
        )
        .status
        .success()
    );
    let prepared = report(run(root.path(), &["wiki", "prepare"]));
    let current = report(run(
        root.path(),
        &[
            "wiki",
            "citations",
            concept,
            "--snapshot",
            prepared["snapshot"].as_str().unwrap(),
        ],
    ));
    assert_eq!(current["source_count"], 1);
    assert_eq!(
        std::fs::read_to_string(vault.join(concept)).unwrap(),
        authored
    );
}
