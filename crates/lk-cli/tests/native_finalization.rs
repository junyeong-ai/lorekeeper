#![cfg(unix)]

use std::collections::BTreeMap;
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
work=request['work'] if kind in ['finalize-section','whole-section'] else request
work_id=work.get('work_id') or work.get('context_revision')
with (root/'calls.jsonl').open('a') as f:
    f.write(json.dumps({'kind':kind,'work_id':work_id,'node_id':work.get('node_id')})+'\n')
if kind in ['finalize-section','whole-section']:
    assert all(key in request for key in ['instructions','max_response_bytes','events'])
    assert 'No tools' in request['instructions']
    assert work['context']['purpose']['parameters']['locale']=='ko'
    if kind=='finalize-section': assert not work['output']['unresolved']
    else: assert work['resources'] and 'output' not in work
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
    if mode=='changed-operational-config':
        path=Path(os.environ['LORE_CONFIG'])
        path.write_text(path.read_text().replace('max_calls: 2','max_calls: 3'))
    if mode=='changed-review':
        path=root/'vault'/(root/'review-target').read_text()
        path.write_text(path.read_text()+'\nThe scoped assessment changed during finalization.\n')
    blocked=root/'blocked-work'
    needs=mode=='needs-action' and (not blocked.exists() or blocked.read_text()==work_id)
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
    output={'summary':'Read the assigned evidence. Deployment is not verified.',
            'findings':[],'source_questions':questions,'unresolved':unresolved,'resolutions':[]}
if kind in ['inspect','reduce']:
    output={'status':'complete','observation':output,'action':None}
    mode=(root/'mode').read_text()
    if mode in ['inspection-action','inspection-action-wrong-id','inspection-action-drift']:
        action={'reason':'Cannot complete this assigned evidence inspection without further action.'}
        if mode=='inspection-action-wrong-id': action['node_id']='not-selected'
        if mode=='inspection-action-drift':
            path=root/'vault'/request['context']['subject']['target']
            path.write_text(path.read_text()+'\nExternal edit during inspection.\n')
        output={'status':'needs-action','observation':None,'action':action}
    if mode in ['oversized-inspection','oversized-inspection-drift'] and 'a-blocked' in request['context']['subject']['target']:
        output['observation']['summary']='x'*2048
        if mode=='oversized-inspection-drift':
            path=root/'vault'/request['context']['subject']['target']
            path.write_text(path.read_text()+'\nExternal edit during oversized inspection.\n')
    if mode=='empty-inspection-evidence':
        output['observation']['unresolved']=[{'id':work['node_id']+':mapping','question':'Cannot establish mapping.','evidence':[]}]
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
        self.add_with_plan(name, kind, true)
    }

    fn add_with_plan(&self, name: &str, kind: TaskKind, planned: bool) -> (QueueTask, Value) {
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
        let plan = if planned {
            let plan = self.json(&["work", "plan", "--task-id", &task.task_id]);
            assert_eq!(plan["parts"], 1);
            plan
        } else {
            Value::Null
        };
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

    fn complete_source(&self, plan: &Value) -> Value {
        let original = std::fs::read_to_string(&self.config).unwrap();
        std::fs::write(
            &self.config,
            original.replace("max_calls: 2", "max_calls: 1"),
        )
        .unwrap();
        let (output, report) = self.run();
        assert!(
            !output.status.success(),
            "finalization remains pending: {report}"
        );
        assert_eq!(report["semantic_calls"], 1, "{report}");
        assert_eq!(self.calls().len(), 1);
        assert_eq!(self.calls()[0]["kind"], "inspect");
        let completed = self.json(&["work", "result", plan["work_id"].as_str().unwrap()]);
        assert!(completed["output"].is_object());
        assert!(!self.work_path(plan, "seal.json").exists());
        std::fs::write(&self.config, original).unwrap();
        completed
    }

    fn source_snapshot(&self, plan: &Value) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut files = BTreeMap::from([(
            PathBuf::from("plan.json"),
            std::fs::read(self.work_path(plan, "plan.json")).unwrap(),
        )]);
        for entry in std::fs::read_dir(self.work_path(plan, "nodes")).unwrap() {
            let entry = entry.unwrap();
            assert!(entry.file_type().unwrap().is_file());
            files.insert(
                PathBuf::from("nodes").join(entry.file_name()),
                std::fs::read(entry.path()).unwrap(),
            );
        }
        assert!(files.len() > 1, "completed source observation is retained");
        files
    }

    fn add_review(&self, task: &QueueTask, body: &str) -> (String, String) {
        let evidence = "wiki/documents/review-evidence.md";
        let evidence_raw = "---\ntype: document\ntitle: Authoritative policy\n---\n# Authoritative policy\nDeployment requires a separately recorded approval.\n";
        std::fs::write(self.root().join("vault").join(evidence), evidence_raw).unwrap();
        let evidence_revision =
            lk_core::concept::source_revision(&lk_vault::parse_page(evidence_raw).unwrap());
        let review = "wiki/explorations/section-review.md";
        let absolute = self.root().join("vault").join(review);
        std::fs::create_dir_all(absolute.parent().unwrap()).unwrap();
        std::fs::write(&absolute, format!(
            "---\ntype: exploration\ntitle: Scoped section review\nreview:\n  status: open\n  subjects: [{}]\n  evidence:\n    - path: {evidence}\n      revision: {evidence_revision}\n---\n# Scoped section review\n{body}\n",
            task.target.vault_path
        )).unwrap();
        std::fs::write(self.root().join("review-target"), review).unwrap();
        let reports = self.json(&["wiki", "reviews"]);
        assert_eq!(reports[0]["effective_state"], "open", "{reports}");
        assert_eq!(reports[0]["errors"], json!([]));
        (review.into(), evidence.into())
    }
}

#[test]
fn planned_section_finalizers_read_current_reviews_without_repeating_saved_source_work() {
    for engine in ["codex", "claude-code"] {
        for review_preexists in [false, true] {
            let fixture = Fixture::new(engine, 32768);
            let (task, plan) = fixture.add("source", TaskKind::Summarize);
            let mut paths = review_preexists.then(|| {
                fixture.add_review(
                    &task,
                    "The proposal is not evidence of approved deployment.",
                )
            });
            let completed = fixture.complete_source(&plan);
            let source_before = fixture.source_snapshot(&plan);
            let (review, evidence) = paths.take().unwrap_or_else(|| {
                fixture.add_review(
                    &task,
                    "A newly scoped assessment requires explicit approval.",
                )
            });
            let (output, report) = fixture.run();
            assert!(
                output.status.success(),
                "{engine}/{review_preexists}: {report}"
            );
            assert_eq!(report["semantic_calls"], 1, "{report}");
            assert_eq!(fixture.source_snapshot(&plan), source_before);
            assert_eq!(fixture.calls().len(), 2);
            assert_eq!(fixture.calls()[1]["kind"], "finalize-section");
            let request: Value = serde_json::from_slice(
                &std::fs::read(fixture.root().join("last-finalization.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(request["work"], completed);
            let context = &request["review_context"];
            assert!(!context["context_revision"].as_str().unwrap().is_empty());
            assert!(!context["dependencies"].is_null());
            let resources = context["resources"].as_array().unwrap();
            for (path, authority) in [(&review, "review"), (&evidence, "page")] {
                let resource = resources
                    .iter()
                    .find(|r| r["address"] == path.as_str())
                    .unwrap_or_else(|| panic!("missing {path}: {context}"));
                assert_eq!(resource["authority"], authority);
                assert_eq!(
                    resource["text"],
                    std::fs::read_to_string(fixture.root().join("vault").join(path)).unwrap()
                );
            }
            assert_eq!(resources.len(), 2, "source evidence must not be duplicated");
            let result: Value = serde_json::from_slice(
                &std::fs::read(fixture.work_path(&plan, "final.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(result["context_revision"], context["context_revision"]);
            assert!(fixture.work_path(&plan, "publication.json").exists());
        }
    }
}

#[test]
fn planned_section_review_changes_during_finalization_refuse_then_reuse_paid_nodes() {
    for engine in ["codex", "claude-code"] {
        let fixture = Fixture::new(engine, 32768);
        let (task, plan) = fixture.add("source", TaskKind::Summarize);
        fixture.complete_source(&plan);
        fixture.add_review(
            &task,
            "The approval requirement remains an open assessment.",
        );
        let source_before = fixture.source_snapshot(&plan);
        let target = fixture.root().join("vault").join(&task.target.vault_path);
        let target_before = std::fs::read(&target).unwrap();
        fixture.mode("changed-review");
        let (output, report) = fixture.run();
        assert!(!output.status.success(), "{engine}: {report}");
        assert_eq!(report["semantic_calls"], 1);
        assert_eq!(fixture.source_snapshot(&plan), source_before);
        assert_eq!(std::fs::read(&target).unwrap(), target_before);
        assert!(!fixture.work_path(&plan, "seal.json").exists());
        assert!(!fixture.work_path(&plan, "final.json").exists());
        let staged = fixture.root().join("vault/.lorekeeper/queue/results");
        assert!(!staged.exists() || std::fs::read_dir(staged).unwrap().next().is_none());
        let old_request: Value = serde_json::from_slice(
            &std::fs::read(fixture.root().join("last-finalization.json")).unwrap(),
        )
        .unwrap();
        fixture.mode("complete");
        let (output, report) = fixture.run();
        assert!(output.status.success(), "{engine}: {report}");
        assert_eq!(report["semantic_calls"], 1);
        assert_eq!(fixture.source_snapshot(&plan), source_before);
        let new_request: Value = serde_json::from_slice(
            &std::fs::read(fixture.root().join("last-finalization.json")).unwrap(),
        )
        .unwrap();
        assert_ne!(old_request["review_context"], new_request["review_context"]);
        assert_eq!(
            fixture
                .calls()
                .iter()
                .filter(|c| c["kind"] == "inspect")
                .count(),
            1
        );
        assert!(fixture.work_path(&plan, "publication.json").exists());
    }
}

#[test]
fn planned_section_review_capacity_defers_without_blocking_independent_publication() {
    for engine in ["codex", "claude-code"] {
        let fixture = Fixture::new(engine, 32768);
        let (blocked, plan) = fixture.add("a-blocked", TaskKind::Summarize);
        fixture.complete_source(&plan);
        fixture.add_review(
            &blocked,
            &"A material scoped assessment remains open. ".repeat(2000),
        );
        let source_before = fixture.source_snapshot(&plan);
        let (independent, _) = fixture.add_with_plan("z-independent", TaskKind::Summarize, false);
        let target = fixture
            .root()
            .join("vault")
            .join(&blocked.target.vault_path);
        let target_before = std::fs::read(&target).unwrap();
        let (output, report) = fixture.run();
        assert!(
            !output.status.success(),
            "blocked Work remains incomplete: {report}"
        );
        assert_eq!(report["semantic_calls"], 1, "{engine}: {report}");
        assert_eq!(fixture.source_snapshot(&plan), source_before);
        assert_eq!(std::fs::read(&target).unwrap(), target_before);
        assert!(!fixture.work_path(&plan, "seal.json").exists());
        assert!(
            !fixture
                .work_path(&plan, "finalization-action.json")
                .exists()
        );
        assert_eq!(fixture.calls().len(), 2);
        assert_eq!(fixture.calls()[1]["kind"], "whole-section");
        let status = fixture.json(&["queue", "status", "--json"]);
        for (id, expected) in [(blocked.task_id, "current"), (independent.task_id, "done")] {
            assert!(
                status["data"]["tasks"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|entry| entry["task_id"] == id && entry["status"] == expected),
                "{status}"
            );
        }
        assert!(
            report["stages"].as_array().unwrap().iter().any(|stage| {
                stage["detail"]
                    .as_str()
                    .is_some_and(|detail| detail.contains("max_input_bytes"))
            }),
            "{report}"
        );
        let (output, repeated) = fixture.run();
        assert!(!output.status.success());
        assert_eq!(repeated["semantic_calls"], 0, "{repeated}");
        assert_eq!(fixture.source_snapshot(&plan), source_before);
    }
}

#[test]
fn whole_sections_use_one_tool_free_call_without_fabricated_work_on_both_hosts() {
    for engine in ["codex", "claude-code"] {
        for kind in [
            TaskKind::Summarize,
            TaskKind::RefineEvents,
            TaskKind::IdentifyThemes,
        ] {
            let fixture = Fixture::new(engine, 32768);
            let (task, _) = fixture.add_with_plan("source", kind, false);
            let (output, report) = fixture.run();
            assert!(
                output.status.success(),
                "{report}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(report["semantic_calls"], 1);
            assert_eq!(fixture.calls().len(), 1);
            assert_eq!(fixture.calls()[0]["kind"], "whole-section");
            assert!(!fixture.root().join("vault/.lorekeeper/work").exists());
            let status = fixture.json(&["queue", "status", "--json"]);
            assert!(
                status["data"]["tasks"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|entry| entry["task_id"] == task.task_id && entry["status"] == "done")
            );
            let (_, repeated) = fixture.run();
            assert_eq!(repeated["semantic_calls"], 0);
        }
    }
}

#[test]
fn whole_section_actions_defer_unchanged_requests_and_allow_independent_publication() {
    for engine in ["codex", "claude-code"] {
        let fixture = Fixture::new(engine, 32768);
        fixture.mode("needs-action");
        let (task, _) = fixture.add_with_plan("a", TaskKind::Summarize, false);
        let (output, report) = fixture.run();
        assert!(!output.status.success());
        assert_eq!(report["semantic_calls"], 1);
        let (_, repeated) = fixture.run();
        assert_eq!(repeated["semantic_calls"], 0);
        fixture.mode("complete");
        let (other, _) = fixture.add_with_plan("b", TaskKind::Summarize, false);
        let (output, report) = fixture.run();
        assert!(!output.status.success(), "blocked a must remain incomplete");
        assert_eq!(report["semantic_calls"], 1);
        let status = fixture.json(&["queue", "status", "--json"]);
        for (id, expected) in [(task.task_id, "current"), (other.task_id, "done")] {
            assert!(
                status["data"]["tasks"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|entry| entry["task_id"] == id && entry["status"] == expected)
            );
        }
    }
}

#[test]
fn whole_sections_reject_malformed_forged_changed_and_oversized_results_on_both_hosts() {
    for engine in ["codex", "claude-code"] {
        for mode in [
            "malformed",
            "forged-guards",
            "changed-target",
            "oversized",
            "section-boundary",
            "exit",
        ] {
            let fixture = Fixture::new(engine, 32768);
            let (task, _) = fixture.add_with_plan("source", TaskKind::Summarize, false);
            fixture.mode(mode);
            let (output, report) = fixture.run();
            assert!(!output.status.success(), "{engine}/{mode}: {report}");
            assert_eq!(report["semantic_calls"], 1);
            let status = fixture.json(&["queue", "status", "--json"]);
            assert!(
                !status["data"]["tasks"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|entry| entry["task_id"] == task.task_id && entry["status"] == "done")
            );
            assert!(
                !fixture
                    .root()
                    .join("vault/.lorekeeper/queue/section-actions")
                    .exists()
            );
        }
    }
}

#[test]
fn whole_section_actions_reconsider_changed_inputs_and_refuse_corruption() {
    let fixture = Fixture::new("codex", 32768);
    let (task, _) = fixture.add_with_plan("source", TaskKind::Summarize, false);
    fixture.mode("needs-action");
    let (_, report) = fixture.run();
    assert_eq!(report["semantic_calls"], 1);
    let store = fixture
        .root()
        .join("vault/.lorekeeper/queue/section-actions");
    let action = std::fs::read_dir(&store)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::write(&action, "{}").unwrap();
    let (output, report) = fixture.run();
    assert!(!output.status.success());
    assert_eq!(report["semantic_calls"], 0);
    assert_eq!(std::fs::read_to_string(&action).unwrap(), "{}");
    std::fs::remove_file(&action).unwrap();
    let (_, report) = fixture.run();
    assert_eq!(report["semantic_calls"], 1);
    let page = fixture.root().join("vault").join(&task.target.vault_path);
    let raw = std::fs::read_to_string(&page).unwrap();
    std::fs::write(&page, format!("{raw}\nA changed source observation.\n")).unwrap();
    fixture.mode("complete");
    let (output, report) = fixture.run();
    assert!(output.status.success(), "{report}");
    assert_eq!(report["semantic_calls"], 1);
    assert_eq!(std::fs::read_dir(store).unwrap().count(), 0);
}

#[test]
fn whole_section_action_retention_preserves_pending_and_removes_unreferenced_receipts() {
    let fixture = Fixture::new("codex", 32768);
    fixture.add_with_plan("source", TaskKind::Summarize, false);
    fixture.mode("needs-action");
    fixture.run();
    let store = fixture
        .root()
        .join("vault/.lorekeeper/queue/section-actions");
    let before = std::fs::read_dir(&store)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let raw = std::fs::read(&before).unwrap();
    let output = fixture
        .command()
        .args(["queue", "prune", "--dry-run"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(std::fs::read(&before).unwrap(), raw);
    let output = fixture.command().args(["queue", "prune"]).output().unwrap();
    assert!(output.status.success());
    assert_eq!(std::fs::read(&before).unwrap(), raw);
    std::fs::remove_file(fixture.root().join("vault/.lorekeeper/queue/source.jsonl")).unwrap();
    let output = fixture.command().args(["queue", "prune"]).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!before.exists());
}

#[test]
fn whole_section_capacity_defers_complete_review_scope_without_blocking_independent_work() {
    let fixture = Fixture::new("codex", 32768);
    let (blocked, _) = fixture.add_with_plan("a", TaskKind::Summarize, false);
    let (independent, _) = fixture.add_with_plan("b", TaskKind::Summarize, false);
    let reviews = fixture.root().join("vault/wiki/explorations");
    std::fs::create_dir_all(&reviews).unwrap();
    std::fs::write(reviews.join("large-review.md"), format!(
        "---\ntype: exploration\ntitle: Large review\nreview:\n  status: unresolved\n  subjects: [{}]\n---\n# Large review\n{}\n", blocked.target.vault_path, "Scoped review evidence. ".repeat(2000)
    )).unwrap();
    let target_before = std::fs::read(
        fixture
            .root()
            .join("vault")
            .join(&blocked.target.vault_path),
    )
    .unwrap();
    let (output, report) = fixture.run();
    assert!(
        !output.status.success(),
        "capacity remains incomplete: {report}"
    );
    assert_eq!(report["semantic_calls"], 1, "{report}");
    assert!(!fixture.root().join("vault/.lorekeeper/work").exists());
    assert_eq!(
        std::fs::read(
            fixture
                .root()
                .join("vault")
                .join(&blocked.target.vault_path)
        )
        .unwrap(),
        target_before
    );
    let status = fixture.json(&["queue", "status", "--json"]);
    for (id, expected) in [(blocked.task_id, "current"), (independent.task_id, "done")] {
        assert!(
            status["data"]["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["task_id"] == id && entry["status"] == expected),
            "{status}"
        );
    }
    assert!(
        report["stages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|stage| stage["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains("above max_input_bytes"))),
        "{report}"
    );
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
fn quiescent_operational_changes_resume_paid_nodes_but_active_changes_refuse_publication() {
    for engine in ["codex", "claude-code"] {
        let fixture = Fixture::new(engine, 32768);
        let original = std::fs::read_to_string(&fixture.config).unwrap();
        std::fs::write(
            &fixture.config,
            original.replace("max_calls: 2", "max_calls: 1"),
        )
        .unwrap();
        let (_, plan) = fixture.add("source", TaskKind::Summarize);
        let before = std::fs::read(fixture.work_path(&plan, "plan.json")).unwrap();
        let (output, first) = fixture.run();
        assert!(!output.status.success());
        assert_eq!(first["semantic_calls"], 1);
        assert_eq!(fixture.calls()[0]["kind"], "inspect");
        std::fs::write(
            &fixture.config,
            original
                .replace(
                    "max_calls: 2",
                    "max_calls: 3, max_parallel: 2, max_screening_batches: 1",
                )
                .replace("timeout_seconds: 30", "timeout_seconds: 60"),
        )
        .unwrap();
        let replanned = fixture.json(&[
            "work",
            "plan",
            "--task-id",
            plan["subject"]["task_id"].as_str().unwrap(),
        ]);
        assert_eq!(replanned["work_id"], plan["work_id"]);
        let (output, second) = fixture.run();
        assert!(output.status.success(), "{second}");
        assert_eq!(second["semantic_calls"], 1);
        assert_eq!(
            fixture
                .calls()
                .iter()
                .filter(|call| call["kind"] == "inspect")
                .count(),
            1
        );
        assert_eq!(
            std::fs::read(fixture.work_path(&plan, "plan.json")).unwrap(),
            before
        );
        assert!(fixture.work_path(&plan, "publication.json").exists());

        let fixture = Fixture::new(engine, 32768);
        let (_, plan) = fixture.add("active", TaskKind::Summarize);
        fixture.mode("changed-operational-config");
        let (output, report) = fixture.run();
        assert!(!output.status.success(), "{report}");
        assert_eq!(report["semantic_calls"], 2);
        assert!(!fixture.work_path(&plan, "seal.json").exists());
        assert!(!fixture.work_path(&plan, "publication.json").exists());
        assert!(
            report["stages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|stage| stage["detail"]
                    .as_str()
                    .is_some_and(|detail| detail.contains("configuration changed"))),
            "{report}"
        );
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
        context_revision: None,
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
    assert!(
        report["stages"].as_array().unwrap().iter().any(|stage| {
            stage["name"] == "bounded section capacity"
                && stage["state"] == "skipped"
                && stage["detail"]
                    .as_str()
                    .is_some_and(|detail| detail.contains("above max_input_bytes"))
        }),
        "{report}"
    );
    assert!(
        fixture.json(&["work", "result", plan["work_id"].as_str().unwrap()])["output"].is_object()
    );
    assert!(fixture.work_path(&plan, "plan.json").exists());
    assert!(!fixture.work_path(&plan, "seal.json").exists());
}

#[test]
fn inspection_actions_retain_unfinished_work_skip_unchanged_calls_and_publish_independent_sections()
{
    for engine in ["codex", "claude-code"] {
        let fixture = Fixture::new(engine, 32768);
        let (task, plan) = fixture.add("a", TaskKind::Summarize);
        let before = std::fs::read(fixture.work_path(&plan, "plan.json")).unwrap();
        fixture.mode("inspection-action");
        let (output, report) = fixture.run();
        assert!(!output.status.success());
        assert_eq!(report["halted"], false, "{report}");
        assert_eq!(report["semantic_calls"], 1, "{report}");
        assert!(fixture.work_path(&plan, "inspection-actions.json").exists());
        let status = fixture.json(&["work", "status", plan["work_id"].as_str().unwrap()]);
        assert_eq!(status["completed"], 0);
        assert_eq!(status["complete"], false);
        assert_eq!(
            std::fs::read(fixture.work_path(&plan, "plan.json")).unwrap(),
            before
        );
        let (_, repeated) = fixture.run();
        assert_eq!(repeated["semantic_calls"], 0, "{repeated}");
        assert_eq!(repeated["halted"], false);
        fixture.mode("complete");
        let (other, _) = fixture.add_with_plan("b", TaskKind::Summarize, false);
        let (_, independent) = fixture.run();
        assert_eq!(independent["semantic_calls"], 1, "{independent}");
        assert_eq!(independent["halted"], false);
        let status = fixture.json(&["queue", "status", "--json"]);
        for (id, expected) in [(task.task_id, "current"), (other.task_id, "done")] {
            assert!(
                status["data"]["tasks"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|row| row["task_id"] == id && row["status"] == expected),
                "{status}"
            );
        }
        let raw = fixture.command().args(["queue", "check"]).output().unwrap();
        let receipts: Value = serde_json::from_slice(&raw.stdout).unwrap();
        assert_eq!(receipts["data"]["pending_mutation"], false);
    }
}

#[test]
fn inspection_actions_and_evidence_reject_forged_stale_empty_and_corrupt_states() {
    for engine in ["codex", "claude-code"] {
        for mode in [
            "inspection-action-wrong-id",
            "inspection-action-drift",
            "empty-inspection-evidence",
        ] {
            let fixture = Fixture::new(engine, 32768);
            let (_, plan) = fixture.add("source", TaskKind::Summarize);
            fixture.mode(mode);
            let (_, report) = fixture.run();
            assert_eq!(report["halted"], true, "{engine}/{mode}: {report}");
            assert!(!fixture.work_path(&plan, "inspection-actions.json").exists());
            if mode != "inspection-action-drift" {
                let status = fixture.json(&["work", "status", plan["work_id"].as_str().unwrap()]);
                assert_eq!(status["completed"], 0);
            }
        }
        let fixture = Fixture::new(engine, 32768);
        let (_, plan) = fixture.add("source", TaskKind::Summarize);
        std::fs::write(fixture.work_path(&plan, "inspection-actions.json"), "[]").unwrap();
        let (_, report) = fixture.run();
        assert_eq!(report["halted"], true, "{report}");
        assert_eq!(report["semantic_calls"], 0);
    }
}

#[test]
fn oversized_observation_defers_its_exact_work_without_retry_or_blocking_independent_work() {
    for engine in ["codex", "claude-code"] {
        let fixture = Fixture::new(engine, 32768);
        let config = std::fs::read_to_string(&fixture.config).unwrap();
        std::fs::write(
            &fixture.config,
            config.replace("max_calls: 2", "max_calls: 6"),
        )
        .unwrap();
        let (_, blocked) = fixture.add("a-blocked", TaskKind::Summarize);
        let plan = std::fs::read(fixture.work_path(&blocked, "plan.json")).unwrap();
        let (_, ready) = fixture.add("z-independent", TaskKind::Summarize);
        fixture.mode("oversized-inspection");
        let (_, report) = fixture.run();
        assert_eq!(report["halted"], false, "{engine}: {report}");
        assert!(
            fixture.work_path(&ready, "publication.json").exists(),
            "{report}"
        );
        assert!(!fixture.work_path(&blocked, "seal.json").exists());
        assert!(!fixture.work_path(&blocked, "final.json").exists());
        assert_eq!(
            std::fs::read(fixture.work_path(&blocked, "plan.json")).unwrap(),
            plan
        );
        let receipt = fixture.work_path(&blocked, "inspection-actions.json");
        let raw = std::fs::read(&receipt).unwrap();
        let action: Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(action["schema"], "inspection-actions-v2");
        let capacity = action["actions"]
            .as_object()
            .unwrap()
            .values()
            .next()
            .unwrap()["capacity"]
            .clone();
        assert!(
            capacity["actual_bytes"].as_u64().unwrap() > capacity["max_bytes"].as_u64().unwrap()
        );
        assert_eq!(
            std::fs::read_dir(fixture.work_path(&blocked, "nodes"))
                .map(|dir| dir.count())
                .unwrap_or(0),
            0
        );
        let calls = fixture.calls().len();
        let (_, report) = fixture.run();
        assert_eq!(report["halted"], false, "{report}");
        assert_eq!(report["semantic_calls"], 0, "{report}");
        assert_eq!(fixture.calls().len(), calls);
        assert_eq!(std::fs::read(&receipt).unwrap(), raw);
        let mut corrupted = action;
        corrupted["actions"]
            .as_object_mut()
            .unwrap()
            .values_mut()
            .next()
            .unwrap()["capacity"]["actual_bytes"] = json!(1);
        std::fs::write(&receipt, serde_json::to_vec(&corrupted).unwrap()).unwrap();
        let (_, report) = fixture.run();
        assert_eq!(report["halted"], true, "{report}");
        assert_eq!(fixture.calls().len(), calls);
    }
}

#[test]
fn oversized_observation_refuses_to_bind_a_changed_request() {
    let fixture = Fixture::new("codex", 32768);
    let (_, blocked) = fixture.add("a-blocked", TaskKind::Summarize);
    fixture.mode("oversized-inspection-drift");
    let (_, report) = fixture.run();
    assert_eq!(report["halted"], true, "{report}");
    assert!(
        !fixture
            .work_path(&blocked, "inspection-actions.json")
            .exists()
    );
    assert!(!fixture.work_path(&blocked, "seal.json").exists());
}
