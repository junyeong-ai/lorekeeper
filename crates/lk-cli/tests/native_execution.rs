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
        std::fs::write(&config, format!("vault:\n  root: vault\nidentity:\n  name: Test\n  email: test@example.com\nsources:\n  notes:\n    type: manual\n    params: {{inbox_dir: inbox}}\n    labels: [knowledge]\n    concept_policy: supported-new\nexecution:\n  default_profile: primary\n  profiles:\n    primary:\n      engine: {engine}\n      executable: {}\n      timeout_seconds: {timeout}\n      work: {{extraction_context: full-context}}\n", serde_json::to_string(&agent).unwrap())).unwrap();
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
        let json: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap_or_else(|e| {
            panic!(
                "{e}: {}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            )
        });
        let duration = json["duration_ms"].as_u64().expect("job wall time");
        let semantic = json["semantic_duration_ms"]
            .as_u64()
            .expect("agent wall time");
        if json["execution"]["work"]["max_parallel"]
            .as_u64()
            .unwrap_or(1)
            == 1
        {
            assert!(semantic <= duration, "{json}");
        }
        if json["semantic_calls"] == 0 {
            assert_eq!(semantic, 0, "{json}");
        }
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
import subprocess
if candidate['discovery'] is None:
    discovery={key:candidate[key] for key in ['path','file_revision']}
    discovery['queries']=[{'query':candidate['title'],'phrase':True,'spacing':False}]
    path=root/'discovery.json'
    path.write_text(json.dumps(discovery))
    subprocess.run([os.environ['LORE_BIN'],'wiki','audit-discover','--file',str(path)],check=True)
    candidate=json.loads(subprocess.check_output([os.environ['LORE_BIN'],'wiki','audit-plan','--path',candidate['path']]))['candidates'][0]
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
    record_discovery(fixture, &format!("wiki/concepts/{name}.md"));
}

fn record_discovery(fixture: &Fixture, path: &str) {
    let result = fixture
        .command()
        .args(["graph", "backlinks-sync"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let raw = std::fs::read_to_string(fixture.root().join("vault").join(path)).unwrap();
    let request = fixture.root().join("discovery.json");
    std::fs::write(
        &request,
        serde_json::to_vec(&serde_json::json!({
            "path": path, "file_revision": lk_core::concept::file_revision(&raw),
            "queries": [{"query":"Unlinked note", "phrase":true, "spacing":false}]
        }))
        .unwrap(),
    )
    .unwrap();
    let result = fixture
        .command()
        .args(["wiki", "audit-discover", "--file"])
        .arg(request)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn knowledge_reserves_pending_order_before_discovery_across_call_limited_runs() {
    let discover = r#"
case "$*" in
  *"discover."*)
    "$LORE_BIN" wiki audit-plan > "$TEST_EXECUTION_ROOT/plan.json" || exit 40
    python3 - <<'DISCOVERY'
import json, os, subprocess
from pathlib import Path
root=Path(os.environ['TEST_EXECUTION_ROOT'])
candidate=json.loads((root/'plan.json').read_text())['candidates'][0]
request={key:candidate[key] for key in ['path','file_revision']}
request['queries']=[{'query':candidate['title'],'phrase':True,'spacing':False}]
path=root/'discovery.json'
path.write_text(json.dumps(request))
subprocess.run([os.environ['LORE_BIN'],'wiki','audit-discover','--file',str(path)],check=True)
DISCOVERY
    exit $?
esac
"#;
    for engine in ["claude-code", "codex"] {
        let fixture = Fixture::new(engine, 30, &format!("{discover}\n{RECORD_FIRST_AUDIT}"));
        let config = std::fs::read_to_string(&fixture.config).unwrap().replace(
            "  default_profile: primary\n",
            "  default_profile: primary\n  work: {max_calls: 1}\n",
        ) + "graph:\n  audit:\n    batch_size: 1\n";
        std::fs::write(&fixture.config, config).unwrap();
        let documents = fixture.root().join("vault/wiki/documents");
        std::fs::create_dir_all(&documents).unwrap();
        let write_document = |name| {
            std::fs::write(
                documents.join(format!("{name}.md")),
                format!("---\ntype: document\ntitle: Source {name}\n---\nA complete authored source {name}.\n"),
            )
            .unwrap();
        };
        let state_path = fixture
            .root()
            .join("vault/.lorekeeper/knowledge-audit.json");
        let state = || -> serde_json::Value {
            serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap()
        };
        write_document("z");
        let (output, result) = fixture.run("knowledge");
        assert!(!output.status.success(), "{result}");
        assert_eq!(result["semantic_calls"], 1, "{result}");
        let reserved = state();
        assert_eq!(
            reserved["pending"],
            serde_json::json!(["wiki/documents/z.md"])
        );
        assert_eq!(reserved["entries"], serde_json::json!({}));
        assert_eq!(reserved["next_class"], "changed");
        assert!(reserved["discovery"]["wiki/documents/z.md"].is_object());

        write_document("a");
        let before = std::fs::read(&state_path).unwrap();
        let planned = fixture
            .command()
            .args(["wiki", "audit-plan"])
            .output()
            .unwrap();
        assert!(planned.status.success());
        let planned: serde_json::Value = serde_json::from_slice(&planned.stdout).unwrap();
        assert_eq!(planned["candidates"][0]["path"], "wiki/documents/z.md");
        assert_eq!(std::fs::read(&state_path).unwrap(), before);
        let (output, result) = fixture.run("knowledge");
        assert!(output.status.success(), "{result}");
        assert_eq!(result["semantic_calls"], 1, "{result}");
        assert_eq!(result["audit"][0]["assessed_path"], "wiki/documents/z.md");
        assert_eq!(
            state()["pending"],
            serde_json::json!(["wiki/documents/a.md"])
        );

        write_document("0");
        let (output, result) = fixture.run("knowledge");
        assert!(!output.status.success(), "{result}");
        assert_eq!(result["semantic_calls"], 1, "{result}");
        let reserved = state();
        assert_eq!(
            reserved["pending"],
            serde_json::json!(["wiki/documents/a.md", "wiki/documents/0.md"])
        );
        assert_eq!(reserved["entries"].as_object().unwrap().len(), 1);
        assert!(reserved["discovery"]["wiki/documents/a.md"].is_object());
        let (output, result) = fixture.run("knowledge");
        assert!(output.status.success(), "{result}");
        assert_eq!(result["semantic_calls"], 1, "{result}");
        assert_eq!(result["audit"][0]["assessed_path"], "wiki/documents/a.md");
        assert_eq!(
            state()["pending"],
            serde_json::json!(["wiki/documents/0.md"])
        );
    }
}

#[test]
fn knowledge_discovers_unlinked_documents_before_assessment_on_both_hosts() {
    let discover = r#"
case "$*" in
  *"discover."*)
    "$LORE_BIN" wiki audit-plan > "$TEST_EXECUTION_ROOT/plan.json" || exit 40
    python3 - <<'DISCOVERY'
import json, os, subprocess
from pathlib import Path
root=Path(os.environ['TEST_EXECUTION_ROOT'])
for candidate in json.loads((root/'plan.json').read_text())['candidates']:
    request={key:candidate[key] for key in ['path','file_revision']}
    request['queries']=[{'query':'production order retention','phrase':False,'spacing':False}]
    path=root/'discovery.json'
    path.write_text(json.dumps(request))
    subprocess.run([os.environ['LORE_BIN'],'wiki','audit-discover','--file',str(path)],check=True)
DISCOVERY
    exit $?
esac
"#;
    for engine in ["claude-code", "codex"] {
        let fixture = Fixture::new(engine, 30, &format!("{discover}\n{RECORD_FIRST_AUDIT}"));
        std::fs::write(
            &fixture.config,
            std::fs::read_to_string(&fixture.config).unwrap()
                + "graph:\n  audit:\n    batch_size: 1\n",
        )
        .unwrap();
        let documents = fixture.root().join("vault/wiki/documents");
        std::fs::create_dir_all(&documents).unwrap();
        for (name, days) in [("a", 30), ("b", 7)] {
            std::fs::write(documents.join(format!("{name}.md")), format!("---\ntype: document\ntitle: Policy {name}\n---\nProduction order retention is exactly {days} days, effective 2026-10-01.\n")).unwrap();
        }
        let plan: serde_json::Value = serde_json::from_slice(
            &fixture
                .command()
                .args(["wiki", "audit-plan", "--path", "wiki/documents/a.md"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        let candidate = &plan["candidates"][0];
        let unsearched = fixture.root().join("unsearched-assessment.json");
        std::fs::write(&unsearched, serde_json::to_vec(&serde_json::json!({"assessments":[{
            "path":candidate["path"], "file_revision":candidate["file_revision"], "input_revision":candidate["input_revision"],
            "disposition":"reviewed", "reason":"A source was read without searching for related material.", "reviews":[]
        }]})).unwrap()).unwrap();
        let refused = fixture
            .command()
            .args(["wiki", "audit-record", "--file"])
            .arg(unsearched)
            .output()
            .unwrap();
        assert!(!refused.status.success());
        assert!(String::from_utf8_lossy(&refused.stderr).contains("related-document discovery"));
        assert!(
            !fixture
                .root()
                .join("vault/.lorekeeper/knowledge-audit.json")
                .exists()
        );
        let (output, result) = fixture.run("knowledge");
        assert!(
            output.status.success(),
            "{}\n{}",
            result,
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(result["semantic_calls"], 2);
        let selected: serde_json::Value = serde_json::from_slice(
            &fixture
                .command()
                .args(["wiki", "audit-plan", "--path", "wiki/documents/a.md"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        let candidate = &selected["candidates"][0];
        assert_eq!(candidate["evidence"], serde_json::json!([]));
        assert_eq!(candidate["related_pages"][0]["path"], "wiki/documents/b.md");
        assert_eq!(
            candidate["discovery"]["matches"],
            candidate["related_pages"]
        );
        assert_eq!(selected["reviewed_current"], 1);
    }
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
        let fixture = Fixture::new(
            engine,
            30,
            r#"python3 - "$@" <<'CHECK'
import json, os, sys
from pathlib import Path
prompt='\n'.join(sys.argv[1:])
selection,_=json.JSONDecoder().raw_decode(prompt.split('Selected queue task manifest: ',1)[1])
path=Path(selection)
assert isinstance(json.loads(path.read_text()),list)
granted=[Path(sys.argv[index+1]).resolve() for index,arg in enumerate(sys.argv[:-1]) if arg=='--add-dir']
assert path.parent.resolve() in granted, 'selection must be readable inside an explicitly granted directory'
(Path(os.environ['TEST_EXECUTION_ROOT'])/'selection-path').write_text(str(path))
CHECK
"#,
        );
        fixture.ingest();
        let (output, result) = fixture.run("process");
        assert!(!output.status.success());
        assert_eq!(result["ok"], false);
        assert_eq!(result["execution"]["engine"], engine);
        let selection = std::fs::read_to_string(fixture.root().join("selection-path")).unwrap();
        assert!(
            !Path::new(&selection).parent().unwrap().exists(),
            "selection directories must retire after the consumer exits"
        );
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
fn knowledge_audit_progresses_before_unrelated_queue_backlog_and_reports_it_unfinished() {
    let behavior = RECORD_FIRST_AUDIT.replace(
        "wiki audit-plan >",
        "wiki audit-plan --path wiki/concepts/stable.md >",
    );
    let fixture = Fixture::new("codex", 30, &behavior);
    fixture.ingest();
    write_audit_concept(&fixture, "stable");
    let config = std::fs::read_to_string(&fixture.config).unwrap()
        .replace("  default_profile: primary\n", "  default_profile: primary\n  work: {max_input_bytes: 262144, max_result_bytes: 65536, max_calls: 1}\n");
    std::fs::write(&fixture.config, config).unwrap();
    let (output, result) = fixture.run("knowledge");
    assert!(
        !output.status.success(),
        "global pending work must remain unfinished: {result}"
    );
    assert_eq!(result["semantic_calls"], 1, "{result}");
    assert_eq!(
        result["audit"].as_array().unwrap().len(),
        1,
        "{result}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        result["audit"][0]["assessed_path"],
        "wiki/concepts/stable.md"
    );
    assert!(
        result["audit_deferred"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["path"]
                .as_str()
                .unwrap()
                .starts_with("wiki/documents/"))
    );
    assert!(
        result["stages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|stage| stage["name"] == "queue completion" && stage["state"] == "failed")
    );
    let status = fixture
        .command()
        .args(["wiki", "audit-plan", "--path", "wiki/concepts/stable.md"])
        .output()
        .unwrap();
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["reviewed_current"], 1);
}

#[test]
fn knowledge_defers_selected_evidence_with_unfinished_semantics_and_record_rechecks_it() {
    let fixture = Fixture::new("codex", 30, "exit 0");
    write_audit_concept(&fixture, "stable");
    let source = fixture.root().join("vault/wiki/documents/source.md");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::write(&source, "---\ntype: document\ntitle: Supporting evidence\nllm_inputs: {summary: pending}\n---\n[stable](../concepts/stable.md)\n\n## Summary\nUnfinished interpretation.\n").unwrap();
    let (output, result) = fixture.run("knowledge");
    assert!(!output.status.success(), "{result}");
    assert!(result["audit"].is_null());
    if let Ok(arguments) = std::fs::read_to_string(fixture.root().join("arguments")) {
        assert!(
            !arguments.contains("maintain."),
            "blocked inputs must not be assessed: {arguments}"
        );
    }
    assert!(
        result["audit_deferred"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["path"] == "wiki/concepts/stable.md"
                && item["reasons"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|reason| reason
                        .as_str()
                        .unwrap()
                        .contains("source.md: unfinished summary")))
    );
    let planned = fixture
        .command()
        .args(["wiki", "audit-plan", "--path", "wiki/concepts/stable.md"])
        .output()
        .unwrap();
    let planned: serde_json::Value = serde_json::from_slice(&planned.stdout).unwrap();
    let candidate = &planned["candidates"][0];
    let request = fixture.root().join("assessment.json");
    std::fs::write(&request, serde_json::to_vec(&serde_json::json!({"assessments":[{
        "path":candidate["path"], "file_revision":candidate["file_revision"], "input_revision":candidate["input_revision"],
        "disposition":"reviewed", "reason":"Attempted assessment of unfinished evidence.", "reviews":[]
    }]})).unwrap()).unwrap();
    let refused = fixture
        .command()
        .args(["wiki", "audit-record", "--file"])
        .arg(request)
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("unfinished semantic work"));
    let retained: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            fixture
                .root()
                .join("vault/.lorekeeper/knowledge-audit.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(retained["entries"].as_object().unwrap().is_empty());
}

#[test]
fn current_authored_assessment_does_not_hide_unfinished_bookkeeping_or_force_a_reread() {
    let fixture = Fixture::new("codex", 30, RECORD_FIRST_AUDIT);
    let path = fixture.root().join("vault/wiki/documents/subject.md");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let ready = "---\ntype: document\ntitle: Subject\nllm_inputs: {summary: captured, summary_done: captured}\n---\n## Summary\nA completed interpretation.\n";
    std::fs::write(&path, ready).unwrap();
    let (first, report) = fixture.run("knowledge");
    assert!(
        first.status.success(),
        "{report}\n{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let receipt_path = fixture
        .root()
        .join("vault/.lorekeeper/knowledge-audit.json");
    assert!(report["semantic_duration_ms"].as_u64().unwrap() > 0);
    let receipt = std::fs::read(&receipt_path).unwrap();
    let pending = ready.replace(", summary_done: captured", "");
    assert_eq!(
        lk_core::concept::source_revision(&lk_vault::parse_page(ready).unwrap()),
        lk_core::concept::source_revision(&lk_vault::parse_page(&pending).unwrap())
    );
    std::fs::write(&path, pending).unwrap();
    let (second, report) = fixture.run("knowledge");
    assert!(
        !second.status.success(),
        "an unchanged receipt must not hide unfinished inputs: {report}"
    );
    assert_eq!(report["semantic_calls"], 0);
    assert!(
        report["audit_deferred"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["path"] == "wiki/documents/subject.md")
    );
    assert_eq!(std::fs::read(&receipt_path).unwrap(), receipt);
    std::fs::write(&path, ready).unwrap();
    let (third, report) = fixture.run("knowledge");
    assert!(
        third.status.success(),
        "{report}\n{}",
        String::from_utf8_lossy(&third.stderr)
    );
    assert_eq!(
        report["semantic_calls"], 0,
        "restored bookkeeping does not require a new semantic judgment"
    );
    assert!(report["audit"].as_array().unwrap().is_empty());
    assert_eq!(std::fs::read(receipt_path).unwrap(), receipt);
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
    assert_eq!(result["audit_progress"]["before"]["unreviewed"], 1);
    assert!(result["audit_progress"]["after"].is_null());
    assert!(
        result["stages"]
            .as_array()
            .unwrap()
            .iter()
            .any(
                |stage| stage["name"] == "related-document discovery" && stage["state"] == "failed"
            ),
        "{}\n{}",
        result,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(fixture.root().join("arguments").exists());
    let reserved: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            fixture
                .root()
                .join("vault/.lorekeeper/knowledge-audit.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(reserved["entries"], serde_json::json!({}));
    assert_eq!(
        reserved["pending"],
        serde_json::json!(["wiki/concepts/unverified.md"])
    );
    assert!(reserved.get("discovery").is_none());
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
    assert!(
        result["duration_ms"].as_u64().unwrap()
            >= runs
                .iter()
                .map(|run| run["duration_ms"].as_u64().unwrap())
                .sum()
    );
    for (index, run) in runs.iter().enumerate() {
        let progress = &run["audit_progress"];
        assert_eq!(progress["before"]["reviewed_current"], index);
        assert_eq!(progress["after"]["reviewed_current"], index + 1);
        assert_eq!(progress["before"]["total"], progress["after"]["total"]);
        assert_eq!(
            progress["before"]["unreviewed"].as_u64().unwrap(),
            progress["after"]["unreviewed"].as_u64().unwrap() + 1
        );
        assert_eq!(run["semantic_calls"], 1);
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
    std::fs::write(
        &fixture.config,
        std::fs::read_to_string(&fixture.config).unwrap() + "graph:\n  audit:\n    batch_size: 1\n",
    )
    .unwrap();
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
        .args(["wiki", "audit-plan", "--limit", "2"])
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
        assert len(request['children'])>=2
        row['child_count']=len(request['children'])
    unresolved=[concern for child in request.get('children', []) for concern in child['output']['unresolved']]
    output={'work_id':work_id, 'node_id':node_id, 'input_revision':request['input_revision'],
            'summary':'Fixture observation of this exact request; structural coverage is not factual verification.',
            'findings':[], 'unresolved':unresolved, 'resolutions':[]}
    row['output_bytes']=len(json.dumps(output, ensure_ascii=False, separators=(',', ':')).encode())
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
    record_discovery(&fixture, "wiki/concepts/large.md");
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
            "      work: {extraction_context: full-context}\n",
            "      work: {max_input_bytes: 16384, max_result_bytes: 2048, max_calls: 1, extraction_context: full-context}\n",
        );
        std::fs::write(&fixture.config, config).unwrap();
        let (output, result) = fixture.run("knowledge");
        assert!(
            !output.status.success(),
            "one call cannot finish this audit"
        );
        assert_eq!(
            result["execution"]["work"],
            serde_json::json!({"max_input_bytes":16384,"max_result_bytes":2048,"max_calls":1,"max_parallel":1,"extraction_context":"full-context"})
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
        let steps = result["work_steps"].as_array().unwrap();
        assert_eq!(steps.len(), 1, "no invented step after budget exhaustion");
        assert_eq!(steps[0]["outcome"], "recorded");
        assert_eq!(steps[0]["role"], "evidence");
        assert_eq!(steps[0]["work_id"], calls[0]["work_id"]);
        assert_eq!(
            steps[0]["agent_duration_ms"],
            result["semantic_duration_ms"]
        );
        assert_eq!(result["semantic_calls"], 1);
        assert_eq!(result["work_steps_omitted"], 0);
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
        let receipts: serde_json::Value = serde_json::from_slice(
            &std::fs::read(
                fixture
                    .root()
                    .join("vault/.lorekeeper/knowledge-audit.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(receipts["entries"].as_object().unwrap().is_empty());
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
    fixture.ingest();
    let pending = fixture.command().args(["queue", "check"]).output().unwrap();
    assert!(!pending.status.success());
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
        let path = fixture.root().join("vault/wiki/concepts/large.md");
        let body = std::fs::read_to_string(&path).unwrap()
            + &"\nEvidence clause with Unicode 한🙂 remains source scoped.\n".repeat(360);
        std::fs::write(path, body).unwrap();
        record_discovery(&fixture, "wiki/concepts/large.md");
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
        let steps = result["work_steps"].as_array().unwrap();
        assert_eq!(steps.len(), calls.len());
        let mut invocation_ms = 0;
        for (step, call) in steps.iter().zip(&calls) {
            assert_eq!(step["work_id"], call["work_id"]);
            assert!(step["node_id"].as_str().is_some());
            assert!(step["preparation_duration_ms"].as_u64().is_some());
            invocation_ms += step["agent_duration_ms"].as_u64().unwrap();
            if call["action"] == "finish" {
                assert_eq!(step["role"], "finalizer");
                assert_eq!(step["outcome"], "staged");
                assert!(step["request_json_bytes"].is_null());
                assert!(step["result_json_bytes"].is_null());
                assert!(step["revalidation_record_duration_ms"].is_null());
                assert!(step["verification_duration_ms"].as_u64().is_some());
            } else {
                assert_eq!(step["role"], "evidence");
                assert_eq!(step["outcome"], "recorded");
                assert_eq!(step["node_id"], call["node_id"]);
                assert_eq!(step["node_kind"], call["kind"]);
                assert_eq!(step["request_json_bytes"], call["input_bytes"]);
                assert_eq!(step["result_json_bytes"], call["output_bytes"]);
                assert!(step["revalidation_record_duration_ms"].as_u64().is_some());
                assert!(step["verification_duration_ms"].is_null());
            }
        }
        assert_eq!(
            invocation_ms,
            result["semantic_duration_ms"].as_u64().unwrap()
        );
        assert_eq!(result["work_steps_omitted"], 0);
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
                .any(|call| call["child_count"].as_u64().is_some_and(|count| count > 2))
        );
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
        assert!(inspected.len() < status["parts"].as_u64().unwrap() as usize * 2 - 1);
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
    let step = &result["work_steps"][0];
    assert_eq!(result["work_steps"].as_array().unwrap().len(), 1);
    assert_eq!(step["outcome"], "rejected");
    assert_ne!(step["node_id"], "not-selected");
    assert_eq!(step["result_json_bytes"], calls[0]["output_bytes"]);
    assert!(step["revalidation_record_duration_ms"].as_u64().is_some());
    assert!(step["agent_duration_ms"].as_u64().is_some());
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
fn bounded_telemetry_reports_failed_invocations_without_inventing_results() {
    let fixture = bounded_audit_fixture("codex", 1, "exit 23");
    let (output, report) = fixture.run("knowledge");
    assert!(!output.status.success());
    assert_eq!(report["semantic_calls"], 1);
    let steps = report["work_steps"].as_array().unwrap();
    assert_eq!(steps.len(), 1);
    let step = &steps[0];
    assert_eq!(step["role"], "evidence");
    assert_eq!(step["outcome"], "failed");
    assert!(step["preparation_duration_ms"].as_u64().is_some());
    assert!(step["agent_duration_ms"].as_u64().is_some());
    assert_eq!(step["agent_duration_ms"], report["semantic_duration_ms"]);
    assert!(step["request_json_bytes"].as_u64().is_some());
    assert!(step["result_json_bytes"].is_null());
    assert!(step["revalidation_record_duration_ms"].is_null());
    assert!(!step.to_string().contains("Evidence clause"));
    assert!(!step.to_string().contains("test@example.com"));

    std::fs::write(
        fixture.root().join("agent executable"),
        "#!/bin/sh\nexit 23\n",
    )
    .unwrap();
    let (output, report) = fixture.run("knowledge");
    assert!(!output.status.success());
    assert_eq!(report["semantic_calls"], 0);
    let steps = report["work_steps"].as_array().unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0]["outcome"], "failed");
    assert!(steps[0]["preparation_duration_ms"].as_u64().is_some());
    assert!(steps[0]["agent_duration_ms"].is_null());
    assert!(steps[0]["result_json_bytes"].is_null());
    assert!(steps[0]["revalidation_record_duration_ms"].is_null());
}

#[test]
fn bounded_telemetry_rejects_finalizer_exit_zero_without_a_sealed_result() {
    let behavior = RECORD_BOUNDED_AUDIT.replace(
        "    request=cli('work', 'result', work_id)",
        "    raise SystemExit(0)\n    request=cli('work', 'result', work_id)",
    );
    let fixture = bounded_audit_fixture("codex", 64, &behavior);
    let (output, report) = fixture.run("knowledge");
    assert!(!output.status.success());
    let steps = report["work_steps"].as_array().unwrap();
    let step = steps.last().unwrap();
    assert_eq!(step["role"], "finalizer");
    assert_eq!(step["outcome"], "rejected");
    assert!(step["agent_duration_ms"].as_u64().is_some());
    assert!(step["verification_duration_ms"].as_u64().is_some());
    assert!(step["request_json_bytes"].is_null());
    assert!(step["result_json_bytes"].is_null());
    assert!(step["revalidation_record_duration_ms"].is_null());
    assert_eq!(
        steps
            .iter()
            .map(|step| step["agent_duration_ms"].as_u64().unwrap())
            .sum::<u64>(),
        report["semantic_duration_ms"].as_u64().unwrap(),
    );
    let receipts: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            fixture
                .root()
                .join("vault/.lorekeeper/knowledge-audit.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(receipts["entries"].as_object().unwrap().is_empty());
}

#[test]
fn bounded_telemetry_preserves_cancelled_attempt_and_unfinished_work() {
    let fixture = bounded_audit_fixture(
        "codex",
        1,
        "printf 'started' > \"$TEST_EXECUTION_ROOT/started\"\nexec sleep 30",
    );
    let stdout = std::fs::File::create(fixture.root().join("result.json")).unwrap();
    let stderr = std::fs::File::create(fixture.root().join("stderr.log")).unwrap();
    let mut child = fixture
        .command()
        .args(["run", "knowledge", "--json"])
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
    let pid = rustix::process::Pid::from_raw(i32::try_from(child.id()).unwrap()).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).unwrap();
    while child.try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < deadline,
            "cancelled controller did not stop"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture.root().join("result.json")).unwrap())
            .unwrap();
    assert_eq!(report["cancelled"], true, "{report}");
    assert_eq!(report["semantic_calls"], 1);
    let steps = report["work_steps"].as_array().unwrap();
    assert_eq!(steps.len(), 1);
    let step = &steps[0];
    assert_eq!(step["outcome"], "cancelled");
    assert!(step["agent_duration_ms"].as_u64().is_some());
    assert_eq!(step["agent_duration_ms"], report["semantic_duration_ms"]);
    assert!(step["result_json_bytes"].is_null());
    assert!(step["revalidation_record_duration_ms"].is_null());
    let status = execution_json(
        &fixture,
        &["work", "status", step["work_id"].as_str().unwrap()],
    );
    assert_eq!(status["completed"], 0);
    assert_eq!(status["complete"], false);
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
    record_discovery(&fixture, "wiki/concepts/second.md");
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

fn write_synthesis_pair(
    fixture: &Fixture,
    name: &str,
    pending_summary: Option<&str>,
    repetitions: usize,
) {
    let root = fixture.root().join("vault");
    std::fs::create_dir_all(root.join("wiki/documents")).unwrap();
    std::fs::create_dir_all(root.join("wiki/concepts")).unwrap();
    let inputs = pending_summary
        .map(|hash| format!("llm_inputs:\n  summary: {hash}\n"))
        .unwrap_or_default();
    std::fs::write(
        root.join(format!("wiki/documents/{name}-source.md")),
        format!(
            "---\ntype: document\ntitle: {name} source\n{inputs}---\n## Summary\nSource interpretation.\n\n## Content\n{}\n## Related Concepts\n- [{name}](../concepts/{name}.md)\n",
            "A source-scoped observation retains its conditions and attribution.\n".repeat(repetitions)
        ),
    )
    .unwrap();
    std::fs::write(
        root.join(format!("wiki/concepts/{name}.md")),
        format!(
            "---\ntype: concept\ntitle: {name}\nllm_inputs:\n  synthesis: previous\n---\n## Synthesis\nEstablished source-scoped knowledge.\n\n## Sources\n\n## Related\n"
        ),
    )
    .unwrap();
}

fn planned_queue_subjects(fixture: &Fixture) -> Vec<serde_json::Value> {
    let directory = fixture.root().join("vault/.lorekeeper/work");
    if !directory.exists() {
        return Vec::new();
    }
    std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| {
            let raw = std::fs::read(entry.unwrap().path().join("plan.json")).unwrap();
            let plan: serde_json::Value = serde_json::from_slice(&raw).unwrap();
            plan["context"]["subject"].clone()
        })
        .collect()
}

#[test]
fn synthesis_waits_for_source_publication_before_preparing_bounded_work() {
    let behavior = r#"
exec python3 - "$@" <<'SELECTED'
import json, os, subprocess, sys
from pathlib import Path
root=Path(os.environ['TEST_EXECUTION_ROOT'])
prompt='\n'.join(sys.argv[1:])
path,_=json.JSONDecoder().raw_decode(prompt.split('Selected queue task manifest: ',1)[1])
ids=json.loads(Path(path).read_text())
lore=os.environ['LORE_BIN']
rows={row['task_id']:row for row in json.loads(subprocess.check_output([lore,'queue','status','--json']))['data']['tasks']}
tasks=[json.loads(line) for file in (root/'vault/.lorekeeper/queue').glob('*.jsonl') for line in file.read_text().splitlines()]
selected=[task for task in tasks if task['task_id'] in ids]
assert len(selected)==1 and selected[0]['kind']=='summarize', selected
assert not (root/'vault/.lorekeeper/work').exists(), 'dependent synthesis must not be prepared before source publication'
task=selected[0]
result={key:task[key] for key in ['task_id','cache_hash']}
result.update(target_revision=rows[task['task_id']]['target_revision'],body='Completed the source interpretation before dependent synthesis.')
results=root/'vault/.lorekeeper/queue/results'
results.mkdir(exist_ok=True)
(results/(task['task_id']+'.section.json')).write_text(json.dumps(result))
SELECTED
"#;
    for (engine, repetitions) in [
        ("claude-code", 1),
        ("codex", 1),
        ("claude-code", 400),
        ("codex", 400),
    ] {
        let fixture = Fixture::new(engine, 30, behavior);
        let config = std::fs::read_to_string(&fixture.config).unwrap().replace(
            "  default_profile: primary\n",
            "  default_profile: primary\n  work: {max_input_bytes: 8192, max_result_bytes: 1024, max_calls: 1}\n",
        );
        std::fs::write(&fixture.config, config).unwrap();
        let queue = fixture.root().join("vault/.lorekeeper/queue");
        let client = lk_queue::QueueLlmClient::new(queue.clone());
        let summary = client.build_task(
            "summary",
            lk_queue::TaskKind::Summarize,
            lk_queue::SummarizeRequest {
                text: "A source-scoped observation retains its conditions and attribution.".into(),
                max_sentences: 3,
                focus: None,
                locale: lk_core::i18n::Locale::En,
                source_type: None,
                target: lk_queue::TaskTarget {
                    vault_path: "wiki/documents/a-dependent-source.md".into(),
                    kind: lk_queue::TargetKind::DocumentSummary,
                    anchor: "## Summary".into(),
                },
            },
        );
        write_synthesis_pair(
            &fixture,
            "a-dependent",
            Some(&summary.cache_hash),
            repetitions,
        );
        std::fs::create_dir_all(&queue).unwrap();
        std::fs::write(
            queue.join("source.jsonl"),
            serde_json::to_string(&summary).unwrap() + "\n",
        )
        .unwrap();
        let prepared = fixture
            .command()
            .args(["graph", "backlinks-sync"])
            .output()
            .unwrap();
        assert!(prepared.status.success(), "{engine}: {prepared:?}");
        let status = fixture
            .command()
            .args(["queue", "status", "--json"])
            .output()
            .unwrap();
        let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
        let original = status["data"]["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|task| task["kind"] == "synthesize-concept")
            .unwrap()["task_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let (output, result) = fixture.run("process");
        assert!(
            !output.status.success(),
            "one call retains synthesis: {result}"
        );
        assert_eq!(result["semantic_calls"], 1, "{engine}: {result}");
        let status = fixture
            .command()
            .args(["queue", "status", "--json"])
            .output()
            .unwrap();
        let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
        let rows = status["data"]["tasks"].as_array().unwrap();
        assert!(
            rows.iter()
                .any(|task| task["task_id"] == summary.task_id && task["status"] == "done"),
            "{engine}: {status}"
        );
        assert!(
            rows.iter()
                .any(|task| task["task_id"] == original && task["status"] == "stale"),
            "{engine}: {status}"
        );
        assert!(
            rows.iter().any(|task| task["kind"] == "synthesize-concept"
                && task["task_id"] != original
                && task["status"] == "current"),
            "{engine}: {status}"
        );
        assert!(
            planned_queue_subjects(&fixture)
                .iter()
                .all(|subject| subject["task_id"] != original)
        );
    }
}

#[test]
fn independent_synthesis_progresses_while_another_sources_publication_is_blocked() {
    let fixture = Fixture::new("codex", 30, RECORD_BOUNDED_AUDIT);
    let config = std::fs::read_to_string(&fixture.config).unwrap().replace(
        "  default_profile: primary\n",
        "  default_profile: primary\n  work: {max_input_bytes: 8192, max_result_bytes: 1024, max_calls: 1}\n",
    );
    std::fs::write(&fixture.config, config).unwrap();
    write_synthesis_pair(&fixture, "a-blocked", Some("pending"), 400);
    write_synthesis_pair(&fixture, "z-ready", None, 400);
    for _ in 0..2 {
        let (output, result) = fixture.run("process");
        assert!(
            !output.status.success(),
            "bounded work remains pending: {result}"
        );
        assert_eq!(result["semantic_calls"], 1, "{result}");
        let subjects = planned_queue_subjects(&fixture);
        assert!(!subjects.is_empty());
        assert!(
            subjects
                .iter()
                .all(|subject| subject["target"] == "wiki/concepts/z-ready.md")
        );
    }
    let calls = bounded_calls(&fixture);
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["work_id"], calls[1]["work_id"]);
    assert_ne!(calls[0]["node_id"], calls[1]["node_id"]);
}

#[test]
fn blocked_synthesis_reports_dependencies_without_planning_or_launching_a_host() {
    let fixture = Fixture::new("codex", 30, "exit 0");
    write_synthesis_pair(&fixture, "blocked", Some("pending"), 400);
    let (output, result) = fixture.run("process");
    assert!(!output.status.success(), "{result}");
    assert_eq!(result["semantic_calls"], 0, "{result}");
    assert!(
        result["stages"].as_array().unwrap().iter().any(|stage| {
            stage["name"] == "queue dependencies"
                && stage["state"] == "failed"
                && stage["detail"]
                    .as_str()
                    .is_some_and(|detail| detail.contains("blocked-source.md: unfinished summary"))
        }),
        "{result}"
    );
    assert!(!fixture.root().join("arguments").exists());
    assert!(planned_queue_subjects(&fixture).is_empty());
}

fn build_extraction_behavior(discovery_handler: &str) -> String {
    let mut behavior = RECORD_BOUNDED_AUDIT.replace(
        "    if match is None:\n        raise SystemExit('expected an exact bounded native request')",
        discovery_handler,
    );
    behavior = behavior.replace(
        "        assert all('text' in item for item in request['input']['ranges'])",
        "        assert all('text' in item for item in request['input']['ranges'])\n        row['addresses']=[item['address'] for item in request['input']['ranges']]",
    );
    let start = behavior
        .find("    assert subject['kind']=='audit'")
        .unwrap();
    let end = behavior[start..].find("    row={'action':action").unwrap() + start;
    behavior.replace_range(start..end, r#"    assert subject['kind']=='queue'
    context=request['context']['purpose']
    result={key:subject[key] for key in ['task_id','cache_hash','target_revision']}
    result.update(context_revision=cli('work','status',work_id)['context_revision'],
                  discovery=context['discovery']['queries'],concept_policy=context['parameters']['concept_policy'],
                  target=context['target'],date=context['parameters']['date'],concepts=[])
    path=root/'extraction-result.json'
    path.write_text(json.dumps(result))
    cli('work','finish',work_id,'--file',str(path))
"#);
    behavior
}

#[test]
fn oversized_extraction_records_discovery_before_planning_on_both_hosts() {
    let behavior = build_extraction_behavior(
        r#"    if match is None:
        match=re.search(r'discover ("[^"\n]+")', '\n'.join(sys.argv[1:]))
        assert match is not None
        assert not (root/'vault/.lorekeeper/work').exists(), 'discovery must precede bounded planning'
        selection=Path(json.loads(match.group(1)))
        granted=[Path(sys.argv[index+1]).resolve() for index,arg in enumerate(sys.argv[:-1]) if arg=='--add-dir']
        assert selection.parent.resolve() in granted, 'discovery selection requires explicit directory access'
        ids=json.loads(selection.read_text())
        lore=os.environ['LORE_BIN']
        rows=json.loads(subprocess.check_output([lore,'queue','status','--json']))['data']['tasks']
        for task_id in ids:
            row=next(row for row in rows if row['task_id']==task_id)
            request={'target_revision':row['target_revision'],'queries':[{'query':'transactional outbox','phrase':True,'spacing':False}]}
            path=root/'extraction-discovery.json'
            path.write_text(json.dumps(request))
            subprocess.run([lore,'work','discover','--task-id',task_id,'--file',str(path)],check=True)
        (root/'discovery-called').write_text('recorded')
        raise SystemExit(0)"#,
    );
    for engine in ["claude-code", "codex"] {
        let fixture = Fixture::new(engine, 30, &behavior);
        let config = std::fs::read_to_string(&fixture.config).unwrap().replace(
            "  default_profile: primary\n",
            "  default_profile: primary\n  work: {max_input_bytes: 8192, max_result_bytes: 1024, max_calls: 64}\n",
        );
        std::fs::write(&fixture.config, config).unwrap();
        let root = fixture.root().join("vault");
        let source = "Transactional outbox commits an event with its state change.\n\n".repeat(400);
        let request = lk_queue::ExtractConceptsRequest {
            concept_policy: lk_core::config::ConceptPolicy::SupportedNew,
            rss_original: None,
            text: source.clone(),
            source_id: "notes".into(),
            source_type: lk_core::config::SourceType::Manual,
            date: jiff::civil::date(2026, 10, 20),
            focus: None,
            locale: lk_core::i18n::Locale::En,
            categories: Vec::new(),
            target: lk_queue::TaskTarget {
                vault_path: "wiki/documents/source.md".into(),
                kind: lk_queue::TargetKind::DocumentConcepts,
                anchor: "## Related Concepts".into(),
            },
        };
        let queue = root.join(".lorekeeper/queue");
        let client = lk_queue::QueueLlmClient::new(queue.clone());
        let task = client.build_task("ext", lk_queue::TaskKind::ExtractConcepts, request);
        std::fs::create_dir_all(root.join("wiki/documents")).unwrap();
        std::fs::write(root.join("wiki/documents/source.md"), format!("---\ntype: document\ntitle: Source\nllm_inputs:\n  concepts: {}\n---\n## Summary\nSource observation.\n\n## Content\n{source}\n\n## Related Concepts\n", task.cache_hash)).unwrap();
        std::fs::write(root.join("wiki/documents/related.md"), "---\ntype: document\ntitle: Related\n---\n## Summary\nA transactional outbox has source-scoped delivery assumptions.\n").unwrap();
        std::fs::create_dir_all(&queue).unwrap();
        std::fs::write(
            queue.join("fixture.jsonl"),
            serde_json::to_string(&task).unwrap() + "\n",
        )
        .unwrap();
        let (output, result) = fixture.run("process");
        assert!(output.status.success(), "{engine}: {result}");
        assert!(fixture.root().join("discovery-called").exists());
        let calls = bounded_calls(&fixture);
        assert!(calls.iter().any(|call| call["action"] == "finish"));
        assert!(calls.iter().any(
            |call| call["addresses"].as_array().is_some_and(|addresses| {
                addresses
                    .iter()
                    .any(|address| address == "wiki/documents/related.md")
            })
        ));
        assert!(
            std::fs::read_to_string(root.join("wiki/documents/source.md"))
                .unwrap()
                .contains("concepts_done:")
        );
    }
}

#[test]
fn small_extraction_hands_expanded_discovery_to_bounded_work_on_both_hosts() {
    let behavior = build_extraction_behavior(
        r#"    if match is None:
        prompt='\n'.join(sys.argv[1:])
        path,_=json.JSONDecoder().raw_decode(prompt.split('Selected queue task manifest: ',1)[1])
        ids=json.loads(Path(path).read_text())
        lore=os.environ['LORE_BIN']
        assert not (root/'vault/.lorekeeper/work').exists(), 'small retained inputs must start in the ordinary selected batch'
        rows={row['task_id']:row for row in json.loads(subprocess.check_output([lore,'queue','status','--json']))['data']['tasks']}
        tasks=[json.loads(line) for file in (root/'vault/.lorekeeper/queue').glob('*.jsonl') for line in file.read_text().splitlines()]
        selected=[task for task in tasks if task['task_id'] in ids]
        assert len(selected)==2
        results=root/'vault/.lorekeeper/queue/results'
        results.mkdir(exist_ok=True)
        for task in selected:
            row=rows[task['task_id']]
            if task['kind']=='extract-concepts':
                request={'target_revision':row['target_revision'],'queries':[{'query':'transactional outbox','phrase':True,'spacing':False}]}
                file=root/'extraction-discovery.json'
                file.write_text(json.dumps(request))
                subprocess.run([lore,'work','discover','--task-id',task['task_id'],'--file',str(file)],check=True)
                context=json.loads(subprocess.check_output([lore,'work','context','--task-id',task['task_id']]))
                if context['requires_bounded_work']:
                    assert context['resources'] is None
                    assert context['resource_count']>=2 and context['part_count']>1
                    plan=json.loads(subprocess.check_output([lore,*context['plan_command'][1:]]))
                    assert plan['parts']>1
            else:
                assert task['kind']=='summarize'
                result={key:task[key] for key in ['task_id','cache_hash']}
                result.update(target_revision=row['target_revision'],body='Completed the other selected source.')
                (results/(task['task_id']+'.section.json')).write_text(json.dumps(result))
        (root/'selected-handoff.json').write_text(json.dumps(ids))
        raise SystemExit(0)"#,
    );
    for engine in ["claude-code", "codex"] {
        for expands in [true, false] {
            let fixture = Fixture::new(engine, 30, &behavior);
            let config = std::fs::read_to_string(&fixture.config).unwrap().replace(
                "  default_profile: primary\n",
                "  default_profile: primary\n  work: {max_input_bytes: 8192, max_result_bytes: 1024, max_calls: 64}\n",
            );
            std::fs::write(&fixture.config, config).unwrap();
            let root = fixture.root().join("vault");
            let queue = root.join(".lorekeeper/queue");
            let client = lk_queue::QueueLlmClient::new(queue.clone());
            let task = client.build_task(
                "ext",
                lk_queue::TaskKind::ExtractConcepts,
                lk_queue::ExtractConceptsRequest {
                    concept_policy: lk_core::config::ConceptPolicy::SupportedNew,
                    rss_original: None,
                    text: "A transactional outbox commits an event with its state change.".into(),
                    source_id: "notes".into(),
                    source_type: lk_core::config::SourceType::Manual,
                    date: jiff::civil::date(2026, 10, 20),
                    focus: None,
                    locale: lk_core::i18n::Locale::En,
                    categories: vec![],
                    target: lk_queue::TaskTarget {
                        vault_path: "wiki/documents/source.md".into(),
                        kind: lk_queue::TargetKind::DocumentConcepts,
                        anchor: "## Related Concepts".into(),
                    },
                },
            );
            let summary = client.build_task(
                "sum",
                lk_queue::TaskKind::Summarize,
                lk_queue::SummarizeRequest {
                    text: "A separate short source.".into(),
                    max_sentences: 3,
                    focus: None,
                    locale: lk_core::i18n::Locale::En,
                    source_type: None,
                    target: lk_queue::TaskTarget {
                        vault_path: "wiki/documents/summary.md".into(),
                        kind: lk_queue::TargetKind::DocumentSummary,
                        anchor: "## Summary".into(),
                    },
                },
            );
            std::fs::create_dir_all(root.join("wiki/documents")).unwrap();
            std::fs::write(root.join("wiki/documents/source.md"), format!("---\ntype: document\nllm_inputs:\n  concepts: {}\n---\n## Content\nShort source.\n\n## Related Concepts\n",task.cache_hash)).unwrap();
            std::fs::write(
                root.join("wiki/documents/summary.md"),
                format!(
                    "---\ntype: document\nllm_inputs:\n  summary: {}\n---\n## Summary\n",
                    summary.cache_hash
                ),
            )
            .unwrap();
            std::fs::write(
                root.join("wiki/documents/related.md"),
                format!(
                    "---\ntype: document\n---\n{}",
                    "A transactional outbox has source-scoped delivery assumptions.\n\n"
                        .repeat(if expands { 400 } else { 1 })
                ),
            )
            .unwrap();
            std::fs::create_dir_all(&queue).unwrap();
            std::fs::write(
                queue.join("fixture.jsonl"),
                [
                    serde_json::to_string(&task).unwrap(),
                    serde_json::to_string(&summary).unwrap(),
                ]
                .join("\n")
                    + "\n",
            )
            .unwrap();
            let (output, result) = fixture.run("process");
            assert_eq!(
                output.status.success(),
                expands,
                "{engine}, expands={expands}: {result}"
            );
            assert!(fixture.root().join("selected-handoff.json").exists());
            assert!(
                std::fs::read_to_string(root.join("wiki/documents/summary.md"))
                    .unwrap()
                    .contains("summary_done:")
            );
            assert_eq!(
                std::fs::read_to_string(root.join("wiki/documents/source.md"))
                    .unwrap()
                    .contains("concepts_done:"),
                expands
            );
            if expands {
                assert!(
                    bounded_calls(&fixture)
                        .iter()
                        .any(|call| call["action"] == "finish")
                );
                assert!(
                    result["stages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|stage| stage["name"] == "expanded extraction work")
                );
            } else {
                assert!(bounded_calls(&fixture).is_empty());
                assert!(
                    result["stages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|stage| stage["name"] == "selected queue receipt"
                            && stage["state"] == "failed")
                );
            }
        }
    }
}

const RECORD_PROGRESSIVE_EXTRACTION: &str = r#"
case "$*" in
  *--output-schema*|*--safe-mode*) /bin/cat > "$TEST_EXECUTION_ROOT/native-request.txt" ;;
esac
exec python3 - "$@" <<'PROGRESSIVE'
import json, os, re, subprocess, sys
from pathlib import Path
root=Path(os.environ['TEST_EXECUTION_ROOT'])
lore=os.environ['LORE_BIN']
def cli(*args):
    result=subprocess.run([lore,*args],capture_output=True,text=True)
    if result.returncode:
        print(result.stdout,file=sys.stderr)
        print(result.stderr,file=sys.stderr)
        raise SystemExit(result.returncode)
    return json.loads(result.stdout)
def log(row):
    with (root/'progressive-calls.jsonl').open('a') as stream:
        stream.write(json.dumps(row)+'\n')
native='--output-schema' in sys.argv or '--safe-mode' in sys.argv
if native:
    request=json.loads((root/'native-request.txt').read_text().split('\nNative request:\n',1)[1])
    assert request['policy']=='progressive-evidence-v1'
    assert request['examination_scope']=='candidate-card-only'
    assert request['source_context']['role']=='complete-retained-input'
    assert request['discovery']==[{'query':'transactional outbox','phrase':True,'spacing':False}]
    assert len(request['candidates'])<120
    output={key:request[key] for key in ['selection_id','batch_id','input_revision']}
    output['decisions']=[{'candidate_key':candidate['candidate_key'],
        'decision':'inspect' if candidate['candidate_key'].endswith('/candidate-000.md') else 'not-selected',
        'reason':'This assigned card has a fixture-defined scope; not-selected is not a full-body judgment.'}
        for candidate in request['candidates']]
    log({'action':'screen','selection_id':request['selection_id'],'batch_id':request['batch_id'],
         'input_bytes':len(json.dumps(request, ensure_ascii=False, separators=(',', ':')).encode()),
         'output_bytes':len(json.dumps(output, ensure_ascii=False, separators=(',', ':')).encode()),
         'keys':[candidate['candidate_key'] for candidate in request['candidates']]})
    if '--safe-mode' in sys.argv:
        print(json.dumps({'type':'result','subtype':'success','is_error':False,'structured_output':output}))
    else:
        print(json.dumps(output))
    raise SystemExit(0)
prompt='\n'.join(sys.argv[1:])
match=re.search(r'discover ("[^"\n]+")',prompt)
if match:
    ids=json.loads(Path(json.loads(match.group(1))).read_text())
else:
    path,_=json.JSONDecoder().raw_decode(prompt.split('Selected queue task manifest: ',1)[1])
    ids=json.loads(Path(path).read_text())
tasks={task['task_id']:task for file in (root/'vault/.lorekeeper/queue').glob('*.jsonl')
       for line in file.read_text().splitlines() if line for task in [json.loads(line)]}
for task_id in ids:
    task=tasks[task_id]
    assert task['kind']=='extract-concepts'
    context=cli('work','context','--task-id',task_id)
    if context['status']=='needs-discovery':
        rows=cli('queue','status','--json')['data']['tasks']
        row=next(row for row in rows if row['task_id']==task_id)
        file=root/'progressive-discovery.json'
        file.write_text(json.dumps({'target_revision':row['target_revision'],
            'queries':[{'query':'transactional outbox','phrase':True,'spacing':False}]}))
        cli('work','discover','--task-id',task_id,'--file',str(file))
        context=cli('work','context','--task-id',task_id)
        log({'action':'discovery','task_id':task_id})
    if context['requires_screening']:
        assert context['status']=='needs-screening'
        assert context['resources'] is None
        cli('work','select','--task-id',task_id)
        continue
    assert context['status']=='ready'
    assert not context['requires_bounded_work']
    assert context['resource_count']==2
    addresses=[resource['address'] for resource in context['resources']]
    assert 'wiki/documents/candidate-000.md' in addresses
    assert sum(address.startswith('retained-input://') for address in addresses)==1
    assert all('candidate-' not in address or address.endswith('/candidate-000.md') for address in addresses)
    assert task['input']['text'].startswith('A transactional outbox')
    assert 'transactional outbox' in (root/'vault/wiki/documents/candidate-000.md').read_text()
    subject=context['subject']
    result={key:subject[key] for key in ['task_id','cache_hash','target_revision']}
    result.update(context_revision=context['context_revision'],selection=context['selection'],
        discovery=context['context']['purpose']['discovery']['queries'],
        concept_policy=task['input']['concept_policy'],target=task['target'],date=task['input']['date'],concepts=[])
    directory=root/'vault/.lorekeeper/queue/results'
    directory.mkdir(exist_ok=True)
    (directory/(task_id+'.json')).write_text(json.dumps(result))
    log({'action':'final','task_id':task_id,'resources':addresses,'selection':context['selection']})
PROGRESSIVE
"#;

fn progressive_extraction_fixture(
    engine: &str,
    behavior: &str,
    candidates: usize,
    input_bytes: usize,
    result_bytes: usize,
) -> (Fixture, lk_queue::QueueTask) {
    let fixture = Fixture::new(engine, 30, behavior);
    let config = std::fs::read_to_string(&fixture.config)
        .unwrap()
        .replace(
            "      work: {extraction_context: full-context}\n",
            "      work: {extraction_context: progressive}\n",
        )
        .replace(
            "  default_profile: primary\n",
            &format!("  default_profile: primary\n  work: {{max_input_bytes: {input_bytes}, max_result_bytes: {result_bytes}, max_calls: 64}}\n"),
        );
    std::fs::write(&fixture.config, config).unwrap();
    let root = fixture.root().join("vault");
    let queue = root.join(".lorekeeper/queue");
    let client = lk_queue::QueueLlmClient::new(queue.clone());
    let task = client.build_task(
        "ext",
        lk_queue::TaskKind::ExtractConcepts,
        lk_queue::ExtractConceptsRequest {
            concept_policy: lk_core::config::ConceptPolicy::SupportedNew,
            rss_original: None,
            text: "A transactional outbox commits an event with its state change.".into(),
            source_id: "notes".into(),
            source_type: lk_core::config::SourceType::Manual,
            date: jiff::civil::date(2026, 10, 20),
            focus: None,
            locale: lk_core::i18n::Locale::En,
            categories: Vec::new(),
            target: lk_queue::TaskTarget {
                vault_path: "wiki/documents/source.md".into(),
                kind: lk_queue::TargetKind::DocumentConcepts,
                anchor: "## Related Concepts".into(),
            },
        },
    );
    std::fs::create_dir_all(root.join("wiki/documents")).unwrap();
    std::fs::write(
        root.join("wiki/documents/source.md"),
        format!("---\ntype: document\ntitle: Source\nllm_inputs:\n  concepts: {}\n---\n## Summary\nA retained observation.\n\n## Content\nA transactional outbox commits an event with its state change.\n\n## Related Concepts\n",task.cache_hash),
    ).unwrap();
    for candidate in 0..candidates {
        std::fs::write(
            root.join(format!("wiki/documents/candidate-{candidate:03}.md")),
            format!("---\ntype: document\ntitle: Candidate {candidate:03}\n---\n## Summary\nA transactional outbox has candidate-specific delivery assumptions.\n\n## Content\n{}", "The transactional outbox observation keeps the original scope and does not imply equivalence.\n".repeat(20)),
        ).unwrap();
    }
    std::fs::create_dir_all(&queue).unwrap();
    std::fs::write(
        queue.join("fixture.jsonl"),
        serde_json::to_string(&task).unwrap() + "\n",
    )
    .unwrap();
    (fixture, task)
}

fn execution_json(fixture: &Fixture, arguments: &[&str]) -> serde_json::Value {
    let output = fixture.command().args(arguments).output().unwrap();
    assert!(
        output.status.success(),
        "{arguments:?}: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn screening_decisions(request: &serde_json::Value, defer: bool) -> serde_json::Value {
    serde_json::json!({
        "selection_id":request["selection_id"],
        "batch_id":request["batch_id"],
        "input_revision":request["input_revision"],
        "decisions":request["candidates"].as_array().unwrap().iter().map(|candidate| {
            let selected = candidate["candidate_key"] == "wiki/documents/candidate-000.md";
            serde_json::json!({"candidate_key":candidate["candidate_key"],
                "decision":if selected {if defer {"defer"} else {"inspect"}} else {"not-selected"},
                "reason":"Card-scoped fixture assessment; unexamined bodies are not negative evidence."})
        }).collect::<Vec<_>>()
    })
}

#[test]
fn progressive_native_screening_keeps_complete_inventory_and_reads_only_admitted_bodies() {
    for engine in ["claude-code", "codex"] {
        let (fixture, _) = progressive_extraction_fixture(
            engine,
            RECORD_PROGRESSIVE_EXTRACTION,
            120,
            65536,
            16384,
        );
        let (output, report) = fixture.run("process");
        assert!(
            output.status.success(),
            "{engine}: {report}\n{}",
            String::from_utf8_lossy(&output.stderr),
        );
        let calls = std::fs::read_to_string(fixture.root().join("progressive-calls.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        let batches = calls
            .iter()
            .filter(|call| call["action"] == "screen")
            .collect::<Vec<_>>();
        assert!(batches.len() > 1);
        let steps = report["work_steps"].as_array().unwrap();
        assert_eq!(steps.len(), batches.len());
        for (step, batch) in steps.iter().zip(&batches) {
            assert_eq!(step["role"], "screening");
            assert_eq!(step["selection_id"], batch["selection_id"]);
            assert_eq!(step["batch_id"], batch["batch_id"]);
            assert!(step["work_id"].is_null());
            assert!(step["node_id"].is_null());
            assert_eq!(step["request_json_bytes"], batch["input_bytes"]);
            assert_eq!(step["result_json_bytes"], batch["output_bytes"]);
            assert!(step["preparation_duration_ms"].as_u64().is_some());
            assert!(step["agent_duration_ms"].as_u64().is_some());
            assert!(step["revalidation_record_duration_ms"].as_u64().is_some());
            assert_eq!(step["outcome"], "recorded");
        }
        let keys = batches
            .iter()
            .flat_map(|batch| batch["keys"].as_array().unwrap())
            .map(|key| key.as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(keys.len(), 120);
        assert_eq!(
            keys.iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            120,
        );
        let finalizer = calls.iter().find(|call| call["action"] == "final").unwrap();
        assert_eq!(finalizer["resources"].as_array().unwrap().len(), 2);
        assert_eq!(finalizer["selection"]["policy"], "progressive-evidence-v1");
        let source =
            std::fs::read_to_string(fixture.root().join("vault/wiki/documents/source.md")).unwrap();
        assert!(source.contains("concepts_done:"));
        assert!(
            !source.contains("candidate-"),
            "screening must not manufacture citation edges"
        );
        let concepts = fixture.root().join("vault/wiki/concepts");
        if concepts.exists() {
            assert!(std::fs::read_dir(concepts).unwrap().next().is_none());
        }
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
}

#[test]
fn progressive_cli_defers_resume_exact_packets_and_publication_rechecks_excluded_inputs() {
    let (fixture, task) = progressive_extraction_fixture("codex", "exit 0", 6, 16384, 4096);
    let root = fixture.root().join("vault");
    let target = root.join("wiki/documents/source.md");
    let before = std::fs::read_to_string(&target).unwrap();
    let discovery = fixture.root().join("progressive-discovery.json");
    std::fs::write(
        &discovery,
        serde_json::to_vec(&serde_json::json!({
            "target_revision":lk_core::concept::file_revision(&before),
            "queries":[{"query":"transactional outbox","phrase":true,"spacing":false}],
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
            discovery.to_str().unwrap(),
        ],
    );
    let path = fixture.root().join("screening-output.json");
    let mut first = None;
    let mut recorded = std::collections::BTreeSet::new();
    loop {
        let status = execution_json(&fixture, &["work", "select", "--task-id", &task.task_id]);
        let request = &status["request"];
        if request.is_null() {
            assert_eq!(status["status"], "deferred");
            assert_eq!(status["progress"]["unknown"], 0);
            assert_eq!(status["progress"]["deferred"], 1);
            assert_eq!(status["progress"]["complete"], false);
            break;
        }
        assert!(recorded.insert(request["batch_id"].as_str().unwrap().to_owned()));
        if request["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .any(|candidate| candidate["candidate_key"] == "wiki/documents/candidate-000.md")
        {
            first = Some(request.clone());
        }
        std::fs::write(
            &path,
            serde_json::to_vec(&screening_decisions(request, true)).unwrap(),
        )
        .unwrap();
        execution_json(
            &fixture,
            &[
                "work",
                "select-record",
                "--task-id",
                &task.task_id,
                "--file",
                path.to_str().unwrap(),
            ],
        );
    }
    assert_eq!(std::fs::read_to_string(&target).unwrap(), before);
    let pending = execution_json(&fixture, &["work", "context", "--task-id", &task.task_id]);
    assert_eq!(pending["status"], "needs-screening");
    assert!(pending["resources"].is_null());
    let first = first.unwrap();
    let batch_id = first["batch_id"].as_str().unwrap();
    let reread = execution_json(
        &fixture,
        &[
            "work",
            "select",
            "--task-id",
            &task.task_id,
            "--batch-id",
            batch_id,
        ],
    );
    assert_eq!(reread["request"], first);
    let revision = reread["receipt"]["revision"].as_str().unwrap();
    std::fs::write(
        &path,
        serde_json::to_vec(&screening_decisions(&first, false)).unwrap(),
    )
    .unwrap();
    let refused = fixture
        .command()
        .args([
            "work",
            "select-record",
            "--task-id",
            &task.task_id,
            "--file",
            path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        !refused.status.success(),
        "conflicting receipts need an exact previous revision"
    );
    execution_json(
        &fixture,
        &[
            "work",
            "select-record",
            "--task-id",
            &task.task_id,
            "--file",
            path.to_str().unwrap(),
            "--previous-revision",
            revision,
        ],
    );
    let context = execution_json(&fixture, &["work", "context", "--task-id", &task.task_id]);
    assert_eq!(context["status"], "ready");
    assert_eq!(context["resource_count"], 2);
    let result = serde_json::json!({
        "task_id":task.task_id,"cache_hash":task.cache_hash,
        "target_revision":context["subject"]["target_revision"],
        "context_revision":context["context_revision"],"selection":context["selection"],
        "discovery":context["context"]["purpose"]["discovery"]["queries"],
        "concept_policy":"supported-new","target":task.target,"date":"2026-10-20","concepts":[],
    });
    let results = root.join(".lorekeeper/queue/results");
    std::fs::create_dir_all(&results).unwrap();
    std::fs::write(
        results.join(format!("{}.json", task.task_id)),
        serde_json::to_vec(&result).unwrap(),
    )
    .unwrap();
    let excluded = root.join("wiki/documents/candidate-005.md");
    let excluded_before = std::fs::read_to_string(&excluded).unwrap();
    std::fs::write(
        &excluded,
        format!("{excluded_before}\nAn excluded full body changed.\n"),
    )
    .unwrap();
    let refused = fixture.command().args(["queue", "apply"]).output().unwrap();
    assert!(
        !refused.status.success(),
        "excluded bodies remain complete-inventory guards"
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), before);
    std::fs::write(&excluded, excluded_before).unwrap();
    let added = root.join("wiki/documents/new-match.md");
    std::fs::write(&added, "---\ntype: document\ntitle: New candidate\n---\nA transactional outbox has new scoped evidence.\n").unwrap();
    let refused = fixture.command().args(["queue", "apply"]).output().unwrap();
    assert!(
        !refused.status.success(),
        "new complete-inventory matches invalidate the old result"
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), before);
    std::fs::remove_file(added).unwrap();
    let applied = fixture.command().args(["queue", "apply"]).output().unwrap();
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    assert!(
        std::fs::read_to_string(target)
            .unwrap()
            .contains("concepts_done:")
    );
    assert!(
        fixture
            .command()
            .args(["queue", "check"])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        !fixture.root().join("arguments").exists(),
        "this recovery path makes no host invocation"
    );
}

#[test]
fn progressive_large_source_observations_feed_guarded_admitted_work_before_publication() {
    let behavior = build_extraction_behavior(
        "    if match is None:\n        raise SystemExit('unexpected ordinary worker in bounded progressive fixture')",
    )
    .replace(
        "    context=request['context']['purpose']",
        "    context=request['context']['purpose']\n    assert context['extraction_scope']['policy']=='progressive-evidence-v1'\n    assert context['source_observations']['work_id']==(root/'source-work-id').read_text()",
    )
    .replace(
        "    result.update(context_revision=cli('work','status',work_id)['context_revision'],",
        "    result.update(selection=context['extraction_scope']['selection'],context_revision=cli('work','status',work_id)['context_revision'],",
    );
    let (fixture, original) = progressive_extraction_fixture("codex", &behavior, 2, 8192, 1024);
    let config = std::fs::read_to_string(&fixture.config)
        .unwrap()
        .replace("max_calls: 64", "max_calls: 1");
    std::fs::write(&fixture.config, config).unwrap();
    let root = fixture.root().join("vault");
    let queue = root.join(".lorekeeper/queue");
    let client = lk_queue::QueueLlmClient::new(queue.clone());
    let mut input = original.input;
    input.insert(
        "target".into(),
        serde_json::to_value(original.target).unwrap(),
    );
    let mut request: lk_queue::ExtractConceptsRequest =
        serde_json::from_value(serde_json::Value::Object(input)).unwrap();
    request.text =
        "A transactional outbox observation preserves its exact source conditions and attribution.\n"
            .repeat(105);
    let text = request.text.clone();
    let task = client.build_task("ext", lk_queue::TaskKind::ExtractConcepts, request);
    std::fs::write(
        queue.join("fixture.jsonl"),
        serde_json::to_string(&task).unwrap() + "\n",
    )
    .unwrap();
    let target = root.join("wiki/documents/source.md");
    let before = format!(
        "---\ntype: document\ntitle: Source\nllm_inputs:\n  concepts: {}\n---\n## Summary\nA retained observation.\n\n## Content\n{text}\n## Related Concepts\n",
        task.cache_hash
    );
    std::fs::write(&target, &before).unwrap();
    let file = fixture.root().join("source-discovery.json");
    std::fs::write(
        &file,
        serde_json::to_vec(&serde_json::json!({
            "target_revision":lk_core::concept::file_revision(&before),
            "queries":[{"query":"transactional outbox","phrase":true,"spacing":false}],
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
    let source = execution_json(&fixture, &["work", "select", "--task-id", &task.task_id]);
    assert_eq!(source["status"], "needs-source-observations");
    let source_id = source["source_work"]["work_id"].as_str().unwrap();
    let source_directory = root.join(".lorekeeper/work").join(source_id);
    let consumption = source_directory.join("consumption.json");
    assert!(!consumption.exists());
    std::fs::write(fixture.root().join("source-work-id"), source_id).unwrap();
    let explicit_source = execution_json(
        &fixture,
        &["work", "source-plan", "--task-id", &task.task_id],
    );
    assert_eq!(explicit_source["work_id"], source_id);
    assert_eq!(explicit_source["resources"], 1);
    assert!(explicit_source["parts"].as_u64().unwrap() > 1);
    let mut source_completed = false;
    for _ in 0..16 {
        let (output, report) = fixture.run("process");
        assert!(
            !output.status.success(),
            "one call cannot finish source observations, screening and publication: {report}"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), before);
        assert!(!consumption.exists());
        let progress = execution_json(&fixture, &["work", "status", source_id]);
        if progress["complete"] == true {
            source_completed = true;
            break;
        }
    }
    assert!(
        source_completed,
        "bounded source inspection must make progress"
    );
    let observations = execution_json(&fixture, &["work", "result", source_id]);
    assert_eq!(
        observations["context"]["purpose"]["extraction_scope"]["policy"],
        "source-proposals-v1"
    );
    assert!(bounded_calls(&fixture).iter().all(|call| {
        call["action"] == "inspect"
            && call["addresses"].as_array().is_none_or(|addresses| {
                addresses
                    .iter()
                    .all(|address| address.as_str().unwrap().starts_with("retained-input://"))
            })
    }));
    std::fs::write(&file, "{}").unwrap();
    let refused = fixture
        .command()
        .args([
            "work",
            "finish",
            source_id,
            "--file",
            file.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    let error = String::from_utf8_lossy(&refused.stderr);
    assert!(
        error.contains("source observations") && error.contains("cannot publish"),
        "{error}"
    );
    assert!(!source_directory.join("final.json").exists());
    assert!(!consumption.exists());
    let mut recorded = std::collections::BTreeSet::new();
    loop {
        let screening = execution_json(&fixture, &["work", "select", "--task-id", &task.task_id]);
        if screening["status"] == "ready" {
            break;
        }
        let packet = &screening["request"];
        assert!(recorded.insert(packet["batch_id"].as_str().unwrap().to_owned()));
        assert_eq!(
            packet["source_context"]["role"],
            "complete-bounded-source-observations"
        );
        assert_eq!(packet["source_context"]["observation"], observations);
        std::fs::write(
            &file,
            serde_json::to_vec(&screening_decisions(packet, false)).unwrap(),
        )
        .unwrap();
        execution_json(
            &fixture,
            &[
                "work",
                "select-record",
                "--task-id",
                &task.task_id,
                "--file",
                file.to_str().unwrap(),
            ],
        );
    }
    let context = execution_json(&fixture, &["work", "context", "--task-id", &task.task_id]);
    assert_eq!(context["status"], "ready");
    assert_eq!(context["resource_count"], 2);
    assert_eq!(context["requires_bounded_work"], true);
    assert_eq!(
        context["context"]["purpose"]["source_observations"],
        observations
    );
    let admitted = execution_json(&fixture, &["work", "plan", "--task-id", &task.task_id]);
    let admitted_id = admitted["work_id"].as_str().unwrap();
    assert_ne!(admitted_id, source_id);
    assert_eq!(admitted["resources"], 2);
    assert!(!consumption.exists());
    let selected = root.join("wiki/documents/candidate-000.md");
    let selected_before = std::fs::read_to_string(&selected).unwrap();
    std::fs::write(
        &selected,
        format!("{selected_before}\nChanged selected evidence.\n"),
    )
    .unwrap();
    let refused = fixture
        .command()
        .args(["work", "status", admitted_id])
        .output()
        .unwrap();
    assert!(
        !refused.status.success(),
        "full selected bodies remain guards after card screening"
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), before);
    std::fs::write(selected, selected_before).unwrap();
    let mut published = false;
    for _ in 0..24 {
        let (output, report) = fixture.run("process");
        if output.status.success() {
            published = true;
            break;
        }
        assert!(
            report["stages"].as_array().unwrap().iter().any(|stage| {
                stage["name"] == "semantic call budget" && stage["state"] == "failed"
            }),
            "bounded admitted work must progress or report its explicit budget: {report}"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), before);
        assert!(!consumption.exists());
    }
    assert!(published, "admitted bounded work must finish and publish");
    assert!(
        std::fs::read_to_string(target)
            .unwrap()
            .contains("concepts_done:")
    );
    let consumed: serde_json::Value =
        serde_json::from_slice(&std::fs::read(consumption).unwrap()).unwrap();
    assert_eq!(consumed["work_id"], source_id);
    assert_eq!(consumed["task_id"], task.task_id);
    assert_eq!(consumed["cache_hash"], task.cache_hash);
    assert_eq!(
        consumed["target_revision"],
        lk_core::concept::file_revision(&before)
    );
    assert!(!source_directory.join("final.json").exists());
    assert!(bounded_calls(&fixture).iter().any(|call| {
        call["action"] == "inspect"
            && call["addresses"].as_array().is_some_and(|addresses| {
                addresses
                    .iter()
                    .any(|address| address == "wiki/documents/candidate-000.md")
            })
    }));
    assert!(
        bounded_calls(&fixture)
            .iter()
            .any(|call| call["action"] == "finish")
    );
}

#[test]
fn sealed_queue_work_restages_a_missing_result_without_calling_the_model_again() {
    let fixture = Fixture::new("codex", 30, RECORD_BOUNDED_AUDIT);
    std::os::unix::fs::symlink(
        fixture.root().join("vault"),
        fixture.root().join("vault-alias"),
    )
    .unwrap();
    let config = std::fs::read_to_string(&fixture.config)
        .unwrap()
        .replace("concept_policy: supported-new", "concept_policy: disabled")
        .replace("root: vault", "root: vault-alias")
        .replace(
            "  default_profile: primary\n",
            "  default_profile: primary\n  work: {max_calls: 1}\n",
        );
    std::fs::write(&fixture.config, config).unwrap();
    fixture.ingest();
    let status = fixture
        .command()
        .args(["queue", "status", "--json"])
        .output()
        .unwrap();
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    let task = status["data"]["tasks"][0]["task_id"].as_str().unwrap();
    let plan = fixture
        .command()
        .args(["work", "plan", "--task-id", task])
        .output()
        .unwrap();
    assert!(
        plan.status.success(),
        "{}",
        String::from_utf8_lossy(&plan.stderr)
    );
    let plan: serde_json::Value = serde_json::from_slice(&plan.stdout).unwrap();
    let id = plan["work_id"].as_str().unwrap();
    let (first, result) = fixture.run("process");
    assert!(
        !first.status.success(),
        "a call-limited observation cannot count as completed publication: {result}"
    );
    let calls = bounded_calls(&fixture);
    assert_eq!(calls.len(), 1);
    let completed = fixture
        .command()
        .args(["work", "result", id])
        .output()
        .unwrap();
    assert!(
        completed.status.success(),
        "{}",
        String::from_utf8_lossy(&completed.stderr)
    );
    let completed: serde_json::Value = serde_json::from_slice(&completed.stdout).unwrap();
    let subject = &completed["context"]["subject"];
    let result = serde_json::json!({
        "task_id": subject["task_id"],
        "cache_hash": subject["cache_hash"],
        "target_revision": subject["target_revision"],
        "body": "Verified cached section result.",
    });
    let file = fixture.root().join("section.json");
    std::fs::write(&file, serde_json::to_vec(&result).unwrap()).unwrap();
    let sealed = fixture
        .command()
        .args(["work", "finish", id, "--file"])
        .arg(&file)
        .output()
        .unwrap();
    assert!(
        sealed.status.success(),
        "{}",
        String::from_utf8_lossy(&sealed.stderr)
    );
    let sealed: serde_json::Value = serde_json::from_slice(&sealed.stdout).unwrap();
    std::fs::remove_file(
        fixture
            .root()
            .join("vault")
            .join(sealed["staged_result"].as_str().unwrap()),
    )
    .unwrap();
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
    assert_eq!(bounded_calls(&fixture).len(), 1);
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
        let no_receipt = bounded_audit_fixture(engine, 1, "/bin/cat > /dev/null\nexit 0");
        let (output, result) = no_receipt.run("knowledge");
        assert!(!output.status.success());
        assert!(result["stages"].as_array().unwrap().iter().any(|stage| {
            stage["name"] == "bounded work receipt" && stage["state"] == "failed"
        }));
        let receipts: serde_json::Value = serde_json::from_slice(
            &std::fs::read(
                no_receipt
                    .root()
                    .join("vault/.lorekeeper/knowledge-audit.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(receipts["entries"].as_object().unwrap().is_empty());
        assert!(bounded_calls(&no_receipt).is_empty());
        let step = &result["work_steps"][0];
        assert_eq!(result["work_steps"].as_array().unwrap().len(), 1);
        assert_eq!(step["outcome"], "rejected", "{result}");
        assert!(step["request_json_bytes"].as_u64().is_some());
        assert!(step["agent_duration_ms"].as_u64().is_some());
        assert!(step["result_json_bytes"].is_null());
        assert!(step["revalidation_record_duration_ms"].is_null());
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
            let receipts: serde_json::Value = serde_json::from_slice(
                &std::fs::read(
                    fixture
                        .root()
                        .join("vault/.lorekeeper/knowledge-audit.json"),
                )
                .unwrap(),
            )
            .unwrap();
            assert!(receipts["entries"].as_object().unwrap().is_empty());
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
        for name in ["a", "b"] {
            write_audit_concept(&fixture, name);
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
        ("merge_pair a-duplicate z-canonical || exit 31", true),
        (
            "merge_pair a-duplicate m-intermediate || exit 31\nmerge_pair m-intermediate z-canonical || exit 31",
            true,
        ),
        (
            "rm \"$TEST_EXECUTION_ROOT/vault/wiki/concepts/a-duplicate.md\"",
            false,
        ),
        (
            "LORE_WORKFLOW_ID=ffffffffffffffffffffffffffffffff merge_pair a-duplicate z-canonical || exit 31",
            false,
        ),
    ] {
        let behavior = format!(
            "{}\n{action}\n{}",
            r#"merge_pair() {
python3 - "$1" "$2" <<'MERGE'
import json, os, subprocess, sys
lore = os.environ['LORE_BIN']
source, survivor = sys.argv[1:]
def revision(slug):
    view = json.loads(subprocess.check_output([lore, 'wiki', 'read', f'wiki/concepts/{slug}.md']))
    return view['file_revision']
subprocess.run([lore, 'graph', 'merge', source, survivor, '--force',
                '--from-revision', revision(source), '--into-revision', revision(survivor)], check=True)
MERGE
}
"#,
            r#"
"$LORE_BIN" graph backlinks-sync || exit 32
"$LORE_BIN" wiki audit-plan --path wiki/concepts/z-canonical.md > "$TEST_EXECUTION_ROOT/plan.json" || exit 33
python3 - <<'ASSESS'
import json, os
from pathlib import Path
root=Path(os.environ['TEST_EXECUTION_ROOT'])
candidate=json.loads((root/'plan.json').read_text())['candidates'][0]
import subprocess
if candidate['discovery'] is None:
    discovery={key:candidate[key] for key in ['path','file_revision']}
    discovery['queries']=[{'query':candidate['title'],'phrase':True,'spacing':False}]
    path=root/'discovery.json'
    path.write_text(json.dumps(discovery))
    subprocess.run([os.environ['LORE_BIN'],'wiki','audit-discover','--file',str(path)],check=True)
    candidate=json.loads(subprocess.check_output([os.environ['LORE_BIN'],'wiki','audit-plan','--path',candidate['path']]))['candidates'][0]
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
        record_discovery(&fixture, "wiki/concepts/a-duplicate.md");
        record_discovery(&fixture, "wiki/concepts/m-intermediate.md");
        record_discovery(&fixture, "wiki/concepts/z-canonical.md");
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

const PARALLEL_OBSERVATIONS: &str = r#"
case "$*" in
  *--output-schema*|*--safe-mode*)
    export TEST_PARALLEL_REQUEST="$TEST_EXECUTION_ROOT/parallel-request-$$.txt"
    /bin/cat > "$TEST_PARALLEL_REQUEST"
    ;;
  *) echo 'unexpected interactive invocation in bounded observation test' >&2; exit 47 ;;
esac
exec python3 - "$@" <<'PARALLEL'
import json, os, sys, time
from pathlib import Path
root=Path(os.environ['TEST_EXECUTION_ROOT'])
request=json.loads(Path(os.environ['TEST_PARALLEL_REQUEST']).read_text().split('\nNative request:\n',1)[1])
node=request['node_id']
mode=(root/'parallel-mode').read_text()
expected=int((root/'parallel-expected').read_text())
record={'node_id':node,'work_id':request['work_id'],'kind':request['kind'],'pid':os.getpid()}
marker=root/('parallel-start-'+node+'.json')
pending=marker.with_suffix('.tmp')
with pending.open('x') as stream:
    json.dump(record,stream)
os.link(pending,marker)
pending.unlink()
deadline=time.monotonic()+15
while len(list(root.glob('parallel-start-*.json')))<expected:
    if time.monotonic()>=deadline:
        raise SystemExit('peer never entered the observation barrier')
    time.sleep(.01)
peers=sorted(path.name for path in root.glob('parallel-start-*.json'))
with (root/('parallel-overlap-'+node+'.json')).open('x') as stream:
    json.dump(peers,stream)
leader=('parallel-start-'+node+'.json')==peers[0]
if mode=='wait':
    while True:
        time.sleep(1)
if leader and mode=='source-drift':
    with (root/'vault/wiki/concepts/large.md').open('a') as stream:
        stream.write('\nChanged inside the private observation fixture.\n')
if leader and mode=='config-drift':
    with (root/'config.yaml').open('a') as stream:
        stream.write('\n# Changed inside the private observation fixture.\n')
if leader and mode=='failed':
    raise SystemExit('intentional failed observation peer')
if leader and mode=='malformed':
    print('not a structured observation')
    raise SystemExit(0)
output={key:request[key] for key in ['work_id','node_id','input_revision']}
output.update(summary='Read this exact fixture request without a factual correctness claim.',findings=[],source_questions=[],unresolved=[],resolutions=[])
if '--safe-mode' in sys.argv:
    print(json.dumps({'type':'result','subtype':'success','is_error':False,'structured_output':output}))
else:
    print(json.dumps(output))
PARALLEL
"#;

fn parallel_observation_fixture(
    engine: &str,
    calls: usize,
    parallel: usize,
    targets: usize,
    mode: &str,
) -> Fixture {
    let fixture = bounded_audit_fixture(engine, calls, PARALLEL_OBSERVATIONS);
    let mut config = std::fs::read_to_string(&fixture.config).unwrap().replace(
        &format!("max_calls: {calls}}}"),
        &format!("max_calls: {calls}, max_parallel: {parallel}}}"),
    );
    if targets == 2 {
        config = config.replace("batch_size: 1", "batch_size: 2");
        write_audit_concept(&fixture, "second");
        let path = fixture.root().join("vault/wiki/concepts/second.md");
        let body = std::fs::read_to_string(&path).unwrap()
            + &"\nAnother scoped fixture observation.\n".repeat(350);
        std::fs::write(path, body).unwrap();
        record_discovery(&fixture, "wiki/concepts/second.md");
    }
    std::fs::write(&fixture.config, config).unwrap();
    std::fs::write(fixture.root().join("parallel-mode"), mode).unwrap();
    std::fs::write(
        fixture.root().join("parallel-expected"),
        calls.min(parallel).to_string(),
    )
    .unwrap();
    fixture
}

fn parallel_starts(fixture: &Fixture) -> Vec<serde_json::Value> {
    let mut paths = std::fs::read_dir(fixture.root())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
                && path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("parallel-start-")
        })
        .collect::<Vec<_>>();
    paths.sort();
    paths
        .iter()
        .map(|path| serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap())
        .collect()
}

fn parallel_receipts(fixture: &Fixture) -> usize {
    parallel_starts(fixture)
        .iter()
        .map(|row| row["work_id"].as_str().unwrap().to_owned())
        .collect::<std::collections::BTreeSet<_>>()
        .iter()
        .map(|work| {
            std::fs::read_dir(
                fixture
                    .root()
                    .join("vault/.lorekeeper/work")
                    .join(work)
                    .join("nodes"),
            )
            .into_iter()
            .flatten()
            .count()
        })
        .sum()
}

fn assert_parallel_no_publication(fixture: &Fixture) {
    let settlement = execution_json(fixture, &["queue", "check"]);
    assert_eq!(settlement["data"]["pending_mutation"], false);
    assert!(
        settlement["data"]["staged_results"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let receipts: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            fixture
                .root()
                .join("vault/.lorekeeper/knowledge-audit.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(receipts["entries"].as_object().unwrap().is_empty());
}

#[test]
fn parallel_native_observers_overlap_on_both_hosts_with_same_and_distinct_work() {
    for engine in ["claude-code", "codex"] {
        for targets in [1, 2] {
            let fixture = parallel_observation_fixture(engine, 2, 2, targets, "good");
            let (output, report) = fixture.run("knowledge");
            assert!(
                !output.status.success(),
                "two calls do not complete this work: {report}"
            );
            assert_eq!(report["semantic_calls"], 2);
            let starts = parallel_starts(&fixture);
            assert_eq!(starts.len(), 2, "{engine}: {report}");
            assert_eq!(
                starts
                    .iter()
                    .map(|row| row["work_id"].as_str().unwrap())
                    .collect::<std::collections::BTreeSet<_>>()
                    .len(),
                targets
            );
            for row in &starts {
                let peers: Vec<String> = serde_json::from_slice(
                    &std::fs::read(fixture.root().join(format!(
                        "parallel-overlap-{}.json",
                        row["node_id"].as_str().unwrap()
                    )))
                    .unwrap(),
                )
                .unwrap();
                assert_eq!(
                    peers.len(),
                    2,
                    "both observation processes reached the barrier"
                );
            }
            assert_eq!(parallel_receipts(&fixture), 2);
            assert_eq!(
                report["work_steps"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|step| step["outcome"] == "recorded")
                    .count(),
                2
            );
            assert_parallel_no_publication(&fixture);
        }
    }
}

#[test]
fn parallel_native_observers_respect_remaining_call_budget() {
    for engine in ["claude-code", "codex"] {
        let fixture = parallel_observation_fixture(engine, 1, 4, 1, "good");
        let (output, report) = fixture.run("knowledge");
        assert!(!output.status.success(), "{report}");
        assert_eq!(report["semantic_calls"], 1);
        assert_eq!(parallel_starts(&fixture).len(), 1);
        assert_eq!(parallel_receipts(&fixture), 1);
        assert!(report["stages"].as_array().unwrap().iter().any(|stage| {
            stage["name"] == "semantic call budget" && stage["state"] == "failed"
        }));
        assert_parallel_no_publication(&fixture);
    }
}

#[test]
fn parallel_native_observer_failure_retains_an_independent_good_receipt() {
    for mode in ["malformed", "failed"] {
        let fixture = parallel_observation_fixture("codex", 2, 2, 1, mode);
        let (output, report) = fixture.run("knowledge");
        assert!(!output.status.success(), "{report}");
        assert_eq!(report["semantic_calls"], 2);
        assert_eq!(parallel_starts(&fixture).len(), 2);
        assert_eq!(parallel_receipts(&fixture), 1, "{mode}: {report}");
        assert_eq!(
            report["work_steps"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|step| step["outcome"] == "recorded")
                .count(),
            1
        );
        assert_parallel_no_publication(&fixture);
    }
}

#[test]
fn parallel_native_observers_revalidate_source_and_configuration_after_all_hosts_return() {
    for mode in ["source-drift", "config-drift"] {
        let fixture = parallel_observation_fixture("codex", 2, 2, 1, mode);
        let (output, report) = fixture.run("knowledge");
        assert!(!output.status.success(), "{report}");
        assert_eq!(report["semantic_calls"], 2);
        assert_eq!(parallel_starts(&fixture).len(), 2);
        assert_eq!(parallel_receipts(&fixture), 0, "{mode}: {report}");
        assert!(
            report["work_steps"]
                .as_array()
                .unwrap()
                .iter()
                .all(|step| step["outcome"] != "recorded")
        );
        assert_parallel_no_publication(&fixture);
    }
}

#[test]
fn cancellation_stops_both_parallel_observers_without_recording_or_publishing() {
    let fixture = parallel_observation_fixture("codex", 2, 2, 1, "wait");
    let mut child = fixture
        .command()
        .args(["run", "knowledge", "--json"])
        .stdout(std::fs::File::create(fixture.root().join("parallel-result.json")).unwrap())
        .stderr(std::fs::File::create(fixture.root().join("parallel-stderr.log")).unwrap())
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let starts = parallel_starts(&fixture);
        if starts.len() == 2
            && starts.iter().all(|row| {
                fixture
                    .root()
                    .join(format!(
                        "parallel-overlap-{}.json",
                        row["node_id"].as_str().unwrap()
                    ))
                    .exists()
            })
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "parallel observers never reached the barrier"
        );
        assert!(child.try_wait().unwrap().is_none());
        std::thread::sleep(Duration::from_millis(20));
    }
    let pid = rustix::process::Pid::from_raw(i32::try_from(child.id()).unwrap()).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::INT).unwrap();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "cancelled parallel controller did not stop"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(!status.success());
    let report: serde_json::Value = serde_json::from_slice(
        &std::fs::read(fixture.root().join("parallel-result.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(report["cancelled"], true, "{report}");
    assert_eq!(report["semantic_calls"], 2);
    assert_eq!(parallel_receipts(&fixture), 0);
    for row in parallel_starts(&fixture) {
        assert!(
            !Command::new("kill")
                .args(["-0", &row["pid"].as_i64().unwrap().to_string()])
                .output()
                .unwrap()
                .status
                .success(),
            "cancelled observer remains alive: {row}"
        );
    }
    assert_parallel_no_publication(&fixture);
    assert!(
        fixture
            .command()
            .args(["workflow", "--", "/usr/bin/true"])
            .output()
            .unwrap()
            .status
            .success()
    );
}

#[test]
fn parallel_native_audit_completes_reduction_and_one_serial_finalizer_on_both_hosts() {
    let behavior = RECORD_BOUNDED_AUDIT
        .replace(
            "/bin/cat > \"$TEST_EXECUTION_ROOT/native-request.txt\"",
            "export TEST_BOUNDED_REQUEST=\"$TEST_EXECUTION_ROOT/native-request-$$.txt\"; /bin/cat > \"$TEST_BOUNDED_REQUEST\"",
        )
        .replace(
            "(root/'native-request.txt').read_text()",
            "Path(os.environ['TEST_BOUNDED_REQUEST']).read_text()",
        );
    for engine in ["claude-code", "codex"] {
        let fixture = bounded_audit_fixture(engine, 64, &behavior);
        let config = std::fs::read_to_string(&fixture.config)
            .unwrap()
            .replace("max_calls: 64}", "max_calls: 64, max_parallel: 3}");
        std::fs::write(&fixture.config, config).unwrap();
        let (output, report) = fixture.run("knowledge");
        assert!(output.status.success(), "{engine}: {report}");
        let calls = bounded_calls(&fixture);
        assert_eq!(
            calls
                .iter()
                .filter(|call| call["action"] == "finish")
                .count(),
            1
        );
        assert_eq!(calls.last().unwrap()["action"], "finish");
        assert!(calls.iter().any(|call| call["kind"] == "inspect"));
        assert!(calls.iter().any(|call| call["kind"] == "reduce"));
        assert_eq!(
            report["semantic_calls"].as_u64().unwrap() as usize,
            calls.len()
        );
        let work = calls[0]["work_id"].as_str().unwrap();
        assert!(calls.iter().all(|call| call["work_id"] == work));
        let status = execution_json(&fixture, &["work", "status", work]);
        assert_eq!(status["complete"], true);
        assert_eq!(
            status["completed"].as_u64().unwrap() as usize,
            calls.len() - 1
        );
        assert_eq!(
            report["audit"][0]["assessed_path"],
            "wiki/concepts/large.md"
        );
        let plan = execution_json(&fixture, &["wiki", "audit-plan"]);
        assert_eq!(plan["reviewed_current"], 1);
        assert_eq!(plan["unreviewed"], 0);
        assert!(plan["candidates"].as_array().unwrap().is_empty());
        let settlement = execution_json(&fixture, &["queue", "check"]);
        assert_eq!(settlement["data"]["pending_mutation"], false);
        assert!(
            settlement["data"]["staged_results"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(
            fixture
                .command()
                .args(["workflow", "--", "/usr/bin/true"])
                .output()
                .unwrap()
                .status
                .success()
        );
    }
}

#[test]
fn native_synthesis_queue_planning_captures_two_ready_subjects_once_per_checkpoint() {
    let fixture = Fixture::new("codex", 30, RECORD_BOUNDED_AUDIT);
    let config = std::fs::read_to_string(&fixture.config).unwrap().replace(
        "  default_profile: primary\n",
        "  default_profile: primary\n  work: {max_input_bytes: 8192, max_result_bytes: 1024, max_calls: 1}\n",
    );
    std::fs::write(&fixture.config, config).unwrap();
    write_synthesis_pair(&fixture, "a-ready", None, 400);
    write_synthesis_pair(&fixture, "b-ready", None, 400);
    let (output, report) = fixture.run("process");
    assert!(
        !output.status.success(),
        "one call retains both syntheses: {report}"
    );
    assert_eq!(report["semantic_calls"], 1);
    assert_eq!(report["queue_planning"]["attempts"], 1);
    assert_eq!(report["queue_planning"]["audit_captures"], 1);
    assert_eq!(report["queue_planning"]["synthesis_subjects"], 2);
    assert!(
        report["stages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|stage| { stage["name"] == "semantic call budget" && stage["state"] == "failed" })
    );
    let subjects = planned_queue_subjects(&fixture);
    assert_eq!(subjects.len(), 2);
    assert_eq!(
        subjects
            .iter()
            .map(|subject| subject["target"].as_str().unwrap())
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from(["wiki/concepts/a-ready.md", "wiki/concepts/b-ready.md"])
    );
    let status = execution_json(&fixture, &["queue", "status", "--json"]);
    assert_eq!(
        status["data"]["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|task| task["kind"] == "synthesize-concept" && task["status"] == "current")
            .count(),
        2
    );
    assert_eq!(bounded_calls(&fixture).len(), 1);
    assert!(
        !fixture
            .root()
            .join("vault/.lorekeeper/mutation/pending.json")
            .exists()
    );
}

fn queue_window_fixture(entries: usize, large: bool) -> Fixture {
    let fixture = Fixture::new(
        "codex",
        30,
        if large {
            RECORD_BOUNDED_AUDIT
        } else {
            "exit 0"
        },
    );
    let config = std::fs::read_to_string(&fixture.config).unwrap()
        .replace("concept_policy: supported-new", "concept_policy: disabled")
        .replace("  default_profile: primary\n", &format!("  default_profile: primary\n  work: {{max_input_bytes: {}, max_result_bytes: 1024, max_calls: 1}}\n", if large { 8192 } else { 262144 }));
    std::fs::write(&fixture.config, config).unwrap();
    for index in 0..entries {
        let name = format!("source-{index:03}");
        let content = if large {
            "Exact retained evidence.\n".repeat(700)
        } else {
            "One retained fact.\n".into()
        };
        std::fs::write(
            fixture.root().join(format!("vault/inbox/{name}.md")),
            format!("# {name}\n\n{content}"),
        )
        .unwrap();
    }
    let result = fixture.command().arg("ingest").output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    fixture
}

#[test]
fn native_queue_limits_preparation_and_reserves_whole_input_members_in_fair_order() {
    let fixture = queue_window_fixture(65, false);
    let (output, report) = fixture.run("process");
    assert!(!output.status.success(), "mock publishes nothing: {report}");
    assert_eq!(report["semantic_calls"], 1);
    assert_eq!(report["queue_planning"]["prepared_targets"], 64);
    let cursor_path = fixture
        .root()
        .join("vault/.lorekeeper/execution-cursor.json");
    let cursor: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&cursor_path).unwrap()).unwrap();
    assert_eq!(cursor["queue"], "wiki/documents/source-064.md");
    assert_eq!(
        execution_json(&fixture, &["queue", "status", "--json"])["data"]["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|task| task["status"] == "current")
            .count(),
        65
    );
    let (_, report) = fixture.run("process");
    assert_eq!(report["queue_planning"]["prepared_targets"], 64);
    let cursor: serde_json::Value =
        serde_json::from_slice(&std::fs::read(cursor_path).unwrap()).unwrap();
    assert_eq!(cursor["queue"], "wiki/documents/source-063.md");
}

#[test]
fn native_queue_does_not_materialize_the_sixty_fifth_target_before_first_call() {
    let fixture = queue_window_fixture(65, true);
    let (output, report) = fixture.run("process");
    assert!(
        !output.status.success(),
        "one call retains incomplete work: {report}"
    );
    assert_eq!(report["semantic_calls"], 1);
    assert_eq!(report["queue_planning"]["prepared_targets"], 64);
    let subjects = planned_queue_subjects(&fixture);
    assert_eq!(subjects.len(), 64);
    assert!(
        !subjects
            .iter()
            .any(|subject| subject["target"] == "wiki/documents/source-064.md")
    );
    let (_, report) = fixture.run("process");
    assert_eq!(report["semantic_calls"], 1);
    assert_eq!(planned_queue_subjects(&fixture).len(), 65);
    let calls = bounded_calls(&fixture);
    assert_eq!(calls.len(), 2);
    assert_ne!(calls[0]["work_id"], calls[1]["work_id"]);
}

#[test]
fn native_queue_keeps_global_unreadable_refusal_outside_prepared_window() {
    let fixture = queue_window_fixture(65, false);
    std::fs::write(
        fixture.root().join("vault/wiki/documents/source-064.md"),
        [0xff],
    )
    .unwrap();
    let (output, report) = fixture.run("process");
    assert!(!output.status.success(), "{report}");
    assert_eq!(report["semantic_calls"], 0);
    assert_eq!(report["queue_planning"]["prepared_targets"], 0);
    assert!(!fixture.root().join("arguments").exists());
    assert!(
        !fixture
            .root()
            .join("vault/.lorekeeper/execution-cursor.json")
            .exists()
    );
}

#[test]
fn native_queue_shares_discovery_without_changing_existing_task_selection_ids() {
    let (fixture, task) =
        progressive_extraction_fixture("codex", RECORD_PROGRESSIVE_EXTRACTION, 12, 65536, 16384);
    let mut input = task.input.clone();
    input.insert("target".into(), serde_json::to_value(&task.target).unwrap());
    let mut input: lk_queue::ExtractConceptsRequest =
        serde_json::from_value(serde_json::Value::Object(input)).unwrap();
    input.target.vault_path = "wiki/documents/source-two.md".into();
    let queue = fixture.root().join("vault/.lorekeeper/queue");
    let second = lk_queue::QueueLlmClient::new(queue.clone()).build_task(
        "ext",
        lk_queue::TaskKind::ExtractConcepts,
        input,
    );
    let source =
        std::fs::read_to_string(fixture.root().join("vault/wiki/documents/source.md")).unwrap();
    std::fs::write(
        fixture.root().join("vault/wiki/documents/source-two.md"),
        source.replace(&task.cache_hash, &second.cache_hash),
    )
    .unwrap();
    std::fs::write(
        queue.join("fixture.jsonl"),
        format!(
            "{}\n{}\n",
            serde_json::to_string(&task).unwrap(),
            serde_json::to_string(&second).unwrap()
        ),
    )
    .unwrap();
    let config = std::fs::read_to_string(&fixture.config)
        .unwrap()
        .replace("max_calls: 64", "max_calls: 1");
    std::fs::write(&fixture.config, config).unwrap();
    let status = execution_json(&fixture, &["queue", "status", "--json"]);
    let mut expected = std::collections::BTreeSet::new();
    for task in [&task, &second] {
        let row = status["data"]["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["task_id"] == task.task_id)
            .unwrap();
        let file = fixture.root().join("recipe.json");
        std::fs::write(&file, serde_json::to_vec(&serde_json::json!({"target_revision":row["target_revision"],"queries":[{"query":"transactional outbox","phrase":true,"spacing":false}]})).unwrap()).unwrap();
        let result = fixture
            .command()
            .args(["work", "discover", "--task-id", &task.task_id, "--file"])
            .arg(&file)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let selected = execution_json(&fixture, &["work", "select", "--task-id", &task.task_id]);
        expected.insert(
            selected["request"]["selection_id"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
    }
    assert_eq!(expected.len(), 2, "task purposes remain distinct");
    let (output, report) = fixture.run("process");
    assert!(
        !output.status.success(),
        "one screening call cannot settle both tasks: {report}"
    );
    assert_eq!(report["semantic_calls"], 1);
    assert_eq!(report["queue_planning"]["discovery_captures"], 1);
    assert_eq!(report["queue_planning"]["prepared_targets"], 2);
    assert!(
        expected.contains(report["work_steps"][0]["selection_id"].as_str().unwrap()),
        "existing selection IDs remain valid: {report}"
    );
}
