use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command, Output};

use lk_core::concept::{file_revision, source_revision};
use serde_json::{Value, json};

struct Fixture {
    directory: tempfile::TempDir,
    config: PathBuf,
    vault: PathBuf,
}

impl Fixture {
    fn build() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config.yaml");
        let vault = directory.path().join("vault");
        std::fs::write(
            &config,
            "vault:\n  root: vault\n  locale: en\nidentity:\n  name: Test\n  email: test@example.com\nsources:\n  notes:\n    type: manual\n    params: {inbox_dir: inbox}\ngraph:\n  audit: {batch_size: 1}\n",
        )
        .unwrap();
        Self {
            directory,
            config,
            vault,
        }
    }

    fn write(&self, path: &str, raw: &str) {
        let path = self.vault.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, raw).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_lore"));
        for (key, _) in std::env::vars().filter(|(key, _)| key.starts_with("LORE_")) {
            command.env_remove(key);
        }
        command
            .arg("--config")
            .arg(&self.config)
            .args(args)
            .env("LORE_INSTALL_DATA_DIR", self.directory.path().join("data"))
            .env("XDG_CONFIG_HOME", self.directory.path().join("config"))
            .output()
            .unwrap()
    }

    fn plan(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn bytes(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        walkdir::WalkDir::new(&self.vault)
            .into_iter()
            .map(Result::unwrap)
            .filter(|entry| entry.file_type().is_file())
            .map(|entry| {
                (
                    entry.path().strip_prefix(&self.vault).unwrap().to_owned(),
                    std::fs::read(entry.path()).unwrap(),
                )
            })
            .collect()
    }

    fn task(&self, path: &str) -> lk_queue::QueueTask {
        lk_queue::QueueLlmClient::new(self.vault.join(".lorekeeper/queue")).build_task(
            "pending",
            lk_queue::TaskKind::Summarize,
            lk_queue::SummarizeRequest {
                text: "Source".into(),
                max_sentences: 1,
                focus: None,
                locale: Default::default(),
                source_type: None,
                target: lk_queue::TaskTarget {
                    vault_path: path.into(),
                    kind: lk_queue::TargetKind::DocumentSummary,
                    anchor: "## Summary".into(),
                },
            },
        )
    }
}

fn authored(title: &str) -> String {
    format!("---\ntype: document\ntitle: {title}\n---\n## Summary\nA complete authored source.\n")
}

#[test]
fn automatic_readiness_skips_blocked_inputs_without_reserving_or_changing_default_plan() {
    let fixture = Fixture::build();
    let task = fixture.task("wiki/documents/a.md");
    let blocked = format!(
        "---\ntype: document\ntitle: A\nllm_inputs: {{summary: {}}}\n---\n## Summary\n\n",
        task.cache_hash
    );
    fixture.write("wiki/documents/a.md", &blocked);
    fixture.write("wiki/documents/b.md", &authored("B"));
    fixture.write(
        ".lorekeeper/queue/tasks.jsonl",
        &(serde_json::to_string(&task).unwrap() + "\n"),
    );
    fixture.write(
        ".lorekeeper/queue/results/pending.section.json",
        &serde_json::to_string(&lk_queue::SectionResult {
            context_revision: None,
            task_id: task.task_id.clone(),
            cache_hash: task.cache_hash.clone(),
            target_revision: file_revision(&blocked),
            body: "A staged summary.".into(),
        })
        .unwrap(),
    );
    let before = fixture.bytes();
    let ordinary = fixture.plan(&["wiki", "audit-plan"]);
    assert!(ordinary.get("readiness").is_none());
    assert_eq!(ordinary["candidates"][0]["path"], "wiki/documents/a.md");
    let ready = fixture.plan(&["wiki", "audit-plan", "--ready"]);
    assert_eq!(ready["candidates"][0]["path"], "wiki/documents/b.md");
    assert_eq!(ready["readiness"]["scope"], "encountered");
    assert_eq!(
        ready["readiness"]["ready"][0]["path"],
        "wiki/documents/b.md"
    );
    assert_eq!(ready["readiness"]["deferred_count"], 1);
    let deferred = &ready["readiness"]["deferred"][0];
    for key in ["kind", "path", "file_revision", "input_revision"] {
        assert_eq!(deferred[key], ordinary["candidates"][0][key]);
    }
    assert!(
        deferred["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|reason| { reason.as_str().unwrap().contains("unpublished result") })
    );
    assert_eq!(fixture.plan(&["wiki", "audit-plan", "--ready"]), ready);
    assert_eq!(fixture.bytes(), before);
    assert!(
        !fixture
            .vault
            .join(".lorekeeper/knowledge-audit.json")
            .exists()
    );
}

#[test]
fn selected_readiness_keeps_blocked_subjects_and_scoped_review_evidence_visible() {
    let fixture = Fixture::build();
    let subject = "wiki/documents/a.md";
    let evidence = "daily/evidence.md";
    fixture.write(subject, &authored("A"));
    fixture.write("wiki/documents/b.md", &authored("B"));
    let raw = "---\ntype: daily\nllm_inputs: {summary: pending}\n---\n## Summary\nAn unfinished observation.\n";
    fixture.write(evidence, raw);
    let revision = source_revision(&lk_vault::parse_page(raw).unwrap());
    fixture.write(
        "wiki/explorations/review.md",
        &format!("---\ntype: exploration\ntitle: Scope review\nreview:\n  status: open\n  subjects: [{subject}]\n  evidence:\n    - path: {evidence}\n      revision: {revision}\n---\n## Question\nDoes this source support the subject?\n"),
    );
    let before = fixture.bytes();
    let selected = fixture.plan(&[
        "wiki",
        "audit-plan",
        "--ready",
        "--limit",
        "2",
        "--path",
        subject,
        "--path",
        "wiki/documents/b.md",
    ]);
    assert_eq!(selected["candidates"].as_array().unwrap().len(), 2);
    assert_eq!(selected["coverage_scope"], "selection");
    assert_eq!(selected["readiness"]["scope"], "selection");
    assert_eq!(selected["readiness"]["ready"].as_array().unwrap().len(), 1);
    assert_eq!(
        selected["readiness"]["ready"][0]["path"],
        "wiki/documents/b.md"
    );
    assert_eq!(selected["readiness"]["deferred_count"], 1);
    assert_eq!(selected["readiness"]["deferred"][0]["path"], subject);
    assert_eq!(
        selected["readiness"]["deferred"][0]["reasons"][0],
        "daily/evidence.md: unfinished summary"
    );
    assert_eq!(fixture.bytes(), before);
    assert!(!fixture.vault.join(".lorekeeper").exists());
}

#[cfg(unix)]
#[test]
fn readiness_uses_verified_alias_owners_but_keeps_hard_link_task_ownership_distinct() {
    let fixture = Fixture::build();
    let subject = "wiki/documents/a.md";
    fixture.write(subject, &authored("A"));
    std::fs::create_dir_all(fixture.vault.join("wiki/aliases")).unwrap();
    std::os::unix::fs::symlink("../documents/a.md", fixture.vault.join("wiki/aliases/a.md"))
        .unwrap();
    std::fs::hard_link(
        fixture.vault.join(subject),
        fixture.vault.join("wiki/documents/hard.md"),
    )
    .unwrap();
    let mut task = serde_json::to_value(fixture.task("wiki/documents/hard.md")).unwrap();
    task["input"]["max_sentences"] = json!("invalid");
    fixture.write(".lorekeeper/queue/tasks.jsonl", &(task.to_string() + "\n"));
    let ready = fixture.plan(&["wiki", "audit-plan", "--ready", "--path", subject]);
    assert_eq!(ready["readiness"]["deferred_count"], 0);
    task["target"]["vault_path"] = json!("wiki/aliases/a.md");
    fixture.write(".lorekeeper/queue/tasks.jsonl", &(task.to_string() + "\n"));
    let before = fixture.bytes();
    let blocked = fixture.plan(&["wiki", "audit-plan", "--ready", "--path", subject]);
    assert_eq!(blocked["candidates"].as_array().unwrap().len(), 1);
    assert_eq!(blocked["readiness"]["deferred_count"], 1);
    assert!(
        blocked["readiness"]["deferred"][0]["reasons"][0]
            .as_str()
            .unwrap()
            .contains("unreadable task")
    );
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn unknown_queue_or_staged_targets_refuse_readiness_without_changing_default_planning() {
    let fixture = Fixture::build();
    fixture.write("wiki/documents/a.md", &authored("A"));
    fixture.write(".lorekeeper/queue/tasks.jsonl", "not-json\n");
    let before = fixture.bytes();
    assert!(
        !fixture.plan(&["wiki", "audit-plan"])["candidates"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let refused = fixture.run(&["wiki", "audit-plan", "--ready"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("unknown targets"));
    assert_eq!(fixture.bytes(), before);
    std::fs::remove_file(fixture.vault.join(".lorekeeper/queue/tasks.jsonl")).unwrap();
    fixture.write(
        ".lorekeeper/queue/results/orphan.section.json",
        &serde_json::to_string(&lk_queue::SectionResult {
            context_revision: None,
            task_id: "orphan".into(),
            cache_hash: "hash".into(),
            target_revision: "revision".into(),
            body: "Unpublished.".into(),
        })
        .unwrap(),
    );
    let before = fixture.bytes();
    let refused = fixture.run(&["wiki", "audit-plan", "--ready"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("unknown targets"));
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn deferred_details_are_bounded_while_later_ready_work_and_exact_encountered_count_remain() {
    let fixture = Fixture::build();
    for index in 0..105 {
        fixture.write(
            &format!("wiki/documents/a{index:03}.md"),
            "---\ntype: document\nllm_inputs: {summary: pending}\n---\n## Summary\n\n",
        );
    }
    fixture.write("wiki/documents/z.md", &authored("Ready"));
    let before = fixture.bytes();
    let result = fixture.plan(&["wiki", "audit-plan", "--ready"]);
    assert_eq!(result["candidates"][0]["path"], "wiki/documents/z.md");
    assert_eq!(result["readiness"]["deferred_count"], 105);
    assert_eq!(
        result["readiness"]["deferred"].as_array().unwrap().len(),
        100
    );
    assert_eq!(result["readiness"]["ready"].as_array().unwrap().len(), 1);
    assert_eq!(fixture.bytes(), before);
    assert!(!fixture.vault.join(".lorekeeper").exists());
}

#[test]
fn partitioned_input_size_is_separate_from_unfinished_semantic_readiness() {
    let fixture = Fixture::build();
    let config = std::fs::read_to_string(&fixture.config).unwrap();
    std::fs::write(
        &fixture.config,
        config.replace("batch_size: 1", "batch_size: 1, max_input_bytes: 64"),
    )
    .unwrap();
    fixture.write("wiki/documents/a.md", &authored(&"A".repeat(100)));
    let before = fixture.bytes();
    let result = fixture.plan(&["wiki", "audit-plan", "--ready"]);
    assert_eq!(result["input"]["status"], "oversized");
    assert_eq!(result["readiness"]["deferred_count"], 0);
    assert_eq!(result["readiness"]["ready"].as_array().unwrap().len(), 1);
    assert_eq!(fixture.bytes(), before);
}
