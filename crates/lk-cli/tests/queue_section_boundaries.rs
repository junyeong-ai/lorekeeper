use std::path::PathBuf;
use std::process::{Command, Output};

const PAGE: &str = "wiki/documents/source.md";
const PENDING: &str = ".lorekeeper/queue/tasks.jsonl";
const RESULT: &str = ".lorekeeper/queue/results/section.section.json";

struct Workspace {
    root: tempfile::TempDir,
}

impl Workspace {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("vault")).unwrap();
        std::fs::write(
            root.path().join("config.yaml"),
            "vault:\n  root: vault\n  locale: en\nidentity:\n  name: Tester\n  email: tester@example.com\nexecution:\n  work: {extraction_context: full-context}\nsources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: inbox\n      archive_after_ingest: false\n",
        )
        .unwrap();
        Self { root }
    }

    fn vault(&self) -> PathBuf {
        self.root.path().join("vault")
    }

    fn write(&self, path: &str, content: &str) {
        let full = self.vault().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, content).unwrap();
    }

    fn read(&self, path: &str) -> String {
        std::fs::read_to_string(self.vault().join(path)).unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_lore"));
        for (key, _) in std::env::vars() {
            if key.starts_with("LORE_") {
                command.env_remove(key);
            }
        }
        command
            .arg("--config")
            .arg(self.root.path().join("config.yaml"))
            .args(args)
            .output()
            .unwrap()
    }

    fn json(&self, args: &[&str]) -> serde_json::Value {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn stage_section(&self, body: &str) -> lk_queue::QueueTask {
        let request = lk_queue::SummarizeRequest {
            text: "Authoritative source content.".into(),
            max_sentences: 3,
            focus: None,
            locale: lk_core::i18n::Locale::En,
            source_type: None,
            target: lk_queue::TaskTarget {
                vault_path: PAGE.into(),
                kind: lk_queue::TargetKind::DocumentSummary,
                anchor: "## Summary".into(),
            },
        };
        let task = lk_queue::QueueLlmClient::new(self.vault().join(".lorekeeper/queue"))
            .build_task("fixture", lk_queue::TaskKind::Summarize, request);
        let page = format!(
            "---\ntype: document\ntitle: Source\nllm_inputs:\n  summary: {}\n---\n# Source\n\n## Summary\n\nOriginal summary.\n\n## Content\n\nAuthoritative source content.\n\n## Related Concepts\n\nRetain related context.\n",
            task.cache_hash
        );
        self.write(PAGE, &page);
        self.write(PENDING, &serde_json::to_string(&task).unwrap());
        self.write(
            RESULT,
            &serde_json::to_string(&lk_queue::SectionResult {
                context_revision: None,
                task_id: task.task_id.clone(),
                cache_hash: task.cache_hash.clone(),
                target_revision: lk_core::concept::file_revision(&page),
                body: body.into(),
            })
            .unwrap(),
        );
        task
    }
}

#[test]
fn section_publication_rejects_markdown_that_changes_its_owned_boundaries() {
    for body in [
        "<pre>\nUnclosed HTML.",
        "<!--\nUnclosed comment.",
        "```text\nUnclosed code.",
        "Unexpected sibling\n---\nWrong section.",
        "# Unexpected parent\nWrong section.",
    ] {
        let workspace = Workspace::new();
        workspace.stage_section(body);
        let original = workspace.read(PAGE);
        let pending = workspace.read(PENDING);
        let result = workspace.read(RESULT);

        for args in [vec!["queue", "apply", "--dry-run"], vec!["queue", "apply"]] {
            let output = workspace.run(&args);
            let error = String::from_utf8_lossy(&output.stderr);
            assert!(!output.status.success(), "{body:?}, {args:?}: {error}");
            assert!(error.contains("section content"), "{body:?}: {error}");
            assert_eq!(workspace.read(PAGE), original);
            assert_eq!(workspace.read(PENDING), pending);
            assert_eq!(workspace.read(RESULT), result);
        }
    }
}

#[test]
fn valid_html_and_empty_sections_publish_without_changing_neighboring_sections() {
    for body in ["<pre>\n```\n## Literal example\n</pre>", ""] {
        let workspace = Workspace::new();
        let task = workspace.stage_section(body);
        let original = workspace.read(PAGE);
        let pending = workspace.read(PENDING);
        let result = workspace.read(RESULT);
        let preview = workspace.run(&["queue", "apply", "--dry-run"]);
        assert!(
            preview.status.success(),
            "{body:?}: {}",
            String::from_utf8_lossy(&preview.stderr)
        );
        assert_eq!(workspace.read(PAGE), original);
        assert_eq!(workspace.read(PENDING), pending);
        assert_eq!(workspace.read(RESULT), result);

        let applied = workspace.run(&["queue", "apply"]);
        assert!(
            applied.status.success(),
            "{body:?}: {}",
            String::from_utf8_lossy(&applied.stderr)
        );
        let published = workspace.read(PAGE);
        for (heading, expected) in [
            ("Summary", body),
            ("Content", "Authoritative source content."),
            ("Related Concepts", "Retain related context."),
        ] {
            assert_eq!(
                lk_vault::section_body(&published, heading).map(str::trim),
                Some(expected),
                "{body:?}: {published}"
            );
        }
        let page = lk_vault::parse_page(&published).unwrap();
        assert_eq!(
            page.frontmatter.get("llm_inputs").unwrap()["summary_done"],
            task.cache_hash
        );
        assert_eq!(workspace.read(PENDING), pending);
        assert!(!workspace.vault().join(RESULT).exists());
    }
}

#[test]
fn completed_source_reingest_preserves_literal_content_and_published_page_bytes() {
    let workspace = Workspace::new();
    let source = "# Source\n\n```python\nvalue = \"\"\"first\n\n\nsecond\"\"\"\n```\n";
    workspace.write("inbox/source.md", source);
    let collected = workspace.run(&["ingest", "notes"]);
    assert!(
        collected.status.success(),
        "{}",
        String::from_utf8_lossy(&collected.stderr)
    );
    let task = std::fs::read_dir(workspace.vault().join(".lorekeeper/queue"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .flat_map(|entry| {
            std::fs::read_to_string(entry.path())
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str::<lk_queue::QueueTask>(line).unwrap())
                .collect::<Vec<_>>()
        })
        .find(|task| task.target.kind == lk_queue::TargetKind::DocumentSummary)
        .unwrap();
    assert!(
        task.input["text"]
            .as_str()
            .unwrap()
            .contains("first\n\n\nsecond")
    );
    let before = workspace.read(&task.target.vault_path);
    assert!(before.contains("first\n\n\nsecond"));
    let result = lk_queue::SectionResult {
        context_revision: None,
        task_id: task.task_id,
        cache_hash: task.cache_hash,
        target_revision: lk_core::concept::file_revision(&before),
        body: "A reviewed source summary.".into(),
    };
    workspace.write(RESULT, &serde_json::to_string(&result).unwrap());
    let published = workspace.run(&["queue", "apply"]);
    assert!(
        published.status.success(),
        "{}",
        String::from_utf8_lossy(&published.stderr)
    );
    let completed = workspace.read(&task.target.vault_path);
    assert_eq!(workspace.read("inbox/source.md"), source);
    let repeated = workspace.run(&["ingest", "notes"]);
    assert!(
        repeated.status.success(),
        "{}",
        String::from_utf8_lossy(&repeated.stderr)
    );
    assert_eq!(workspace.read(&task.target.vault_path), completed);
    assert_eq!(workspace.read("inbox/source.md"), source);
}

#[test]
fn new_concept_grounding_cannot_hide_the_concept_page_sections() {
    let workspace = Workspace::new();
    let concept = "Boundary Topic";
    let request = lk_queue::ExtractConceptsRequest {
        concept_policy: lk_core::config::ConceptPolicy::SupportedNew,
        rss_original: None,
        text: "Boundary Topic is supported by the retained source.".into(),
        source_id: "notes".into(),
        source_type: lk_core::config::SourceType::Manual,
        date: jiff::civil::date(2026, 5, 23),
        focus: None,
        locale: lk_core::i18n::Locale::En,
        categories: Vec::new(),
        target: lk_queue::TaskTarget {
            vault_path: PAGE.into(),
            kind: lk_queue::TargetKind::DocumentConcepts,
            anchor: "## Related Concepts".into(),
        },
    };
    let task = lk_queue::QueueLlmClient::new(workspace.vault().join(".lorekeeper/queue"))
        .build_task("fixture", lk_queue::TaskKind::ExtractConcepts, request);
    let original = format!(
        "---\ntype: document\ntitle: Source\nllm_inputs:\n  concepts: {}\n---\n## Summary\n\nRetain summary.\n\n## Content\n\nBoundary Topic is supported by the retained source.\n\n## Related Concepts\n\n",
        task.cache_hash
    );
    workspace.write(PAGE, &original);
    let pending = serde_json::to_string(&task).unwrap();
    workspace.write(PENDING, &pending);
    let target_revision = lk_core::concept::file_revision(&original);
    let discovery = vec![lk_core::discovery::DiscoveryQuery {
        query: concept.into(),
        phrase: true,
        spacing: false,
    }];
    workspace.write(
        ".lorekeeper/discovery-request.json",
        &serde_json::json!({"target_revision":target_revision,"queries":discovery}).to_string(),
    );
    let file = workspace.vault().join(".lorekeeper/discovery-request.json");
    workspace.json(&[
        "work",
        "discover",
        "--task-id",
        &task.task_id,
        "--file",
        file.to_str().unwrap(),
    ]);
    let context = workspace.json(&["work", "context", "--task-id", &task.task_id]);
    let result = lk_queue::TaskResult {
        selection: None,
        concept_policy: lk_core::config::ConceptPolicy::SupportedNew,
        target_revision,
        context_revision: context["context_revision"].as_str().unwrap().into(),
        discovery,
        task_id: task.task_id,
        cache_hash: task.cache_hash,
        target: task.target,
        date: jiff::civil::date(2026, 5, 23),
        concepts: vec![lk_queue::ReportedConcept {
            admission: lk_queue::ConceptAdmission::New,
            rss_evidence: Vec::new(),
            concept: lk_core::concept::ExtractedConcept {
                name: concept.into(),
                category: None,
                aliases: Vec::new(),
            },
            synthesis: Some("<pre>\nGrounding with an unclosed HTML block.".into()),
        }],
    };
    let result = serde_json::to_string(&result).unwrap();
    let result_path = ".lorekeeper/queue/results/extraction.json";
    workspace.write(result_path, &result);
    for args in [vec!["queue", "apply", "--dry-run"], vec!["queue", "apply"]] {
        let output = workspace.run(&args);
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{args:?}: {error}");
        assert!(error.contains("section content"), "{error}");
        assert_eq!(workspace.read(PAGE), original);
        assert_eq!(workspace.read(PENDING), pending);
        assert_eq!(workspace.read(result_path), result);
        assert!(
            !workspace
                .vault()
                .join("wiki/concepts/boundary-topic.md")
                .exists()
        );
    }
}
