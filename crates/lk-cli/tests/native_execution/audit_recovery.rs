use super::*;

#[test]
fn bounded_audit_actions_are_guarded_and_unchanged_holds_spend_no_calls() {
    for engine in ["codex", "claude-code"] {
        let fixture = bounded_audit_fixture(engine, 64, RECORD_BOUNDED_AUDIT);
        std::fs::write(fixture.root().join("audit-response-mode"), "needs-action").unwrap();
        let (output, report) = fixture.run("knowledge");
        assert!(!output.status.success(), "{report}");
        assert_eq!(report["halted"], false, "{report}");
        let calls = bounded_calls(&fixture);
        let finalizer = calls
            .iter()
            .find(|call| call["action"] == "finish")
            .unwrap();
        let work = fixture
            .root()
            .join("vault/.lorekeeper/work")
            .join(finalizer["work_id"].as_str().unwrap());
        let action = work.join("finalization-action.json");
        let before = std::fs::read(&action).unwrap();
        assert!(!work.join("final.json").exists());
        let (output, report) = fixture.run("knowledge");
        assert!(!output.status.success(), "{report}");
        assert_eq!(report["semantic_calls"], 0, "{report}");
        assert_eq!(bounded_calls(&fixture).len(), calls.len());
        assert_eq!(std::fs::read(action).unwrap(), before);
    }
}

#[test]
fn bounded_audit_requires_a_structured_assessment_after_host_success() {
    for engine in ["codex", "claude-code"] {
        let fixture = bounded_audit_fixture(engine, 64, RECORD_BOUNDED_AUDIT);
        std::fs::write(fixture.root().join("audit-response-mode"), "missing").unwrap();
        let (output, report) = fixture.run("knowledge");
        assert!(!output.status.success(), "{report}");
        assert_eq!(report["halted"], true, "{report}");
        let calls = bounded_calls(&fixture);
        let finalizer = calls
            .iter()
            .find(|call| call["action"] == "finish")
            .unwrap();
        let work = fixture
            .root()
            .join("vault/.lorekeeper/work")
            .join(finalizer["work_id"].as_str().unwrap());
        assert!(!work.join("final.json").exists());
        assert!(!work.join("assessment.json").exists());
        assert!(!work.join("finalization-action.json").exists());
    }
}

fn review_during_finalization(repeated: bool) -> String {
    RECORD_BOUNDED_AUDIT.replace(
        "    assessment={key:subject[key] for key in ['path','file_revision','input_revision']}",
        &format!(r#"    marker=root/'audit-repaired'
    if {repeated} or not marker.exists():
        directory=root/'vault/.lorekeeper/work'/work_id
        if not marker.exists():
            before={{str(path.relative_to(root)):list(path.read_bytes()) for path in directory.rglob('*') if path.is_file()}}
            (root/'paid-before.json').write_text(json.dumps(before))
            (root/'old-work').write_text(work_id)
        ordinal=len(list((root/'vault/wiki/explorations').glob('repair-*.md')))
        review='wiki/explorations/repair-'+str(ordinal)+'.md'
        fingerprint=cli('wiki','fingerprint','--file',str(root/'vault'/subject['path']))
        metadata={{'status':'open','subjects':[subject['path']],'evidence':[{{'path':subject['path'],'revision':fingerprint['source_revision']}}]}}
        content='---\ntype: exploration\ntitle: Evidence gap\nreview: '+json.dumps(metadata)+'\n---\n## Question\nDoes the retained source support this claim?\n'
        path=root/'curation.json'
        path.write_text(json.dumps({{'evidence':[{{'path':subject['path'],'revision':fingerprint['file_revision']}}],'concepts':[],'reviews':[{{'path':review,'revision':None,'content':content}}]}}))
        cli('wiki','curate','--file',str(path))
        marker.write_text('review published')
        cli('work','plan','--path',subject['path'])
        with (root/'bounded-calls.jsonl').open('a') as stream:
            stream.write(json.dumps({{'action':'changed-input','work_id':work_id}})+'\n')
        respond({{'results':[{{'status':'needs-action','disposition':None,'reason':'Scoped repair changed the selected inputs.'}}]}})
        raise SystemExit(0)
    assert request['context']['purpose']['review_dependencies']==['wiki/explorations/repair-0.md']
    assessment={{key:subject[key] for key in ['path','file_revision','input_revision']}}"#,
            repeated = if repeated { "True" } else { "False" }),
    ).replace(
        "reason='Fixture completes guarded structural coverage; no semantic truth assertion.', reviews=[]",
        "reason='A current scoped review records the remaining evidence gap.', reviews=['wiki/explorations/repair-0.md']",
    ).replace("disposition='reviewed'", "disposition='follow-up'")
        .replace("\"$TEST_EXECUTION_ROOT/native-request.txt\"", "\"$TEST_EXECUTION_ROOT/native-request-$$.txt\"")
        .replace("(root/'native-request.txt')", "(root/('native-request-'+str(os.getpid())+'.txt'))")
}

#[test]
fn audit_replans_after_its_own_review_and_preserves_paid_predecessor_on_both_hosts() {
    for engine in ["codex", "claude-code"] {
        for parallel in [1, 2] {
            let fixture = bounded_audit_fixture(engine, 64, &review_during_finalization(false));
            let config = std::fs::read_to_string(&fixture.config).unwrap().replace(
                "max_calls: 64}",
                &format!("max_calls: 64, max_parallel: {parallel}}}"),
            );
            std::fs::write(&fixture.config, config).unwrap();
            let (output, report) = fixture.run("knowledge");
            assert!(
                output.status.success(),
                "{report}\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let calls = bounded_calls(&fixture);
            assert_eq!(report["semantic_calls"], calls.len());
            assert_eq!(
                calls
                    .iter()
                    .filter(|call| call["action"] == "changed-input")
                    .count(),
                1
            );
            let old = std::fs::read_to_string(fixture.root().join("old-work")).unwrap();
            assert!(
                calls
                    .iter()
                    .any(|call| call["action"] == "inspect" && call["work_id"] != old)
            );
            assert_eq!(
                report["audit"][0]["assessed_path"],
                "wiki/concepts/large.md"
            );
            let before: std::collections::BTreeMap<String, Vec<u8>> = serde_json::from_slice(
                &std::fs::read(fixture.root().join("paid-before.json")).unwrap(),
            )
            .unwrap();
            for (path, bytes) in before {
                assert_eq!(std::fs::read(fixture.root().join(path)).unwrap(), bytes);
            }
        }
    }
}

#[test]
fn audit_replanning_never_swallows_corruption_or_configuration_drift() {
    for change in ["corrupt-plan", "configuration"] {
        let mutation = if change == "corrupt-plan" {
            "(directory/'plan.json').write_text('{}')"
        } else {
            "(root/'config.yaml').write_text((root/'config.yaml').read_text()+'\\n# changed during finalization\\n')"
        };
        let behavior = review_during_finalization(false).replace(
            "        raise SystemExit(0)",
            &format!("        {mutation}\n        raise SystemExit(0)"),
        );
        let fixture = bounded_audit_fixture("codex", 64, &behavior);
        let (output, report) = fixture.run("knowledge");
        assert!(!output.status.success());
        assert_eq!(report["halted"], true, "{change}: {report}");
        assert_eq!(
            bounded_calls(&fixture)
                .iter()
                .filter(|call| call["action"] == "changed-input")
                .count(),
            1
        );
        assert!(
            !report["stages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|stage| stage["name"] == "bounded audit replanning")
        );
        if change == "corrupt-plan" {
            let old = std::fs::read_to_string(fixture.root().join("old-work")).unwrap();
            assert_eq!(
                std::fs::read_to_string(
                    fixture
                        .root()
                        .join("vault/.lorekeeper/work")
                        .join(old)
                        .join("plan.json")
                )
                .unwrap(),
                "{}"
            );
        }
    }
}

#[test]
fn audit_repeated_context_changes_defer_without_unbounded_retries_on_both_hosts() {
    for engine in ["codex", "claude-code"] {
        let fixture = bounded_audit_fixture(engine, 64, &review_during_finalization(true));
        let (output, report) = fixture.run("knowledge");
        assert!(!output.status.success());
        assert_eq!(report["halted"], false, "{report}");
        let calls = bounded_calls(&fixture);
        assert_eq!(report["semantic_calls"], calls.len());
        assert_eq!(
            calls
                .iter()
                .filter(|call| call["action"] == "changed-input")
                .count(),
            2
        );
        assert!(report["semantic_calls"].as_u64().unwrap() < 64);
        assert!(
            report["stages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|stage| stage["name"] == "bounded audit inputs unstable"
                    && stage["state"] == "failed")
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
}

#[test]
fn audit_replanning_keeps_the_current_call_budget_and_resumes_paid_nodes() {
    let fixture = bounded_audit_fixture("codex", 64, &review_during_finalization(false));
    let (output, complete) = fixture.run("knowledge");
    assert!(output.status.success(), "{complete}");
    let calls = bounded_calls(&fixture);
    let changed = calls
        .iter()
        .position(|call| call["action"] == "changed-input")
        .unwrap()
        + 1;
    let fixture = bounded_audit_fixture("codex", changed, &review_during_finalization(false));
    let (output, report) = fixture.run("knowledge");
    assert!(!output.status.success());
    assert_eq!(report["semantic_calls"], changed);
    assert_eq!(bounded_calls(&fixture).len(), changed);
    assert!(
        report["stages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|stage| stage["name"] == "semantic call budget")
    );
    let old = std::fs::read_to_string(fixture.root().join("old-work")).unwrap();
    let mut completed = false;
    for _ in 0..4 {
        let (output, resumed) = fixture.run("knowledge");
        assert!(resumed["semantic_calls"].as_u64().unwrap() <= changed as u64);
        assert!(
            bounded_calls(&fixture)
                .iter()
                .skip(changed)
                .all(|call| call["work_id"] != old)
        );
        if output.status.success() {
            assert_eq!(
                resumed["audit"][0]["assessed_path"],
                "wiki/concepts/large.md"
            );
            completed = true;
            break;
        }
        assert!(
            resumed["stages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|stage| stage["name"] == "semantic call budget"),
            "{resumed}"
        );
    }
    assert!(
        completed,
        "fresh Work must resume without repeating paid predecessor reads"
    );
}

#[test]
fn bounded_audit_replans_a_workflow_merge_to_its_current_survivor() {
    let behavior = RECORD_BOUNDED_AUDIT.replace(
        "    assessment={key:subject[key] for key in ['path','file_revision','input_revision']}",
        r"    if not (root/'merged-once').exists():
        target='wiki/concepts/z-survivor.md'
        before={str(path.relative_to(root)):list(path.read_bytes()) for path in (root/'vault/.lorekeeper/work'/work_id).rglob('*') if path.is_file()}
        (root/'paid-before.json').write_text(json.dumps(before))
        subprocess.run([os.environ['LORE_BIN'],'graph','merge','large','z-survivor','--force','--from-revision',cli('wiki','read',subject['path'])['file_revision'],'--into-revision',cli('wiki','read',target)['file_revision']],capture_output=True,text=True,check=True)
        subprocess.run([os.environ['LORE_BIN'],'graph','backlinks-sync'],capture_output=True,text=True,check=True)
        candidate=cli('wiki','audit-plan','--path',target)['candidates'][0]
        discovery={key:candidate[key] for key in ['path','file_revision']}
        discovery['queries']=[{'query':candidate['title'],'phrase':True,'spacing':False}]
        path=root/'discovery.json'
        path.write_text(json.dumps(discovery))
        cli('wiki','audit-discover','--file',str(path))
        (root/'merged-once').write_text('merged')
        respond({'results':[{'status':'needs-action','disposition':None,'reason':'The supported merge changed this audit subject.'}]})
        raise SystemExit(0)
    assessment={key:subject[key] for key in ['path','file_revision','input_revision']}",
    );
    let fixture = bounded_audit_fixture("codex", 64, &behavior);
    write_audit_concept(&fixture, "z-survivor");
    let (output, report) = fixture.run("knowledge");
    assert!(
        output.status.success(),
        "{report}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        report["audit"][0]["requested_path"],
        "wiki/concepts/large.md"
    );
    assert_eq!(
        report["audit"][0]["assessed_path"],
        "wiki/concepts/z-survivor.md"
    );
    let before: std::collections::BTreeMap<String, Vec<u8>> =
        serde_json::from_slice(&std::fs::read(fixture.root().join("paid-before.json")).unwrap())
            .unwrap();
    for (path, bytes) in before {
        assert_eq!(std::fs::read(fixture.root().join(path)).unwrap(), bytes);
    }
}
