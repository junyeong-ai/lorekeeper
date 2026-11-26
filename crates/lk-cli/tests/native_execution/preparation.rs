use super::*;

#[test]
fn oversized_work_context_defers_only_its_target_without_truncating_inputs() {
    for engine in ["codex", "claude-code"] {
        for extraction in [true, false] {
            let input_bytes = 65536;
            let (fixture, original) =
                progressive_extraction_fixture(engine, WHOLE_SECTION, 0, input_bytes, 16384);
            let queue = fixture.root().join("vault/.lorekeeper/queue");
            let client = lk_queue::QueueLlmClient::new(queue.clone());
            let phrase = "The declared source scope remains complete. ";
            let focus = phrase.repeat(input_bytes / phrase.len() + 1);
            let bad = if extraction {
                let mut input = original.input.clone();
                input.insert(
                    "target".into(),
                    serde_json::to_value(&original.target).unwrap(),
                );
                let mut request: lk_queue::ExtractConceptsRequest =
                    serde_json::from_value(serde_json::Value::Object(input)).unwrap();
                request.focus = Some(focus);
                client.build_task("large", lk_queue::TaskKind::ExtractConcepts, request)
            } else {
                client.build_task(
                    "large",
                    lk_queue::TaskKind::Summarize,
                    lk_queue::SummarizeRequest {
                        text: original.input["text"].as_str().unwrap().into(),
                        max_sentences: 5,
                        focus: Some(focus),
                        locale: lk_core::i18n::Locale::En,
                        source_type: Some(lk_core::config::SourceType::Manual),
                        target: lk_queue::TaskTarget {
                            vault_path: original.target.vault_path.clone(),
                            kind: lk_queue::TargetKind::DocumentSummary,
                            anchor: "## Summary".into(),
                        },
                    },
                )
            };
            let good = client.build_task(
                "ready",
                lk_queue::TaskKind::Summarize,
                lk_queue::SummarizeRequest {
                    text: "A retained observation supports an independent summary.".into(),
                    max_sentences: 5,
                    focus: None,
                    locale: lk_core::i18n::Locale::En,
                    source_type: Some(lk_core::config::SourceType::Manual),
                    target: lk_queue::TaskTarget {
                        vault_path: "wiki/documents/z-ready.md".into(),
                        kind: lk_queue::TargetKind::DocumentSummary,
                        anchor: "## Summary".into(),
                    },
                },
            );
            let path = fixture.root().join("vault/wiki/documents/source.md");
            let mut body = std::fs::read_to_string(&path)
                .unwrap()
                .replace(&original.cache_hash, &bad.cache_hash);
            if !extraction {
                body = body.replace("  concepts:", "  summary:");
            }
            std::fs::write(&path, &body).unwrap();
            std::fs::write(fixture.root().join("vault/wiki/documents/z-ready.md"), format!("---\ntype: document\ntitle: Ready\nllm_inputs:\n  summary: {}\n---\n## Summary\n\n## Content\nA retained observation supports an independent summary.\n",good.cache_hash)).unwrap();
            std::fs::write(
                queue.join("fixture.jsonl"),
                format!(
                    "{}\n{}\n",
                    serde_json::to_string(&bad).unwrap(),
                    serde_json::to_string(&good).unwrap()
                ),
            )
            .unwrap();
            let config = std::fs::read(&fixture.config).unwrap();
            let (_, report) = fixture.run("process");
            assert_eq!(report["halted"], false, "{engine}/{extraction}: {report}");
            assert_eq!(report["semantic_calls"], 1, "{report}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
            assert_eq!(std::fs::read(&fixture.config).unwrap(), config);
            let status = execution_json(&fixture, &["queue", "status", "--json"]);
            let tasks = status["data"]["tasks"].as_array().unwrap();
            assert_eq!(
                tasks
                    .iter()
                    .find(|task| task["task_id"] == bad.task_id)
                    .unwrap()["status"],
                "current"
            );
            assert_eq!(
                tasks
                    .iter()
                    .find(|task| task["task_id"] == good.task_id)
                    .unwrap()["status"],
                "done"
            );
            let (_, repeated) = fixture.run("process");
            assert_eq!(repeated["semantic_calls"], 0, "{repeated}");
            assert_eq!(repeated["halted"], false, "{repeated}");
        }
    }
}

const DISCOVERY_ACTION: &str = r#"
python3 - "$@" <<'ACTION'
import json,os,sys
from pathlib import Path
request=json.loads(Path(os.environ['TEST_INTERACTIVE_INPUT']).read_text().split('\nNative request:\n',1)[1])
value={'subjects':[{'subject_ref':subject['subject_ref'],'status':'needs-action','queries':None,
    'reason':'A required source read needs an explicit decision.'} for subject in request['subjects']]}
if '--output-last-message' in sys.argv:
    Path(sys.argv[sys.argv.index('--output-last-message')+1]).write_text(json.dumps(value))
if '--json-schema' in sys.argv:
    value={'type':'result','subtype':'success','is_error':False,'structured_output':value}
print(json.dumps(value))
ACTION
"#;

#[test]
fn discovery_action_preserves_independent_recipe_and_audit_publication() {
    for engine in ["codex", "claude-code"] {
        let behavior = RECORD_AUDIT.replace("    value={'subjects':rows}",
            "    rows[0]={'subject_ref':rows[0]['subject_ref'],'status':'needs-action','queries':None,'reason':'The first subject requires an explicit source decision.'}\n    value={'subjects':rows}");
        let fixture = Fixture::new(engine, 30, &behavior);
        let config = std::fs::read_to_string(&fixture.config).unwrap()
            + "\ngraph:\n  audit: {batch_size: 2, subjects: [concept, document]}\n";
        std::fs::write(&fixture.config, config).unwrap();
        let directory = fixture.root().join("vault/wiki/concepts");
        std::fs::create_dir_all(&directory).unwrap();
        for name in ["blocked-alpha", "ready-beta"] {
            std::fs::write(
                directory.join(format!("{name}.md")),
                include_str!("../behaviour/corpus/small/vault/wiki/concepts/unlinked-note.md")
                    .replace("Unlinked note", name),
            )
            .unwrap();
        }
        let (_, report) = fixture.run("knowledge");
        assert_eq!(report["halted"], false, "{report}");
        assert_eq!(
            report["retained_progress"]["discovery_recipes"], 1,
            "{report}"
        );
        assert_eq!(report["retained_progress"]["action_receipts"], 1);
        assert_eq!(
            report["audit"].as_array().map(Vec::len),
            Some(1),
            "{report}"
        );
        let held = std::fs::read_dir(
            fixture
                .root()
                .join("vault/.lorekeeper/audit-preparation-attempts"),
        )
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
        let receipt: serde_json::Value =
            serde_json::from_slice(&std::fs::read(held).unwrap()).unwrap();
        assert_ne!(receipt["subject"], report["audit"][0]["assessed_path"]);
        let (_, repeated) = fixture.run("knowledge");
        assert_eq!(repeated["semantic_calls"], 0, "{repeated}");
        assert_eq!(repeated["halted"], false, "{repeated}");
    }
}

#[test]
fn native_discovery_holds_failed_inputs_and_explicit_actions_without_pausing_operations() {
    for engine in ["codex", "claude-code"] {
        for audit in [false, true] {
            for action in [false, true] {
                let behavior = if action { DISCOVERY_ACTION } else { "exit 51" };
                let fixture = if audit {
                    let fixture = Fixture::new(engine, 30, behavior);
                    let directory = fixture.root().join("vault/wiki/concepts");
                    std::fs::create_dir_all(&directory).unwrap();
                    std::fs::write(
                        directory.join("blocked.md"),
                        include_str!(
                            "../behaviour/corpus/small/vault/wiki/concepts/unlinked-note.md"
                        ),
                    )
                    .unwrap();
                    fixture
                } else {
                    let (fixture, task) =
                        progressive_extraction_fixture(engine, behavior, 2, 65536, 16384);
                    let mut input = task.input.clone();
                    input.insert("target".into(), serde_json::to_value(&task.target).unwrap());
                    let mut request: lk_queue::ExtractConceptsRequest =
                        serde_json::from_value(serde_json::Value::Object(input)).unwrap();
                    let sentence = request.text.clone();
                    request.text = sentence.repeat(
                        lk_core::semantic::INSPECTION_CHARACTERS.get() / sentence.chars().count()
                            + 1,
                    );
                    let source = request.text.clone();
                    let queue = fixture.root().join("vault/.lorekeeper/queue");
                    let task = lk_queue::QueueLlmClient::new(queue.clone()).build_task(
                        "ext",
                        lk_queue::TaskKind::ExtractConcepts,
                        request,
                    );
                    std::fs::write(
                        queue.join("fixture.jsonl"),
                        serde_json::to_string(&task).unwrap() + "\n",
                    )
                    .unwrap();
                    std::fs::write(fixture.root().join("vault/wiki/documents/source.md"),
                        format!("---\ntype: document\ntitle: Source\nllm_inputs:\n  concepts: {}\n---\n## Summary\nA retained observation.\n\n## Content\n{source}\n\n## Related Concepts\n",task.cache_hash)).unwrap();
                    fixture
                };
                let operation = if audit { "knowledge" } else { "process" };
                let (_, first) = fixture.run(operation);
                assert_eq!(
                    first["semantic_calls"], 1,
                    "{engine}/{audit}/{action}: {first}"
                );
                assert_eq!(first["halted"], false, "{first}");
                assert_eq!(first["work_steps"][0]["role"], "discovery", "{first}");
                assert_eq!(first["retained_progress"]["discovery_recipes"], 0);
                assert_eq!(
                    first["retained_progress"]["action_receipts"],
                    usize::from(action)
                );
                let (_, repeated) = fixture.run(operation);
                assert_eq!(
                    repeated["semantic_calls"],
                    usize::from(!action),
                    "a run without admissible work probes only an unanswered hold: {repeated}"
                );
                assert_eq!(repeated["halted"], false, "{repeated}");
                let directory = fixture.root().join(if audit {
                    "vault/.lorekeeper/audit-preparation-attempts"
                } else {
                    "vault/.lorekeeper/queue/preparation-attempts"
                });
                let path = std::fs::read_dir(directory)
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .path();
                let raw = std::fs::read(&path).unwrap();
                let receipt: serde_json::Value = serde_json::from_slice(&raw).unwrap();
                assert_eq!(
                    receipt["disposition"]["kind"],
                    if action {
                        "needs-action"
                    } else {
                        "invocation-failed"
                    }
                );
                let stage = if audit {
                    "audit-discovery"
                } else {
                    "extraction-discovery"
                };
                let (selector, identity) = if audit {
                    ("--path", receipt["key"]["path"].as_str().unwrap())
                } else {
                    ("--task-id", receipt["key"]["task_id"].as_str().unwrap())
                };
                let revision = receipt["input_revision"].as_str().unwrap();
                execution_json(
                    &fixture,
                    &[
                        "work",
                        "retry-preparation",
                        "--stage",
                        stage,
                        selector,
                        identity,
                        "--input-revision",
                        revision,
                        "--dry-run",
                    ],
                );
                assert_eq!(std::fs::read(&path).unwrap(), raw);
                let wrong = fixture
                    .command()
                    .args([
                        "work",
                        "retry-preparation",
                        "--stage",
                        stage,
                        selector,
                        identity,
                        "--input-revision",
                        &"0".repeat(64),
                    ])
                    .output()
                    .unwrap();
                assert!(!wrong.status.success());
                assert_eq!(std::fs::read(&path).unwrap(), raw);
                execution_json(
                    &fixture,
                    &[
                        "work",
                        "retry-preparation",
                        "--stage",
                        stage,
                        selector,
                        identity,
                        "--input-revision",
                        revision,
                    ],
                );
                assert!(!path.exists());
            }
        }
    }
}

#[test]
fn whole_section_failures_are_held_without_repeating_or_creating_semantic_progress() {
    for engine in ["codex", "claude-code"] {
        for (behavior, kind) in [
            ("exit 51", "exited"),
            ("printf 'invalid response'", "invalid-response"),
            ("exec sleep 30", "timed-out"),
        ] {
            let fixture = Fixture::new(
                engine,
                if kind == "timed-out" {
                    INVOCATION_TIMEOUT_SECONDS
                } else {
                    30
                },
                &format!("/bin/cat > /dev/null\n{behavior}"),
            );
            let config = std::fs::read_to_string(&fixture.config)
                .unwrap()
                .replace("concept_policy: supported-new", "concept_policy: disabled");
            std::fs::write(&fixture.config, config).unwrap();
            fixture.ingest();
            let (_, first) = fixture.run("process");
            assert_eq!(first["semantic_calls"], 1, "{engine}/{kind}: {first}");
            if kind == "timed-out" {
                assert_eq!(
                    first["work_steps"][0]["host_failure"]["phase"], "invocation",
                    "{first}"
                );
            }
            assert_eq!(first["halted"], false, "{first}");
            assert_eq!(first["retained_progress"]["action_receipts"], 0);
            assert_eq!(first["retained_progress"]["staged_requests"], 0);
            let (_, repeated) = fixture.run("process");
            assert_eq!(
                repeated["semantic_calls"],
                usize::from(kind != "invalid-response"),
                "a run without admissible work probes only an unanswered hold: {engine}/{kind}: {repeated}"
            );
            assert_eq!(repeated["halted"], false, "{repeated}");
            let path = std::fs::read_dir(
                fixture
                    .root()
                    .join("vault/.lorekeeper/queue/preparation-attempts"),
            )
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
            let receipt: serde_json::Value =
                serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            assert_eq!(receipt["key"]["stage"], "whole-section");
            assert_eq!(receipt["disposition"]["failure"]["kind"], kind);
        }
    }
}
