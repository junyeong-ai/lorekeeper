use super::*;
use std::collections::{BTreeMap, BTreeSet};

pub(super) const INSPECT_FIXTURE: &str = r#"
import json, os, subprocess, sys
from pathlib import Path
root=Path(os.environ['TEST_EXECUTION_ROOT'])
subject=sys.argv[1]
def cli(*args): return json.loads(subprocess.check_output([os.environ['LORE_BIN'],*args]))
while True:
    candidate=cli('wiki','audit-plan','--path',subject)['candidates'][0]
    if (candidate.get('discovery') or {}).get('selection') is None: break
    packet=cli('wiki','audit-candidates',subject)
    output=packet.get('refresh')
    if output is None:
        request=packet.get('request')
        if request is None:
            assert packet['progress']['complete'], packet
            break
        output={'action':'record','path':subject,'purpose_revision':request['purpose_revision'],
                'packet_revision':packet['request_revision'],'reassess':False,
                'decisions':[{'candidate_key':card['candidate_key'],'decision':'inspect',
                              'reason':'Explicit fixture decision to inspect the original full context.'}
                             for card in request['candidates']]}
    file=root/'fixture-audit-select.json'
    file.write_text(json.dumps(output))
    cli('wiki','audit-select','--file',str(file))
"#;

const SCREEN: &str = r#"
python3 - "$@" <<'SCREEN'
import json, os, subprocess, sys
from pathlib import Path
root=Path(os.environ['TEST_EXECUTION_ROOT'])
prompt=Path(os.environ['TEST_INTERACTIVE_INPUT']).read_text() if Path(os.environ['TEST_INTERACTIVE_INPUT']).exists() else ''
prompt=prompt or '\n'.join(sys.argv[1:])
prefix='screen. Read the bounded audit discovery packet at '
if prefix not in prompt: raise SystemExit(100)
file,_=json.JSONDecoder().raw_decode(prompt.split(prefix,1)[1])
request=json.loads(Path(file).read_text())
revision=prompt.split('its exact packet_revision is ',1)[1].split('.',1)[0]
assert request['schema']=='audit-applicability-v1', request
assert not request['reassess'], 'automatic screening must not reassess a prior decision'
mode=(root/'screen-mode').read_text()
assert mode!='forbid', 'capacity admission must not invoke a screening host'
with (root/'screen-calls.jsonl').open('a') as stream:
    stream.write(json.dumps({'packet_revision':revision,'keys':[card['candidate_key'] for card in request['candidates']]})+'\n')
if mode=='missing': raise SystemExit(0)
def cli(*args): return json.loads(subprocess.check_output([os.environ['LORE_BIN'],*args]))
if mode=='other-packet':
    other=cli('wiki','audit-candidates',request['subject'],'--limit','1')
    request=other['request']
    revision=other['request_revision']
output={'action':'record','path':request['subject'],'purpose_revision':request['purpose_revision'],
        'packet_revision':revision,'reassess':False,
        'decisions':[{'candidate_key':card['candidate_key'],
                      'decision':('defer' if mode=='defer-first' and card['candidate']['path'].endswith('/candidate-00.md') else 'not-selected'),
                      'reason':'The declared scope is inapplicable, or its explicit prerequisite is unresolved.'}
                     for card in request['candidates']]}
file=root/'native-audit-select.json';file.write_text(json.dumps(output))
cli('wiki','audit-select','--file',str(file))
SCREEN
screen_status=$?
if [ "$screen_status" -ne 100 ]; then exit "$screen_status"; fi
"#;

const SUBJECT: &str = "wiki/concepts/a-subject.md";
const QUERY: &str = "candidate-window";

fn json(fixture: &Fixture, args: &[&str]) -> serde_json::Value {
    let output = fixture.command().args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn discover(fixture: &Fixture, subject: &str, query: &str) {
    let output = fixture
        .command()
        .args(["graph", "backlinks-sync"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let raw = std::fs::read_to_string(fixture.root().join("vault").join(subject)).unwrap();
    let file = fixture.root().join("screening-discovery.json");
    std::fs::write(
        &file,
        serde_json::to_vec(&serde_json::json!({
            "path":subject,"file_revision":lk_core::concept::file_revision(&raw),
            "queries":[{"query":query,"phrase":true,"spacing":false}],
        }))
        .unwrap(),
    )
    .unwrap();
    let output = fixture
        .command()
        .args(["wiki", "audit-discover", "--file"])
        .arg(file)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fixture(
    engine: &str,
    candidates: usize,
    mode: &str,
    capacity: bool,
    audit_host: &str,
) -> Fixture {
    let fixture = Fixture::new(engine, 30, &format!("{SCREEN}\n{audit_host}"));
    let limit = if capacity {
        ", max_screening_batches: 1"
    } else {
        ""
    };
    let config = std::fs::read_to_string(&fixture.config).unwrap().replace(
        "  default_profile: primary\n",
        &format!("  default_profile: primary\n  work: {{max_input_bytes: 65536, max_result_bytes: 16384, max_calls: 1{limit}}}\n"),
    ) + if capacity { "graph:\n  audit: {batch_size: 2}\n" } else { "graph:\n  audit: {batch_size: 1}\n" };
    std::fs::write(&fixture.config, config).unwrap();
    std::fs::write(fixture.root().join("screen-mode"), mode).unwrap();
    let vault = fixture.root().join("vault");
    std::fs::create_dir_all(vault.join("wiki/concepts")).unwrap();
    std::fs::create_dir_all(vault.join("wiki/documents")).unwrap();
    std::fs::write(
        vault.join(SUBJECT),
        "---\ntype: concept\ntitle: Subject\n---\n## Synthesis\nA scoped claim.\n\n## Sources\n",
    )
    .unwrap();
    for index in 0..candidates {
        std::fs::write(vault.join(format!("wiki/documents/candidate-{index:02}.md")),
            format!("---\ntype: document\ntitle: Candidate {index:02}\n---\nThe {QUERY} scope covers candidate {index:02}.\n")).unwrap();
    }
    discover(&fixture, SUBJECT, QUERY);
    fixture
}

fn packet(fixture: &Fixture) -> serde_json::Value {
    json(fixture, &["wiki", "audit-candidates", SUBJECT])
}

fn calls(fixture: &Fixture) -> Vec<serde_json::Value> {
    std::fs::read_to_string(fixture.root().join("screen-calls.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn receipts(fixture: &Fixture) -> BTreeMap<PathBuf, Vec<u8>> {
    let directory = fixture
        .root()
        .join("vault/.lorekeeper/audit-selection")
        .join(lk_core::concept::file_revision(SUBJECT))
        .join("packets");
    std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            (path.clone(), std::fs::read(path).unwrap())
        })
        .collect()
}

#[test]
fn native_audit_screening_resumes_partial_packets_without_repeating_paid_decisions() {
    for engine in ["codex", "claude-code"] {
        let fixture = fixture(engine, 21, "not-selected", false, RECORD_AUDIT);
        let (_, first) = fixture.run("knowledge");
        assert_eq!(first["halted"], false, "{first}");
        assert_eq!(first["semantic_calls"], 1, "{first}");
        let partial = packet(&fixture);
        assert!(partial["progress"]["not_selected"].as_u64().unwrap() > 0);
        assert!(partial["progress"]["unknown"].as_u64().unwrap() > 0);
        let before = receipts(&fixture);
        let (_, second) = fixture.run("knowledge");
        assert_eq!(second["halted"], false, "{second}");
        assert_eq!(second["semantic_calls"], 1, "{second}");
        let complete = packet(&fixture);
        assert_eq!(complete["progress"]["not_selected"], 21);
        assert_eq!(complete["progress"]["unknown"], 0);
        let recorded = calls(&fixture);
        assert_eq!(recorded.len(), 2);
        let keys = recorded
            .iter()
            .flat_map(|call| call["keys"].as_array().unwrap())
            .map(|key| key.as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(keys.len(), 21);
        assert_eq!(keys.iter().copied().collect::<BTreeSet<_>>().len(), 21);
        for (path, raw) in before {
            assert_eq!(std::fs::read(path).unwrap(), raw);
        }
        let (_, third) = fixture.run("knowledge");
        assert_eq!(third["semantic_calls"], 1, "{third}");
        assert_eq!(third["audit"][0]["assessed_path"], SUBJECT, "{third}");
        assert_eq!(calls(&fixture).len(), 2);
    }
}

#[test]
fn native_audit_screening_keeps_defer_and_advances_later_unknown_candidates() {
    for engine in ["codex", "claude-code"] {
        let fixture = fixture(engine, 21, "defer-first", false, RECORD_AUDIT);
        let (_, first) = fixture.run("knowledge");
        assert_eq!(first["halted"], false, "{first}");
        assert_eq!(first["semantic_calls"], 1, "{first}");
        assert_eq!(packet(&fixture)["progress"]["deferred"], 1);
        let before = receipts(&fixture);
        let (_, second) = fixture.run("knowledge");
        assert_eq!(second["semantic_calls"], 1, "{second}");
        let complete = packet(&fixture);
        assert_eq!(complete["progress"]["deferred"], 1);
        assert_eq!(complete["progress"]["not_selected"], 20);
        assert_eq!(complete["progress"]["unknown"], 0);
        assert!(complete["request"].is_null());
        assert_eq!(complete["progress"]["complete"], false);
        let (_, third) = fixture.run("knowledge");
        assert_eq!(third["halted"], false, "{third}");
        assert_eq!(calls(&fixture).len(), 2);
        assert_eq!(packet(&fixture)["progress"]["deferred"], 1);
        for (path, raw) in before {
            assert_eq!(std::fs::read(path).unwrap(), raw);
        }
    }
}

#[test]
fn native_audit_screening_requires_the_exact_assigned_packet_receipt() {
    for engine in ["codex", "claude-code"] {
        for mode in ["missing", "other-packet"] {
            let fixture = fixture(engine, 3, mode, false, RECORD_AUDIT);
            let (_, report) = fixture.run("knowledge");
            assert_eq!(report["halted"], true, "{mode}: {report}");
            assert_eq!(report["semantic_calls"], 1, "{report}");
            assert!(
                report["stages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|stage| stage["name"] == "audit screening receipt"
                        && stage["state"] == "failed"),
                "{report}"
            );
            assert_eq!(
                packet(&fixture)["progress"]["unknown"],
                if mode == "missing" { 3 } else { 2 }
            );
            if mode == "missing" {
                let (_, repeated) = fixture.run("knowledge");
                assert_eq!(repeated["halted"], true, "{repeated}");
                assert_eq!(repeated["semantic_calls"], 1, "{repeated}");
                assert_eq!(
                    calls(&fixture)[0]["packet_revision"],
                    calls(&fixture)[1]["packet_revision"]
                );
            }
        }
    }
}

#[test]
fn native_audit_screening_capacity_spends_no_screening_calls_and_preserves_ready_peer() {
    for engine in ["codex", "claude-code"] {
        let fixture = fixture(engine, 21, "forbid", true, RECORD_AUDIT);
        let config = std::fs::read_to_string(&fixture.config)
            .unwrap()
            .replace("batch_size: 2", "batch_size: 1");
        std::fs::write(&fixture.config, config).unwrap();
        let peer = "wiki/documents/candidate-00.md";
        discover(&fixture, peer, "unmatched independent fixture");
        screen_fixture_candidates(&fixture, peer);
        assert_eq!(
            json(&fixture, &["wiki", "audit-plan"])["candidates"][0]["path"],
            SUBJECT
        );
        let (_, report) = fixture.run("knowledge");
        assert_eq!(report["halted"], false, "{report}");
        assert_eq!(report["semantic_calls"], 1, "{report}");
        assert_eq!(report["audit"][0]["assessed_path"], peer, "{report}");
        assert!(calls(&fixture).is_empty());
        let held = packet(&fixture);
        assert_eq!(held["progress"]["unknown"], 21);
        assert_eq!(held["admission"]["required_batches"], 2);
    }
}

#[test]
fn native_audit_screening_indivisible_capacity_without_batch_cap_advances_ready_peer() {
    for engine in ["codex", "claude-code"] {
        let fixture = fixture(engine, 1, "forbid", false, RECORD_BOUNDED_AUDIT);
        let config = std::fs::read_to_string(&fixture.config).unwrap().replace(
            "max_input_bytes: 65536, max_result_bytes: 16384",
            "max_input_bytes: 4096, max_result_bytes: 1024",
        );
        std::fs::write(&fixture.config, config).unwrap();
        let subject = format!(
            "---\ntype: concept\ntitle: {}\n---\n## Synthesis\nA scoped claim.\n\n## Sources\n",
            "Bounded subject scope ".repeat(256),
        );
        std::fs::write(fixture.root().join("vault").join(SUBJECT), &subject).unwrap();
        discover(&fixture, SUBJECT, QUERY);
        let subject = std::fs::read_to_string(fixture.root().join("vault").join(SUBJECT)).unwrap();
        let peer = "wiki/documents/candidate-00.md";
        discover(&fixture, peer, "unmatched independent fixture");
        screen_fixture_candidates(&fixture, peer);
        assert_eq!(
            json(&fixture, &["wiki", "audit-plan"])["candidates"][0]["path"],
            SUBJECT
        );
        let (_, report) = fixture.run("knowledge");
        assert_eq!(report["halted"], false, "{report}");
        assert_eq!(report["semantic_calls"], 1, "{report}");
        assert_eq!(report["budget_exhausted"], true, "{report}");
        assert_eq!(report["retained_progress"]["evidence_nodes"], 1, "{report}");
        let steps = report["work_steps"].as_array().unwrap();
        assert_eq!(steps.len(), 1, "{report}");
        assert_eq!(steps[0]["role"], "evidence", "{report}");
        assert_eq!(steps[0]["node_kind"], "inspect", "{report}");
        let work_id = steps[0]["work_id"].as_str().unwrap();
        let plan: serde_json::Value = serde_json::from_slice(
            &std::fs::read(
                fixture
                    .root()
                    .join("vault/.lorekeeper/work")
                    .join(work_id)
                    .join("plan.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(plan["context"]["subject"]["path"], peer);
        assert!(calls(&fixture).is_empty());
        assert!(
            report["execution"]["work"]["max_screening_batches"].is_null(),
            "{report}"
        );
        assert_eq!(
            std::fs::read_to_string(fixture.root().join("vault").join(SUBJECT)).unwrap(),
            subject,
        );
        let plan = json(&fixture, &["wiki", "audit-plan", "--path", SUBJECT]);
        assert_eq!(
            plan["candidates"][0]["discovery"]["selection"]["unknown"],
            1
        );
    }
}
