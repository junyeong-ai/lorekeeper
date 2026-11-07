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

#[test]
fn fixed_assessment_capacity_defers_before_discovery_and_after_scope_expansion() {
    let behavior = r#"python3 - "$@" <<'DISCOVER'
import json, os, shutil, subprocess, sys
from pathlib import Path
root=Path(os.environ['TEST_EXECUTION_ROOT'])
prompt=Path(os.environ['TEST_INTERACTIVE_INPUT']).read_text()
prompt=prompt or '\n'.join(sys.argv[1:])
assert 'discover.' in prompt, 'fixed output identity must not launch an inspector or finalizer'
with (root/'discovery-calls').open('a') as stream: stream.write('discovery\n')
for path in (root/'reviews').glob('*.md'):
    shutil.copyfile(path,root/'vault/wiki/explorations'/path.name)
def cli(*args): return json.loads(subprocess.check_output([os.environ['LORE_BIN'],*args]))
candidate=cli('wiki','audit-plan','--path','wiki/concepts/capacity.md')['candidates'][0]
request={key:candidate[key] for key in ['path','file_revision']}
request['queries']=[{'query':'private absent capacity fixture','phrase':True,'spacing':False}]
path=root/'discovery.json';path.write_text(json.dumps(request))
cli('wiki','audit-discover','--file',str(path))
DISCOVER
"#;
    for during_discovery in [false, true] {
        let fixture = Fixture::new("codex", 30, behavior);
        let config = std::fs::read_to_string(&fixture.config).unwrap().replace(
            "  default_profile: primary\n",
            "  default_profile: primary\n  work: {max_input_bytes: 8192, max_result_bytes: 1024, max_calls: 4}\n",
        ) + "graph:\n  audit:\n    batch_size: 1\n    max_input_bytes: 4096\n";
        std::fs::write(&fixture.config, config).unwrap();
        let subject = "wiki/concepts/capacity.md";
        std::fs::create_dir_all(fixture.root().join("vault/wiki/concepts")).unwrap();
        std::fs::create_dir_all(fixture.root().join("vault/wiki/explorations")).unwrap();
        std::fs::write(
            fixture.root().join("vault").join(subject),
            include_str!("../behaviour/corpus/small/vault/wiki/concepts/unlinked-note.md")
                .replace("Unlinked note", "Capacity")
                .replace("unlinked-note", "capacity"),
        )
        .unwrap();
        let output = fixture
            .command()
            .args(["graph", "backlinks-sync"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let output = fixture
            .command()
            .args(["wiki", "fingerprint", "--file"])
            .arg(fixture.root().join("vault").join(subject))
            .output()
            .unwrap();
        assert!(output.status.success());
        let fingerprint: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let reviews = fixture.root().join("reviews");
        std::fs::create_dir(&reviews).unwrap();
        let mut paths = Vec::new();
        for index in 0..10 {
            let name = format!("review-{index}-{}.md", "x".repeat(120));
            let path = format!("wiki/explorations/{name}");
            let metadata = serde_json::json!({"status":"open","subjects":[subject],"evidence":[{"path":subject,"revision":fingerprint["source_revision"]}]});
            let content = format!(
                "---\ntype: exploration\ntitle: Capacity review {index}\nreview: {metadata}\n---\n## Question\nThe fixture source leaves this question open.\n"
            );
            std::fs::write(reviews.join(&name), &content).unwrap();
            if !during_discovery {
                std::fs::write(fixture.root().join("vault").join(&path), content).unwrap();
            }
            paths.push(path);
        }
        let minimum = serde_json::json!({"assessments":[{"path":subject,"file_revision":"f".repeat(64),"input_revision":"a".repeat(64),"disposition":"reviewed","reason":"x","reviews":paths}]});
        assert!(serde_json::to_vec(&minimum).unwrap().len() > 1024);
        let (output, report) = fixture.run("knowledge");
        assert!(!output.status.success(), "{report}");
        assert_eq!(
            report["halted"],
            false,
            "{report}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            report["semantic_calls"],
            usize::from(during_discovery),
            "{report}"
        );
        assert!(!fixture.root().join("vault/.lorekeeper/work").exists());
        if during_discovery {
            assert_eq!(
                std::fs::read_to_string(fixture.root().join("discovery-calls")).unwrap(),
                "discovery\n"
            );
        }
        let (_, report) = fixture.run("knowledge");
        assert_eq!(report["semantic_calls"], 0, "{report}");
        assert_eq!(report["halted"], false, "{report}");
    }
}
