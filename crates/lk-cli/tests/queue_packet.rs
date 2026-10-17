use std::path::Path;
use std::process::{Command, Output};

use lk_core::concept::{Citation, citation_digest, file_revision, source_revision};
use serde_json::{Value, json};

fn run(root: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_lore"));
    for (key, _) in std::env::vars() {
        if key.starts_with("LORE_") {
            cmd.env_remove(key);
        }
    }
    cmd.args(["queue", "packet", "--root"])
        .arg(root)
        .args(args)
        .output()
        .unwrap()
}

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("wiki/concepts")).unwrap();
    std::fs::create_dir_all(dir.path().join(".lorekeeper/queue")).unwrap();
    let source = "---\ntitle: Source\nupdated: 2026-10-15\nsource_url: https://example.com/source\n---\n\nOriginal mechanism, condition and exception.\n";
    std::fs::write(dir.path().join("source.md"), source).unwrap();
    let parsed = lk_core::frontmatter::parse_page(source).unwrap();
    let citations = vec![Citation {
        path: "source.md".into(),
        revision: source_revision(&parsed),
    }];
    let hash = citation_digest(&citations);
    let mut queue = String::new();
    for id in ["first", "second"] {
        let target = format!("wiki/concepts/{id}.md");
        std::fs::write(dir.path().join(&target),format!("---\ntitle: {id}\nllm_inputs:\n  synthesis: {hash}\n---\n\n## Synthesis\n\nExisting definition.\n")).unwrap();
        let task = json!({"task_id":id,"kind":"synthesize-concept","created_at":"2026-10-15T12:00:00Z","cache_hash":hash,"input":{"locale":"en","citations":citations,"generation_policy":lk_queue::TaskKind::SynthesizeConcept.generation_policy()},"target":{"vault_path":target,"kind":"concept-synthesis","anchor":"## Synthesis"}});
        queue.push_str(&serde_json::to_string(&task).unwrap());
        queue.push('\n');
    }
    std::fs::write(dir.path().join(".lorekeeper/queue/tasks.jsonl"), queue).unwrap();
    dir
}

#[test]
fn shared_sources_are_deduplicated_exactly_and_content_requires_opt_in() {
    let dir = fixture();
    let args = ["--task-id", "first", "--task-id", "second"];
    let out = run(dir.path(), &args);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let packet: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(packet["sources"].as_array().unwrap().len(), 1);
    assert_eq!(packet["tasks"].as_array().unwrap().len(), 2);
    assert_eq!(packet["tasks"][0]["source_indices"], json!([0]));
    assert_eq!(packet["tasks"][1]["source_indices"], json!([0]));
    assert!(packet["sources"][0].get("content").is_none());
    assert!(packet["tasks"][0].get("target_content").is_none());
    assert!(
        packet["semantic_validation"]
            .as_str()
            .unwrap()
            .contains("not a read receipt")
    );
    let raw = std::fs::read_to_string(dir.path().join("source.md")).unwrap();
    assert_eq!(packet["sources"][0]["file_revision"], file_revision(&raw));
    assert_eq!(
        packet["sources"][0]["source_url"],
        "https://example.com/source"
    );
    let out = run(dir.path(), &["--task-id", "first", "--include-content"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let packet: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(packet["sources"][0]["content"], raw);
    let target = std::fs::read_to_string(dir.path().join("wiki/concepts/first.md")).unwrap();
    assert_eq!(
        packet["tasks"][0]["target_revision"],
        file_revision(&target)
    );
    assert_eq!(
        packet["tasks"][0]["generation_policy"],
        lk_queue::TaskKind::SynthesizeConcept.generation_policy()
    );
    assert!(!dir.path().join(".lorekeeper/mutation").exists());
    assert!(!dir.path().join(".lorekeeper/evidence").exists());
}

#[test]
fn stale_sources_and_unknown_ids_fail_without_partial_stdout() {
    let dir = fixture();
    let out = run(dir.path(), &["--task-id", "absent"]);
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    let path = dir.path().join("source.md");
    std::fs::write(
        &path,
        std::fs::read_to_string(&path)
            .unwrap()
            .replace("Original mechanism", "Changed mechanism"),
    )
    .unwrap();
    let out = run(dir.path(), &["--task-id", "first"]);
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("stale"));
}

#[test]
fn pending_mutations_are_reported_without_recovery() {
    let dir = fixture();
    let state = dir.path().join(".lorekeeper/mutation");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join("pending.json"), "incomplete").unwrap();
    let out = run(dir.path(), &["--task-id", "first"]);
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert_eq!(
        std::fs::read_to_string(state.join("pending.json")).unwrap(),
        "incomplete"
    );
    assert!(!state.join("lock").exists());
}
