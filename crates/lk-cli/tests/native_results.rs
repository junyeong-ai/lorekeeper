#![cfg(unix)]

use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const HOST: &str = r#"#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
root=Path(os.environ['TEST_RESULT_ROOT'])
engine=os.environ['TEST_RESULT_ENGINE']
if sys.argv[1:]==['--version']:
    print('codex-cli 0.164.0' if engine=='codex' else '2.1.309 (Claude Code)'); raise SystemExit()
raw=sys.stdin.read()
request=json.loads(raw.split('\nNative request:\n',1)[1])
with (root/'calls.jsonl').open('a') as f: f.write(json.dumps(request)+'\n')
mode=(root/'mode').read_text()
if mode in ['host-failure','host-envelope-error']:
    print(json.dumps({'type':'result','subtype':'error_during_execution','is_error':True,'errors':['PRIVATE_CANARY_ERROR']}))
    raise SystemExit(1 if mode=='host-failure' else 0)
if request.get('schema') in ['native-selected-results-v3','native-finalize-results-v2','native-finalize-frontier-v2']:
    assert '--safe-mode' not in sys.argv
    assert '--output-schema' in sys.argv or '--json-schema' in sys.argv
    assert 'Do not write result files' in raw or 'do not write result files' in raw
    if engine=='claude-code':
        assert '--allowedTools' in sys.argv and '--verbose' not in sys.argv
        assert sys.argv[sys.argv.index('--output-format')+1]=='json'
    else:
        assert sys.argv[sys.argv.index('--sandbox')+1]=='workspace-write'
    completed=request.get('completed') or request.get('frontier')
    if request['schema']=='native-finalize-frontier-v2':
        (root/'last-frontier.json').write_text(json.dumps(request))
        assert completed['observations'] and completed['proof']['nodes']
    tasks=request.get('tasks') or [{'task':{'kind':completed['context']['purpose']['task_kind'],
        'target':completed['context']['purpose']['target']}}]
    results=[]
    for item in tasks:
        task=item['task']; target=task['target']['vault_path']
        if (root/'stage-synthesis').exists() and task['kind']=='synthesize-concept':
            assert '--model=stage-synthesis' in sys.argv
            assert os.environ.get('LORE_EXECUTION_PROFILE')=='primary'
            if engine=='claude-code': assert os.environ.get('CLAUDE_CODE_EFFORT_LEVEL')=='high'
            else: assert 'model_reasoning_effort="high"' in sys.argv
        if mode.startswith('discovery') and ('recover' in target or 'frontier' in target):
            context=item.get('context') or completed['context']
            old=context['purpose']['discovery']['queries']
            fresh=any(q['query']=='fresh terminology' for q in old)
            if mode!='discovery-once' or not fresh:
                query='protocol' if mode=='discovery-same' or (mode=='discovery-cycle' and fresh) else 'fresh terminology'
                value={'queries':[{'query':query,'phrase':True,'spacing':False}]}
                if mode=='discovery-empty': value={'queries':[]}
                if mode=='discovery-identity': value['task_id']='forged'
                if mode=='discovery-target-drift':
                    path=root/'vault'/target; path.write_text(path.read_text()+'\nExternal change.\n')
                results.append({'status':'needs-discovery','value':value,'reason':'Explicit fresh owner discovery is required.'}); continue
        if mode=='frontier-action' or (mode=='action' and 'blocked' in target) or (mode=='batch-capacity' and 'blocked' in target and request['max_response_bytes'] < item['max_response_bytes']):
            results.append({'status':'needs-action','value':None,'reason':'A scoped review action is required.'}); continue
        if mode=='changed-target':
            path=root/'vault'/target; path.write_text(path.read_text()+'\nExternal change.\n')
        if mode=='changed-review':
            path=root/'vault/wiki/explorations/review.md'; path.parent.mkdir(exist_ok=True)
            path.write_text('---\ntype: exploration\ntitle: Review\nreview:\n  status: unresolved\n  subjects: ['+target+']\n---\n# New scoped issue\n')
        if task['kind']=='extract-concepts':
            value={'concepts':[]}
            if mode=='new-concept': value={'concepts':[{'name':'Transactional Outbox','category':None,'aliases':[],
                'admission':{'decision':'new'},'synthesis':'The source proposes a transactional outbox; deployment is unverified.','rss_evidence':[]}]}
        else: value={'synthesis':'The source reports a proposal; its deployment remains unknown.','related':None}
        if mode=='metadata': value['task_id']='forged'
        if mode=='oversized': value={'synthesis':'x'*40000,'related':None}
        results.append({'status':'complete','value':value,'reason':None})
    output={'results':results}
    if mode=='wrong-count': output={'results':[]}
    if mode=='prose': output='Completed without a native result.'
    if engine=='codex':
        if mode!='missing':
            Path(sys.argv[sys.argv.index('--output-last-message')+1]).write_text(json.dumps(output))
        print(json.dumps({'type':'item.completed','item':{'type':'command_execution','output':'x'*100000}}))
        print('NOT THE RESULT')
        raise SystemExit()
else:
    assert request['kind'] in ['inspect','reduce']
    assert mode!='frontier', 'complete paid frontier must not invoke another evidence node'
    output={'status':'complete','observation':{'summary':'Exact supplied evidence; deployment unknown.',
        'findings':[],'source_questions':[],'unresolved':[],'resolutions':[]},'action':None}
if engine=='claude-code': output={'type':'result','subtype':'success','is_error':False,'structured_output':output}
print(json.dumps(output))
"#;

struct Fixture {
    directory: tempfile::TempDir,
    config: PathBuf,
    engine: String,
}

#[test]
fn native_generation_failure_reports_only_observed_host_metadata() {
    for (engine, mode) in [
        ("codex", "host-failure"),
        ("claude-code", "host-failure"),
        ("claude-code", "host-envelope-error"),
    ] {
        let fixture = Fixture::new(engine);
        let (task, _) = fixture.add("diagnostic", lk_queue::TaskKind::SynthesizeConcept, false);
        std::fs::write(fixture.root().join("mode"), mode).unwrap();
        let report = fixture.run();
        assert_eq!(report["halted"], true, "{report}");
        assert_eq!(report["semantic_calls"], 1, "{report}");
        assert_eq!(report["queue_application"]["applied"], 0);
        assert!(
            !serde_json::to_string(&report)
                .unwrap()
                .contains("PRIVATE_CANARY")
        );
        let step = &report["work_steps"][0];
        assert_eq!(step["role"], "finalizer");
        assert!(step["result_json_bytes"].is_null());
        let failure = &step["host_failure"];
        assert_eq!(
            failure["phase"],
            if mode == "host-failure" {
                "invocation"
            } else {
                "response"
            }
        );
        assert_eq!(
            failure["exit_code"],
            if mode == "host-failure" { 1 } else { 0 }
        );
        if engine == "codex" {
            assert_eq!(failure["stdout_capture"], "not-captured");
            assert!(failure["stdout_bytes"].is_null());
            assert!(failure["stdout_format"].is_null());
        } else {
            assert_eq!(failure["stdout_capture"], "complete");
            assert_eq!(failure["subtype"]["recognized"], "error_during_execution");
            assert_eq!(failure["errors"]["members"], 1);
        }
        let queue = fixture.json(&["queue", "status", "--json"]);
        assert!(
            queue["data"]["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["task_id"] == task.task_id && row["status"] == "current")
        );
    }
}
impl Fixture {
    fn new(engine: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let agent = root.join("agent");
        std::fs::write(&agent, HOST).unwrap();
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
        let config = root.join("config.yaml");
        std::fs::write(&config,format!("vault: {{root: vault, locale: en}}\nidentity: {{name: Test, email: test@example.com}}\nsources:\n  notes: {{type: manual, params: {{inbox_dir: inbox}}, concept_policy: supported-new}}\nexecution:\n  default_profile: primary\n  work: {{max_input_bytes: 65536, max_result_bytes: 8192, max_calls: 6, extraction_context: full-context}}\n  profiles:\n    primary: {{engine: {engine}, executable: {}, timeout_seconds: 30}}\n",serde_json::to_string(&agent).unwrap())).unwrap();
        for path in ["vault/wiki/documents", "vault/wiki/concepts", "vault/inbox"] {
            std::fs::create_dir_all(root.join(path)).unwrap();
        }
        std::fs::write(root.join("mode"), "complete").unwrap();
        let fixture = Self {
            directory,
            config,
            engine: engine.into(),
        };
        let deployed = fixture
            .command()
            .args(["self", "deploy", "--skills", "none"])
            .output()
            .unwrap();
        assert!(
            deployed.status.success(),
            "{}",
            String::from_utf8_lossy(&deployed.stderr)
        );
        fixture
    }
    fn root(&self) -> &Path {
        self.directory.path()
    }
    fn route_synthesis(&self) {
        let executable = serde_json::to_string(&self.root().join("agent")).unwrap();
        let effort = if self.engine == "codex" {
            "model_reasoning_effort: high"
        } else {
            "effort: high"
        };
        let config=std::fs::read_to_string(&self.config).unwrap().replace("  profiles:\n",
            &format!("  stages: {{synthesis: synthesizer}}\n  profiles:\n    synthesizer: {{engine: {}, executable: {executable}, timeout_seconds: 30, model: stage-synthesis, {effort}}}\n",self.engine));
        std::fs::write(&self.config, config).unwrap();
        std::fs::write(self.root().join("stage-synthesis"), "enabled").unwrap();
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_lore"));
        command
            .args(["--config", self.config.to_str().unwrap()])
            .env("XDG_CONFIG_HOME", self.root().join("config"))
            .env("LORE_INSTALL_DATA_DIR", self.root().join("data"))
            .env("TEST_RESULT_ROOT", self.root())
            .env("TEST_RESULT_ENGINE", &self.engine);
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
    fn run(&self) -> Value {
        let output = self
            .command()
            .args(["run", "process", "--json"])
            .output()
            .unwrap();
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "{error}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        })
    }
    fn add(
        &self,
        name: &str,
        kind: lk_queue::TaskKind,
        planned: bool,
    ) -> (lk_queue::QueueTask, Option<Value>) {
        self.add_with_text(
            name,
            kind,
            planned,
            "An explicit protocol proposal; deployment is unknown.",
        )
    }
    fn add_with_text(
        &self,
        name: &str,
        kind: lk_queue::TaskKind,
        planned: bool,
        text: &str,
    ) -> (lk_queue::QueueTask, Option<Value>) {
        let queue = self.root().join("vault/.lorekeeper/queue");
        let client = lk_queue::QueueLlmClient::new(queue.clone());
        let task = if kind == lk_queue::TaskKind::ExtractConcepts {
            client.build_task("fixture",kind,serde_json::from_value::<lk_queue::ExtractConceptsRequest>(json!({
                "text":text,"source_id":"notes","source_type":"manual","date":"2026-10-24","locale":"en","concept_policy":"supported-new","focus":null,"categories":[],
                "target":{"vault_path":format!("wiki/documents/{name}.md"),"kind":"document-concepts","anchor":"## Related Concepts"}
            })).unwrap())
        } else {
            let path = format!("wiki/documents/evidence-{name}.md");
            let raw = format!(
                "---\ntype: document\ntitle: Evidence {name}\n---\n# Evidence\nThe protocol defines [{name}](../concepts/{name}.md); deployment is unknown.\n{text}\n"
            );
            std::fs::write(self.root().join("vault").join(&path), &raw).unwrap();
            let revision = lk_core::concept::source_revision(&lk_vault::parse_page(&raw).unwrap());
            client.build_task("fixture",kind,serde_json::from_value::<lk_queue::ConceptSynthesisRequest>(json!({
                "citations":[{"path":path,"revision":revision}],"locale":"en","related_anchor":null,
                "target":{"vault_path":format!("wiki/concepts/{name}.md"),"kind":"concept-synthesis","anchor":"## Synthesis"}
            })).unwrap())
        };
        let page = format!(
            "---\ntype: {}\ntitle: {name}\nsource_count: 0\nllm_inputs:\n  {}: {}\n---\n# {name}\n\n{}\n\n## Content\nAn explicit protocol proposal; deployment is unknown.\n\n## Sources\n\n## Related\n",
            task.target.kind.page_format(),
            task.target.kind.llm_inputs_key(),
            task.cache_hash,
            task.target.anchor
        );
        std::fs::write(
            self.root().join("vault").join(&task.target.vault_path),
            page,
        )
        .unwrap();
        std::fs::create_dir_all(&queue).unwrap();
        lk_queue::write_tasks_atomic(
            &queue.join(format!("{name}.jsonl")),
            std::slice::from_ref(&task),
        )
        .unwrap();
        if kind == lk_queue::TaskKind::ExtractConcepts {
            let status = self.json(&["queue", "status", "--json"]);
            let row = status["data"]["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["task_id"] == task.task_id)
                .unwrap();
            let file = self.root().join("discovery.json");
            std::fs::write(&file,serde_json::to_vec(&json!({"target_revision":row["target_revision"],"queries":[{"query":"protocol","phrase":true,"spacing":false}]})).unwrap()).unwrap();
            self.json(&[
                "work",
                "discover",
                "--task-id",
                &task.task_id,
                "--file",
                file.to_str().unwrap(),
            ]);
        }
        if planned {
            let output = self
                .command()
                .args(["graph", "backlinks-sync"])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let plan = planned.then(|| self.json(&["work", "plan", "--task-id", &task.task_id]));
        (task, plan)
    }
    fn paid_frontier(&self, kind: lk_queue::TaskKind) -> (lk_queue::QueueTask, Value, Vec<Value>) {
        let config = std::fs::read_to_string(&self.config).unwrap();
        std::fs::write(
            &self.config,
            config
                .replace("max_input_bytes: 65536", "max_input_bytes: 262144")
                .replace("max_result_bytes: 8192", "max_result_bytes: 32768"),
        )
        .unwrap();
        let (task, plan) = self.add_with_text(
            "frontier",
            kind,
            true,
            &"A retained protocol observation. ".repeat(36000),
        );
        let plan = plan.unwrap();
        let id = plan["work_id"].as_str().unwrap();
        let status = self.json(&["work", "status", id, "--limit", "100"]);
        let ready = status["ready"].as_array().unwrap();
        assert!(ready.len() >= 5, "{status}");
        let mut observations = Vec::new();
        for (index, node) in ready.iter().enumerate() {
            let node = node.as_str().unwrap();
            let request = self.json(&["work", "read", id, node]);
            assert_eq!(request["kind"], "inspect");
            let evidence = request["input"]["ranges"][0]["range"].clone();
            let output = json!({"work_id":id,"node_id":node,"input_revision":request["input_revision"],
                "summary":"Complete assigned source; deployment remains unknown.",
                "findings":[{"statement":format!("Source {index}: {}", "observed detail ".repeat(180)),"evidence":[evidence.clone()]}],
                "source_questions":(0..4).map(|n|json!({"id":format!("{node}:source:{n}"),"question":format!("Was condition {index}/{n} verified? {}", "unknown source condition ".repeat(35)),"evidence":[evidence.clone()]})).collect::<Vec<_>>(),
                "unresolved":[],"resolutions":[]});
            let file = self.root().join("observation.json");
            std::fs::write(&file, serde_json::to_vec(&output).unwrap()).unwrap();
            self.json(&["work", "record", "--file", file.to_str().unwrap()]);
            observations.push(output);
        }
        assert!(serde_json::to_vec(&observations).unwrap().len() > 32768);
        (task, plan, observations)
    }

    fn recovery(&self, task: &lk_queue::QueueTask) -> PathBuf {
        self.root()
            .join("vault/.lorekeeper/queue/discovery")
            .join(format!(
                "{}.recovery.json",
                lk_core::rss_original::digest(task.task_id.as_bytes())
            ))
    }
    fn done(&self, task: &lk_queue::QueueTask) -> bool {
        let raw = std::fs::read_to_string(self.root().join("vault").join(&task.target.vault_path))
            .unwrap();
        let page = lk_vault::parse_page(&raw).unwrap();
        page.frontmatter
            .get("llm_inputs")
            .and_then(|inputs| inputs.get(task.target.kind.completion_key()))
            .and_then(Value::as_str)
            == Some(task.cache_hash.as_str())
    }
    fn calls(&self) -> usize {
        std::fs::read_to_string(self.root().join("calls.jsonl"))
            .map(|raw| raw.lines().count())
            .unwrap_or(0)
    }
}

#[test]
fn native_owns_extraction_and_synthesis_results_for_selected_and_bounded_work_on_both_hosts() {
    for engine in ["codex", "claude-code"] {
        for kind in [
            lk_queue::TaskKind::ExtractConcepts,
            lk_queue::TaskKind::SynthesizeConcept,
        ] {
            for planned in [false, true] {
                let fixture = Fixture::new(engine);
                if kind == lk_queue::TaskKind::SynthesizeConcept {
                    fixture.route_synthesis();
                }
                let (task, plan) = fixture.add("subject", kind, planned);
                let report = fixture.run();
                assert!(fixture.done(&task), "{engine}/{kind:?}/{planned}: {report}");
                assert!(!report["halted"].as_bool().unwrap(), "{report}");
                let steps = report["work_steps"].as_array().unwrap();
                let step = steps
                    .iter()
                    .find(|step| step["role"] == "finalizer")
                    .unwrap();
                assert_eq!(step["outcome"], "staged");
                if kind == lk_queue::TaskKind::SynthesizeConcept {
                    assert_eq!(step["stages"][0], "synthesis");
                    assert_eq!(step["execution"]["profile"], "synthesizer");
                }
                assert!(step["result_json_bytes"].as_u64().unwrap() > 0);
                if let Some(plan) = plan {
                    let directory = fixture
                        .root()
                        .join("vault/.lorekeeper/work")
                        .join(plan["work_id"].as_str().unwrap());
                    assert!(directory.join("seal.json").exists());
                    let result: Value = serde_json::from_slice(
                        &std::fs::read(directory.join("final.json")).unwrap(),
                    )
                    .unwrap();
                    assert_eq!(result["task_id"], task.task_id);
                    assert_eq!(result["cache_hash"], task.cache_hash);
                }
            }
        }
    }
}

#[test]
fn generation_batches_split_only_when_resolved_profiles_differ() {
    for routed in [false, true] {
        let fixture = Fixture::new("codex");
        if routed {
            fixture.route_synthesis();
        }
        let (extraction, _) = fixture.add("extract", lk_queue::TaskKind::ExtractConcepts, false);
        let (synthesis, _) =
            fixture.add("synthesize", lk_queue::TaskKind::SynthesizeConcept, false);
        let report = fixture.run();
        assert!(fixture.done(&synthesis), "{report}");
        assert_eq!(fixture.done(&extraction), routed, "{report}");
        if !routed {
            assert!(
                report["queue_application"]["rejected"].as_u64().unwrap() > 0,
                "shared read scope must be revalidated after synthesis publication: {report}"
            );
        }
        let count = if routed { 2 } else { 1 };
        assert_eq!(fixture.calls(), count, "{report}");
        assert_eq!(report["semantic_calls"], count);
        if !routed {
            let raw = std::fs::read_to_string(fixture.root().join("calls.jsonl")).unwrap();
            let request: Value = serde_json::from_str(raw.lines().next().unwrap()).unwrap();
            assert_eq!(request["tasks"].as_array().unwrap().len(), 2);
        }
    }
}

#[test]
fn native_actions_defer_unchanged_work_without_calls_and_keep_independent_publication() {
    for planned in [false, true] {
        let fixture = Fixture::new("codex");
        let (blocked, _) = fixture.add("blocked", lk_queue::TaskKind::ExtractConcepts, planned);
        let (ready, _) = fixture.add("ready", lk_queue::TaskKind::ExtractConcepts, planned);
        std::fs::write(fixture.root().join("mode"), "action").unwrap();
        let report = fixture.run();
        assert!(!fixture.done(&blocked), "{report}");
        assert!(fixture.done(&ready), "{report}");
        let count = fixture.calls();
        let report = fixture.run();
        assert_eq!(fixture.calls(), count, "{report}");
        assert!(!fixture.done(&blocked));
    }
}

#[test]
fn extraction_discovery_recovery_replans_selected_and_root_on_both_hosts() {
    for engine in ["codex", "claude-code"] {
        for planned in [false, true] {
            let fixture = Fixture::new(engine);
            let (task, plan) = fixture.add("recover", lk_queue::TaskKind::ExtractConcepts, planned);
            std::fs::write(fixture.root().join("mode"), "discovery-once").unwrap();
            let report = fixture.run();
            assert!(fixture.done(&task), "{engine}/{planned}: {report}");
            assert!(!report["halted"].as_bool().unwrap(), "{report}");
            assert!(
                report["stages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|s| s["name"] == "extraction discovery recovery"),
                "{report}"
            );
            let calls = std::fs::read_to_string(fixture.root().join("calls.jsonl")).unwrap();
            let requests: Vec<Value> = calls
                .lines()
                .map(|s| serde_json::from_str(s).unwrap())
                .collect();
            assert!(
                requests
                    .iter()
                    .any(|r| r.to_string().contains("fresh terminology")),
                "{calls}"
            );
            if let Some(plan) = plan {
                let directory = fixture
                    .root()
                    .join("vault/.lorekeeper/work")
                    .join(plan["work_id"].as_str().unwrap());
                assert!(directory.join("plan.json").exists());
                assert!(
                    !directory.join("seal.json").exists(),
                    "recovery must not seal old-scope work"
                );
            }
        }
    }
}

#[test]
fn unchanged_discovery_and_cycles_defer_without_repeating_calls_and_keep_independent_progress() {
    for engine in ["codex", "claude-code"] {
        for mode in ["discovery-same", "discovery-cycle"] {
            let fixture = Fixture::new(engine);
            let (task, _) = fixture.add("recover", lk_queue::TaskKind::ExtractConcepts, false);
            let (ready, _) = fixture.add("ready", lk_queue::TaskKind::ExtractConcepts, false);
            std::fs::write(fixture.root().join("mode"), mode).unwrap();
            let report = fixture.run();
            assert!(!fixture.done(&task), "{engine}/{mode}: {report}");
            assert!(fixture.done(&ready), "{report}");
            assert!(!report["halted"].as_bool().unwrap(), "{report}");
            let raw = std::fs::read(fixture.recovery(&task)).unwrap();
            let receipt: Value = serde_json::from_slice(&raw).unwrap();
            assert!(!receipt["receipts"].as_array().unwrap().is_empty());
            let requests = std::fs::read_to_string(fixture.root().join("calls.jsonl")).unwrap();
            let actual: Vec<_> = requests
                .lines()
                .map(|line| {
                    let request: Value = serde_json::from_str(line).unwrap();
                    lk_core::concept::file_revision(&serde_json::to_string(&request).unwrap())
                })
                .collect();
            for receipt in receipt["receipts"].as_array().unwrap() {
                assert!(
                    actual
                        .iter()
                        .any(|digest| receipt["request_revision"] == *digest),
                    "receipt must bind an actually delivered request"
                );
            }
            let calls = fixture.calls();
            let report = fixture.run();
            assert_eq!(fixture.calls(), calls, "{engine}/{mode}: {report}");
            assert_eq!(std::fs::read(fixture.recovery(&task)).unwrap(), raw);
        }
    }
}

#[test]
fn discovery_recovery_refuses_invalid_queries_non_extraction_and_changed_target() {
    for engine in ["codex", "claude-code"] {
        for (mode, kind) in [
            ("discovery-empty", lk_queue::TaskKind::ExtractConcepts),
            ("discovery-identity", lk_queue::TaskKind::ExtractConcepts),
            (
                "discovery-target-drift",
                lk_queue::TaskKind::ExtractConcepts,
            ),
            ("discovery-same", lk_queue::TaskKind::SynthesizeConcept),
        ] {
            let fixture = Fixture::new(engine);
            let (task, _) = fixture.add("recover", kind, false);
            std::fs::write(fixture.root().join("mode"), mode).unwrap();
            let report = fixture.run();
            assert!(
                report["halted"].as_bool().unwrap(),
                "{engine}/{mode}: {report}"
            );
            assert!(!fixture.done(&task));
            assert!(!fixture.recovery(&task).exists());
        }
    }
}

#[test]
fn corrupt_discovery_history_is_fatal_and_never_overwritten() {
    let fixture = Fixture::new("codex");
    let (task, _) = fixture.add("recover", lk_queue::TaskKind::ExtractConcepts, false);
    std::fs::write(fixture.recovery(&task), "{corrupt").unwrap();
    let report = fixture.run();
    assert!(report["halted"].as_bool().unwrap(), "{report}");
    assert_eq!(fixture.calls(), 0);
    assert_eq!(
        std::fs::read_to_string(fixture.recovery(&task)).unwrap(),
        "{corrupt"
    );
}

#[test]
fn discovery_recovery_does_not_hide_a_corrupt_active_generation_action() {
    let fixture = Fixture::new("codex");
    let (task, _) = fixture.add("recover", lk_queue::TaskKind::ExtractConcepts, false);
    std::fs::write(fixture.root().join("mode"), "discovery-same").unwrap();
    fixture.run();
    let calls = std::fs::read_to_string(fixture.root().join("calls.jsonl")).unwrap();
    let request: Value = serde_json::from_str(calls.lines().last().unwrap()).unwrap();
    let key = blake3::hash(&serde_json::to_vec(&(&task.task_id, &task.cache_hash)).unwrap());
    let directory = fixture
        .root()
        .join("vault/.lorekeeper/queue/result-actions");
    std::fs::create_dir_all(&directory).unwrap();
    let wrong_context = "f".repeat(64);
    assert_ne!(request["tasks"][0]["context_revision"], wrong_context);
    let action = json!({"schema":"native-result-action-v1","task_id":task.task_id,"cache_hash":task.cache_hash,
        "context_revision":wrong_context,"request_revision":lk_core::concept::file_revision(&serde_json::to_string(&request).unwrap()),
        "reason":"A stored action with an inconsistent scope."});
    let path = directory.join(format!("{key}.json"));
    std::fs::write(&path, serde_json::to_vec(&action).unwrap()).unwrap();
    let count = fixture.calls();
    let report = fixture.run();
    assert!(report["halted"].as_bool().unwrap(), "{report}");
    assert_eq!(fixture.calls(), count);
    assert_eq!(
        serde_json::from_slice::<Value>(&std::fs::read(path).unwrap()).unwrap(),
        action
    );
    assert!(!fixture.done(&task));
}

#[test]
fn frontier_discovery_keeps_paid_nodes_unsealed_and_same_request_spends_no_more_calls() {
    for engine in ["codex", "claude-code"] {
        let fixture = Fixture::new(engine);
        let (task, plan, _) = fixture.paid_frontier(lk_queue::TaskKind::ExtractConcepts);
        let directory = fixture
            .root()
            .join("vault/.lorekeeper/work")
            .join(plan["work_id"].as_str().unwrap());
        let saved: Vec<_> = std::fs::read_dir(directory.join("nodes"))
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                let raw = std::fs::read(&path).unwrap();
                (path, raw)
            })
            .collect();
        std::fs::write(fixture.root().join("mode"), "discovery-same").unwrap();
        let report = fixture.run();
        assert!(!report["halted"].as_bool().unwrap(), "{engine}: {report}");
        assert!(!fixture.done(&task));
        let receipt: Value =
            serde_json::from_slice(&std::fs::read(fixture.recovery(&task)).unwrap()).unwrap();
        assert_eq!(receipt["receipts"].as_array().unwrap().len(), 1);
        assert_eq!(receipt["receipts"][0]["disposition"], "unchanged");
        assert!(!directory.join("final.json").exists());
        assert!(!directory.join("seal.json").exists());
        assert!(!directory.join("frontier-seal.json").exists());
        for (path, raw) in saved {
            assert_eq!(std::fs::read(path).unwrap(), raw);
        }
        let calls = fixture.calls();
        let report = fixture.run();
        assert_eq!(fixture.calls(), calls, "{report}");
        assert!(!fixture.done(&task));
    }
}

#[test]
fn changed_target_reconsiders_discovery_without_retagging_old_receipts() {
    let fixture = Fixture::new("codex");
    let (task, _) = fixture.add("recover", lk_queue::TaskKind::ExtractConcepts, false);
    std::fs::write(fixture.root().join("mode"), "discovery-same").unwrap();
    let report = fixture.run();
    assert!(!report["halted"].as_bool().unwrap(), "{report}");
    let prior: Value =
        serde_json::from_slice(&std::fs::read(fixture.recovery(&task)).unwrap()).unwrap();
    let count = fixture.calls();
    let target = fixture.root().join("vault").join(&task.target.vault_path);
    let mut raw = std::fs::read_to_string(&target).unwrap();
    raw.push_str("\nA newly authored source detail.\n");
    std::fs::write(target, raw).unwrap();
    let report = fixture.run();
    assert!(!report["halted"].as_bool().unwrap(), "{report}");
    assert!(fixture.calls() > count, "{report}");
    let next: Value =
        serde_json::from_slice(&std::fs::read(fixture.recovery(&task)).unwrap()).unwrap();
    assert_eq!(next["receipts"][0], prior["receipts"][0]);
    assert!(
        next["receipts"].as_array().unwrap().len() > prior["receipts"].as_array().unwrap().len()
    );
    let count = fixture.calls();
    fixture.run();
    assert_eq!(fixture.calls(), count);
    assert!(!fixture.done(&task));
}

#[test]
fn native_assembled_new_concept_keeps_original_target_policy_and_source_support() {
    for engine in ["codex", "claude-code"] {
        let fixture = Fixture::new(engine);
        let (task, _) = fixture.add("subject", lk_queue::TaskKind::ExtractConcepts, false);
        std::fs::write(fixture.root().join("mode"), "new-concept").unwrap();
        let report = fixture.run();
        assert!(fixture.done(&task), "{report}");
        let page = std::fs::read_to_string(
            fixture
                .root()
                .join("vault/wiki/concepts/transactional-outbox.md"),
        )
        .unwrap();
        assert!(page.contains("deployment is unverified"));
        assert_eq!(
            lk_vault::parse_page(&page)
                .unwrap()
                .frontmatter
                .get("source_count"),
            Some(&json!(1))
        );
        let snapshot = fixture.json(&["wiki", "prepare"]);
        let citations = fixture.json(&[
            "wiki",
            "citations",
            "wiki/concepts/transactional-outbox.md",
            "--snapshot",
            snapshot["snapshot"].as_str().unwrap(),
        ]);
        assert!(
            citations.to_string().contains(&task.target.vault_path),
            "{citations}"
        );
    }
}

#[test]
fn malformed_missing_oversized_and_changed_scope_results_never_publish() {
    for engine in ["codex", "claude-code"] {
        for mode in [
            "metadata",
            "wrong-count",
            "prose",
            "oversized",
            "changed-target",
            "changed-review",
        ] {
            let fixture = Fixture::new(engine);
            let (task, _) = fixture.add("subject", lk_queue::TaskKind::SynthesizeConcept, false);
            std::fs::write(fixture.root().join("mode"), mode).unwrap();
            let report = fixture.run();
            assert_eq!(report["halted"], true, "{engine}/{mode}: {report}");
            assert!(!fixture.done(&task));
        }
    }
    let fixture = Fixture::new("codex");
    let (task, _) = fixture.add("subject", lk_queue::TaskKind::ExtractConcepts, false);
    std::fs::write(fixture.root().join("mode"), "missing").unwrap();
    let report = fixture.run();
    assert_eq!(report["halted"], true, "{report}");
    assert!(!fixture.done(&task));
}

#[test]
fn result_identity_capacity_defers_without_calls_and_preserves_independent_work() {
    for planned in [false, true] {
        let fixture = Fixture::new("codex");
        let config = std::fs::read_to_string(&fixture.config).unwrap();
        std::fs::write(
            &fixture.config,
            config.replace("max_result_bytes: 8192", "max_result_bytes: 4096"),
        )
        .unwrap();
        let (blocked, _) = fixture.add("blocked", lk_queue::TaskKind::ExtractConcepts, false);
        let status = fixture.json(&["queue", "status", "--json"]);
        let row = status["data"]["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["task_id"] == blocked.task_id)
            .unwrap();
        let discovery = fixture.root().join("large-discovery.json");
        std::fs::write(&discovery, serde_json::to_vec(&json!({"target_revision":row["target_revision"],"queries":[{"query":"x".repeat(4096),"phrase":true,"spacing":false}]})).unwrap()).unwrap();
        fixture.json(&[
            "work",
            "discover",
            "--task-id",
            &blocked.task_id,
            "--file",
            discovery.to_str().unwrap(),
        ]);
        if planned {
            fixture.json(&["work", "plan", "--task-id", &blocked.task_id]);
        }
        let (ready, _) = fixture.add("ready", lk_queue::TaskKind::ExtractConcepts, false);
        let report = fixture.run();
        assert!(!report["halted"].as_bool().unwrap(), "{planned}: {report}");
        assert!(fixture.done(&ready), "{planned}: {report}");
        assert!(!fixture.done(&blocked));
        let calls = std::fs::read_to_string(fixture.root().join("calls.jsonl")).unwrap();
        let mut observed_capacity = false;
        for line in calls.lines() {
            let call: Value = serde_json::from_str(line).unwrap();
            if call["schema"] == "native-selected-results-v3" {
                assert!(
                    call["tasks"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .all(|task| task["task"]["task_id"] != blocked.task_id)
                );
            } else {
                assert_ne!(call["schema"], "native-finalize-results-v2");
            }
        }
        for stage in report["stages"].as_array().unwrap() {
            observed_capacity |= stage["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains("native result envelope"));
        }
        assert!(observed_capacity, "{report}");
    }
}

#[test]
fn changed_batch_response_allowance_reconsiders_capacity_action() {
    let fixture = Fixture::new("codex");
    let (blocked, _) = fixture.add("blocked", lk_queue::TaskKind::ExtractConcepts, false);
    let (other, _) = fixture.add("other", lk_queue::TaskKind::ExtractConcepts, false);
    for (task, query) in [
        (&blocked, "unmatched-alone".to_string()),
        (&other, "z".repeat(2000)),
    ] {
        let status = fixture.json(&["queue", "status", "--json"]);
        let row = status["data"]["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["task_id"] == task.task_id)
            .unwrap();
        let file = fixture.root().join("recipe.json");
        std::fs::write(&file,serde_json::to_vec(&json!({"target_revision":row["target_revision"],"queries":[{"query":query,"phrase":true,"spacing":false}]})).unwrap()).unwrap();
        fixture.json(&[
            "work",
            "discover",
            "--task-id",
            &task.task_id,
            "--file",
            file.to_str().unwrap(),
        ]);
    }
    std::fs::write(fixture.root().join("mode"), "batch-capacity").unwrap();
    let report = fixture.run();
    assert!(fixture.done(&other), "{report}");
    assert!(
        fixture.done(&blocked),
        "a larger standalone response allowance is a changed request: {report}"
    );
    let calls: Vec<Value> = std::fs::read_to_string(fixture.root().join("calls.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(calls[0]["tasks"].as_array().unwrap().len(), 2);
    assert_eq!(calls[1]["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(calls[1]["tasks"][0]["task"]["task_id"], blocked.task_id);
    assert!(
        calls[1]["max_response_bytes"].as_u64().unwrap()
            > calls[0]["max_response_bytes"].as_u64().unwrap()
    );
}

#[test]
fn batch_actions_reconsider_once_individually_then_defer_without_calls() {
    for engine in ["codex", "claude-code"] {
        let fixture = Fixture::new(engine);
        let (first, _) = fixture.add("blocked-first", lk_queue::TaskKind::ExtractConcepts, false);
        let (second, _) = fixture.add("blocked-second", lk_queue::TaskKind::ExtractConcepts, false);
        std::fs::write(fixture.root().join("mode"), "action").unwrap();
        let report = fixture.run();
        assert_eq!(report["halted"], false, "{report}");
        assert!(!fixture.done(&first));
        assert!(!fixture.done(&second));
        let calls: Vec<Value> = std::fs::read_to_string(fixture.root().join("calls.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(calls.len(), 3, "{calls:?}");
        assert_eq!(calls[0]["tasks"].as_array().unwrap().len(), 2);
        assert_eq!(calls[1]["tasks"].as_array().unwrap().len(), 1);
        assert_eq!(calls[2]["tasks"].as_array().unwrap().len(), 1);
        assert_ne!(
            calls[1]["tasks"][0]["task"]["task_id"],
            calls[2]["tasks"][0]["task"]["task_id"]
        );
        let directory = fixture
            .root()
            .join("vault/.lorekeeper/queue/result-actions");
        let before: Vec<_> = std::fs::read_dir(&directory)
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                let raw = std::fs::read(&path).unwrap();
                (path, raw)
            })
            .collect();
        let report = fixture.run();
        assert_eq!(fixture.calls(), 3, "{report}");
        assert_eq!(report["halted"], false, "{report}");
        for (path, raw) in before {
            assert_eq!(std::fs::read(path).unwrap(), raw);
        }
    }
}

#[test]
fn legacy_request_action_does_not_suppress_current_singleton_contract() {
    let fixture = Fixture::new("codex");
    let (task, _) = fixture.add("blocked", lk_queue::TaskKind::ExtractConcepts, false);
    std::fs::write(fixture.root().join("mode"), "action").unwrap();
    let report = fixture.run();
    assert_eq!(fixture.calls(), 1, "{report}");
    let raw = std::fs::read_to_string(fixture.root().join("calls.jsonl")).unwrap();
    let mut old_request: Value = serde_json::from_str(raw.lines().next().unwrap()).unwrap();
    old_request["schema"] = json!("native-selected-results-v1");
    let old_revision =
        lk_core::concept::file_revision(&serde_json::to_string(&old_request).unwrap());
    let directory = fixture
        .root()
        .join("vault/.lorekeeper/queue/result-actions");
    let path = std::fs::read_dir(directory)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let mut action: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    action["request_revision"] = json!(old_revision);
    std::fs::write(&path, serde_json::to_vec(&action).unwrap()).unwrap();
    let report = fixture.run();
    assert_eq!(
        fixture.calls(),
        2,
        "the changed native contract requires one fresh singleton: {report}"
    );
    let current: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_ne!(current["request_revision"], old_revision);
    let report = fixture.run();
    assert_eq!(fixture.calls(), 2, "{report}");
    assert!(!fixture.done(&task));
}

#[test]
fn current_complete_frontier_finalizes_without_a_reduction_or_rewriting_paid_nodes() {
    for engine in ["codex", "claude-code"] {
        for kind in [
            lk_queue::TaskKind::ExtractConcepts,
            lk_queue::TaskKind::SynthesizeConcept,
        ] {
            let fixture = Fixture::new(engine);
            if kind == lk_queue::TaskKind::SynthesizeConcept {
                fixture.route_synthesis();
            }
            let (task, plan, observations) = fixture.paid_frontier(kind);
            let id = plan["work_id"].as_str().unwrap();
            let directory = fixture.root().join("vault/.lorekeeper/work").join(id);
            let original_plan = std::fs::read(directory.join("plan.json")).unwrap();
            let nodes = observations
                .iter()
                .map(|output| {
                    let path = directory
                        .join("nodes")
                        .join(format!("{}.json", output["node_id"].as_str().unwrap()));
                    let raw = std::fs::read(&path).unwrap();
                    (path, raw)
                })
                .collect::<Vec<_>>();
            std::fs::write(fixture.root().join("mode"), "frontier").unwrap();
            let report = fixture.run();
            assert_eq!(report["halted"], false, "{engine}/{kind:?}: {report}");
            assert!(fixture.done(&task), "{report}");
            assert_eq!(fixture.calls(), 1, "{report}");
            assert!(directory.join("frontier-seal.json").is_file());
            assert!(!directory.join("seal.json").exists());
            assert_eq!(
                std::fs::read(directory.join("plan.json")).unwrap(),
                original_plan
            );
            for (path, raw) in nodes {
                assert_eq!(std::fs::read(path).unwrap(), raw);
            }
            let request: Value = serde_json::from_slice(
                &std::fs::read(fixture.root().join("last-frontier.json")).unwrap(),
            )
            .unwrap();
            let frontier = &request["frontier"];
            assert_eq!(
                frontier["observations"].as_array().unwrap().len(),
                observations.len()
            );
            for (sent, original) in frontier["observations"]
                .as_array()
                .unwrap()
                .iter()
                .zip(&observations)
            {
                assert_eq!(
                    sent["output"]["source_questions"],
                    original["source_questions"]
                );
            }
            let root = frontier["root_node"].as_str().unwrap();
            assert!(
                !directory
                    .join("nodes")
                    .join(format!("{root}.json"))
                    .exists()
            );
            assert!(serde_json::to_vec(&request).unwrap().len() <= 262144);
        }
    }
}

#[test]
fn frontier_finalization_keeps_exact_action_and_fresh_scope_guards() {
    for mode in ["action", "changed-target", "changed-review"] {
        let fixture = Fixture::new("codex");
        fixture.route_synthesis();
        let (task, plan, _) = fixture.paid_frontier(lk_queue::TaskKind::SynthesizeConcept);
        let directory = fixture
            .root()
            .join("vault/.lorekeeper/work")
            .join(plan["work_id"].as_str().unwrap());
        std::fs::write(
            fixture.root().join("mode"),
            if mode == "action" {
                "frontier-action"
            } else {
                mode
            },
        )
        .unwrap();
        let report = fixture.run();
        assert!(!fixture.done(&task), "{mode}: {report}");
        assert!(!directory.join("frontier-seal.json").exists());
        if mode == "action" {
            assert_eq!(report["halted"], false, "{report}");
            assert!(directory.join("finalization-action.json").is_file());
            let count = fixture.calls();
            let again = fixture.run();
            assert_eq!(again["halted"], false, "{again}");
            assert_eq!(fixture.calls(), count);
        } else {
            assert_eq!(report["halted"], true, "{mode}: {report}");
        }
    }
}
