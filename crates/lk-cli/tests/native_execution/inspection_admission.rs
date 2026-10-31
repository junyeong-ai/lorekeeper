use super::*;

const MIXED_EVIDENCE: &str = r#"
/bin/cat > "$TEST_EXECUTION_ROOT/prefix-$$.txt"
exec python3 - "$@" <<'PREFIX'
import json, os, sys
from pathlib import Path
root=Path(os.environ['TEST_EXECUTION_ROOT'])
request=json.loads((root/('prefix-'+str(os.getpid())+'.txt')).read_text().split('\nNative request:\n',1)[1])
assert request['kind']=='inspect'
first=request['input']==json.loads((root/'first-evidence.json').read_text())
record=(root/'record-prefix').exists()
with (root/'prefix-calls.jsonl').open('a') as log:
    log.write(json.dumps({'first':first,'record':record})+'\n')
if not record:
    if first:
        raise SystemExit(1)
    response={'status':'needs-action','observation':None,'action':{'reason':'The second assigned inspection requires explicit action.'}}
else:
    assert first, 'A held or later inspection must not be invoked'
    response={'status':'complete','observation':{'summary':'The exact first assigned synthetic source range was inspected.',
        'findings':[],'source_questions':[],'unresolved':[],'resolutions':[]},'action':None}
if '--safe-mode' in sys.argv:
    response={'type':'result','subtype':'success','is_error':False,'structured_output':response}
print(json.dumps(response))
PREFIX
"#;

fn work_files(fixture: &Fixture) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
    walkdir::WalkDir::new(fixture.root().join("vault/.lorekeeper/work"))
        .into_iter()
        .map(Result::unwrap)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| {
            (
                entry.path().to_owned(),
                std::fs::read(entry.path()).unwrap(),
            )
        })
        .collect()
}

#[test]
fn partitioned_action_admission_preserves_paid_scope_without_replanning_on_both_hosts() {
    for engine in ["codex", "claude-code"] {
        let fixture = evidence_wave_fixture(engine, "action");
        let agent = fixture.root().join("agent executable");
        let script = std::fs::read_to_string(&agent).unwrap();
        assert!(script.contains("if leader and mode=='action':"));
        std::fs::write(
            &agent,
            script.replace("if leader and mode=='action':", "if mode=='action':"),
        )
        .unwrap();
        let (_, first) = fixture.run("process");
        assert_eq!(first["semantic_calls"], 2, "{first}");
        assert_eq!(first["halted"], false);
        let before = work_files(&fixture);
        let (_, repeated) = fixture.run("process");
        assert_eq!(repeated["semantic_calls"], 0, "{repeated}");
        assert_eq!(repeated["halted"], false);
        assert_eq!(repeated["queue_planning"]["attempts"], 1, "{repeated}");
        assert_eq!(repeated["evidence_preparation_io"]["captures"], 0);
        assert_eq!(before, work_files(&fixture));
        assert_eq!(screening_receipts(&fixture), 2);
    }
}

const HOLD_EVIDENCE: &str = r#"
/bin/cat > "$TEST_EXECUTION_ROOT/admission-$$.txt"
exec python3 - "$@" <<'ADMISSION'
import json, os, sys
from pathlib import Path
root=Path(os.environ['TEST_EXECUTION_ROOT'])
request=json.loads((root/('admission-'+str(os.getpid())+'.txt')).read_text().split('\nNative request:\n',1)[1])
if request['kind'] in ('inspect','reduce'):
    response={'status':'needs-action','observation':None,'action':{'reason':'An explicit scoped evidence action is required.'}}
elif request['kind']=='whole-section':
    response={'status':'complete','body':'Independent fixture evidence is summarized.','reason':None}
else:
    raise SystemExit('unexpected fixture role')
if '--safe-mode' in sys.argv:
    response={'type':'result','subtype':'success','is_error':False,'structured_output':response}
print(json.dumps(response))
ADMISSION
"#;

fn source_evidence_fixture(
    engine: &str,
    behavior: &str,
    context: &str,
) -> (Fixture, lk_queue::QueueTask) {
    let (fixture, original) = progressive_extraction_fixture(engine, behavior, 2, 32768, 4096);
    let config = std::fs::read_to_string(&fixture.config)
        .unwrap()
        .replace(
            "max_calls: 64",
            &format!("max_calls: 4, max_parallel: 2, extraction_context: {context}"),
        )
        .replace(
            "extraction_context: progressive",
            &format!("extraction_context: {context}"),
        );
    std::fs::write(&fixture.config, config).unwrap();
    let root = fixture.root().join("vault");
    let queue = root.join(".lorekeeper/queue");
    let client = lk_queue::QueueLlmClient::new(queue.clone());
    let mut input = original.input;
    input.insert(
        "target".into(),
        serde_json::to_value(original.target).unwrap(),
    );
    let mut input: lk_queue::ExtractConceptsRequest =
        serde_json::from_value(serde_json::Value::Object(input)).unwrap();
    input.text = "A transactional outbox observation preserves its source scope.\n".repeat(1400);
    let text = input.text.clone();
    let task = client.build_task("ext", lk_queue::TaskKind::ExtractConcepts, input);
    lk_queue::write_tasks_atomic(&queue.join("fixture.jsonl"), std::slice::from_ref(&task))
        .unwrap();
    let target = root.join(&task.target.vault_path);
    let raw = format!(
        "---\ntype: document\ntitle: Source\nllm_inputs:\n  concepts: {}\n---\n## Content\n{text}\n## Related Concepts\n",
        task.cache_hash
    );
    std::fs::write(target, &raw).unwrap();
    let file = fixture.root().join("discovery.json");
    std::fs::write(
        &file,
        serde_json::to_vec(&serde_json::json!({
            "target_revision":lk_core::concept::file_revision(&raw),
            "queries":[{"query":"transactional outbox","phrase":true,"spacing":false}]
        }))
        .unwrap(),
    )
    .unwrap();
    execution_json(
        &fixture,
        &[
            "work",
            "discover",
            "--task-id",
            &task.task_id,
            "--file",
            file.to_str().unwrap(),
        ],
    );
    (fixture, task)
}
#[test]
fn source_action_admission_keeps_manual_access_and_independent_publication() {
    let (fixture, task) = source_evidence_fixture("codex", HOLD_EVIDENCE, "progressive");
    let root = fixture.root().join("vault");
    let queue = root.join(".lorekeeper/queue");
    let client = lk_queue::QueueLlmClient::new(queue.clone());
    let (_, first) = fixture.run("process");
    assert_eq!(first["halted"], false, "{first}");
    assert_eq!(first["semantic_calls"], 2, "{first}");
    let before = work_files(&fixture);
    let (_, repeated) = fixture.run("process");
    assert_eq!(repeated["semantic_calls"], 0, "{repeated}");
    assert_eq!(repeated["halted"], false);
    assert_eq!(repeated["queue_planning"]["attempts"], 1, "{repeated}");
    assert_eq!(repeated["evidence_preparation_io"]["captures"], 0);
    assert_eq!(before, work_files(&fixture));
    let manual = execution_json(
        &fixture,
        &["work", "source-plan", "--task-id", &task.task_id],
    );
    assert!(manual["work_id"].is_string());

    let summary = client.build_task(
        "summary",
        lk_queue::TaskKind::Summarize,
        serde_json::from_value::<lk_queue::SummarizeRequest>(serde_json::json!({
            "text":"Independent evidence.","max_sentences":2,"locale":"en",
            "target":{"vault_path":"wiki/documents/zz-independent.md","anchor":"## Summary","kind":"document-summary"}
        }))
        .unwrap(),
    );
    std::fs::write(
        root.join(&summary.target.vault_path),
        format!(
            "---\ntype: document\ntitle: Independent\nllm_inputs:\n  summary: {}\n---\n## Summary\n",
            summary.cache_hash
        ),
    )
    .unwrap();
    lk_queue::write_tasks_atomic(
        &queue.join("independent.jsonl"),
        std::slice::from_ref(&summary),
    )
    .unwrap();
    let (_, independent) = fixture.run("process");
    assert_eq!(independent["halted"], false, "{independent}");
    assert_eq!(independent["semantic_calls"], 1);
    assert_eq!(independent["queue_application"]["applied"], 1);
    assert_eq!(before, work_files(&fixture));

    let action = before
        .keys()
        .find(|path| path.file_name().unwrap() == "inspection-actions.json")
        .unwrap();
    std::fs::write(action, "{}").unwrap();
    let (_, corrupt) = fixture.run("process");
    assert_eq!(corrupt["halted"], true, "{corrupt}");
    assert_eq!(corrupt["semantic_calls"], 0);
    assert_eq!(std::fs::read(action).unwrap(), b"{}");
}

#[test]
fn later_evidence_actions_preserve_ready_prefix_on_both_hosts() {
    for engine in ["codex", "claude-code"] {
        for context in ["progressive", "full-context"] {
            let (fixture, task) = source_evidence_fixture(engine, MIXED_EVIDENCE, context);
            let planned = execution_json(
                &fixture,
                &[
                    "work",
                    if context == "progressive" {
                        "source-plan"
                    } else {
                        "plan"
                    },
                    "--task-id",
                    &task.task_id,
                ],
            );
            let id = planned["work_id"].as_str().unwrap();
            let status = execution_json(&fixture, &["work", "status", id]);
            assert_eq!(status["completed"], 0);
            let ready = status["ready"].as_array().unwrap();
            assert!(ready.len() >= 2);
            let first = ready[0].as_str().unwrap();
            let held = ready[1].as_str().unwrap();
            let input = execution_json(&fixture, &["work", "read", id, first]);
            let later = execution_json(&fixture, &["work", "read", id, held]);
            assert_ne!(input["input"], later["input"]);
            std::fs::write(
                fixture.root().join("first-evidence.json"),
                serde_json::to_vec(&input["input"]).unwrap(),
            )
            .unwrap();

            let (_, primed) = fixture.run("process");
            assert_eq!(primed["semantic_calls"], 2, "{engine}/{context}: {primed}");
            assert_eq!(primed["halted"], true);
            let directory = fixture.root().join("vault/.lorekeeper/work").join(id);
            let actions: serde_json::Value = serde_json::from_slice(
                &std::fs::read(directory.join("inspection-actions.json")).unwrap(),
            )
            .unwrap();
            let actions = actions["actions"].as_object().unwrap();
            assert_eq!(actions.len(), 1);
            assert!(actions.contains_key(held));
            let before = work_files(&fixture);
            std::fs::write(fixture.root().join("record-prefix"), "").unwrap();

            let (_, prefix) = fixture.run("process");
            assert_eq!(prefix["semantic_calls"], 1, "{engine}/{context}: {prefix}");
            assert_eq!(prefix["halted"], false);
            assert_eq!(prefix["ok"], false);
            assert_eq!(prefix["queue_application"]["applied"], 0);
            assert_eq!(prefix["work_steps"].as_array().unwrap().len(), 1);
            assert_eq!(prefix["work_steps"][0]["node_id"], first);
            assert_eq!(prefix["work_steps"][0]["outcome"], "recorded");
            let after = work_files(&fixture);
            for (path, bytes) in &before {
                assert_eq!(after.get(path), Some(bytes));
            }
            assert_eq!(after.len(), before.len() + 1);
            assert!(
                directory
                    .join("nodes")
                    .join(format!("{first}.json"))
                    .is_file()
            );
            let status = execution_json(&fixture, &["work", "status", id]);
            assert_eq!(status["completed"], 1);
            assert_eq!(status["ready"][0], held);

            let (_, repeated) = fixture.run("process");
            assert_eq!(
                repeated["semantic_calls"], 0,
                "{engine}/{context}: {repeated}"
            );
            assert_eq!(repeated["halted"], false);
            assert_eq!(repeated["queue_planning"]["attempts"], 1);
            assert_eq!(after, work_files(&fixture));
            let calls = std::fs::read_to_string(fixture.root().join("prefix-calls.jsonl")).unwrap();
            assert_eq!(calls.lines().count(), 3);
        }
    }
}
