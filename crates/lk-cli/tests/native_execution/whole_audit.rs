use super::*;

fn fixture(engine: &str) -> Fixture {
    let behavior = RECORD_AUDIT.replace(
        "if '--output-last-message' in sys.argv:",
        "for reply, subject in zip(value['results'], request['subjects']):\n    if subject['context']['subject']['path']=='wiki/concepts/a-held.md':\n        reply.update(status='needs-action',disposition=None,reason='The selected source requires an explicit decision.')\nif '--output-last-message' in sys.argv:",
    );
    let fixture = Fixture::new(engine, 30, &behavior);
    let config =
        std::fs::read_to_string(&fixture.config).unwrap() + "graph:\n  audit:\n    batch_size: 2\n";
    std::fs::write(&fixture.config, config).unwrap();
    for name in ["a-held", "b-ready"] {
        write_audit_concept(&fixture, name);
    }
    fixture
}

fn action(fixture: &Fixture) -> PathBuf {
    fixture
        .root()
        .join("vault/.lorekeeper/audit-actions")
        .join(format!(
            "{}.json",
            lk_core::concept::file_revision("wiki/concepts/a-held.md")
        ))
}

#[test]
fn whole_actions_preserve_independent_progress_and_ignore_batch_peers_on_both_hosts() {
    for engine in ["codex", "claude-code"] {
        let fixture = fixture(engine);
        let (output, report) = fixture.run("knowledge");
        assert!(!output.status.success(), "{report}");
        assert_eq!(report["halted"], false, "{report}");
        assert_eq!(report["semantic_calls"], 1, "{report}");
        assert_eq!(
            report["audit"][0]["assessed_path"], "wiki/concepts/b-ready.md",
            "{report}"
        );
        let before = std::fs::read(action(&fixture)).unwrap();
        write_audit_concept(&fixture, "c-later");
        let (_, report) = fixture.run("knowledge");
        assert_eq!(report["semantic_calls"], 1, "{report}");
        assert_eq!(
            report["audit"][0]["assessed_path"], "wiki/concepts/c-later.md",
            "{report}"
        );
        assert_eq!(std::fs::read(action(&fixture)).unwrap(), before);
        let (_, report) = fixture.run("knowledge");
        assert_eq!(report["semantic_calls"], 0, "{report}");
        assert_eq!(std::fs::read(action(&fixture)).unwrap(), before);

        let output = fixture
            .command()
            .args(["wiki", "audit-plan", "--path", "wiki/concepts/a-held.md"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let plan: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let candidate = &plan["candidates"][0];
        let file = fixture.root().join("manual-assessment.json");
        std::fs::write(&file, serde_json::to_vec(&serde_json::json!({"assessments":[{
            "path":candidate["path"], "file_revision":candidate["file_revision"], "input_revision":candidate["input_revision"],
            "disposition":"reviewed", "reason":"Explicit fixture assessment of the complete current input.", "reviews":[],
        }]})).unwrap()).unwrap();
        let output = fixture
            .command()
            .args(["wiki", "audit-record", "--file"])
            .arg(file)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!action(&fixture).exists());
        let (output, report) = fixture.run("knowledge");
        assert!(output.status.success(), "{report}");
        assert_eq!(report["semantic_calls"], 0, "{report}");
    }
}

#[test]
fn explicit_unfinished_plans_never_bypass_whole_holds_and_corrupt_holds_refuse() {
    let fixture = fixture("codex");
    let (_, report) = fixture.run("knowledge");
    assert_eq!(report["semantic_calls"], 1, "{report}");
    let before = std::fs::read(action(&fixture)).unwrap();
    let planned = fixture
        .command()
        .args(["work", "plan", "--path", "wiki/concepts/a-held.md"])
        .output()
        .unwrap();
    assert!(
        planned.status.success(),
        "{}",
        String::from_utf8_lossy(&planned.stderr)
    );
    let (_, report) = fixture.run("knowledge");
    assert_eq!(report["semantic_calls"], 0, "{report}");
    assert_eq!(std::fs::read(action(&fixture)).unwrap(), before);
    std::fs::write(action(&fixture), b"{}").unwrap();
    let (_, report) = fixture.run("knowledge");
    assert_eq!(report["semantic_calls"], 0, "{report}");
    assert_eq!(report["halted"], true, "{report}");
    assert_eq!(std::fs::read(action(&fixture)).unwrap(), b"{}");
}
