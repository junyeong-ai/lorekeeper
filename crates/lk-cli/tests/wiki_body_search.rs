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

fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("vault/knowledge/documents")).unwrap();
    std::fs::create_dir_all(root.path().join("vault/observations/mail")).unwrap();
    std::fs::write(root.path().join("config.yaml"), "vault:\n  root: vault\n  dirs:\n    wiki: knowledge\n    daily: observations\nidentity:\n  name: T\n  email: t@example.com\nsources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: inbox\n").unwrap();
    std::fs::write(root.path().join("vault/knowledge/documents/a.md"), "---\ntitle: Example\ntype: document\ncategory: ops\n---\n\n## Evidence\n한글 a+b and a+b\nA+B\n[target](unusual-destination.md)\n").unwrap();
    std::fs::write(
        root.path().join("vault/observations/mail/a.md"),
        "---\ntype: daily\ncategory: ops\nsource_record: {status: active}\n---\na+b\n",
    )
    .unwrap();
    root
}

fn report(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn cli_body_literal_regex_scope_and_read_revision_are_connected_without_writes() {
    let root = fixture();
    let output = run(
        root.path(),
        &[
            "wiki",
            "search",
            "a+b",
            "--body",
            "--context",
            "0",
            "--json",
        ],
    );
    let first = report(&output);
    assert_eq!(first["mode"], "body");
    assert_eq!(first["total"], 1);
    assert_eq!(first["hits"][0]["line"], 8);
    assert_eq!(first["hits"][0]["column"], 4);
    let path = first["hits"][0]["path"].as_str().unwrap();
    assert_eq!(path, "knowledge/documents/a.md");
    let revision = first["hits"][0]["file_revision"].as_str().unwrap();
    let read = run(
        root.path(),
        &[
            "wiki",
            "read",
            path,
            "--revision",
            revision,
            "--section",
            "1",
        ],
    );
    assert!(
        report(&read)["content"]["text"]
            .as_str()
            .unwrap()
            .contains("한글 a+b")
    );
    let output = run(
        root.path(),
        &[
            "wiki",
            "search",
            "a\\+b",
            "--body",
            "--regex",
            "--ignore-case",
            "--include-sources",
            "--limit",
            "1",
            "--json",
        ],
    );
    let regex = report(&output);
    assert_eq!(regex["total"], 3);
    assert_eq!(regex["matching_pages"], 2);
    assert_eq!(regex["next_offset"], 1);
    assert_eq!(regex["truncated"], true);
    let output = run(
        root.path(),
        &[
            "wiki",
            "search",
            "a+b",
            "--body",
            "--include-sources",
            "--category",
            "ops",
            "--source-status",
            "active",
            "--type",
            "daily",
            "--json",
        ],
    );
    assert_eq!(report(&output)["total"], 1);
    let output = run(
        root.path(),
        &[
            "wiki",
            "search",
            "unusual-destination.md",
            "--body",
            "--json",
        ],
    );
    assert_eq!(report(&output)["total"], 1);
    std::fs::write(
        root.path().join("vault").join(path),
        "---\ntitle: Changed\n---\nchanged\n",
    )
    .unwrap();
    let stale = run(root.path(), &["wiki", "read", path, "--revision", revision]);
    assert!(!stale.status.success());
    assert!(stale.stdout.is_empty());
    assert!(!root.path().join("vault/.lorekeeper").exists());
}

#[test]
fn invalid_body_combinations_fail_without_partial_json() {
    let root = fixture();
    for args in [
        vec!["wiki", "search", "a", "--regex"],
        vec!["wiki", "search", "a", "--ignore-case"],
        vec!["wiki", "search", "a", "--context", "1"],
        vec!["wiki", "search", "a", "--body", "--snapshot", "old"],
        vec!["wiki", "search", "[", "--body", "--regex", "--json"],
        vec!["wiki", "search", "a", "--body", "--limit", "101", "--json"],
        vec!["wiki", "search", "a", "--body", "--context", "6", "--json"],
        vec!["wiki", "search", "", "--body", "--json"],
    ] {
        let output = run(root.path(), &args);
        assert!(!output.status.success(), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
    }
    assert!(!root.path().join("vault/.lorekeeper").exists());
}
