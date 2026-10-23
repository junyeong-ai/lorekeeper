#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use lk_queue::{QueueLlmClient, QueueTask, SummarizeRequest, TaskKind, ThemeRequest};
use serde_json::{Value, json};

const HOST: &str = r#"#!/usr/bin/env python3
import json, os, signal, sys, time
from pathlib import Path
root=Path(os.environ['TEST_SECTION_ROOT'])
engine=os.environ['TEST_SECTION_ENGINE']
if sys.argv[1:]==['--version']:
    print('codex-cli 0.163.0' if engine=='codex' else '2.1.309 (Claude Code)')
    raise SystemExit()
assert '--output-schema' in sys.argv or '--safe-mode' in sys.argv, 'interactive fallback is forbidden'
raw=sys.stdin.read()
request=json.loads(raw.split('\nNative request:\n',1)[1])
kind=request['kind']
work=request['work'] if kind=='finalize-section' else request
with (root/'calls.jsonl').open('a') as f:
    f.write(json.dumps({'kind':kind,'work_id':work['work_id'],'node_id':work['node_id']})+'\n')
if kind=='finalize-section':
    assert all(key in request for key in ['instructions','max_response_bytes','events'])
    assert 'No tools' in request['instructions']
    assert work['context']['purpose']['parameters']['locale']=='ko'
    assert not work['output']['unresolved']
    (root/'last-finalization.json').write_text(json.dumps(request))
    mode=(root/'mode').read_text()
    if mode=='exit': raise SystemExit(23)
    if mode=='cancel':
        os.kill(os.getppid(), signal.SIGINT)
        time.sleep(30)
    if mode=='malformed':
        print('not JSON'); raise SystemExit()
    if mode=='changed-target':
        path=root/'vault'/work['context']['subject']['target']
        path.write_text(path.read_text()+'\nExternal editor change.\n')
    blocked=root/'blocked-work'
    needs=mode=='needs-action' and (not blocked.exists() or blocked.read_text()==work['work_id'])
    if needs:
        output={'status':'needs-action','body':None,'reason':'A scoped open review must be recorded before completing this section.'}
    else:
        events=request['events']
        body='원문이 설명한 결정이며 실제 배포는 확인되지 않았다.'
        if events is not None:
            if mode=='reversed-events': events=list(reversed(events))
            body='\n\n'.join(e['heading']+'\n\n'+body+'\n'+'\n'.join(e['inline_links']) for e in events)
        if mode=='oversized': body='x'*request['max_response_bytes']
        if mode=='section-boundary': body='## Unexpected section\nEscaped body.'
        output={'status':'complete','body':body,'reason':None}
        if mode=='forged-guards': output['task_id']='forged'
else:
    inherited=[q for child in request.get('children',[]) for q in child['output'].get('source_questions',[])]
    questions=inherited
    if kind=='inspect':
        span=request['input']['ranges'][0]['range']
        questions=[{'id':work['node_id']+':deployment','question':'The retained source does not verify deployment.',
                    'evidence':[{key:span[key] for key in ['resource','start','end']}]}]
    unresolved=[]
    if (root/'mode').read_text()=='unresolved' and kind=='inspect':
        unresolved=[{'id':work['node_id']+':review','question':'A scoped review must be recorded first.',
                     'evidence':[request['input']['ranges'][0]['range']]}]
    output={'work_id':work['work_id'],'node_id':work['node_id'],'input_revision':work['input_revision'],
            'summary':'Read the assigned evidence. Deployment is not verified.',
            'findings':[],'source_questions':questions,'unresolved':unresolved,'resolutions':[]}
if engine=='claude-code':
    output={'type':'result','subtype':'success','is_error':False,'structured_output':output}
print(json.dumps(output))
"#;

struct Fixture {
    directory: tempfile::TempDir,
    config: PathBuf,
    engine: String,
}

impl Fixture {
    fn new(engine: &str, input_limit: usize) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let agent = root.join("agent");
        std::fs::write(&agent, HOST).unwrap();
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
        let config = root.join("config.yaml");
        std::fs::write(&config, format!(
            "vault:\n  root: vault\n  locale: ko\nidentity: {{name: Test, email: test@example.com}}\nsources:\n  notes:\n    type: manual\n    params: {{inbox_dir: inbox}}\n    concept_policy: disabled\nexecution:\n  default_profile: primary\n  work: {{max_input_bytes: {input_limit}, max_result_bytes: 1024, max_calls: 2}}\n  profiles:\n    primary:\n      engine: {engine}\n      executable: {}\n      timeout_seconds: 30\n",
            serde_json::to_string(&agent).unwrap()
        )).unwrap();
        std::fs::create_dir_all(root.join("vault/wiki/documents")).unwrap();
        std::fs::create_dir_all(root.join("vault/inbox")).unwrap();
        std::fs::write(root.join("mode"), "complete").unwrap();
        let fixture = Self {
            directory,
            config,
            engine: engine.into(),
        };
        let output = fixture
            .command()
            .args(["self", "deploy", "--skills", "none"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        fixture
    }

    fn root(&self) -> &Path {
        self.directory.path()
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_lore"));
        command
            .arg("--config")
            .arg(&self.config)
            .env("XDG_CONFIG_HOME", self.root().join("config"))
            .env("LORE_INSTALL_DATA_DIR", self.root().join("data"))
            .env("TEST_SECTION_ROOT", self.root())
            .env("TEST_SECTION_ENGINE", &self.engine);
        command
    }

    fn json(&self, args: &[&str]) -> Value {
        let output = self.command().args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn run(&self) -> (Output, Value) {
        let output = self
            .command()
            .args(["run", "process", "--json"])
            .output()
            .unwrap();
        let report = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
            panic!(
                "{e}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (output, report)
    }

    fn add(&self, name: &str, kind: TaskKind) -> (QueueTask, Value) {
        let target_kind = match kind {
            TaskKind::RefineEvents => "daily-refine-events",
            TaskKind::IdentifyThemes => "weekly-synthesis-themes",
            _ => "document-summary",
        };
        let anchor = if kind == TaskKind::RefineEvents {
            "## Events"
        } else {
            "## Summary"
        };
        let text = if kind == TaskKind::RefineEvents {
            "### First event\n\nA proposal with unverified deployment.\n[Original](https://example.com/first)\n\n```text\n### Not an event\n```\n\n### Second event\n\nAnother observation.\n[Original](https://example.com/second)\n"
        } else {
            "The retained source describes a proposal; deployment is unverified."
        };
        let target = json!({"vault_path":format!("wiki/documents/{name}.md"),"kind":target_kind,"anchor":anchor});
        let client = QueueLlmClient::new(self.root().join("vault/.lorekeeper/queue"));
        let task = if kind == TaskKind::IdentifyThemes {
            client.build_task(
                "fixture",
                kind,
                serde_json::from_value::<ThemeRequest>(
                    json!({"text":text,"max_themes":3,"locale":"ko","target":target}),
                )
                .unwrap(),
            )
        } else {
            client.build_task("fixture", kind, serde_json::from_value::<SummarizeRequest>(json!({"text":text,"max_sentences":3,"focus":"Preserve proposal versus observed deployment.","locale":"ko","source_type":"manual","target":target})).unwrap())
        };
        let page = format!(
            "---\ntype: {}\ntitle: {name}\nllm_inputs:\n  {}: {}\n---\n# {name}\n\n{anchor}\n\n{}\n\n## Content\n{text}\n",
            task.target.kind.page_format(),
            task.target.kind.llm_inputs_key(),
            task.cache_hash,
            if kind == TaskKind::RefineEvents {
                text
            } else {
                ""
            }
        );
        std::fs::write(
            self.root().join("vault").join(&task.target.vault_path),
            page,
        )
        .unwrap();
        let queue = self.root().join("vault/.lorekeeper/queue");
        std::fs::create_dir_all(&queue).unwrap();
        lk_queue::write_tasks_atomic(
            &queue.join(format!("{name}.jsonl")),
            std::slice::from_ref(&task),
        )
        .unwrap();
        let plan = self.json(&["work", "plan", "--task-id", &task.task_id]);
        assert_eq!(plan["parts"], 1);
        (task, plan)
    }

    fn work_path(&self, plan: &Value, file: &str) -> PathBuf {
        self.root()
            .join("vault/.lorekeeper/work")
            .join(plan["work_id"].as_str().unwrap())
            .join(file)
    }

    fn calls(&self) -> Vec<Value> {
        let path = self.root().join("calls.jsonl");
        if !path.exists() {
            return Vec::new();
        }
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect()
    }

    fn mode(&self, mode: &str) {
        std::fs::write(self.root().join("mode"), mode).unwrap();
    }
}

#[test]
fn section_finalizers_use_typed_bodies_and_resume_saved_work_on_both_hosts() {
    for engine in ["codex", "claude-code"] {
        for kind in [
            TaskKind::Summarize,
            TaskKind::RefineEvents,
            TaskKind::IdentifyThemes,
        ] {
            let fixture = Fixture::new(engine, 32768);
            let (task, plan) = fixture.add("source", kind);
            let plan_before = std::fs::read(fixture.work_path(&plan, "plan.json")).unwrap();
            let (output, report) = fixture.run();
            assert!(
                output.status.success(),
                "{report}\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(report["semantic_calls"], 2);
            assert_eq!(
                fixture
                    .calls()
                    .iter()
                    .filter(|c| c["kind"] == "finalize-section")
                    .count(),
                1
            );
            let steps = report["work_steps"].as_array().unwrap();
            let step = steps.iter().find(|s| s["role"] == "finalizer").unwrap();
            assert_eq!(step["outcome"], "staged");
            assert!(step["request_json_bytes"].as_u64().unwrap() <= 32768);
            assert!(step["result_json_bytes"].as_u64().unwrap() > 0);
            assert!(step["revalidation_record_duration_ms"].is_number());
            assert_eq!(
                std::fs::read(fixture.work_path(&plan, "plan.json")).unwrap(),
                plan_before
            );
            assert!(fixture.work_path(&plan, "publication.json").exists());
            assert!(
                std::fs::read(fixture.work_path(&plan, "final.json"))
                    .unwrap()
                    .len()
                    <= 1024
            );
            let status = fixture.json(&["queue", "status", "--json"]);
            assert!(
                status["data"]["tasks"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|t| t["task_id"] == task.task_id && t["status"] == "done")
            );
            let request: Value = serde_json::from_slice(
                &std::fs::read(fixture.root().join("last-finalization.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(
                request["work"]["output"]["source_questions"]
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
            let (_, report) = fixture.run();
            assert_eq!(report["semantic_calls"], 0);
        }
    }
}

#[test]
fn native_section_records_use_the_verified_vault_owner_through_a_root_alias() {
    let fixture = Fixture::new("codex", 32768);
    std::os::unix::fs::symlink(
        fixture.root().join("vault"),
        fixture.root().join("vault-alias"),
    )
    .unwrap();
    let config = std::fs::read_to_string(&fixture.config).unwrap();
    std::fs::write(
        &fixture.config,
        config.replace("root: vault", "root: vault-alias"),
    )
    .unwrap();
    let (task, plan) = fixture.add("source", TaskKind::Summarize);
    let before = std::fs::read(fixture.work_path(&plan, "plan.json")).unwrap();
    let (output, report) = fixture.run();
    assert!(
        output.status.success(),
        "{report}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(report["semantic_calls"], 2);
    assert_eq!(
        std::fs::read(fixture.work_path(&plan, "plan.json")).unwrap(),
        before
    );
    assert!(fixture.work_path(&plan, "publication.json").exists());
    let status = fixture.json(&["queue", "status", "--json"]);
    assert!(
        status["data"]["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["task_id"] == task.task_id && entry["status"] == "done")
    );
}

#[test]
fn needs_action_retries_spend_no_calls_and_do_not_starve_independent_work() {
    for engine in ["codex", "claude-code"] {
        let fixture = Fixture::new(engine, 32768);
        let (_, blocked) = fixture.add("a-blocked", TaskKind::Summarize);
        fixture.mode("needs-action");
        std::fs::write(
            fixture.root().join("blocked-work"),
            blocked["work_id"].as_str().unwrap(),
        )
        .unwrap();
        let (_, independent) = fixture.add("z-independent", TaskKind::Summarize);
        for _ in 0..4 {
            fixture.run();
        }
        assert!(
            fixture
                .work_path(&blocked, "finalization-action.json")
                .exists()
        );
        assert!(!fixture.work_path(&blocked, "seal.json").exists());
        assert!(fixture.work_path(&independent, "publication.json").exists());
        let calls = fixture.calls().len();
        let (output, report) = fixture.run();
        assert!(!output.status.success());
        assert_eq!(report["semantic_calls"], 0);
        assert_eq!(fixture.calls().len(), calls);
        assert!(
            report["stages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["name"] == "queue dependencies")
        );
        let receipt = fixture.work_path(&blocked, "finalization-action.json");
        let original = std::fs::read(&receipt).unwrap();
        std::fs::write(&receipt, "{}").unwrap();
        let (output, report) = fixture.run();
        assert!(!output.status.success());
        assert_eq!(report["semantic_calls"], 0);
        assert_eq!(std::fs::read(&receipt).unwrap(), b"{}");
        std::fs::write(receipt, original).unwrap();
    }
}

#[test]
fn rejected_structured_finalizers_never_fall_back_or_mark_completion() {
    for engine in ["codex", "claude-code"] {
        for mode in [
            "malformed",
            "forged-guards",
            "oversized",
            "section-boundary",
            "changed-target",
            "exit",
            "cancel",
        ] {
            let fixture = Fixture::new(engine, 32768);
            let (_, plan) = fixture.add("source", TaskKind::Summarize);
            fixture.mode(mode);
            let (output, report) = fixture.run();
            assert!(!output.status.success(), "{mode}: {report}");
            assert_eq!(report["semantic_calls"], 2, "{mode}: {report}");
            assert!(!fixture.work_path(&plan, "seal.json").exists(), "{mode}");
            assert!(
                !fixture
                    .work_path(&plan, "finalization-action.json")
                    .exists(),
                "{mode}"
            );
            assert_eq!(fixture.calls().len(), 2);
            let step = report["work_steps"]
                .as_array()
                .unwrap()
                .iter()
                .find(|step| step["role"] == "finalizer")
                .unwrap();
            assert_eq!(
                step["outcome"],
                if mode == "cancel" {
                    "cancelled"
                } else if mode == "exit" {
                    "failed"
                } else {
                    "rejected"
                }
            );
        }
    }
}

#[test]
fn changed_complete_observations_reconsider_an_old_action_on_both_hosts() {
    for engine in ["codex", "claude-code"] {
        let fixture = Fixture::new(engine, 32768);
        let (_, plan) = fixture.add("source", TaskKind::Summarize);
        fixture.mode("needs-action");
        fixture.run();
        let id = plan["work_id"].as_str().unwrap();
        let receipt = fixture.work_path(&plan, "finalization-action.json");
        let original = std::fs::read(&receipt).unwrap();
        let (_, report) = fixture.run();
        assert_eq!(report["semantic_calls"], 0);
        let completed = fixture.json(&["work", "result", id]);
        let mut output = completed["output"].clone();
        output["summary"] =
            json!("Explicitly corrected guarded observation; deployment remains unverified.");
        let path = fixture.root().join("corrected-observation.json");
        std::fs::write(&path, serde_json::to_vec(&output).unwrap()).unwrap();
        fixture.json(&["work", "record", "--file", path.to_str().unwrap()]);
        fixture.mode("complete");
        let (run, report) = fixture.run();
        assert!(run.status.success(), "{report}");
        assert_eq!(report["semantic_calls"], 1);
        assert!(fixture.work_path(&plan, "publication.json").exists());
        assert_eq!(std::fs::read(receipt).unwrap(), original);
    }
}

#[test]
fn explicitly_finished_work_restages_before_considering_old_action_receipts() {
    let fixture = Fixture::new("codex", 32768);
    let (task, plan) = fixture.add("source", TaskKind::Summarize);
    fixture.mode("needs-action");
    fixture.run();
    let id = plan["work_id"].as_str().unwrap();
    let completed = fixture.json(&["work", "result", id]);
    let result = lk_queue::SectionResult {
        task_id: task.task_id,
        cache_hash: task.cache_hash,
        target_revision: completed["context"]["subject"]["target_revision"]
            .as_str()
            .unwrap()
            .into(),
        body: "명시적 검수에서 원문의 제안과 확인되지 않은 배포 상태를 보존했다.".into(),
    };
    let path = fixture.root().join("explicit-result.json");
    std::fs::write(&path, serde_json::to_vec(&result).unwrap()).unwrap();
    let staged = fixture.json(&["work", "finish", id, "--file", path.to_str().unwrap()]);
    std::fs::remove_file(
        fixture
            .root()
            .join("vault")
            .join(staged["staged_result"].as_str().unwrap()),
    )
    .unwrap();
    let (run, report) = fixture.run();
    assert!(run.status.success(), "{report}");
    assert_eq!(report["semantic_calls"], 0);
    assert!(fixture.work_path(&plan, "publication.json").exists());
}

#[test]
fn known_processing_blockers_remain_incomplete_without_a_finalizer_call() {
    let fixture = Fixture::new("codex", 32768);
    let (_, plan) = fixture.add("source", TaskKind::Summarize);
    fixture.mode("unresolved");
    for _ in 0..2 {
        let (run, report) = fixture.run();
        assert!(!run.status.success(), "{report}");
    }
    assert_eq!(fixture.calls().len(), 1);
    assert!(!fixture.work_path(&plan, "seal.json").exists());
    assert!(
        !fixture
            .work_path(&plan, "finalization-action.json")
            .exists()
    );
}

#[test]
fn changed_event_order_is_rejected_before_sealing() {
    let fixture = Fixture::new("codex", 32768);
    let (_, plan) = fixture.add("events", TaskKind::RefineEvents);
    fixture.mode("reversed-events");
    let (output, report) = fixture.run();
    assert!(!output.status.success(), "{report}");
    assert!(!fixture.work_path(&plan, "seal.json").exists());
}

#[test]
fn near_limit_saved_work_is_retained_without_spending_a_finalizer_call() {
    let fixture = Fixture::new("codex", 4096);
    let (_, plan) = fixture.add("source", TaskKind::Summarize);
    let (output, report) = fixture.run();
    assert!(!output.status.success(), "{report}");
    assert_eq!(report["semantic_calls"], 1);
    assert_eq!(fixture.calls().len(), 1);
    assert!(report["stages"].as_array().unwrap().iter().any(|stage| {
        stage["name"] == "bounded section finalization"
            && stage["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains("exceeds max_input_bytes"))
    }));
    assert!(
        fixture.json(&["work", "result", plan["work_id"].as_str().unwrap()])["output"].is_object()
    );
    assert!(fixture.work_path(&plan, "plan.json").exists());
    assert!(!fixture.work_path(&plan, "seal.json").exists());
}
