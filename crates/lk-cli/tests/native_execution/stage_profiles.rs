use super::*;

pub(super) fn route_profiles(
    fixture: &Fixture,
    base: &str,
    stages: &[(&str, &str)],
    profiles: &[(&str, &str)],
) {
    let path = fixture.root().join("agent executable");
    let raw = std::fs::read_to_string(&path).unwrap();
    let traced = |raw: &str, name: &str| {
        let trace = format!(
            r#"python3 - "$@" <<'TRACE'
import json,os,sys
from pathlib import Path
root=Path(os.environ['TEST_EXECUTION_ROOT'])/'stage-calls'
root.mkdir(exist_ok=True)
model=next((arg.split('=',1)[1] for arg in sys.argv if arg.startswith('--model=')),None)
row={{'profile':'{name}','model':model,'arguments':sys.argv[1:],
     'effort':os.environ.get('CLAUDE_CODE_EFFORT_LEVEL'),
     'policy':os.environ.get('LORE_EXECUTION_PROFILE')}}
(root/(str(os.getpid())+'.json')).write_text(json.dumps(row))
TRACE
"#
        );
        let marker = r#"printf '%s\n' "$@""#;
        assert_eq!(raw.matches(marker).count(), 1);
        raw.replacen(marker, &(trace + marker), 1)
    };
    std::fs::write(&path, traced(&raw, "primary")).unwrap();
    let mut additions = String::new();
    for (name, engine) in profiles {
        let executable = fixture.root().join(format!("{name} executable"));
        let version = if *engine == "codex" {
            "codex-cli 0.163.0"
        } else {
            "2.1.303 (Claude Code)"
        };
        let original = if base == "codex" {
            "codex-cli 0.163.0"
        } else {
            "2.1.303 (Claude Code)"
        };
        std::fs::write(&executable, traced(&raw.replace(original, version), name)).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let effort = if *engine == "codex" {
            "model_reasoning_effort: medium"
        } else {
            "effort: high"
        };
        additions.push_str(&format!("    {name}: {{engine: {engine}, executable: {}, timeout_seconds: 30, model: route-{name}, {effort}}}\n",serde_json::to_string(&executable).unwrap()));
    }
    let mapping = stages
        .iter()
        .map(|(stage, name)| ((*stage).to_owned(), (*name).to_owned()))
        .collect::<std::collections::BTreeMap<_, _>>();
    let config = std::fs::read_to_string(&fixture.config).unwrap().replace(
        "  profiles:\n",
        &format!(
            "  stages: {}\n  profiles:\n{additions}",
            serde_json::to_string(&mapping).unwrap()
        ),
    );
    std::fs::write(&fixture.config, config).unwrap();
    configure_profile_options(fixture, base);
}

fn routed_calls(fixture: &Fixture) -> Vec<serde_json::Value> {
    std::fs::read_dir(fixture.root().join("stage-calls"))
        .unwrap()
        .map(|entry| {
            serde_json::from_slice(&std::fs::read(entry.unwrap().path()).unwrap()).unwrap()
        })
        .collect()
}

#[test]
fn bounded_stages_route_hosts_and_keep_the_origin_policy_on_both_engines() {
    for (base, reader) in [("codex", "claude-code"), ("claude-code", "codex")] {
        let fixture = bounded_audit_fixture(base, 64, RECORD_BOUNDED_AUDIT);
        route_profiles(
            &fixture,
            base,
            &[("inspect", "reader"), ("reduce", "reducer")],
            &[("reader", reader), ("reducer", base)],
        );
        let config = std::fs::read_to_string(&fixture.config).unwrap().replace(
            "  default_profile: primary\n",
            "  default_profile: primary\n  operations: {knowledge: reader}\n",
        );
        std::fs::write(&fixture.config, config).unwrap();
        record_discovery(&fixture, "wiki/concepts/large.md");
        let output = fixture
            .command()
            .args(["run", "knowledge", "--profile", "primary", "--json"])
            .output()
            .unwrap();
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(
            output.status.success(),
            "{report}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(report["execution"]["policy_profile"], "primary");
        assert_eq!(report["execution"]["work"]["max_input_bytes"], 8192);
        let calls = routed_calls(&fixture);
        assert_eq!(
            report["semantic_calls"].as_u64().unwrap() as usize,
            calls.len()
        );
        assert!(calls.iter().all(|call| call["policy"] == "primary"));
        let mut seen = std::collections::BTreeSet::new();
        for step in report["work_steps"].as_array().unwrap() {
            let stage = step["stages"][0].as_str().unwrap();
            seen.insert(stage);
            let (profile, engine) = match stage {
                "inspect" => ("reader", reader),
                "reduce" => ("reducer", base),
                "audit" => ("primary", base),
                _ => panic!("{step}"),
            };
            assert_eq!(step["execution"]["profile"], profile);
            assert_eq!(step["execution"]["engine"], engine);
            let actual = calls
                .iter()
                .find(|call| call["profile"] == profile)
                .unwrap();
            assert_eq!(actual["model"], step["execution"]["requested_model"]);
            if engine == "claude-code" {
                assert_eq!(actual["effort"], step["execution"]["requested_effort"]);
            } else {
                assert!(
                    actual["arguments"]
                        .as_array()
                        .unwrap()
                        .contains(&serde_json::json!(format!(
                            "model_reasoning_effort=\"{}\"",
                            step["execution"]["requested_effort"].as_str().unwrap()
                        )))
                );
            }
        }
        assert_eq!(
            seen,
            std::collections::BTreeSet::from(["inspect", "reduce", "audit"])
        );
        let (_, repeated) = fixture.run("knowledge");
        assert_eq!(repeated["semantic_calls"], 0, "{repeated}");
        assert_eq!(routed_calls(&fixture).len(), calls.len());
    }
}

#[test]
fn routed_stages_share_the_job_call_budget() {
    let fixture = bounded_audit_fixture("codex", 64, RECORD_BOUNDED_AUDIT);
    route_profiles(
        &fixture,
        "codex",
        &[("inspect", "reader"), ("reduce", "reducer")],
        &[("reader", "claude-code"), ("reducer", "codex")],
    );
    record_discovery(&fixture, "wiki/concepts/large.md");
    let plan = execution_json(
        &fixture,
        &["work", "plan", "--path", "wiki/concepts/large.md"],
    );
    let limit = plan["parts"].as_u64().unwrap() + 1;
    let config = std::fs::read_to_string(&fixture.config)
        .unwrap()
        .replace("max_calls: 64", &format!("max_calls: {limit}"));
    std::fs::write(&fixture.config, config).unwrap();
    let (_, report) = fixture.run("knowledge");
    assert_eq!(report["halted"], false, "{report}");
    assert_eq!(report["budget_exhausted"], true, "{report}");
    assert_eq!(report["semantic_calls"], limit, "{report}");
    assert_eq!(routed_calls(&fixture).len() as u64, limit);
    assert!(
        report["work_steps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["stages"][0] == "reduce")
    );
}

#[test]
fn routed_screening_and_sections_leave_unmapped_extraction_on_the_base() {
    let section = WHOLE_SECTION.replace("if request['kind']!=", "if request.get('kind')!=");
    let (fixture, _) = progressive_extraction_fixture(
        "codex",
        &format!("{section}\n{RECORD_PROGRESSIVE_EXTRACTION}"),
        4,
        65536,
        16384,
    );
    route_profiles(
        &fixture,
        "codex",
        &[("screening", "reader"), ("section", "short")],
        &[("reader", "claude-code"), ("short", "codex")],
    );
    let root = fixture.root().join("vault");
    let queue = root.join(".lorekeeper/queue");
    let request:lk_queue::SummarizeRequest=serde_json::from_value(serde_json::json!({"text":"Independent source.","max_sentences":2,"locale":"en","target":{"vault_path":"wiki/documents/zz-independent.md","anchor":"## Summary","kind":"document-summary"}})).unwrap();
    let task = lk_queue::QueueLlmClient::new(queue.clone()).build_task(
        "summary",
        lk_queue::TaskKind::Summarize,
        request,
    );
    std::fs::write(root.join(&task.target.vault_path),format!("---\ntype: document\ntitle: Independent\nllm_inputs:\n  summary: {}\n---\n## Summary\n",task.cache_hash)).unwrap();
    lk_queue::write_tasks_atomic(
        &queue.join("independent.jsonl"),
        std::slice::from_ref(&task),
    )
    .unwrap();
    let (output, report) = fixture.run("process");
    assert!(
        output.status.success(),
        "{report}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut seen = std::collections::BTreeSet::new();
    for step in report["work_steps"].as_array().unwrap() {
        let stage = step["stages"][0].as_str().unwrap();
        seen.insert(stage);
        let profile = match stage {
            "screening" => "reader",
            "section" => "short",
            "extraction" => "primary",
            _ => panic!("{step}"),
        };
        assert_eq!(step["execution"]["profile"], profile);
    }
    assert_eq!(
        seen,
        std::collections::BTreeSet::from(["screening", "section", "extraction"])
    );
    let calls = routed_calls(&fixture);
    assert!(calls.iter().all(|call| call["policy"] == "primary"));
    assert_eq!(
        calls.len() as u64,
        report["semantic_calls"].as_u64().unwrap()
    );
}

#[test]
fn separated_audit_discovery_and_assessment_use_their_explicit_profiles() {
    let behavior = format!(
        r#"
if [ ! -f "$TEST_EXECUTION_ROOT/discovery-finished" ]; then
python3 - <<'DISCOVER'
import json,os,subprocess
from pathlib import Path
root=Path(os.environ['TEST_EXECUTION_ROOT']);lore=os.environ['LORE_BIN']
plan=json.loads(subprocess.check_output([lore,'wiki','audit-plan']))
candidate=plan['candidates'][0]
request={{k:candidate[k] for k in ['path','file_revision']}}
request['queries']=[{{'query':candidate['title'],'phrase':True,'spacing':False}}]
path=root/'discovery.json';path.write_text(json.dumps(request))
subprocess.run([lore,'wiki','audit-discover','--file',str(path)],check=True)
(root/'discovery-finished').write_text('complete')
DISCOVER
exit $?
fi
{RECORD_BOUNDED_AUDIT}
"#
    );
    let fixture = Fixture::new("codex", 30, &behavior);
    let config=std::fs::read_to_string(&fixture.config).unwrap()
        .replace("  default_profile: primary\n","  default_profile: primary\n  work: {max_input_bytes: 8192, max_result_bytes: 1024, max_calls: 64}\n")
        + "graph:\n  audit:\n    batch_size: 1\n    max_input_bytes: 4096\n";
    std::fs::write(&fixture.config, config).unwrap();
    route_profiles(
        &fixture,
        "codex",
        &[("discovery", "scout"), ("audit", "reviewer")],
        &[("scout", "codex"), ("reviewer", "claude-code")],
    );
    write_audit_concept(&fixture, "large");
    let path = fixture.root().join("vault/wiki/concepts/large.md");
    let text =
        std::fs::read_to_string(&path).unwrap() + &"\nAssigned source observation.\n".repeat(400);
    std::fs::write(path, text).unwrap();
    let (output, report) = fixture.run("knowledge");
    assert!(
        output.status.success(),
        "{report}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let steps = report["work_steps"].as_array().unwrap();
    let discovery = steps
        .iter()
        .find(|step| step["stages"][0] == "discovery")
        .unwrap();
    let audit = steps
        .iter()
        .find(|step| step["stages"][0] == "audit")
        .unwrap();
    for (step, stage, profile) in [
        (discovery, "discovery", "scout"),
        (audit, "audit", "reviewer"),
    ] {
        assert_eq!(step["stages"][0], stage);
        assert_eq!(step["execution"]["profile"], profile);
    }
    assert_eq!(discovery["role"], "interactive");
    assert_eq!(discovery["outcome"], "returned");
    assert_eq!(
        report["retained_progress"]["evidence_nodes"]
            .as_u64()
            .unwrap(),
        steps
            .iter()
            .filter(|s| s["role"] == "evidence" && s["outcome"] == "recorded")
            .count() as u64
    );
    assert_eq!(
        report["semantic_calls"].as_u64().unwrap(),
        routed_calls(&fixture).len() as u64
    );
    assert_eq!(report["audit"].as_array().unwrap().len(), 1);
    assert!(
        routed_calls(&fixture)
            .iter()
            .all(|call| call["policy"] == "primary")
    );
}
