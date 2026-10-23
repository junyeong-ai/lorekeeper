#![cfg(unix)]

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::SystemTime;

use lk_queue::{QueueLlmClient, QueueTask, SectionResult, SummarizeRequest, TaskKind};
use serde_json::{Value, json};

const HOST: &str = r#"#!/usr/bin/env python3
import json, os, subprocess, sys
from pathlib import Path
root=Path(os.environ['TEST_APPLY_ROOT'])
engine=os.environ['TEST_APPLY_ENGINE']
if sys.argv[1:]==['--version']:
    print('codex-cli 0.163.0' if engine=='codex' else '2.1.309 (Claude Code)')
    raise SystemExit()
prompt='\n'.join(sys.argv[1:])
def cli(*args):
    return json.loads(subprocess.check_output([os.environ['LORE_BIN'], *args]))
def record(value):
    with (root/'calls.jsonl').open('a') as output:
        output.write(json.dumps(value)+'\n')
if 'Selected queue task manifest: ' in prompt:
    selection,_=json.JSONDecoder().raw_decode(prompt.split('Selected queue task manifest: ',1)[1])
    ids=json.loads(Path(selection).read_text())
    rows={row['task_id']:row for row in cli('queue','status','--json')['data']['tasks']}
    tasks=[json.loads(line) for path in (root/'vault/.lorekeeper/queue').glob('*.jsonl') for line in path.read_text().splitlines()]
    selected=[task for task in tasks if task['task_id'] in ids]
    assert len(selected)==len(ids)
    results=root/'vault/.lorekeeper/queue/results'
    results.mkdir(exist_ok=True)
    for task in selected:
        target=task['target']['vault_path']
        record({'role':'queue','task_id':task['task_id'],'target':target})
        assert target=='wiki/documents/b-independent.md', 'rejected or dependent work must not invoke the host'
        result={key:task[key] for key in ['task_id','cache_hash']}
        result.update(target_revision=rows[task['task_id']]['target_revision'],body='Independent source interpretation completed.')
        (results/(task['task_id']+'.section.json')).write_text(json.dumps(result))
else:
    if 'this exact batch: ' in prompt:
        targets,_=json.JSONDecoder().raw_decode(prompt.split('this exact batch: ',1)[1])
    else:
        targets,_=json.JSONDecoder().raw_decode(prompt.split('these exact audit subjects: ',1)[1])
    assert len(targets)==1
    target=targets[0]
    candidate=cli('wiki','audit-plan','--path',target)['candidates'][0]
    if candidate['discovery'] is None:
        request={key:candidate[key] for key in ['path','file_revision']}
        request['queries']=[{'query':candidate['title'],'phrase':True,'spacing':False}]
        path=root/'audit-discovery.json'
        path.write_text(json.dumps(request))
        cli('wiki','audit-discover','--file',str(path))
        candidate=cli('wiki','audit-plan','--path',target)['candidates'][0]
    if 'discover.' in prompt:
        record({'role':'discovery','target':target})
    else:
        assessment={key:candidate[key] for key in ['path','file_revision','input_revision']}
        assessment.update(disposition='reviewed',reason='Read the independent authored note and its exact scoped evidence.',reviews=[])
        path=root/'assessment.json'
        path.write_text(json.dumps({'assessments':[assessment]}))
        cli('wiki','audit-record','--file',str(path))
        record({'role':'audit','target':target})
"#;

struct Fixture {
    directory: tempfile::TempDir,
    config: PathBuf,
    engine: String,
}

impl Fixture {
    fn new(engine: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let agent = root.join("agent");
        std::fs::write(&agent, HOST).unwrap();
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
        let config = root.join("config.yaml");
        std::fs::write(&config, format!(
            "vault:\n  root: vault\n  locale: en\nidentity: {{name: Test, email: test@example.com}}\nsources:\n  notes:\n    type: manual\n    params: {{inbox_dir: inbox}}\n    concept_policy: disabled\nexecution:\n  default_profile: primary\n  work: {{max_input_bytes: 262144, max_result_bytes: 65536, max_calls: 4}}\n  profiles:\n    primary:\n      engine: {engine}\n      executable: {}\n      timeout_seconds: 30\n      work: {{extraction_context: full-context}}\ngraph:\n  audit:\n    batch_size: 1\n",
            serde_json::to_string(&agent).unwrap()
        )).unwrap();
        std::fs::create_dir_all(root.join("vault/inbox")).unwrap();
        let fixture = Self {
            directory,
            config,
            engine: engine.into(),
        };
        fixture.success(&["self", "deploy", "--skills", "none"]);
        fixture
    }

    fn root(&self) -> &Path {
        self.directory.path()
    }

    fn vault(&self) -> PathBuf {
        self.root().join("vault")
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_lore"));
        command
            .arg("--config")
            .arg(&self.config)
            .env("XDG_CONFIG_HOME", self.root().join("config"))
            .env("LORE_INSTALL_DATA_DIR", self.root().join("data"))
            .env("TEST_APPLY_ROOT", self.root())
            .env("TEST_APPLY_ENGINE", &self.engine);
        command
    }

    fn success(&self, arguments: &[&str]) -> Output {
        let output = self.command().args(arguments).output().unwrap();
        assert!(
            output.status.success(),
            "{arguments:?}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn json(&self, arguments: &[&str]) -> Value {
        serde_json::from_slice(&self.success(arguments).stdout).unwrap()
    }

    fn run(&self, operation: &str) -> (Output, Value) {
        let output = self
            .command()
            .args(["run", operation, "--json"])
            .output()
            .unwrap();
        let report = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "{error}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (output, report)
    }

    fn write(&self, path: &str, text: &str) -> PathBuf {
        let path = self.vault().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path
    }

    fn add_summary(&self, name: &str) -> QueueTask {
        let client = QueueLlmClient::new(self.vault().join(".lorekeeper/queue"));
        let task = client.build_task(
            name,
            TaskKind::Summarize,
            SummarizeRequest {
                text: format!("Source observations for {name} retain their conditions."),
                max_sentences: 3,
                focus: None,
                locale: lk_core::i18n::Locale::En,
                source_type: Some(lk_core::config::SourceType::Manual),
                target: lk_queue::TaskTarget {
                    vault_path: format!("wiki/documents/{name}.md"),
                    kind: lk_queue::TargetKind::DocumentSummary,
                    anchor: "## Summary".into(),
                },
            },
        );
        self.write(
            &task.target.vault_path,
            &format!(
                "---\ntype: document\ntitle: {name}\nllm_inputs:\n  summary: {}\n---\n# {name}\n\n## Summary\n\n## Content\nOriginal source observations.\n\n## Related Concepts\n\n",
                task.cache_hash
            ),
        );
        self.write(
            &format!(".lorekeeper/queue/{name}.jsonl"),
            &(serde_json::to_string(&task).unwrap() + "\n"),
        );
        task
    }

    fn stage(&self, task: &QueueTask, revision: &str, body: &str) -> PathBuf {
        let result = SectionResult {
            task_id: task.task_id.clone(),
            cache_hash: task.cache_hash.clone(),
            target_revision: revision.into(),
            body: body.into(),
        };
        self.write(
            &format!(".lorekeeper/queue/results/{}.section.json", task.task_id),
            &serde_json::to_string(&result).unwrap(),
        )
    }

    fn target_revision(&self, task: &QueueTask) -> String {
        lk_core::concept::file_revision(
            &std::fs::read_to_string(self.vault().join(&task.target.vault_path)).unwrap(),
        )
    }

    fn seal_summary(&self, task: &QueueTask, body: &str) -> PathBuf {
        let plan = self.json(&["work", "plan", "--task-id", &task.task_id]);
        assert_eq!(plan["parts"], 1);
        let id = plan["work_id"].as_str().unwrap();
        let request = self.json(&["work", "read", id, plan["ready"][0].as_str().unwrap()]);
        let observation = self.root().join("observation.json");
        std::fs::write(
            &observation,
            serde_json::to_vec(&json!({
                "work_id":request["work_id"],"node_id":request["node_id"],
                "input_revision":request["input_revision"],
                "summary":"Recorded the assigned retained evidence.",
                "findings":[],"source_questions":[],"unresolved":[],"resolutions":[]
            }))
            .unwrap(),
        )
        .unwrap();
        self.success(&["work", "record", "--file", observation.to_str().unwrap()]);
        let result = self.root().join("answer.json");
        std::fs::write(
            &result,
            serde_json::to_vec(&SectionResult {
                task_id: task.task_id.clone(),
                cache_hash: task.cache_hash.clone(),
                target_revision: self.target_revision(task),
                body: body.into(),
            })
            .unwrap(),
        )
        .unwrap();
        let sealed = self.json(&["work", "finish", id, "--file", result.to_str().unwrap()]);
        self.vault().join(sealed["staged_result"].as_str().unwrap())
    }

    fn is_completed(&self, task: &QueueTask) -> bool {
        let raw = std::fs::read_to_string(self.vault().join(&task.target.vault_path)).unwrap();
        let page = lk_vault::parse_page(&raw).unwrap();
        lk_pipeline::completed_input(&page, task.target.kind) == Some(task.cache_hash.as_str())
    }

    fn calls(&self) -> Vec<Value> {
        let path = self.root().join("calls.jsonl");
        match std::fs::read_to_string(path) {
            Ok(raw) => raw
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => panic!("{error}"),
        }
    }

    fn add_dependent_synthesis(&self, source: &QueueTask) {
        let path = self.vault().join(&source.target.vault_path);
        let raw = std::fs::read_to_string(&path).unwrap()
            + "- [Dependent synthesis](../concepts/c-dependent.md)\n";
        std::fs::write(path, raw).unwrap();
        self.write(
            "wiki/concepts/c-dependent.md",
            "---\ntype: concept\ntitle: Dependent synthesis\nllm_inputs:\n  synthesis: previous\n---\n## Synthesis\nEstablished source-scoped interpretation.\n\n## Sources\n\n## Related\n",
        );
        self.success(&["graph", "backlinks-sync"]);
        assert!(
            self.json(&["queue", "status", "--json"])["data"]["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["kind"] == "synthesize-concept" && row["status"] == "current")
        );
    }

    fn add_authored_audit(&self, name: &str) {
        let path = format!("wiki/documents/{name}.md");
        let title = format!("Independent authored operational note {name}");
        let raw = format!(
            "---\ntype: document\ntitle: {title}\n---\nAn authored note explains a separate operating procedure.\n"
        );
        self.write(&path, &raw);
        let discovery = self.root().join("discovery.json");
        std::fs::write(
            &discovery,
            serde_json::to_vec(&json!({
                "path":path,"file_revision":lk_core::concept::file_revision(&raw),
                "queries":[{"query":title,"phrase":true,"spacing":false}]
            }))
            .unwrap(),
        )
        .unwrap();
        self.success(&[
            "wiki",
            "audit-discover",
            "--file",
            discovery.to_str().unwrap(),
        ]);
    }

    fn snapshot(&self) -> BTreeMap<PathBuf, (Vec<u8>, SystemTime)> {
        walkdir::WalkDir::new(self.vault())
            .into_iter()
            .map(Result::unwrap)
            .filter(|entry| entry.file_type().is_file())
            .map(|entry| {
                let path = entry.path();
                (
                    path.strip_prefix(self.vault()).unwrap().to_owned(),
                    (
                        std::fs::read(path).unwrap(),
                        path.metadata().unwrap().modified().unwrap(),
                    ),
                )
            })
            .collect()
    }
}

#[test]
fn rejected_target_preserves_its_answer_and_allows_independent_processing() {
    for engine in ["codex", "claude-code"] {
        let fixture = Fixture::new(engine);
        let rejected = fixture.add_summary("a-rejected");
        let independent = fixture.add_summary("b-independent");
        fixture.add_dependent_synthesis(&rejected);
        let result = fixture.stage(
            &rejected,
            &lk_core::concept::file_revision("earlier target bytes"),
            "A previously computed interpretation.",
        );
        let original = std::fs::read(&result).unwrap();
        let target = fixture.vault().join(&rejected.target.vault_path);
        let target_before = std::fs::read(&target).unwrap();
        let (output, report) = fixture.run("process");
        assert!(!output.status.success(), "{report}");
        assert!(
            fixture.is_completed(&independent),
            "{report}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!fixture.is_completed(&rejected));
        assert_eq!(std::fs::read(&result).unwrap(), original);
        assert_eq!(std::fs::read(&target).unwrap(), target_before);
        assert!(
            fixture
                .calls()
                .iter()
                .all(|call| call["target"] == independent.target.vault_path)
        );
        let status = fixture.json(&["queue", "status", "--json"]);
        assert!(
            status["data"]["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| { row["task_id"] == rejected.task_id && row["status"] == "current" }),
            "{status}"
        );
        assert!(
            status["data"]["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| { row["kind"] == "synthesize-concept" && row["status"] == "current" }),
            "{status}"
        );
        let calls = fixture.calls();
        let (output, report) = fixture.run("process");
        assert!(!output.status.success(), "{report}");
        assert_eq!(report["semantic_calls"], 0, "{report}");
        assert_eq!(fixture.calls(), calls);
        assert_eq!(std::fs::read(result).unwrap(), original);
        assert_eq!(std::fs::read(target).unwrap(), target_before);
    }
}

#[test]
fn corrected_staged_answer_reconsiders_rejection_without_invoking_a_host() {
    let fixture = Fixture::new("codex");
    let task = fixture.add_summary("a-rejected");
    let path = fixture.stage(
        &task,
        &lk_core::concept::file_revision("old"),
        "Old interpretation.",
    );
    let (output, report) = fixture.run("process");
    assert!(!output.status.success(), "{report}");
    let calls = fixture.calls();
    fixture.stage(
        &task,
        &fixture.target_revision(&task),
        "Explicitly corrected current interpretation.",
    );
    let (output, report) = fixture.run("process");
    assert!(
        output.status.success(),
        "{report}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(fixture.is_completed(&task));
    assert!(!path.exists());
    assert_eq!(fixture.calls(), calls);
    assert_eq!(report["semantic_calls"], 0);
}

#[test]
fn deterministic_link_repair_retries_preserved_answer_without_a_host() {
    let fixture = Fixture::new("codex");
    let task = fixture.add_summary("a-rejected");
    let result = fixture.seal_summary(&task, "Read [the supporting note](support.md).");
    let original = std::fs::read(&result).unwrap();
    let before = fixture.snapshot();
    let preview = fixture
        .command()
        .args(["queue", "apply", "--dry-run"])
        .output()
        .unwrap();
    assert!(!preview.status.success());
    assert_eq!(fixture.snapshot(), before);
    let (output, report) = fixture.run("process");
    assert!(!output.status.success(), "{report}");
    assert_eq!(std::fs::read(&result).unwrap(), original);
    assert_eq!(report["semantic_calls"], 0, "{report}");
    assert!(fixture.calls().is_empty());
    let retained_work = |snapshot: BTreeMap<PathBuf, (Vec<u8>, SystemTime)>| {
        snapshot
            .into_iter()
            .filter(|(path, _)| path.starts_with(".lorekeeper/work"))
            .collect::<BTreeMap<_, _>>()
    };
    assert_eq!(retained_work(fixture.snapshot()), retained_work(before));
    let calls = fixture.calls();
    fixture.write(
        "wiki/documents/support.md",
        "---\ntype: document\ntitle: Supporting note\n---\nA supporting observation.\n",
    );
    let (output, report) = fixture.run("process");
    assert!(
        output.status.success(),
        "{report}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(fixture.is_completed(&task));
    assert_eq!(fixture.calls(), calls);
    assert_eq!(report["semantic_calls"], 0);
}

#[test]
fn rejected_directory_alias_link_never_publishes_a_corrupt_receipt() {
    let fixture = Fixture::new("codex");
    let rejected = fixture.add_summary("a-rejected");
    let independent = fixture.add_summary("b-independent");
    let metadata = fixture.vault().join("metadata");
    std::fs::create_dir(&metadata).unwrap();
    std::os::unix::fs::symlink(fixture.vault(), metadata.join("root-alias.md")).unwrap();
    let result = fixture.stage(
        &rejected,
        &fixture.target_revision(&rejected),
        "Read [the vault directory](/metadata/root-alias.md).",
    );
    let original = std::fs::read(&result).unwrap();
    let target = fixture.vault().join(&rejected.target.vault_path);
    let target_before = std::fs::read(&target).unwrap();
    let (output, report) = fixture.run("process");
    assert!(!output.status.success(), "{report}");
    assert!(
        fixture.is_completed(&independent),
        "{report}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!fixture.is_completed(&rejected));
    assert_eq!(std::fs::read(&result).unwrap(), original);
    assert_eq!(std::fs::read(&target).unwrap(), target_before);
    let calls = fixture.calls();
    assert!(
        calls
            .iter()
            .all(|call| call["target"] == independent.target.vault_path)
    );
    let (output, report) = fixture.run("process");
    assert!(!output.status.success(), "{report}");
    assert_eq!(report["semantic_calls"], 0, "{report}");
    assert_eq!(fixture.calls(), calls);
    let preview = fixture
        .command()
        .args(["queue", "apply", "--json", "--dry-run"])
        .output()
        .unwrap();
    assert!(!preview.status.success());
    let report: Value = serde_json::from_slice(&preview.stdout).unwrap();
    assert!(report["fatal"].is_null(), "{report}");
    assert_eq!(report["rejected"], 1, "{report}");
    let owners = report["groups"][0]["scope"]["owners"].as_array().unwrap();
    assert!(
        owners
            .iter()
            .all(|owner| owner.as_str().is_some_and(|owner| !owner.is_empty()))
    );
    assert_eq!(std::fs::read(result).unwrap(), original);
    assert_eq!(std::fs::read(target).unwrap(), target_before);
}

#[test]
fn corrupt_rejection_receipt_stops_before_independent_work() {
    for changed_identity in [false, true] {
        let fixture = Fixture::new("codex");
        let rejected = fixture.add_summary("a-rejected");
        let result = fixture.stage(
            &rejected,
            &lk_core::concept::file_revision("old"),
            "Retained interpretation.",
        );
        let original = std::fs::read(&result).unwrap();
        let (output, report) = fixture.run("process");
        assert!(!output.status.success(), "{report}");
        let receipts = std::fs::read_dir(fixture.vault().join(".lorekeeper/queue/rejections"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .collect::<Vec<_>>();
        assert_eq!(receipts.len(), 1);
        let receipt = &receipts[0];
        let mut value: Value = serde_json::from_slice(&std::fs::read(receipt).unwrap()).unwrap();
        assert_eq!(value["schema"], "queue-application-rejection-v1");
        let corrupt = if changed_identity {
            value["identity"]["task_id"] = json!("different-retained-request");
            serde_json::to_string(&value).unwrap()
        } else {
            "{}".into()
        };
        std::fs::write(receipt, &corrupt).unwrap();
        let independent = fixture.add_summary("b-independent");
        let calls = fixture.calls();
        let (output, report) = fixture.run("process");
        assert!(!output.status.success(), "{report}");
        assert_eq!(report["semantic_calls"], 0, "{report}");
        assert_eq!(fixture.calls(), calls);
        assert!(!fixture.is_completed(&independent));
        assert_eq!(std::fs::read(receipt).unwrap(), corrupt.as_bytes());
        assert_eq!(std::fs::read(result).unwrap(), original);
    }
}

#[test]
fn unknown_staged_work_stops_before_independent_host_execution() {
    for malformed in [false, true] {
        let fixture = Fixture::new("codex");
        let task = fixture.add_summary("b-independent");
        let unknown = if malformed {
            fixture.write(".lorekeeper/queue/results/unknown.json", "{unreadable");
            fixture
                .vault()
                .join(".lorekeeper/queue/results/unknown.json")
        } else {
            let mut orphan = task.clone();
            orphan.task_id = "missing-retained-request".into();
            fixture.stage(
                &orphan,
                &fixture.target_revision(&task),
                "Unmatched result.",
            )
        };
        let original = std::fs::read(unknown).unwrap();
        let (output, report) = fixture.run("process");
        assert!(!output.status.success(), "{report}");
        assert_eq!(report["semantic_calls"], 0, "{report}");
        assert!(!fixture.is_completed(&task));
        assert!(fixture.calls().is_empty());
        assert!(
            fixture
                .snapshot()
                .values()
                .any(|(bytes, _)| bytes == &original)
        );
    }
}

#[test]
fn unknown_extraction_preflight_precedes_valid_section_publication() {
    for malformed in [false, true] {
        let fixture = Fixture::new("codex");
        let task = fixture.add_summary("b-independent");
        let section = fixture.stage(
            &task,
            &fixture.target_revision(&task),
            "An already staged independent interpretation.",
        );
        let unknown = if malformed {
            "{unreadable".into()
        } else {
            serde_json::to_string(&lk_queue::TaskResult {
                selection: None,
                task_id: "missing-extraction-request".into(),
                cache_hash: "a".repeat(32),
                target_revision: "b".repeat(64),
                context_revision: "c".repeat(64),
                target: lk_queue::TaskTarget {
                    vault_path: "wiki/documents/missing-source.md".into(),
                    kind: lk_queue::TargetKind::DocumentConcepts,
                    anchor: "## Related Concepts".into(),
                },
                date: jiff::civil::date(2026, 10, 23),
                concept_policy: lk_core::config::ConceptPolicy::SupportedNew,
                discovery: Vec::new(),
                concepts: Vec::new(),
            })
            .unwrap()
        };
        fixture.write(".lorekeeper/queue/results/unknown.json", &unknown);
        let target = fixture.vault().join(&task.target.vault_path);
        let before = std::fs::read(&target).unwrap();
        let answer = std::fs::read(&section).unwrap();
        let output = fixture
            .command()
            .args(["queue", "apply", "--json"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(!report["fatal"].is_null(), "{report}");
        assert_eq!(report["applied"], 0, "{report}");
        assert_eq!(std::fs::read(target).unwrap(), before);
        assert_eq!(std::fs::read(section).unwrap(), answer);
        assert!(!fixture.is_completed(&task));
        assert!(
            fixture
                .snapshot()
                .values()
                .any(|(bytes, _)| bytes == unknown.as_bytes())
        );
        assert!(fixture.calls().is_empty());
    }
}

#[test]
fn independent_authored_audit_progresses_despite_rejected_publication() {
    let fixture = Fixture::new("codex");
    let task = fixture.add_summary("a-rejected");
    fixture.add_authored_audit("z-authored");
    let result = fixture.stage(
        &task,
        &lk_core::concept::file_revision("old"),
        "Retained interpretation.",
    );
    let original = std::fs::read(&result).unwrap();
    let (output, report) = fixture.run("knowledge");
    assert!(!output.status.success(), "{report}");
    assert!(
        fixture.calls().iter().any(|call| call["role"] == "audit"),
        "{report}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fixture
            .calls()
            .iter()
            .all(|call| call["target"] == "wiki/documents/z-authored.md")
    );
    let plan = fixture.json(&[
        "wiki",
        "audit-plan",
        "--path",
        "wiki/documents/z-authored.md",
    ]);
    assert_eq!(plan["reviewed_current"], 1, "{plan}");
    assert!(!fixture.is_completed(&task));
    assert_eq!(std::fs::read(result).unwrap(), original);
}

#[test]
fn knowledge_campaign_continues_completed_audits_while_rejection_remains() {
    let fixture = Fixture::new("codex");
    let task = fixture.add_summary("a-rejected");
    fixture.add_authored_audit("y-authored");
    fixture.add_authored_audit("z-authored");
    let result = fixture.stage(
        &task,
        &lk_core::concept::file_revision("old"),
        "Retained interpretation.",
    );
    let original = std::fs::read(&result).unwrap();
    let output = fixture
        .command()
        .args(["run", "knowledge", "--batches", "2", "--json"])
        .output()
        .unwrap();
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(!output.status.success(), "{report}");
    assert_eq!(report["ok"], false);
    assert_eq!(report["stop_reason"], "batch-limit", "{report}");
    let runs = report["runs"].as_array().unwrap();
    assert_eq!(
        runs.len(),
        2,
        "{report}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut assessed = std::collections::BTreeSet::new();
    let mut calls = 0;
    for run in runs {
        assert_eq!(run["ok"], false, "{run}");
        let audit = run["audit"].as_array().unwrap();
        assert_eq!(audit.len(), 1, "{run}");
        assessed.insert(audit[0]["assessed_path"].as_str().unwrap());
        let count = run["semantic_calls"].as_u64().unwrap();
        assert!(count <= 4, "{run}");
        calls += count;
    }
    assert_eq!(
        assessed,
        std::collections::BTreeSet::from([
            "wiki/documents/y-authored.md",
            "wiki/documents/z-authored.md"
        ])
    );
    assert!(calls <= 8);
    assert_eq!(fixture.calls().len() as u64, calls);
    assert!(!fixture.is_completed(&task));
    assert_eq!(std::fs::read(result).unwrap(), original);
}
