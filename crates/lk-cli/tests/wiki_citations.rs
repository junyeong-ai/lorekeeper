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
fn markdown_line_endings_agree_across_capture_links_search_and_retraction() {
    for endings in [
        ["\n", "\n", "\n", "\n"],
        ["\r\n", "\r\n", "\r\n", "\r\n"],
        ["\r", "\r", "\r", "\r"],
        ["\r", "\r\n", "\r", "\n"],
    ] {
        let root = tempfile::tempdir().unwrap();
        let vault = root.path().join("vault");
        std::fs::create_dir_all(vault.join("wiki/documents")).unwrap();
        std::fs::create_dir_all(vault.join("wiki/concepts")).unwrap();
        std::fs::write(root.path().join("config.yaml"), "vault:\n  root: vault\n  locale: en\nidentity:\n  name: T\n  email: t@example.com\nsources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: inbox\n").unwrap();
        let concept_path = "wiki/concepts/topic.md";
        let concept = "---\ntype: concept\ntitle: Topic\n---\n## Synthesis\nA scoped claim.\n\n## Sources\n\n";
        std::fs::write(vault.join(concept_path), concept).unwrap();
        let origin = "wiki/documents/source.md";
        let link = "- [Topic](../concepts/topic.md)";
        let raw = format!(
            "---\ntype: document\ntitle: Source\n---\n# Source{}## Related Concepts{}{link}{}## Tail{}Keep 한글🙂.\n",
            endings[0], endings[1], endings[2], endings[3]
        );
        std::fs::write(vault.join(origin), &raw).unwrap();
        let live_links = report(run(root.path(), &["wiki", "links", origin]));
        assert_eq!(live_links["links"][0]["line"], 7, "{endings:?}");
        assert_eq!(live_links["links"][0]["context"], link);
        let prepared = report(run(root.path(), &["wiki", "prepare"]));
        let snapshot = prepared["snapshot"].as_str().unwrap();
        let captured_links = report(run(
            root.path(),
            &["wiki", "links", origin, "--snapshot", snapshot],
        ));
        assert_eq!(captured_links["total"], live_links["total"]);
        for key in [
            "line",
            "label",
            "destination",
            "target",
            "context",
            "context_truncated",
        ] {
            assert_eq!(captured_links["links"][0][key], live_links["links"][0][key]);
        }
        for captured in [false, true] {
            let mut args = vec![
                "wiki",
                "search",
                "Topic",
                "--body",
                "--context",
                "0",
                "--json",
            ];
            if captured {
                args.extend(["--snapshot", snapshot]);
            }
            let body = report(run(root.path(), &args));
            assert_eq!(body["total"], 1);
            assert_eq!(body["hits"][0]["line"], 7, "{endings:?}");
        }
        assert_eq!(std::fs::read(vault.join(origin)).unwrap(), raw.as_bytes());
        let revision = lk_core::concept::file_revision(&raw);
        let concept_revision = lk_core::concept::file_revision(concept);
        let mut retract = vec![
            "graph",
            "--json",
            "retract-citation",
            origin,
            concept_path,
            "--revision",
            &revision,
            "--concept-revision",
            &concept_revision,
            "--dry-run",
        ];
        report(run(root.path(), &retract));
        assert_eq!(std::fs::read(vault.join(origin)).unwrap(), raw.as_bytes());
        retract.pop();
        report(run(root.path(), &retract));
        assert_eq!(
            std::fs::read_to_string(vault.join(origin)).unwrap(),
            raw.replace(&format!("{link}{}", endings[2]), "")
        );
        assert_eq!(
            std::fs::read_to_string(vault.join(concept_path)).unwrap(),
            concept
        );
    }
}

#[test]
fn carriage_return_code_blocks_never_become_captured_links_or_outline_sections() {
    for ending in ["\n", "\r\n", "\r"] {
        let root = tempfile::tempdir().unwrap();
        let vault = root.path().join("vault");
        std::fs::create_dir_all(vault.join("wiki/documents")).unwrap();
        std::fs::write(root.path().join("config.yaml"), "vault:\n  root: vault\nidentity:\n  name: T\n  email: t@example.com\nsources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: inbox\n").unwrap();
        let path = "wiki/documents/source.md";
        let raw = format!(
            "---\ntype: document\ntitle: Source\n---\n```md{ending}## Hidden{ending}[Ghost](ghost.md){ending}```\n[Live](live.md)\n"
        );
        std::fs::write(vault.join(path), &raw).unwrap();
        std::fs::write(
            vault.join("wiki/documents/live.md"),
            "---\ntype: document\ntitle: Live\n---\nKept.\n",
        )
        .unwrap();
        let live = report(run(root.path(), &["wiki", "links", path]));
        assert_eq!(live["total"], 1, "{ending:?}");
        assert_eq!(live["links"][0]["destination"], "live.md");
        assert_eq!(live["links"][0]["line"], 9);
        let outline = report(run(root.path(), &["wiki", "read", path]));
        assert_eq!(outline["total_sections"], 1);
        let prepared = report(run(root.path(), &["wiki", "prepare"]));
        let snapshot = prepared["snapshot"].as_str().unwrap();
        let captured = report(run(
            root.path(),
            &["wiki", "links", path, "--snapshot", snapshot],
        ));
        assert_eq!(captured["total"], 1);
        assert_eq!(captured["links"][0]["destination"], "live.md");
        assert_eq!(captured["links"][0]["line"], 9);
        assert_eq!(std::fs::read(vault.join(path)).unwrap(), raw.as_bytes());
    }
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
fn context_groups_preserve_authority_and_pagination_without_live_page_reads() {
    let root = tempfile::tempdir().unwrap();
    let vault = root.path().join("vault");
    std::fs::create_dir_all(vault.join("wiki/concepts")).unwrap();
    std::fs::create_dir_all(vault.join("wiki/documents")).unwrap();
    std::fs::write(root.path().join("config.yaml"), "vault:\n  root: vault\nidentity:\n  name: T\n  email: t@example.com\nsources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: inbox\n").unwrap();
    let concept = "wiki/concepts/topic.md";
    std::fs::write(
        vault.join(concept),
        "---\ntype: concept\ntitle: Topic\n---\nA scoped topic.\n[Context](../documents/a.md)\n",
    )
    .unwrap();
    for (name, links) in [
        (
            "a",
            "[Topic](../concepts/topic.md)\n[Again](../concepts/topic.md)",
        ),
        ("b", "[Topic](../concepts/topic.md)"),
    ] {
        std::fs::write(
            vault.join(format!("wiki/documents/{name}.md")),
            format!("---\ntype: document\ntitle: {name}\n---\n{links}\n"),
        )
        .unwrap();
    }
    let prepared = report(run(root.path(), &["wiki", "prepare"]));
    let id = prepared["snapshot"].as_str().unwrap();
    let index = vault.join(".lorekeeper/search.sqlite");
    let index_before = std::fs::read(&index).unwrap();
    let first = report(run(
        root.path(),
        &["wiki", "context", concept, "--snapshot", id, "--limit", "1"],
    ));
    assert_eq!(first["links"]["total"], 4);
    assert_eq!(first["links"]["links"].as_array().unwrap().len(), 1);
    assert_eq!(first["evidence"]["status"], "available");
    assert_eq!(first["evidence"]["data"]["total"], 2);
    assert_eq!(
        first["evidence"]["data"]["citations"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(first["declared_relations"]["status"], "not-applicable");
    assert_eq!(first["reviews"]["total"], 0);
    assert_eq!(first["next_offset"], 1);
    for name in ["a", "b"] {
        std::fs::remove_file(vault.join(format!("wiki/documents/{name}.md"))).unwrap();
    }
    let second = report(run(
        root.path(),
        &[
            "wiki",
            "context",
            concept,
            "--snapshot",
            id,
            "--limit",
            "1",
            "--offset",
            "1",
        ],
    ));
    assert_ne!(
        first["evidence"]["data"]["citations"],
        second["evidence"]["data"]["citations"]
    );
    assert_eq!(second["capture"], prepared);
    let document = report(run(
        root.path(),
        &["wiki", "context", "wiki/documents/a.md", "--snapshot", id],
    ));
    assert_eq!(document["evidence"]["status"], "not-applicable");
    assert_eq!(document["declared_relations"]["status"], "available");
    assert_eq!(std::fs::read(index).unwrap(), index_before);
    assert!(
        !run(
            root.path(),
            &["wiki", "context", concept, "--snapshot", id, "--limit", "0"]
        )
        .status
        .success()
    );
    report(run(root.path(), &["wiki", "prepare"]));
    assert!(
        !run(root.path(), &["wiki", "context", concept, "--snapshot", id])
            .status
            .success()
    );
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
