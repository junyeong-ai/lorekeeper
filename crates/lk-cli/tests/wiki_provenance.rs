use std::process::Command;

#[test]
fn cli_provenance_resolves_successor_with_pinned_pagination_without_writes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let documents = root.join("vault/knowledge/documents");
    std::fs::create_dir_all(&documents).unwrap();
    let config = root.join("config.yaml");
    std::fs::write(&config, "vault:\n  root: vault\n  dirs:\n    wiki: knowledge\nidentity:\n  name: T\n  email: t@example.com\nsources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: inbox\n").unwrap();
    let old = "---\ntype: document\nsource_identity: {repository: r, document: old}\nsource_record: {status: superseded, superseded_by: new}\n---\nNo links.\n";
    let new = "---\ntype: document\nsource_identity: {repository: r, document: new}\nsource_record: {status: active}\n---\nSuccessor.\n";
    std::fs::write(documents.join("old.md"), old).unwrap();
    std::fs::write(documents.join("new.md"), new).unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_lore"))
            .arg("--config")
            .arg(&config)
            .args(args)
            .output()
            .unwrap()
    };
    let first = run(&[
        "wiki",
        "provenance",
        "knowledge/documents/old.md",
        "--limit",
        "1",
    ]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(first["entries"][0]["resolution"], "resolved");
    assert_eq!(first["next_offset"], 1);
    let next = run(&[
        "wiki",
        "provenance",
        "knowledge/documents/old.md",
        "--limit",
        "1",
        "--offset",
        "1",
        "--revision",
        first["revision"].as_str().unwrap(),
    ]);
    assert!(next.status.success());
    let next: serde_json::Value = serde_json::from_slice(&next.stdout).unwrap();
    assert_eq!(
        next["entries"][0]["target"]["path"],
        "knowledge/documents/new.md"
    );
    assert!(next["entries"][0]["target"]["file_revision"].is_string());
    assert!(!root.join("vault/.lorekeeper").exists());
    assert_eq!(
        std::fs::read_to_string(documents.join("old.md")).unwrap(),
        old
    );
    assert_eq!(
        std::fs::read_to_string(documents.join("new.md")).unwrap(),
        new
    );
    let help = run(&["wiki", "provenance", "--help"]);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("read-only JSON"));
}
