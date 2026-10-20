#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

struct Fixture {
    directory: tempfile::TempDir,
    config: PathBuf,
}

impl Fixture {
    fn new(engine: &str, timeout: u64, behavior: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::create_dir_all(root.join("vault/inbox")).unwrap();
        let agent = root.join("agent executable");
        let version = if engine == "codex" {
            "codex-cli 0.163.0"
        } else {
            "2.1.303 (Claude Code)"
        };
        std::fs::write(&agent, format!("#!/bin/sh\nif [ \"$1\" = --version ]; then printf '%s\\n' '{version}'; exit; fi\nprintf '%s\\n' \"$@\" > \"$TEST_EXECUTION_ROOT/arguments\"\n{behavior}\n")).unwrap();
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
        let config = root.join("config.yaml");
        std::fs::write(&config, format!("vault:\n  root: vault\nidentity:\n  name: Test\n  email: test@example.com\nsources:\n  notes:\n    type: manual\n    params: {{inbox_dir: inbox}}\n    labels: [knowledge]\n    concept_policy: supported-new\nexecution:\n  default_profile: primary\n  profiles:\n    primary:\n      engine: {engine}\n      executable: {}\n      timeout_seconds: {timeout}\n", serde_json::to_string(&agent).unwrap())).unwrap();
        let fixture = Self { directory, config };
        let result = fixture
            .command()
            .args(["self", "deploy", "--skills", "none"])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
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
            .env("TEST_EXECUTION_ROOT", self.root());
        command
    }

    fn ingest(&self) {
        std::fs::write(self.root().join("vault/inbox/source.md"), "# Durable evidence\n\nA transactional outbox commits an event and its state change in one transaction.\n").unwrap();
        let result = self.command().arg("ingest").output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }

    fn run(&self, operation: &str) -> (std::process::Output, serde_json::Value) {
        let result = self
            .command()
            .args(["run", operation, "--json"])
            .output()
            .unwrap();
        let json = serde_json::from_slice(&result.stdout).unwrap_or_else(|e| {
            panic!(
                "{e}: {}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            )
        });
        (result, json)
    }
}

const RECORD_FIRST_AUDIT: &str = r#"
"$LORE_BIN" wiki audit-plan > "$TEST_EXECUTION_ROOT/plan.json" || exit 31
python3 - <<'ASSESS'
import json, os
from pathlib import Path
root=Path(os.environ['TEST_EXECUTION_ROOT'])
candidate=json.loads((root/'plan.json').read_text())['candidates'][0]
assessment={key:candidate[key] for key in ['path','file_revision','input_revision']}
assessment.update(disposition='reviewed', reason='Read the exact selected concept and its scoped evidence.', reviews=[])
(root/'assessment.json').write_text(json.dumps({'assessments':[assessment]}))
ASSESS
"$LORE_BIN" wiki audit-record --file "$TEST_EXECUTION_ROOT/assessment.json" || exit 32
"#;

fn write_audit_concept(fixture: &Fixture, name: &str) {
    let concepts = fixture.root().join("vault/wiki/concepts");
    std::fs::create_dir_all(&concepts).unwrap();
    let template = include_str!("behaviour/corpus/small/vault/wiki/concepts/unlinked-note.md");
    std::fs::write(
        concepts.join(format!("{name}.md")),
        template
            .replace("Unlinked note", name)
            .replace("unlinked-note", name),
    )
    .unwrap();
}

fn write_review_attention(fixture: &Fixture, subject: &str, invalid: bool) {
    write_review_attention_at(fixture, subject, invalid, "wiki/documents/reviewed.md");
}

fn write_review_attention_at(fixture: &Fixture, subject: &str, invalid: bool, evidence: &str) {
    let vault = fixture.root().join("vault");
    std::fs::create_dir_all(vault.join(evidence).parent().unwrap()).unwrap();
    let prior = "---\ntype: document\ntitle: Evidence\n---\nOriginal evidence.\n";
    std::fs::write(vault.join(evidence), prior).unwrap();
    let revision = lk_core::concept::source_revision(&lk_vault::parse_page(prior).unwrap());
    if invalid {
        std::fs::remove_file(vault.join(evidence)).unwrap();
    } else {
        lk_vault::archive_evidence(&vault, &[evidence.into()]).unwrap();
        std::fs::write(vault.join(evidence), prior.replace("Original", "Changed")).unwrap();
    }
    let review = serde_json::json!({
        "status":"open", "subjects":[subject],
        "evidence":[{"path":evidence,"revision":revision}], "resolution":null,
    });
    std::fs::create_dir_all(vault.join("wiki/explorations")).unwrap();
    std::fs::write(
        vault.join("wiki/explorations/review.md"),
        format!("---\ntype: exploration\ntitle: Scoped review\nreview: {review}\n---\nExisting scoped judgment.\n"),
    )
    .unwrap();
}

#[test]
fn empty_work_does_not_launch_or_require_an_agent() {
    let fixture = Fixture::new("codex", 30, "exit 91");
    std::fs::remove_file(fixture.root().join("agent executable")).unwrap();
    let (output, result) = fixture.run("process");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(result["ok"], true);
    assert!(result["execution"].is_null());
    for path in [
        "vault/wiki/AGENTS.md",
        "vault/wiki/index.md",
        "vault/.lorekeeper/search.sqlite",
    ] {
        assert!(fixture.root().join(path).is_file(), "missing {path}");
    }
    assert!(!fixture.root().join("arguments").exists());
}

#[test]
fn both_hosts_receive_distinct_contracts_and_exit_zero_does_not_prove_completion() {
    for engine in ["claude-code", "codex"] {
        let fixture = Fixture::new(engine, 30, "exit 0");
        fixture.ingest();
        let (output, result) = fixture.run("process");
        assert!(!output.status.success());
        assert_eq!(result["ok"], false);
        assert_eq!(result["execution"]["engine"], engine);
        let arguments = std::fs::read_to_string(fixture.root().join("arguments")).unwrap();
        assert!(!arguments.contains("bounded knowledge maintenance"));
        if engine == "codex" {
            assert!(arguments.starts_with("exec\n"));
            assert!(arguments.contains("--sandbox\nworkspace-write\n"));
            assert!(arguments.contains("--ephemeral\n"));
            assert!(!arguments.contains("--allowedTools"));
        } else {
            assert!(arguments.starts_with("-p\n/lore-process:lore-process selected."));
            assert!(arguments.contains("--permission-prompts\nnone\n"));
        }
        let pending = fixture.command().args(["queue", "count"]).output().unwrap();
        assert!(
            String::from_utf8(pending.stdout)
                .unwrap()
                .trim()
                .parse::<usize>()
                .unwrap()
                > 0
        );
    }
}

#[test]
fn knowledge_queue_consumer_receives_bounded_maintenance_context() {
    let fixture = Fixture::new("codex", 30, "exit 0");
    fixture.ingest();
    let (output, result) = fixture.run("knowledge");
    assert!(
        !output.status.success(),
        "the mock leaves current queue work unfinished"
    );
    assert_eq!(result["ok"], false);
    let arguments = std::fs::read_to_string(fixture.root().join("arguments")).unwrap();
    assert!(arguments.contains("bounded knowledge maintenance job"));
    assert!(arguments.contains("Read the selected work's review dependencies"));
    assert!(arguments.contains("native controller owns final reconciliation"));
    assert!(
        result["stages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|stage| stage["name"] == "queue completion" && stage["state"] == "failed"),
        "native finalization must reject the mock's unpublished work: {result}"
    );
}

#[test]
fn daily_preserves_collection_when_the_selected_agent_is_unavailable() {
    let fixture = Fixture::new("codex", 30, "exit 0");
    std::fs::remove_file(fixture.root().join("agent executable")).unwrap();
    std::fs::write(
        fixture.root().join("vault/inbox/source.md"),
        "# Source\n\nOriginal evidence must survive an authentication or execution failure.\n",
    )
    .unwrap();
    let (output, result) = fixture.run("daily");
    assert!(!output.status.success());
    assert_eq!(result["stages"][0]["name"], "ingest");
    assert_eq!(result["stages"][0]["state"], "completed");
    assert!(
        fixture
            .root()
            .join("vault")
            .join(lk_core::manual_original::ORIGINAL_STORE)
            .is_dir()
    );
    assert!(
        result["stages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|stage| stage["name"] == "wiki refresh" && stage["state"] == "completed")
    );
}

#[test]
fn missing_packaged_dependency_refuses_before_launch() {
    let fixture = Fixture::new("claude-code", 30, "exit 0");
    fixture.ingest();
    std::fs::remove_file(
        fixture
            .root()
            .join("data/skills/claude-code/lore-process/SKILL.md"),
    )
    .unwrap();
    let (output, result) = fixture.run("process");
    assert!(!output.status.success());
    assert_eq!(result["ok"], false);
    assert!(!fixture.root().join("arguments").exists());
}

#[test]
fn configuration_drift_cannot_switch_vaults_or_report_the_original_queue_complete() {
    let fixture = Fixture::new(
        "codex",
        30,
        r#"
python3 - <<'EDIT'
import os
from pathlib import Path
p=Path(os.environ['LORE_CONFIG'])
p.write_text(p.read_text().replace('root: vault\n', 'root: other-vault\n', 1))
EDIT
"$LORE_BIN" schema > "$TEST_EXECUTION_ROOT/child-out" 2> "$TEST_EXECUTION_ROOT/child-error"
printf '%s' "$?" > "$TEST_EXECUTION_ROOT/child-status"
exit 0
"#,
    );
    fixture.ingest();
    let original = std::fs::read_to_string(&fixture.config).unwrap();
    std::fs::create_dir_all(fixture.root().join("other-vault/wiki")).unwrap();
    let (output, result) = fixture.run("process");
    assert!(!output.status.success(), "{result}");
    assert_eq!(result["ok"], false);
    assert!(
        result["stages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|stage| stage["name"] == "configuration" && stage["state"] == "failed")
    );
    assert_ne!(
        std::fs::read_to_string(fixture.root().join("child-status")).unwrap(),
        "0"
    );
    assert!(
        std::fs::read_to_string(fixture.root().join("child-error"))
            .unwrap()
            .contains("configuration changed")
    );
    assert!(!fixture.root().join("other-vault/wiki/AGENTS.md").exists());
    assert!(!fixture.root().join("other-vault/wiki/index.md").exists());
    std::fs::write(&fixture.config, original).unwrap();
    let pending = fixture.command().args(["queue", "count"]).output().unwrap();
    assert_eq!(String::from_utf8(pending.stdout).unwrap().trim(), "2");
    assert!(
        fixture
            .command()
            .args(["workflow", "--", "/usr/bin/true"])
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn audit_exit_zero_without_assessments_is_incomplete() {
    let fixture = Fixture::new("codex", 30, "exit 0");
    let concepts = fixture.root().join("vault/wiki/concepts");
    std::fs::create_dir_all(&concepts).unwrap();
    std::fs::write(
        concepts.join("unverified.md"),
        include_str!("behaviour/corpus/small/vault/wiki/concepts/unlinked-note.md"),
    )
    .unwrap();
    let (output, result) = fixture.run("knowledge");
    assert!(!output.status.success());
    assert_eq!(result["ok"], false);
    assert!(
        result["stages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|stage| stage["name"] == "audit completion" && stage["state"] == "failed"),
        "{}\n{}",
        result,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(fixture.root().join("arguments").exists());
    assert!(
        !fixture
            .root()
            .join("vault/.lorekeeper/knowledge-audit.json")
            .exists()
    );
}

#[test]
fn unrelated_review_drift_remains_visible_while_knowledge_campaign_progresses() {
    let fixture = Fixture::new("codex", 30, RECORD_FIRST_AUDIT);
    let config =
        std::fs::read_to_string(&fixture.config).unwrap() + "graph:\n  audit:\n    batch_size: 1\n";
    std::fs::write(&fixture.config, config).unwrap();
    for name in ["a", "b"] {
        write_audit_concept(&fixture, name);
    }
    write_review_attention(&fixture, "wiki/documents/reviewed.md", false);
    let output = fixture
        .command()
        .args(["run", "knowledge", "--batches", "2", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["ok"], true);
    assert_eq!(result["stop_reason"], "batch-limit");
    let runs = result["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 2);
    for run in runs {
        assert_eq!(
            run["review_health"],
            serde_json::json!({
                "open":0,"resolved":0,"needs_reassessment":1,"invalid":0,
            })
        );
        assert_eq!(run["audit"].as_array().unwrap().len(), 1);
    }
    assert_ne!(
        runs[0]["audit"][0]["requested_path"],
        runs[1]["audit"][0]["requested_path"]
    );
    let strict = fixture
        .command()
        .args(["wiki", "reviews", "--check"])
        .output()
        .unwrap();
    assert!(
        !strict.status.success(),
        "the standalone whole-vault health gate stays strict"
    );
    let reports: serde_json::Value = serde_json::from_slice(&strict.stdout).unwrap();
    assert_eq!(reports[0]["effective_state"], "needs-reassessment");
}

#[test]
fn empty_knowledge_plan_still_reports_review_attention_without_launching_agent() {
    let fixture = Fixture::new("codex", 30, "exit 91");
    write_review_attention_at(&fixture, "daily/reviewed.md", false, "daily/reviewed.md");
    let (output, result) = fixture.run("knowledge");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(result["review_health"]["needs_reassessment"], 1);
    assert_eq!(result["review_health"]["invalid"], 0);
    assert_eq!(result["audit"], serde_json::json!([]));
    assert!(result["execution"].is_null());
    assert!(!fixture.root().join("arguments").exists());
}

#[test]
fn invalid_review_evidence_blocks_knowledge_before_launching_agent() {
    let fixture = Fixture::new("codex", 30, RECORD_FIRST_AUDIT);
    write_audit_concept(&fixture, "a");
    write_review_attention(&fixture, "wiki/documents/reviewed.md", true);
    let (output, result) = fixture.run("knowledge");
    assert!(!output.status.success());
    assert_eq!(result["review_health"]["invalid"], 1);
    assert_eq!(result["review_health"]["needs_reassessment"], 0);
    assert!(result["execution"].is_null());
    assert!(result["audit"].is_null());
    assert!(
        result["stages"].as_array().unwrap().iter().any(|stage| {
            stage["name"] == "knowledge review health" && stage["state"] == "failed"
        })
    );
    assert!(!fixture.root().join("arguments").exists());
}

#[test]
fn selected_review_drift_blocks_audit_completion_without_becoming_a_global_health_failure() {
    let fixture = Fixture::new("codex", 30, RECORD_FIRST_AUDIT);
    write_audit_concept(&fixture, "a");
    write_review_attention(&fixture, "wiki/concepts/a.md", false);
    let (output, result) = fixture.run("knowledge");
    assert!(!output.status.success());
    assert_eq!(result["review_health"]["needs_reassessment"], 1);
    assert_eq!(result["review_health"]["invalid"], 0);
    assert!(result["audit"].is_null());
    let stages = result["stages"].as_array().unwrap();
    assert!(
        stages
            .iter()
            .any(|stage| { stage["name"] == "audit completion" && stage["state"] == "failed" })
    );
    assert!(
        stages
            .iter()
            .filter(|stage| stage["name"] == "knowledge review health")
            .all(|stage| stage["state"] == "completed")
    );
    let plan = fixture
        .command()
        .args(["wiki", "audit-plan"])
        .output()
        .unwrap();
    assert!(plan.status.success());
    let plan: serde_json::Value = serde_json::from_slice(&plan.stdout).unwrap();
    assert_eq!(plan["reviewed_current"], 0);
    assert_eq!(plan["unreviewed"], 2);
    assert_eq!(plan["candidates"].as_array().unwrap().len(), 2);
    for kind in ["concept", "document"] {
        assert_eq!(plan["counts_by_kind"][kind]["total"], 1);
        assert_eq!(plan["counts_by_kind"][kind]["unreviewed"], 1);
        assert_eq!(plan["counts_by_kind"][kind]["reviewed_current"], 0);
    }
    let subjects = plan["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|candidate| {
            (
                candidate["kind"].as_str().unwrap(),
                candidate["path"].as_str().unwrap(),
            )
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        subjects,
        std::collections::BTreeSet::from([
            ("concept", "wiki/concepts/a.md"),
            ("document", "wiki/documents/reviewed.md"),
        ])
    );
}

const RECORD_BOUNDED_AUDIT: &str = r#"
case "$*" in
  *--output-schema*|*--safe-mode*) pwd > "$TEST_EXECUTION_ROOT/observation-cwd"; /bin/cat > "$TEST_EXECUTION_ROOT/native-request.txt" ;;
esac
exec python3 - "$@" <<'BOUNDED'
import json, os, re, subprocess, sys
from pathlib import Path
root=Path(os.environ['TEST_EXECUTION_ROOT'])
native='--output-schema' in sys.argv or '--safe-mode' in sys.argv
if native:
    request=json.loads((root/'native-request.txt').read_text().split('\nNative request:\n',1)[1])
    action,work_id,node_id='inspect',request['work_id'],request['node_id']
else:
    match=re.search(r'bounded (inspect|finish) ([0-9a-f]{64})(?: ([0-9a-f]{64}))?', '\n'.join(sys.argv[1:]))
    if match is None:
        raise SystemExit('expected an exact bounded native request')
    action, work_id, node_id=match.groups()
def cli(*args):
    result=subprocess.run([os.environ['LORE_BIN'], *args], capture_output=True, text=True)
    if result.returncode:
        print(result.stdout, file=sys.stderr)
        print(result.stderr, file=sys.stderr)
        raise SystemExit(result.returncode)
    return json.loads(result.stdout)
if action=='inspect':
    if not native:
        request=cli('work', 'read', work_id, node_id)
    row={'action':action, 'work_id':work_id, 'node_id':node_id,
         'kind':request['kind'], 'pid':os.getpid(), 'input_bytes':len(json.dumps(request, ensure_ascii=False, separators=(',', ':')).encode())}
    if request['kind']=='inspect':
        assert all('text' in item for item in request['input']['ranges'])
    else:
        assert len(request['children'])==2
    unresolved=[concern for child in request.get('children', []) for concern in child['output']['unresolved']]
    output={'work_id':work_id, 'node_id':node_id, 'input_revision':request['input_revision'],
            'summary':'Fixture observation of this exact request; structural coverage is not factual verification.',
            'findings':[], 'unresolved':unresolved, 'resolutions':[]}
    if native:
        if '--safe-mode' in sys.argv:
            print(json.dumps({'type':'result','subtype':'success','is_error':False,'structured_output':output}))
        else:
            print(json.dumps(output))
    else:
        path=root/'observation.json'
        path.write_text(json.dumps(output))
        cli('work', 'record', '--file', str(path))
else:
    request=cli('work', 'result', work_id)
    subject=request['context']['subject']
    assert subject['kind']=='audit'
    assessment={key:subject[key] for key in ['path','file_revision','input_revision']}
    assessment.update(disposition='reviewed', reason='Fixture completes guarded structural coverage; no semantic truth assertion.', reviews=[])
    path=root/'assessment.json'
    path.write_text(json.dumps({'assessments':[assessment]}))
    cli('work', 'finish', work_id, '--file', str(path))
    cli('wiki', 'audit-record', '--file', str(path))
    row={'action':action, 'work_id':work_id, 'pid':os.getpid()}
with (root/'bounded-calls.jsonl').open('a') as stream:
    stream.write(json.dumps(row)+'\n')
BOUNDED
"#;

fn bounded_audit_fixture(engine: &str, max_calls: usize, behavior: &str) -> Fixture {
    let fixture = Fixture::new(engine, 30, behavior);
    let config = std::fs::read_to_string(&fixture.config).unwrap().replace(
        "  default_profile: primary\n",
        &format!("  default_profile: primary\n  work: {{max_input_bytes: 8192, max_result_bytes: 1024, max_calls: {max_calls}}}\n"),
    ) + "graph:\n  audit:\n    batch_size: 1\n    max_input_bytes: 4096\n";
    std::fs::write(&fixture.config, config).unwrap();
    write_audit_concept(&fixture, "large");
    let path = fixture.root().join("vault/wiki/concepts/large.md");
    let body = std::fs::read_to_string(&path).unwrap()
        + &"\nEvidence clause with Unicode 한🙂 remains source scoped.\n".repeat(180);
    std::fs::write(path, body).unwrap();
    fixture
}

fn bounded_calls(fixture: &Fixture) -> Vec<serde_json::Value> {
    let path = fixture.root().join("bounded-calls.jsonl");
    if !path.exists() {
        return Vec::new();
    }
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn selected_profile_budget_reaches_controller_and_worker_commands() {
    for engine in ["claude-code", "codex"] {
        let fixture = bounded_audit_fixture(engine, 64, RECORD_BOUNDED_AUDIT);
        let config = std::fs::read_to_string(&fixture.config).unwrap().replace(
            "      timeout_seconds: 30\n",
            "      timeout_seconds: 30\n      work: {max_input_bytes: 16384, max_result_bytes: 2048, max_calls: 1}\n",
        );
        std::fs::write(&fixture.config, config).unwrap();
        let (output, result) = fixture.run("knowledge");
        assert!(
            !output.status.success(),
            "one call cannot finish this audit"
        );
        assert_eq!(
            result["execution"]["work"],
            serde_json::json!({"max_input_bytes":16384,"max_result_bytes":2048,"max_calls":1})
        );
        assert!(
            result["stages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|stage| stage["name"] == "semantic call budget" && stage["state"] == "failed"),
            "{result}"
        );
        let calls = bounded_calls(&fixture);
        assert_eq!(calls.len(), 1, "{result}");
        let arguments = std::fs::read_to_string(fixture.root().join("arguments")).unwrap();
        assert!(
            !arguments.contains("Evidence clause"),
            "source text goes through stdin rather than argv"
        );
        if engine == "codex" {
            assert!(arguments.contains("--sandbox\nread-only\n"));
            assert!(arguments.contains("--disable\nshell_tool\n"));
        } else {
            assert!(arguments.contains("--safe-mode\n--tools\n\n"));
            assert!(!arguments.contains("--plugin-dir"));
        }
        let cwd = std::fs::read_to_string(fixture.root().join("observation-cwd")).unwrap();
        assert_ne!(Path::new(cwd.trim()), fixture.root().join("vault"));
        assert!(
            !Path::new(cwd.trim()).exists(),
            "private observation workspace is removed after the child"
        );
        let work_id = calls[0]["work_id"].as_str().unwrap();
        let output = fixture
            .command()
            .env("LORE_EXECUTION_PROFILE", "primary")
            .args(["work", "status", work_id])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let status: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(status["completed"], 1);
        assert!(
            !fixture
                .root()
                .join("vault/.lorekeeper/knowledge-audit.json")
                .exists()
        );
        assert!(
            !fixture
                .root()
                .join("vault/.lorekeeper/work")
                .join(work_id)
                .join("final.json")
                .exists()
        );
    }
}

fn explicit_large_audit(fixture: &Fixture) -> serde_json::Value {
    let output = fixture
        .command()
        .args(["wiki", "audit-plan", "--path", "wiki/concepts/large.md"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn maintenance_prunes_published_work_only_after_exclusive_access_without_changing_knowledge() {
    let fixture = bounded_audit_fixture("codex", 64, RECORD_BOUNDED_AUDIT);
    let (output, result) = fixture.run("knowledge");
    assert!(output.status.success(), "{result}");
    let calls = bounded_calls(&fixture);
    let work_id = calls[0]["work_id"].as_str().unwrap();
    let work = fixture.root().join("vault/.lorekeeper/work").join(work_id);
    let receipt_path = work.join("publication.json");
    let mut receipt: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&receipt_path).unwrap()).unwrap();
    receipt["published_at"] = "2000-01-01T00:00:00Z".into();
    std::fs::write(&receipt_path, serde_json::to_vec(&receipt).unwrap()).unwrap();
    let knowledge = fixture.root().join("vault/wiki/concepts/large.md");
    let knowledge_before = std::fs::read(&knowledge).unwrap();
    let audit = fixture
        .root()
        .join("vault/.lorekeeper/knowledge-audit.json");
    let audit_before = std::fs::read(&audit).unwrap();
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(fixture.root().join("vault/.lorekeeper/workflow.lock"))
        .unwrap();
    lock.lock().unwrap();
    let busy = fixture.command().arg("maintenance").output().unwrap();
    assert!(
        busy.status.success(),
        "{}",
        String::from_utf8_lossy(&busy.stderr)
    );
    assert!(
        String::from_utf8_lossy(&busy.stderr)
            .contains("exclusive Unix workflow access is unavailable")
    );
    assert!(receipt_path.exists());
    drop(lock);
    let preview = fixture
        .command()
        .args(["maintenance", "--dry-run"])
        .output()
        .unwrap();
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    assert!(String::from_utf8_lossy(&preview.stderr).contains("work: pruned 1 published work"));
    assert!(receipt_path.exists());
    let prune = fixture.command().arg("maintenance").output().unwrap();
    assert!(
        prune.status.success(),
        "{}",
        String::from_utf8_lossy(&prune.stderr)
    );
    assert!(!work.exists());
    assert_eq!(std::fs::read(knowledge).unwrap(), knowledge_before);
    assert_eq!(std::fs::read(audit).unwrap(), audit_before);
}

#[test]
fn oversized_audit_completes_through_fresh_bounded_workers_on_both_hosts() {
    for engine in ["claude-code", "codex"] {
        let fixture = bounded_audit_fixture(engine, 64, RECORD_BOUNDED_AUDIT);
        let before = explicit_large_audit(&fixture);
        assert_eq!(before["input"]["status"], "oversized");
        let (output, result) = fixture.run("knowledge");
        assert!(
            output.status.success(),
            "{result}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(result["ok"], true);
        assert_eq!(result["execution"]["engine"], engine);
        assert_eq!(
            result["audit"][0]["assessed_path"],
            "wiki/concepts/large.md"
        );
        let calls = bounded_calls(&fixture);
        assert!(calls.len() > 3);
        assert_eq!(
            calls
                .iter()
                .filter(|call| call["action"] == "finish")
                .count(),
            1
        );
        let work = calls[0]["work_id"].as_str().unwrap();
        assert!(calls.iter().all(|call| call["work_id"] == work));
        let pids = calls
            .iter()
            .map(|call| call["pid"].as_u64().unwrap())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            pids.len(),
            calls.len(),
            "each node/finalizer needs a fresh worker process"
        );
        let inspected = calls
            .iter()
            .filter(|call| call["action"] == "inspect")
            .collect::<Vec<_>>();
        assert!(inspected.iter().any(|call| call["kind"] == "reduce"));
        assert!(
            inspected
                .iter()
                .all(|call| call["input_bytes"].as_u64().unwrap() <= 8192)
        );
        let status = fixture
            .command()
            .args(["work", "status", work])
            .output()
            .unwrap();
        assert!(status.status.success());
        let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
        assert_eq!(status["complete"], true);
        assert_eq!(
            status["completed"].as_u64().unwrap() as usize,
            inspected.len()
        );
        assert!(status["parts"].as_u64().unwrap() > 1);
        assert_eq!(
            explicit_large_audit(&fixture)["input"]["status"],
            "oversized"
        );
        let plan = fixture
            .command()
            .args(["wiki", "audit-plan"])
            .output()
            .unwrap();
        assert!(plan.status.success());
        let plan: serde_json::Value = serde_json::from_slice(&plan.stdout).unwrap();
        assert_eq!(plan["reviewed_current"], 1);
        assert_eq!(plan["unreviewed"], 0);
    }
}

#[test]
fn native_observation_cannot_record_another_selected_node() {
    let behavior = RECORD_BOUNDED_AUDIT.replace(
        "output={'work_id':work_id, 'node_id':node_id,",
        "output={'work_id':work_id, 'node_id':'not-selected',",
    );
    let fixture = bounded_audit_fixture("codex", 1, &behavior);
    let (output, result) = fixture.run("knowledge");
    assert!(!output.status.success());
    assert!(result["stages"].as_array().unwrap().iter().any(|stage| {
        stage["name"] == "bounded work receipt"
            && stage["state"] == "failed"
            && stage["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains("selected work node"))
    }));
    let calls = bounded_calls(&fixture);
    let id = calls[0]["work_id"].as_str().unwrap();
    let status = fixture
        .command()
        .args(["work", "status", id])
        .output()
        .unwrap();
    assert!(status.status.success());
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["completed"], 0);
}

#[test]
fn bounded_queue_shares_each_runs_budget_across_independent_documents() {
    let fixture = Fixture::new("codex", 30, RECORD_BOUNDED_AUDIT);
    let config = std::fs::read_to_string(&fixture.config).unwrap().replace("concept_policy: supported-new", "concept_policy: disabled")
        .replace("  default_profile: primary\n", "  default_profile: primary\n  work: {max_input_bytes: 8192, max_result_bytes: 1024, max_calls: 1}\n");
    std::fs::write(&fixture.config, config).unwrap();
    for name in ["a-large", "b-large"] {
        std::fs::write(
            fixture.root().join(format!("vault/inbox/{name}.md")),
            format!(
                "# {name}\n\n{}",
                "Independent source evidence.\n".repeat(1200)
            ),
        )
        .unwrap();
    }
    let collected = fixture.command().arg("ingest").output().unwrap();
    assert!(
        collected.status.success(),
        "{}",
        String::from_utf8_lossy(&collected.stderr)
    );
    for _ in 0..2 {
        let (output, result) = fixture.run("process");
        assert!(
            !output.status.success(),
            "a one-call budget retains unfinished documents: {result}"
        );
    }
    let calls = bounded_calls(&fixture);
    assert_eq!(calls.len(), 2);
    assert_ne!(
        calls[0]["work_id"], calls[1]["work_id"],
        "one unfinished document must not consume every resumed run before another independent document gets a turn"
    );
}

#[test]
fn resumed_knowledge_batches_share_budget_across_oversized_subjects() {
    let fixture = bounded_audit_fixture("codex", 1, RECORD_BOUNDED_AUDIT);
    let config = std::fs::read_to_string(&fixture.config)
        .unwrap()
        .replace("batch_size: 1", "batch_size: 2");
    std::fs::write(&fixture.config, config).unwrap();
    let source = fixture.root().join("vault/wiki/concepts/large.md");
    let second = std::fs::read_to_string(&source)
        .unwrap()
        .replace("large", "second");
    std::fs::write(source.with_file_name("second.md"), second).unwrap();
    for _ in 0..2 {
        let (output, result) = fixture.run("knowledge");
        assert!(!output.status.success(), "{result}");
        assert!(
            result["stages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|stage| stage["name"] == "semantic call budget"),
            "{result}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let calls = bounded_calls(&fixture);
    assert_eq!(calls.len(), 2);
    assert_ne!(calls[0]["work_id"], calls[1]["work_id"]);
}

#[test]
fn short_documents_finish_between_resumed_parts_of_a_large_document() {
    let ordinary = r#"
exec python3 - "$@" <<'SELECTED'
import json, os, subprocess, sys
from pathlib import Path
root=Path(os.environ['TEST_EXECUTION_ROOT'])
prompt='\n'.join(sys.argv[1:])
path,_=json.JSONDecoder().raw_decode(prompt.split('Selected queue task manifest: ',1)[1])
ids=json.loads(Path(path).read_text())
lore=os.environ['LORE_BIN']
status=json.loads(subprocess.check_output([lore,'queue','status','--json']))
rows={row['task_id']:row for row in status['data']['tasks']}
tasks=[json.loads(line) for path in (root/'vault/.lorekeeper/queue').glob('*.jsonl') for line in path.read_text().splitlines()]
selected=[task for task in tasks if task['task_id'] in ids]
assert len(selected)==len(ids)
assert len({task['target']['vault_path'] for task in selected})==len(selected)
results=root/'vault/.lorekeeper/queue/results'
results.mkdir(exist_ok=True)
for task in selected:
    assert task['kind']=='summarize'
    assert 'short' in task['target']['vault_path']
    result={key:task[key] for key in ['task_id','cache_hash']}
    result.update(target_revision=rows[task['task_id']]['target_revision'],body='Short source evidence was inspected.')
    (results/(task['task_id']+'.section.json')).write_text(json.dumps(result))
(root/'selected-manifest-path').write_text(path)
SELECTED
"#;
    let behavior = format!(
        "case \"$*\" in\n*--output-schema*|*--safe-mode*)\n{RECORD_BOUNDED_AUDIT}\n;;\n*)\n{ordinary}\n;;\nesac"
    );
    let fixture = Fixture::new("codex", 30, &behavior);
    let config = std::fs::read_to_string(&fixture.config).unwrap().replace("concept_policy: supported-new", "concept_policy: disabled")
        .replace("  default_profile: primary\n", "  default_profile: primary\n  work: {max_input_bytes: 8192, max_result_bytes: 1024, max_calls: 1}\n");
    std::fs::write(&fixture.config, config).unwrap();
    std::fs::write(
        fixture.root().join("vault/inbox/a-large.md"),
        format!("# A large\n\n{}", "Source evidence.\n".repeat(1500)),
    )
    .unwrap();
    std::fs::write(
        fixture.root().join("vault/inbox/z-short.md"),
        "# Z short\n\nShort source evidence.\n",
    )
    .unwrap();
    assert!(
        fixture
            .command()
            .arg("ingest")
            .output()
            .unwrap()
            .status
            .success()
    );
    for _ in 0..2 {
        let (output, _) = fixture.run("process");
        assert!(!output.status.success(), "large work remains pending");
    }
    let status = fixture
        .command()
        .args(["queue", "status", "--json"])
        .output()
        .unwrap();
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    let rows = status["data"]["tasks"].as_array().unwrap();
    assert!(
        rows.iter().any(|row| row["vault_path"]
            .as_str()
            .is_some_and(|path| path.contains("short"))
            && row["status"] == "done"),
        "{status}"
    );
    let manifest = std::fs::read_to_string(fixture.root().join("selected-manifest-path")).unwrap();
    assert!(
        !Path::new(&manifest).exists(),
        "selection metadata is removed after the worker"
    );
}

#[test]
fn native_queue_resumes_an_explicit_single_part_work_plan() {
    let fixture = Fixture::new("codex", 30, RECORD_BOUNDED_AUDIT);
    let config = std::fs::read_to_string(&fixture.config).unwrap().replace("concept_policy: supported-new", "concept_policy: disabled")
        .replace("  default_profile: primary\n", "  default_profile: primary\n  work: {max_input_bytes: 8192, max_result_bytes: 1024, max_calls: 1}\n");
    std::fs::write(&fixture.config, config).unwrap();
    fixture.ingest();
    let status = fixture
        .command()
        .args(["queue", "status", "--json"])
        .output()
        .unwrap();
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    let task = status["data"]["tasks"][0]["task_id"].as_str().unwrap();
    let planned = fixture
        .command()
        .args(["work", "plan", "--task-id", task])
        .output()
        .unwrap();
    assert!(
        planned.status.success(),
        "{}",
        String::from_utf8_lossy(&planned.stderr)
    );
    let plan: serde_json::Value = serde_json::from_slice(&planned.stdout).unwrap();
    assert_eq!(plan["parts"], 1);
    let (output, result) = fixture.run("process");
    assert!(
        !output.status.success(),
        "one call leaves finalization pending: {result}"
    );
    let calls = bounded_calls(&fixture);
    assert_eq!(calls.len(), 1, "{result}");
    assert_eq!(calls[0]["work_id"], plan["work_id"]);
}

#[test]
fn sealed_queue_work_restages_a_missing_result_without_calling_the_model_again() {
    let start = RECORD_BOUNDED_AUDIT
        .find("    assert subject['kind']=='audit'")
        .unwrap();
    let end = RECORD_BOUNDED_AUDIT[start..]
        .find("    row={'action':action")
        .unwrap()
        + start;
    let mut behavior = RECORD_BOUNDED_AUDIT.to_string();
    behavior.replace_range(
        start..end,
        r#"    assert subject['kind']=='queue'
    result={key:subject[key] for key in ['task_id','cache_hash','target_revision']}
    result['body']='Verified cached section result.'
    path=root/'section.json'
    path.write_text(json.dumps(result))
    staged=cli('work','finish',work_id,'--file',str(path))
    (root/'vault'/staged['staged_result']).unlink()
"#,
    );
    let fixture = Fixture::new("codex", 30, &behavior);
    let config = std::fs::read_to_string(&fixture.config)
        .unwrap()
        .replace("concept_policy: supported-new", "concept_policy: disabled");
    std::fs::write(&fixture.config, config).unwrap();
    fixture.ingest();
    let status = fixture
        .command()
        .args(["queue", "status", "--json"])
        .output()
        .unwrap();
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    let task = status["data"]["tasks"][0]["task_id"].as_str().unwrap();
    assert!(
        fixture
            .command()
            .args(["work", "plan", "--task-id", task])
            .output()
            .unwrap()
            .status
            .success()
    );
    let (first, result) = fixture.run("process");
    assert!(
        !first.status.success(),
        "missing staging cannot count as completed publication: {result}"
    );
    let calls = bounded_calls(&fixture);
    assert_eq!(calls.len(), 2);
    std::fs::remove_file(fixture.root().join("agent executable")).unwrap();
    let (second, result) = fixture.run("process");
    assert!(
        second.status.success(),
        "{result}\n{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(
        result["execution"].is_null(),
        "verified cache restoration needs no agent preflight"
    );
    assert_eq!(bounded_calls(&fixture).len(), 2);
    assert!(
        fixture
            .command()
            .args(["queue", "check"])
            .output()
            .unwrap()
            .status
            .success()
    );
}

#[test]
fn bounded_audit_resumes_call_budgets_and_exit_zero_cannot_forge_a_receipt() {
    for engine in ["claude-code", "codex"] {
        let no_receipt = bounded_audit_fixture(engine, 1, "exit 0");
        let (output, result) = no_receipt.run("knowledge");
        assert!(!output.status.success());
        assert!(result["stages"].as_array().unwrap().iter().any(|stage| {
            stage["name"] == "bounded work receipt" && stage["state"] == "failed"
        }));
        assert!(
            !no_receipt
                .root()
                .join("vault/.lorekeeper/knowledge-audit.json")
                .exists()
        );
        assert!(bounded_calls(&no_receipt).is_empty());
        let manifests = std::fs::read_dir(no_receipt.root().join("vault/.lorekeeper/work"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(manifests.len(), 1);
        let work_id = manifests[0].file_name().unwrap().to_str().unwrap();
        let status = no_receipt
            .command()
            .args(["work", "status", work_id])
            .output()
            .unwrap();
        assert!(status.status.success());
        let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
        assert_eq!(status["completed"], 0);
        assert_eq!(status["complete"], false);
        let fixture = bounded_audit_fixture(engine, 1, RECORD_BOUNDED_AUDIT);
        let mut completed = false;
        for attempt in 0..24 {
            let previous = bounded_calls(&fixture);
            let (output, result) = fixture.run("knowledge");
            let calls = bounded_calls(&fixture);
            assert_eq!(
                calls.len(),
                previous.len() + 1,
                "one configured call per invocation: {result}"
            );
            assert!(
                calls
                    .iter()
                    .all(|call| call["work_id"] == calls[0]["work_id"])
            );
            if output.status.success() {
                assert!(attempt > 0);
                assert_eq!(calls.last().unwrap()["action"], "finish");
                assert_eq!(
                    result["audit"][0]["assessed_path"],
                    "wiki/concepts/large.md"
                );
                completed = true;
                break;
            }
            assert_eq!(result["ok"], false);
            assert!(
                result["stages"].as_array().unwrap().iter().any(|stage| {
                    stage["name"] == "semantic call budget" && stage["state"] == "failed"
                }),
                "{result}"
            );
            assert!(
                !fixture
                    .root()
                    .join("vault/.lorekeeper/knowledge-audit.json")
                    .exists()
            );
        }
        assert!(completed, "bounded native work should resume to completion");
        let calls = bounded_calls(&fixture);
        let nodes = calls
            .iter()
            .filter_map(|call| call.get("node_id").and_then(serde_json::Value::as_str))
            .collect::<Vec<_>>();
        let unique = nodes.iter().collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            nodes.len(),
            unique.len(),
            "resume must not replay completed exact nodes"
        );
    }
}

#[test]
fn knowledge_campaign_releases_each_lease_and_stops_on_failure_without_losing_progress() {
    for fail_second in [false, true] {
        let behavior = format!(
            r#"
if [ -f "$TEST_EXECUTION_ROOT/assessed-once" ] && [ {fail_second} = true ]; then exit 0; fi
"$LORE_BIN" wiki audit-plan > "$TEST_EXECUTION_ROOT/plan.json" || exit 31
python3 - <<'ASSESS'
import json, os
from pathlib import Path
root=Path(os.environ['TEST_EXECUTION_ROOT'])
candidate=json.loads((root/'plan.json').read_text())['candidates'][0]
assessment={{key:candidate[key] for key in ['path','file_revision','input_revision']}}
assessment.update(disposition='reviewed', reason='Coordinator fixture records this exact selected input.', reviews=[])
(root/'assessment.json').write_text(json.dumps({{'assessments':[assessment]}}))
ASSESS
"$LORE_BIN" wiki audit-record --file "$TEST_EXECUTION_ROOT/assessment.json" || exit 32
touch "$TEST_EXECUTION_ROOT/assessed-once"
"#
        );
        let fixture = Fixture::new("codex", 30, &behavior);
        let config = std::fs::read_to_string(&fixture.config).unwrap()
            + "graph:\n  audit:\n    batch_size: 1\n";
        std::fs::write(&fixture.config, config).unwrap();
        let concepts = fixture.root().join("vault/wiki/concepts");
        std::fs::create_dir_all(&concepts).unwrap();
        let template = include_str!("behaviour/corpus/small/vault/wiki/concepts/unlinked-note.md");
        for name in ["a", "b"] {
            std::fs::write(
                concepts.join(format!("{name}.md")),
                template
                    .replace("Unlinked note", name)
                    .replace("unlinked-note", name),
            )
            .unwrap();
        }
        let output = fixture
            .command()
            .args(["run", "knowledge", "--batches", "4", "--json"])
            .output()
            .unwrap();
        assert_eq!(
            output.status.success(),
            !fail_second,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let runs = result["runs"].as_array().unwrap();
        assert_eq!(
            result["stop_reason"],
            if fail_second { "failed" } else { "settled" }
        );
        assert_eq!(runs.len(), if fail_second { 2 } else { 3 });
        assert_ne!(runs[0]["workflow_id"], runs[1]["workflow_id"]);
        let audit = fixture
            .command()
            .args(["wiki", "audit-plan"])
            .output()
            .unwrap();
        assert!(audit.status.success());
        let audit: serde_json::Value = serde_json::from_slice(&audit.stdout).unwrap();
        assert_eq!(audit["reviewed_current"], if fail_second { 1 } else { 2 });
        assert_eq!(audit["unreviewed"], if fail_second { 1 } else { 0 });
        assert!(
            fixture
                .command()
                .args(["workflow", "--", "/usr/bin/true"])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            !fixture
                .command()
                .args(["run", "process", "--batches", "2"])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            !fixture
                .command()
                .args(["run", "knowledge", "--batches", "0"])
                .status()
                .unwrap()
                .success()
        );
    }
}

#[test]
fn audit_completion_requires_this_workflows_native_merge_and_current_survivor() {
    for (action, expected) in [
        (
            "\"$LORE_BIN\" graph merge a-duplicate z-canonical --force || exit 31",
            true,
        ),
        (
            "\"$LORE_BIN\" graph merge a-duplicate m-intermediate --force || exit 31\n\"$LORE_BIN\" graph merge m-intermediate z-canonical --force || exit 31",
            true,
        ),
        (
            "rm \"$TEST_EXECUTION_ROOT/vault/wiki/concepts/a-duplicate.md\"",
            false,
        ),
        (
            "LORE_WORKFLOW_ID=ffffffffffffffffffffffffffffffff \"$LORE_BIN\" graph merge a-duplicate z-canonical --force || exit 31",
            false,
        ),
    ] {
        let behavior = format!(
            "{action}\n{}",
            r#"
"$LORE_BIN" graph backlinks-sync || exit 32
"$LORE_BIN" wiki audit-plan --path wiki/concepts/z-canonical.md > "$TEST_EXECUTION_ROOT/plan.json" || exit 33
python3 - <<'ASSESS'
import json, os
from pathlib import Path
root=Path(os.environ['TEST_EXECUTION_ROOT'])
candidate=json.loads((root/'plan.json').read_text())['candidates'][0]
assessment={key:candidate[key] for key in ['path','file_revision','input_revision']}
assessment.update(disposition='reviewed', reason='The final survivor and its evidence were reviewed.', reviews=[])
(root/'assessment.json').write_text(json.dumps({'assessments':[assessment]}))
ASSESS
"$LORE_BIN" wiki audit-record --file "$TEST_EXECUTION_ROOT/assessment.json"
"#
        );
        let fixture = Fixture::new("codex", 30, &behavior);
        let config = std::fs::read_to_string(&fixture.config).unwrap()
            + "graph:\n  audit:\n    batch_size: 1\n";
        std::fs::write(&fixture.config, config).unwrap();
        let concepts = fixture.root().join("vault/wiki/concepts");
        std::fs::create_dir_all(&concepts).unwrap();
        let template = include_str!("behaviour/corpus/small/vault/wiki/concepts/unlinked-note.md");
        for (path, title) in [
            ("a-duplicate", "A duplicate"),
            ("m-intermediate", "M intermediate"),
            ("z-canonical", "Z canonical"),
        ] {
            std::fs::write(
                concepts.join(format!("{path}.md")),
                template
                    .replace("Unlinked note", title)
                    .replace("unlinked-note", path),
            )
            .unwrap();
        }
        let (output, result) = fixture.run("knowledge");
        assert_eq!(
            output.status.success(),
            expected,
            "{action}\n{result}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(result["ok"], expected);
        if expected {
            assert_eq!(
                result["audit"][0]["requested_path"],
                "wiki/concepts/a-duplicate.md"
            );
            assert_eq!(
                result["audit"][0]["assessed_path"],
                "wiki/concepts/z-canonical.md"
            );
        }
        assert!(!concepts.join("a-duplicate.md").exists());
        assert!(concepts.join("z-canonical.md").exists());
    }
}

fn assert_daily_period_finalization(with_review: bool) {
    let fixture = Fixture::new(
        "codex",
        30,
        r#"python3 - <<'PROCESS'
import json, os, subprocess
from pathlib import Path
root = Path(os.environ['TEST_EXECUTION_ROOT'])
vault = root / 'vault'
queue = vault / '.lorekeeper/queue'
lore = os.environ['LORE_BIN']
while True:
    status = json.loads(subprocess.check_output([lore, 'queue', 'status', '--json']))
    current = [row for row in status['data']['tasks'] if row['status'] == 'current']
    if not current:
        break
    row = current[0]
    tasks = [json.loads(line) for path in queue.glob('*.jsonl') for line in path.read_text().splitlines()]
    task = next(task for task in tasks if task['task_id'] == row['task_id'])
    assert task['kind'] in ['summarize', 'identify-themes', 'refine-events'], task
    if task['kind'] == 'refine-events':
        page = (vault / task['target']['vault_path']).read_text()
        body = page.split(task['target']['anchor'] + '\n', 1)[1].split('\n## ', 1)[0].strip()
    elif task['kind'] == 'identify-themes':
        body = ('### Evidence review\nThe evidence review task was completed.'
                if (root / 'resume-period').exists()
                else '### Evidence review\nCompleted the evidence review task.')
    else:
        body = 'Completed the evidence review task and checked retained source evidence.'
    result = {key: task[key] for key in ['task_id', 'cache_hash']}
    result.update(target_revision=row['target_revision'], body=body)
    results = queue / 'results'
    results.mkdir(exist_ok=True)
    (results / (task['task_id'] + '.section.json')).write_text(json.dumps(result))
    subprocess.run([lore, 'queue', 'apply'], check=True)
subprocess.run([lore, 'queue', 'prune', '--json'], check=True)
PROCESS"#,
    );
    let config = std::fs::read_to_string(&fixture.config).unwrap();
    std::fs::write(
        &fixture.config,
        config
            .replace(
                "sources:\n  notes:\n    type: manual\n    params: {inbox_dir: inbox}\n    labels: [knowledge]\n    concept_policy: supported-new\n",
                "sources:\n  completed:\n    type: tasks\n    concept_policy: disabled\npersonal:\n  tasks: {}\n  tracked_sources: [completed]\nsynthesis:\n  weekly:\n    include_sources: [completed]\n",
            )
            .replace("  root: vault\n", "  root: vault\n  timezone: UTC\n"),
    )
    .unwrap();
    let vault = fixture.root().join("vault");
    lk_task::TransitionLog::new(&vault)
        .record(
            &[lk_task::Transition::new(
                "7k2p".parse().unwrap(),
                lk_task::TransitionKind::Done,
                "Review evidence",
                "2020-01-05T12:00:00Z".parse().unwrap(),
            )
            .with_note(Some("Checked retained source evidence.".into()))],
            &jiff::tz::TimeZone::UTC,
        )
        .unwrap();
    for args in [
        vec!["ingest", "--date", "2020-01-05"],
        vec!["run", "process"],
        vec!["synthesis", "weekly", "--date", "2020-01-05"],
        vec!["run", "process"],
    ] {
        let output = fixture.command().args(&args).output().unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let target = "synthesis/weekly/2020-W01.md";
    let before = std::fs::read_to_string(vault.join(target)).unwrap();
    if with_review {
        let revision = lk_core::concept::source_revision(&lk_vault::parse_page(&before).unwrap());
        lk_vault::archive_evidence(&vault, &[target.into()]).unwrap();
        std::fs::create_dir_all(vault.join("wiki/explorations")).unwrap();
        let review = serde_json::json!({
            "status": "resolved",
            "evidence": [{"path": target, "revision": revision}],
            "resolution": "The weekly narrative records the completed evidence review task.",
        });
        std::fs::write(
            vault.join("wiki/explorations/weekly-evidence-review.md"),
            format!("---\ntype: exploration\ntitle: Weekly evidence review\nreview: {review}\n---\nThe retained weekly narrative supports this assessment.\n"),
        )
        .unwrap();
    }
    for args in [
        vec!["wiki", "reviews", "--check"],
        vec!["synthesis", "weekly", "--date", "2020-01-05", "--defer"],
    ] {
        let output = fixture.command().args(&args).output().unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    std::fs::write(fixture.root().join("resume-period"), "").unwrap();
    let (output, result) = fixture.run("daily");
    let stages = result["stages"].as_array().unwrap();
    let initial_review = stages
        .iter()
        .position(|stage| stage["name"] == "knowledge reviews" && stage["state"] == "completed")
        .unwrap();
    let period_settled = stages
        .iter()
        .position(|stage| stage["name"] == "period settle base" && stage["state"] == "completed")
        .unwrap();
    assert!(initial_review < period_settled);
    assert_ne!(std::fs::read_to_string(vault.join(target)).unwrap(), before);
    let reviews = lk_vault::list_reviews(&vault, &Default::default()).unwrap();
    if with_review {
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].changed_evidence, vec![target]);
    } else {
        assert!(reviews.is_empty());
    }
    let queue = fixture.command().args(["queue", "check"]).output().unwrap();
    assert!(
        queue.status.success(),
        "{}",
        String::from_utf8_lossy(&queue.stderr)
    );
    assert_eq!(
        output.status.success(),
        !with_review,
        "Daily must validate reviews after late period publication: {result}"
    );
    assert_eq!(result["ok"], !with_review);
    let final_stages = &stages[period_settled + 1..];
    for name in ["graph lint", "queue completion"] {
        assert!(
            final_stages
                .iter()
                .any(|stage| { stage["name"] == name && stage["state"] == "completed" }),
            "Missing final {name} after period publication: {result}"
        );
    }
    let expected_review_state = if with_review { "failed" } else { "completed" };
    assert!(
        final_stages.iter().any(|stage| {
            stage["name"]
                .as_str()
                .is_some_and(|name| name.contains("reviews"))
                && stage["state"] == expected_review_state
        }),
        "Final strict review validation must reflect the published period evidence: {result}"
    );
}

#[test]
fn daily_rechecks_reviews_after_late_period_publication() {
    assert_daily_period_finalization(true);
}

#[test]
fn daily_finishes_clean_late_period_publication() {
    assert_daily_period_finalization(false);
}

#[test]
fn a_failed_base_stage_does_not_start_dependent_periods() {
    let fixture = Fixture::new(
        "codex",
        30,
        "printf 'attempt\\n' >> \"$TEST_EXECUTION_ROOT/attempts\"\nexit 23",
    );
    fixture.ingest();
    let (output, result) = fixture.run("daily");
    assert!(!output.status.success());
    let names = result["stages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|stage| stage["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(names.contains(&"period resume base"));
    assert!(!names.contains(&"period resume quarter"));
    assert!(!names.contains(&"period resume annual"));
    let period_resume = names
        .iter()
        .position(|name| *name == "period resume base")
        .unwrap();
    let final_stages = &result["stages"].as_array().unwrap()[period_resume + 1..];
    assert!(
        final_stages
            .iter()
            .any(|stage| { stage["name"] == "queue completion" && stage["state"] == "failed" }),
        "Final queue check must preserve refusal after period failure: {result}"
    );
    assert!(
        final_stages.iter().any(|stage| {
            stage["name"]
                .as_str()
                .is_some_and(|name| name.contains("reviews"))
                && stage["state"] == "completed"
        }),
        "Final reviews must still be inspected after period failure: {result}"
    );
    let pending = fixture.command().args(["queue", "count"]).output().unwrap();
    assert!(pending.status.success());
    assert!(
        String::from_utf8(pending.stdout)
            .unwrap()
            .trim()
            .parse::<usize>()
            .unwrap()
            > 0
    );
    assert_eq!(
        std::fs::read_to_string(fixture.root().join("attempts")).unwrap(),
        "attempt\n"
    );
}

#[test]
fn deadline_preserves_pending_work_and_releases_the_workflow() {
    let fixture = Fixture::new("codex", 1, "while :; do sleep 1; done");
    fixture.ingest();
    let (output, result) = fixture.run("process");
    assert!(!output.status.success());
    assert_eq!(result["cancelled"], false);
    assert!(
        result["stages"].as_array().unwrap().iter().any(|stage| {
            stage["name"] == "lore-process"
                && stage["state"] == "failed"
                && stage["detail"].as_str().is_some_and(|detail| {
                    detail.starts_with("session deadline exceeded")
                        || detail.starts_with("agent preflight failed: session deadline exceeded")
                })
        }),
        "result: {result}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !fixture
            .command()
            .args(["queue", "check"])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        fixture
            .command()
            .args(["workflow", "--", "/usr/bin/true"])
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn native_execution_preserves_the_users_git_index_and_commit() {
    let fixture = Fixture::new("codex", 30, "exit 0");
    let root = fixture.root().join("vault");
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.name", "Test"],
        vec!["config", "user.email", "test@example.com"],
    ] {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }
    let note = root.join("personal.md");
    std::fs::write(&note, "initial").unwrap();
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["add", "personal.md"])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["commit", "-qm", "initial"])
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(&note, "staged user text").unwrap();
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["add", "personal.md"])
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(&note, "unstaged user text").unwrap();
    let index = std::fs::read(root.join(".git/index")).unwrap();
    let head = std::fs::read(
        root.join(".git/refs/heads").join(
            String::from_utf8(
                Command::new("git")
                    .arg("-C")
                    .arg(&root)
                    .args(["branch", "--show-current"])
                    .output()
                    .unwrap()
                    .stdout,
            )
            .unwrap()
            .trim(),
        ),
    )
    .unwrap();
    let (output, _) = fixture.run("process");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read(root.join(".git/index")).unwrap(), index);
    let branch = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert_eq!(branch.stdout, head);
    assert_eq!(std::fs::read_to_string(note).unwrap(), "unstaged user text");
}

#[test]
fn cancellation_kills_foreground_children_and_preserves_cross_host_exclusion() {
    let fixture = Fixture::new(
        "codex",
        30,
        "printf '%s\\n' \"$$\" > \"$TEST_EXECUTION_ROOT/started\"\ntrap '' TERM\n/bin/sh -c 'trap \"\" TERM; while :; do sleep 1; done' &\nwait",
    );
    fixture.ingest();
    let stdout = std::fs::File::create(fixture.root().join("result.json")).unwrap();
    let stderr = std::fs::File::create(fixture.root().join("stderr.log")).unwrap();
    let mut child = fixture
        .command()
        .args(["run", "process", "--json"])
        .stdout(stdout)
        .stderr(stderr)
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !fixture.root().join("started").exists() {
        assert!(Instant::now() < deadline, "agent never started");
        assert!(child.try_wait().unwrap().is_none());
        std::thread::sleep(Duration::from_millis(20));
    }
    let blocked = fixture
        .command()
        .args(["workflow", "--", "/usr/bin/true"])
        .output()
        .unwrap();
    assert!(!blocked.status.success());
    let pid = rustix::process::Pid::from_raw(i32::try_from(child.id()).unwrap()).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).unwrap();
    while child.try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < deadline,
            "cancelled controller did not stop"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let result: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture.root().join("result.json")).unwrap())
            .unwrap();
    assert_eq!(
        result["cancelled"],
        true,
        "result: {result}\nstderr: {}",
        std::fs::read_to_string(fixture.root().join("stderr.log")).unwrap()
    );
    assert_eq!(
        result["stages"].as_array().unwrap().last().unwrap()["state"],
        "cancelled"
    );
    loop {
        let released = fixture
            .command()
            .args(["workflow", "--", "/usr/bin/true"])
            .output()
            .unwrap();
        if released.status.success() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "child retained the workflow descriptor after cancellation"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(target_os = "macos")]
#[test]
fn cancellation_stops_further_stages_when_zombie_group_cleanup_returns_eperm() {
    use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, kill_process, waitid};
    use std::os::unix::process::CommandExt;

    let fixture = Fixture::new(
        "codex",
        30,
        "printf '%s\\n' \"$$\" > \"$TEST_EXECUTION_ROOT/started\"\nexec sleep 30",
    );
    fixture.ingest();
    let mut controller = fixture
        .command()
        .args(["run", "process", "--json"])
        .stdout(std::fs::File::create(fixture.root().join("result.json")).unwrap())
        .stderr(std::fs::File::create(fixture.root().join("stderr.log")).unwrap())
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let agent = loop {
        if let Ok(body) = std::fs::read_to_string(fixture.root().join("started"))
            && let Ok(pid) = body.trim().parse::<i32>()
        {
            break pid;
        }
        assert!(Instant::now() < deadline, "agent never started");
        assert!(controller.try_wait().unwrap().is_none());
        std::thread::sleep(Duration::from_millis(20));
    };
    // Keep an exited member in the owned group: macOS killpg returns EPERM for zombies alone.
    let mut zombie = Command::new("/usr/bin/true")
        .process_group(agent)
        .spawn()
        .unwrap();
    waitid(
        WaitId::Pid(Pid::from_raw(i32::try_from(zombie.id()).unwrap()).unwrap()),
        WaitIdOptions::EXITED | WaitIdOptions::NOWAIT,
    )
    .unwrap();
    kill_process(
        Pid::from_raw(i32::try_from(controller.id()).unwrap()).unwrap(),
        Signal::TERM,
    )
    .unwrap();
    while controller.try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < deadline,
            "cancelled controller did not stop"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    zombie.wait().unwrap();
    let result: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture.root().join("result.json")).unwrap())
            .unwrap();
    let stages = result["stages"].as_array().unwrap();
    assert_eq!(result["ok"], false, "{result}");
    assert_eq!(result["cancelled"], true, "{result}");
    assert_eq!(stages.last().unwrap()["name"], "lore-process", "{result}");
    assert_eq!(stages.last().unwrap()["state"], "cancelled", "{result}");
    assert!(
        stages.last().unwrap()["detail"]
            .as_str()
            .unwrap()
            .contains("signal child group:"),
        "{result}"
    );
    let queued = fixture.command().args(["queue", "count"]).output().unwrap();
    assert_eq!(String::from_utf8(queued.stdout).unwrap().trim(), "2");
    assert!(
        fixture
            .command()
            .args(["workflow", "--", "/usr/bin/true"])
            .status()
            .unwrap()
            .success()
    );
}
