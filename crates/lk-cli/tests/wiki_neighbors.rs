use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

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

fn report(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn cli_neighbors_use_captured_distinct_pages_and_explicit_lifecycle_without_writes() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("config.yaml"), "vault:\n  root: vault\nidentity:\n  name: Test\n  email: test@example.com\nsources:\n  notes:\n    type: manual\n    enabled: false\n").unwrap();
    for (path, text) in [
        (
            "wiki/concepts/center.md",
            "---\ntype: concept\ntitle: Center\n---\n[B](../documents/b.md) [Repeated](../documents/b.md#part)\n[C](c.md) [Missing](absent.md) [Self](center.md)\n",
        ),
        (
            "wiki/documents/b.md",
            "---\ntype: document\ntitle: B\n---\n[Center](../concepts/center.md)\n",
        ),
        (
            "wiki/concepts/c.md",
            "---\ntype: concept\ntitle: C\nknowledge_status: retired\n---\nRetired knowledge.\n",
        ),
    ] {
        let path = root.path().join("vault").join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    let prepared = report(run(root.path(), &["wiki", "prepare"]));
    let snapshot = prepared["snapshot"].as_str().unwrap();
    let database = root.path().join("vault/.lorekeeper/search.sqlite");
    let before = std::fs::read(&database).unwrap();
    let base = [
        "wiki",
        "neighbors",
        "wiki/concepts/center.md",
        "--snapshot",
        snapshot,
    ];
    let first = report(run(root.path(), &base));
    assert_eq!(first["total"], 1);
    assert_eq!(first["neighbors"][0]["path"], "wiki/documents/b.md");
    assert_eq!(first["neighbors"][0]["incoming"], true);
    assert_eq!(first["neighbors"][0]["outgoing"], true);
    assert_eq!(first["neighbors"][0]["knowledge_status"], "active");
    assert!(first["neighbors"][0]["file_revision"].is_string());
    let mut args = base.to_vec();
    args.extend(["--knowledge-status", "all", "--limit", "1"]);
    let all = report(run(root.path(), &args));
    assert_eq!(all["total"], 2);
    assert_eq!(all["neighbors"][0]["path"], "wiki/concepts/c.md");
    assert_eq!(all["next_offset"], 1);
    args.extend(["--offset", "1"]);
    assert_eq!(
        report(run(root.path(), &args))["neighbors"][0]["path"],
        "wiki/documents/b.md"
    );
    for extra in [
        vec!["--direction", "mutual"],
        vec!["--type", "document"],
        vec!["--direction", "incoming"],
    ] {
        let mut args = base.to_vec();
        args.extend(extra);
        assert_eq!(report(run(root.path(), &args))["total"], 1);
    }
    let mut args = base.to_vec();
    args.extend(["--knowledge-status", "retired"]);
    let retired = report(run(root.path(), &args));
    assert_eq!(retired["total"], 1);
    assert_eq!(retired["neighbors"][0]["knowledge_status"], "retired");
    std::fs::write(
        root.path().join("vault/wiki/documents/b.md"),
        "---\nmalformed: [\n---\n",
    )
    .unwrap();
    assert_eq!(
        report(run(root.path(), &base))["neighbors"],
        first["neighbors"]
    );
    let occurrences = report(run(
        root.path(),
        &[
            "wiki",
            "links",
            "wiki/concepts/center.md",
            "--snapshot",
            snapshot,
            "--direction",
            "both",
        ],
    ));
    assert_eq!(occurrences["total"], 6);
    for extra in [vec!["--limit", "0"], vec!["--limit", "101"]] {
        let mut args = base.to_vec();
        args.extend(extra);
        assert!(!run(root.path(), &args).status.success());
    }
    assert!(
        !run(
            root.path(),
            &["wiki", "neighbors", "wiki/concepts/center.md"]
        )
        .status
        .success()
    );
    assert!(
        !run(
            root.path(),
            &["wiki", "neighbors", "../center.md", "--snapshot", snapshot]
        )
        .status
        .success()
    );
    assert_eq!(std::fs::read(database).unwrap(), before);
}
