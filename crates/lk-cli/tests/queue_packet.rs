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
        rss_event: None,
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

#[test]
fn rss_original_packet_command_uses_exact_root_even_with_another_configured_vault() {
    let dir = fixture();
    let source = "---\ntype: daily\nsource: rss\ncreated: 2026-10-18\n---\nAbridged daily.\n";
    std::fs::write(dir.path().join("source.md"), source).unwrap();
    let revision = source_revision(&lk_core::frontmatter::parse_page(source).unwrap());
    let hash = citation_digest(&[Citation {
        rss_event: None,
        path: "source.md".into(),
        revision: revision.clone(),
    }]);
    let queue_path = dir.path().join(".lorekeeper/queue/tasks.jsonl");
    let queue = std::fs::read_to_string(&queue_path)
        .unwrap()
        .lines()
        .map(|line| {
            let mut task: Value = serde_json::from_str(line).unwrap();
            task["cache_hash"] = json!(hash);
            task["input"]["citations"][0]["revision"] = json!(revision);
            let target = dir
                .path()
                .join(task["target"]["vault_path"].as_str().unwrap());
            let old = std::fs::read_to_string(&target).unwrap();
            let old_hash = lk_core::frontmatter::parse_page(&old)
                .unwrap()
                .frontmatter
                .get("llm_inputs")
                .unwrap()["synthesis"]
                .as_str()
                .unwrap()
                .to_owned();
            std::fs::write(target, old.replace(&old_hash, &hash)).unwrap();
            serde_json::to_string(&task).unwrap()
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(queue_path, queue).unwrap();
    let event:lk_core::event::Event=serde_json::from_value(json!({"id":"rss:2026-10-18:0000000000000001","source_id":"rss","source_type":"rss","timestamp":"2026-10-18T00:00:00Z","date":"2026-10-18","title":"Collected title","body":"raw detail absent from daily","url":null,"author":null,"labels":[],"category":null,"performance_category":null,"is_self":false,"is_personal":false,"metadata":{}})).unwrap();
    let log = dir.path().join(".lorekeeper/events/rss");
    std::fs::create_dir_all(&log).unwrap();
    std::fs::write(
        log.join("2026-10-18.jsonl"),
        serde_json::to_string(&event).unwrap(),
    )
    .unwrap();
    let out = run(dir.path(), &["--task-id", "first"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let packet: Value = serde_json::from_slice(&out.stdout).unwrap();
    let original = &packet["sources"][0]["original"];
    assert_eq!(original["discovery"]["authority"], "legacy_unbound");
    assert_eq!(
        original["discovery"]["page_revision"],
        packet["sources"][0]["file_revision"]
    );
    assert!(!String::from_utf8(out.stdout).unwrap().contains(&event.body));
    let other = tempfile::TempDir::new().unwrap();
    std::fs::write(other.path().join("config.yaml"),format!("vault:\n  root: {}\nidentity:\n  name: Other\n  email: other@example.com\nsources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: inbox\n",other.path().display())).unwrap();
    let argv = original["discovery_command"]
        .as_array()
        .unwrap()
        .iter()
        .skip(1)
        .map(|v| v.as_str().unwrap())
        .collect::<Vec<_>>();
    let out = Command::new(env!("CARGO_BIN_EXE_lore"))
        .arg("--config")
        .arg(other.path().join("config.yaml"))
        .args(argv)
        .current_dir(other.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let view: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(view["revision"], original["discovery"]["revision"]);
    assert!(!dir.path().join(".lorekeeper/evidence").exists());
}
