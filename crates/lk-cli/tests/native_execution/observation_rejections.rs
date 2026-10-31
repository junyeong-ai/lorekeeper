use super::*;

#[test]
fn authored_observation_rejection_preserves_valid_peers_on_both_hosts() {
    for engine in ["codex", "claude-code"] {
        let fixture = parallel_observation_fixture(engine, 2, 2, 1, "good");
        let path = fixture.root().join("agent executable");
        let script = std::fs::read_to_string(&path).unwrap();
        let original = "output={'status':'complete','observation':output,'action':None}";
        assert!(script.contains(original));
        let script = script.replace(
                original,
                "if leader:\n    output['summary']=' \\u2003\\t'\noutput={'status':'complete','observation':output,'action':None}",
            );
        let print_reply = "if '--safe-mode' in sys.argv:\n    print(json.dumps({'type':'result','subtype':'success','is_error':False,'structured_output':output}))\nelse:\n    print(json.dumps(output))";
        assert!(script.contains(print_reply));
        let recording = r#"def emit(value):
    raw=(json.dumps(value)+'\n').encode()
    (root/('response-'+node+'.json')).write_bytes(raw)
    sys.stdout.buffer.write(raw)
if '--safe-mode' in sys.argv:
    emit({'type':'result','subtype':'success','is_error':False,'structured_output':output})
else:
    emit(output)"#;
        std::fs::write(path, script.replace(print_reply, recording)).unwrap();
        let (output, report) = fixture.run("knowledge");
        assert!(!output.status.success());
        assert_eq!(report["semantic_calls"], 2);
        assert_eq!(parallel_receipts(&fixture), 1);
        assert_eq!(report["halted"], false, "{report}");
        assert_parallel_no_publication(&fixture);
        let before = inspection_admission::work_files(&fixture);
        assert!(
            before
                .keys()
                .any(|path| path.file_name().unwrap() == "observation-rejections.json")
        );
        for (path, raw) in &before {
            if path.file_name().unwrap() != "observation-rejections.json" {
                continue;
            }
            let receipt: serde_json::Value = serde_json::from_slice(raw).unwrap();
            for entry in receipt["entries"].as_object().unwrap().values() {
                let bytes = std::fs::read(fixture.root().join(format!(
                    "response-{}.json",
                    entry["node_id"].as_str().unwrap()
                )))
                .unwrap();
                assert_eq!(
                    entry["response_revision"],
                    lk_core::rss_original::digest(&bytes)
                );
                let decoded: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                let semantic = if engine == "claude-code" {
                    &decoded["structured_output"]
                } else {
                    &decoded
                };
                assert_eq!(
                    entry["response_json_bytes"],
                    serde_json::to_vec(semantic).unwrap().len()
                );
            }
        }
        let (_, repeated) = fixture.run("knowledge");
        assert_eq!(repeated["semantic_calls"], 0, "{repeated}");
        assert_eq!(repeated["halted"], false);
        assert_eq!(before, inspection_admission::work_files(&fixture));
    }
}

const INVALID_EVIDENCE: &str = r#"
/bin/cat > "$TEST_EXECUTION_ROOT/observation-$$.txt"
exec python3 - "$@" <<'OBSERVATION'
import json, os, sys
from pathlib import Path
root=Path(os.environ['TEST_EXECUTION_ROOT'])
request=json.loads((root/('observation-'+str(os.getpid())+'.txt')).read_text().split('\nNative request:\n',1)[1])
if request['kind'] in ('inspect','reduce'):
    response={'status':'complete','observation':{'summary':' \u2003\t',
        'findings':[],'source_questions':[],'unresolved':[],'resolutions':[]},'action':None}
elif request['kind']=='whole-section':
    response={'status':'complete','body':'The independent synthetic source was summarized.','reason':None}
else:
    raise SystemExit('unexpected fixture role')
if '--safe-mode' in sys.argv:
    response={'type':'result','subtype':'success','is_error':False,'structured_output':response}
print(json.dumps(response))
OBSERVATION
"#;

#[test]
fn authored_text_hold_allows_independent_audit_completion_on_both_hosts() {
    let behavior=RECORD_BOUNDED_AUDIT.replace(
        "output={'status':'complete','observation':output,'action':None}",
        "if request['context']['subject'].get('path')=='wiki/concepts/large.md':\n            output['summary']=''\n        output={'status':'complete','observation':output,'action':None}");
    for engine in ["codex", "claude-code"] {
        let fixture = bounded_audit_fixture(engine, 8, &behavior);
        let config = std::fs::read_to_string(&fixture.config)
            .unwrap()
            .replace("batch_size: 1", "batch_size: 2");
        std::fs::write(&fixture.config, config).unwrap();
        write_audit_concept(&fixture, "second");
        record_discovery(&fixture, "wiki/concepts/second.md");
        let (output, report) = fixture.run("knowledge");
        assert!(!output.status.success());
        assert_eq!(report["halted"], false, "{report}");
        assert_eq!(report["audit"].as_array().unwrap().len(), 1);
        assert_eq!(
            report["audit"][0]["assessed_path"],
            "wiki/concepts/second.md"
        );
        let before = inspection_admission::work_files(&fixture);
        let (_, held) = fixture.run("knowledge");
        assert_eq!(held["semantic_calls"], 0, "{held}");
        assert_eq!(held["halted"], false);
        assert_eq!(before, inspection_admission::work_files(&fixture));
    }
}

#[test]
fn ambiguous_authored_text_stays_fatal_without_a_local_rejection() {
    for engine in ["codex", "claude-code"] {
        let fixture = parallel_observation_fixture(engine, 2, 2, 1, "good");
        let path = fixture.root().join("agent executable");
        let script = std::fs::read_to_string(&path).unwrap();
        let original = "if '--safe-mode' in sys.argv:\n    print(json.dumps({'type':'result','subtype':'success','is_error':False,'structured_output':output}))\nelse:\n    print(json.dumps(output))";
        assert!(script.contains(original));
        let replaced = r#"if leader:
    output['observation']['summary']=''
def emit(value):
    raw=json.dumps(value)
    if leader:
        raw=raw.replace('"summary":', '"summary":"Duplicate", "summary":', 1)
    print(raw)
if '--safe-mode' in sys.argv:
    emit({'type':'result','subtype':'success','is_error':False,'structured_output':output})
else:
    emit(output)"#;
        std::fs::write(path, script.replace(original, replaced)).unwrap();
        let (output, report) = fixture.run("knowledge");
        assert!(!output.status.success());
        assert_eq!(report["halted"], true, "{report}");
        assert_eq!(parallel_receipts(&fixture), 1);
        assert!(
            !inspection_admission::work_files(&fixture)
                .keys()
                .any(|path| path.file_name().unwrap() == "observation-rejections.json")
        );
        assert_parallel_no_publication(&fixture);
    }
}

#[test]
fn observation_rejection_keeps_queue_progress_and_fatal_corruption_distinct() {
    for engine in ["codex", "claude-code"] {
        let (fixture, _) =
            inspection_admission::source_evidence_fixture(engine, INVALID_EVIDENCE, "progressive");
        let (_, first) = fixture.run("process");
        assert_eq!(first["halted"], false, "{first}");
        assert_eq!(first["semantic_calls"], 2);
        let before = inspection_admission::work_files(&fixture);
        let (held_output, held) = fixture.run("process");
        assert!(!held_output.status.success());
        assert_eq!(held["semantic_calls"], 0, "{held}");
        assert_eq!(held["halted"], false);
        assert_eq!(held["budget_exhausted"], false);
        assert_eq!(held["retained_progress"]["action_receipts"], 0);
        assert_eq!(before, inspection_admission::work_files(&fixture));
        let root = fixture.root().join("vault");
        let queue = root.join(".lorekeeper/queue");
        let client = lk_queue::QueueLlmClient::new(queue.clone());
        let task = client.build_task("summary", lk_queue::TaskKind::Summarize,
            serde_json::from_value::<lk_queue::SummarizeRequest>(serde_json::json!({
                "text":"Independent source.","max_sentences":2,"locale":"en",
                "target":{"vault_path":"wiki/documents/zz-independent.md","anchor":"## Summary","kind":"document-summary"}
            })).unwrap());
        std::fs::write(root.join(&task.target.vault_path), format!(
            "---\ntype: document\ntitle: Independent\nllm_inputs:\n  summary: {}\n---\n## Summary\n",task.cache_hash)).unwrap();
        lk_queue::write_tasks_atomic(&queue.join("independent.jsonl"), &[task]).unwrap();
        let (_, progress) = fixture.run("process");
        assert_eq!(progress["halted"], false, "{progress}");
        assert_eq!(progress["semantic_calls"], 1);
        assert_eq!(progress["queue_application"]["applied"], 1);
        assert_eq!(before, inspection_admission::work_files(&fixture));
        let rejection = before
            .keys()
            .find(|path| path.file_name().unwrap() == "observation-rejections.json")
            .unwrap();
        std::fs::write(rejection, "{}").unwrap();
        let (_, corrupt) = fixture.run("process");
        assert_eq!(corrupt["halted"], true, "{corrupt}");
        assert_eq!(corrupt["semantic_calls"], 0);
        assert_eq!(std::fs::read(rejection).unwrap(), b"{}");
    }
}
