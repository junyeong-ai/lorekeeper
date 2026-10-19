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
    let vault = fixture.root().join("vault");
    let evidence = "wiki/documents/reviewed.md";
    std::fs::create_dir_all(vault.join("wiki/documents")).unwrap();
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
            assert!(arguments.starts_with("-p\n/lore-process:lore-process\n"));
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
    assert!(arguments.contains("unrelated needs-reassessment remains backlog"));
    assert!(arguments.contains("invalid reviews or review inspection errors fail"));
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
    write_review_attention(&fixture, "wiki/documents/reviewed.md", false);
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
    assert_eq!(plan["candidates"].as_array().unwrap().len(), 1);
}

#[test]
fn oversized_audit_input_retains_work_without_launching_an_agent() {
    let fixture = Fixture::new("codex", 30, "exit 0");
    let config = std::fs::read_to_string(&fixture.config).unwrap()
        + "graph:\n  audit:\n    batch_size: 1\n    max_input_bytes: 1\n";
    std::fs::write(&fixture.config, config).unwrap();
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
    assert!(result["execution"].is_null());
    assert!(
        result["stages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|stage| stage["name"] == "audit input bound" && stage["state"] == "failed")
    );
    assert!(!fixture.root().join("arguments").exists());
    assert!(
        !fixture
            .root()
            .join("vault/.lorekeeper/knowledge-audit.json")
            .exists()
    );
    let plan = fixture
        .command()
        .args(["wiki", "audit-plan"])
        .output()
        .unwrap();
    assert!(plan.status.success());
    let plan: serde_json::Value = serde_json::from_slice(&plan.stdout).unwrap();
    assert_eq!(plan["candidates"].as_array().unwrap().len(), 1);
    assert_eq!(plan["unreviewed"], 1);
    assert_eq!(plan["input"]["status"], "oversized");
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
