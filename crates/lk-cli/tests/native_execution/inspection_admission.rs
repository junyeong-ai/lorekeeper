use super::*;

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

#[test]
fn source_action_admission_keeps_manual_access_and_independent_publication() {
    let (fixture, original) =
        progressive_extraction_fixture("codex", HOLD_EVIDENCE, 2, 32768, 4096);
    let config = std::fs::read_to_string(&fixture.config)
        .unwrap()
        .replace("max_calls: 64", "max_calls: 4, max_parallel: 2");
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
