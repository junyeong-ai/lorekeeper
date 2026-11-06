use super::*;

#[test]
fn explicit_queue_refinement_resumes_serial_and_parallel_hosts_with_independent_publication() {
    let behavior = build_extraction_behavior("")
        .replace("'value':{'concepts':[]}", "'value':{'synthesis':'The source retains its declared conditions and attribution.','related':''}")
        .replace("output={'status':'complete','observation':output,'action':None}", r#"output={'status':'complete','observation':output,'action':None}
        if request['kind']=='inspect':
            count=sum(len(item['text']) for item in request['input']['ranges'])
            if (root/'refinement-ready').exists():
                saved=json.loads((root/'vault/.lorekeeper/work'/work_id/'inspection-refinements.json').read_text())
                refined={part['id'] for entry in saved['entries'].values() for part in entry['parts']}
                if node_id in refined:
                    assert count<=20000, 'native refinement must honor the operator bound'
            else:
                try:
                    with (root/'initial-observation').open('x') as flag: flag.write(node_id)
                except FileExistsError:
                    output={'status':'needs-action','observation':None,'action':{'reason':'This assigned inspection requires explicit smaller units.'}}"#)
        .replace("\"$TEST_EXECUTION_ROOT/native-request.txt\"", "\"$TEST_EXECUTION_ROOT/native-request-$$.txt\"")
        .replace("(root/'native-request.txt')", "(root/('native-request-'+str(os.getpid())+'.txt'))");
    let section = WHOLE_SECTION
        .replace("/bin/cat > \"$TEST_EXECUTION_ROOT/whole-section-request.txt\"",
            "export TEST_REFINEMENT_SECTION_INPUT=\"$TEST_EXECUTION_ROOT/whole-section-request-$$.txt\"\n    /bin/cat > \"$TEST_REFINEMENT_SECTION_INPUT\"")
        .replace("(root/'whole-section-request.txt')", "Path(os.environ['TEST_REFINEMENT_SECTION_INPUT'])")
        .replace("exec < \"$TEST_EXECUTION_ROOT/whole-section-request.txt\"", "exec < \"$TEST_REFINEMENT_SECTION_INPUT\"");
    let behavior = format!("{section}\n{behavior}");
    for engine in ["codex", "claude-code"] {
        for parallel in [1, 6] {
            let fixture = Fixture::new(engine, 30, &behavior);
            let config = std::fs::read_to_string(&fixture.config).unwrap()
                .replace("concept_policy: supported-new", "concept_policy: disabled")
                .replace("  default_profile: primary\n", &format!(
                    "  default_profile: primary\n  work: {{max_input_bytes: 65536, max_result_bytes: 4096, max_calls: 64, max_parallel: {parallel}}}\n"));
            std::fs::write(&fixture.config, &config).unwrap();
            fixture.ingest();
            write_synthesis_pair(&fixture, "held", None, 1500);
            let (_, held) = fixture.run("process");
            assert_eq!(held["halted"], false, "{engine}/{parallel}: {held}");
            let status = execution_json(&fixture, &["queue", "status", "--json"]);
            let tasks = status["data"]["tasks"].as_array().unwrap();
            assert!(
                tasks
                    .iter()
                    .any(|task| task["kind"] == "summarize" && task["status"] == "done"),
                "{status}"
            );
            let selected = tasks
                .iter()
                .find(|task| task["kind"] == "synthesize-concept")
                .unwrap();
            assert_eq!(selected["status"], "current");
            let work_id = held["work_steps"]
                .as_array()
                .unwrap()
                .iter()
                .find(|step| step["role"] == "evidence" && step["outcome"] == "recorded")
                .unwrap()["work_id"]
                .as_str()
                .unwrap();
            let directory = fixture.root().join("vault/.lorekeeper/work").join(work_id);
            let originals = walkdir::WalkDir::new(&directory)
                .into_iter()
                .map(Result::unwrap)
                .filter(|entry| entry.file_type().is_file())
                .map(|entry| {
                    (
                        entry.path().to_owned(),
                        std::fs::read(entry.path()).unwrap(),
                    )
                })
                .collect::<Vec<_>>();
            let actions: serde_json::Value = serde_json::from_slice(
                &std::fs::read(directory.join("inspection-actions.json")).unwrap(),
            )
            .unwrap();
            assert!(!actions["actions"].as_object().unwrap().is_empty());
            let calls = bounded_calls(&fixture).len();
            for step in held["work_steps"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|step| step["role"] == "evidence" && step["agent_duration_ms"].is_number())
            {
                assert_eq!(step["workload"]["kind"], step["node_kind"]);
                if step["node_kind"] == "inspect" {
                    assert!(step["workload"]["characters"].as_u64().unwrap() > 0);
                }
            }
            let (_, repeated) = fixture.run("process");
            assert_eq!(repeated["semantic_calls"], 0, "{repeated}");
            assert_eq!(bounded_calls(&fixture).len(), calls);
            for node in actions["actions"].as_object().unwrap().keys() {
                let args = [
                    "work",
                    "refine",
                    work_id,
                    node.as_str(),
                    "--max-part-characters",
                    "20000",
                ];
                let mut preview = args.to_vec();
                preview.push("--dry-run");
                let sidecar = directory.join("inspection-refinements.json");
                let before = sidecar.exists().then(|| std::fs::read(&sidecar).unwrap());
                let previewed = execution_json(&fixture, &preview);
                assert_eq!(previewed["dry_run"], true);
                assert!(
                    previewed["workload"]["added_observation_nodes"]
                        .as_u64()
                        .unwrap()
                        > 0
                );
                assert_eq!(
                    before,
                    sidecar.exists().then(|| std::fs::read(&sidecar).unwrap())
                );
                assert!(
                    execution_json(&fixture, &args)["inspection_nodes"]
                        .as_array()
                        .unwrap()
                        .len()
                        > 1
                );
                let before = std::fs::read(directory.join("inspection-refinements.json")).unwrap();
                let repeated = execution_json(&fixture, &args);
                assert_eq!(repeated["already_refined"], true);
                assert_eq!(repeated["workload"]["added_observation_nodes"], 0);
                assert!(repeated["workload"]["original_canonical_request_bytes"].is_null());
                assert_eq!(
                    before,
                    std::fs::read(directory.join("inspection-refinements.json")).unwrap()
                );
            }
            std::fs::write(fixture.root().join("refinement-ready"), "").unwrap();
            let (output, resumed) = fixture.run("process");
            assert!(
                output.status.success(),
                "{engine}/{parallel}: {resumed}\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(resumed["semantic_calls"].as_u64().unwrap() <= 64);
            assert_eq!(resumed["execution"]["work"]["max_parallel"], parallel);
            let status = execution_json(&fixture, &["queue", "status", "--json"]);
            assert!(
                status["data"]["tasks"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|task| task["task_id"] == selected["task_id"] && task["status"] == "done"),
                "{status}"
            );
            for (path, bytes) in originals {
                assert_eq!(bytes, std::fs::read(path).unwrap());
            }
            assert_eq!(std::fs::read_to_string(&fixture.config).unwrap(), config);
            assert!(directory.join("publication.json").is_file());
            let sidecar: serde_json::Value = serde_json::from_slice(
                &std::fs::read(directory.join("inspection-refinements.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(sidecar["schema"], "inspection-refinements-v3");
        }
    }
}

#[test]
fn explicit_audit_refinement_resumes_native_workers_on_both_hosts_without_rewriting_paid_state() {
    let behavior = RECORD_BOUNDED_AUDIT.replace(
        "output={'status':'complete','observation':output,'action':None}",
        r#"output={'status':'complete','observation':output,'action':None}
        if request['kind']=='inspect':
            count=sum(len(item['text']) for item in request['input']['ranges'])
            row['characters']=count
            if (root/'refinement-ready').exists():
                assert count<=600, 'refinement must preserve the explicit character bound'
            else:
                try:
                    with (root/'initial-observation').open('x') as flag: flag.write(node_id)
                except FileExistsError:
                    output={'status':'needs-action','observation':None,'action':{'reason':'This assigned inspection requires explicit smaller units.'}}"#,
    ).replace(
        "request=json.loads((root/'native-request.txt').read_text().split('\\nNative request:\\n',1)[1])",
        "delivered=(root/'native-request.txt').read_text()\n    request=json.loads(delivered.split('\\nNative request:\\n',1)[1])\n    if request['kind']=='reduce':\n        assert \"Compose this node's supplied child observations\" in delivered\n        assert 'not a whole-Work assessment' in delivered",
    ).replace("\"$TEST_EXECUTION_ROOT/native-request.txt\"", "\"$TEST_EXECUTION_ROOT/native-request-$$.txt\"")
        .replace("(root/'native-request.txt')", "(root/('native-request-'+str(os.getpid())+'.txt'))");
    assert_ne!(behavior, RECORD_BOUNDED_AUDIT);
    for engine in ["codex", "claude-code"] {
        let fixture = bounded_audit_fixture(engine, 64, &behavior);
        let config = std::fs::read_to_string(&fixture.config)
            .unwrap()
            .replace("max_calls: 64}", "max_calls: 64, max_parallel: 2}");
        std::fs::write(&fixture.config, &config).unwrap();
        let (_, held) = fixture.run("knowledge");
        assert_eq!(held["halted"], false, "{engine}: {held}");
        assert!(held["audit"].as_array().unwrap().is_empty(), "{held}");
        let work_id = held["work_steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|step| step["outcome"] == "recorded")
            .unwrap()["work_id"]
            .as_str()
            .unwrap();
        let directory = fixture.root().join("vault/.lorekeeper/work").join(work_id);
        let originals = walkdir::WalkDir::new(&directory)
            .into_iter()
            .map(Result::unwrap)
            .filter(|entry| entry.file_type().is_file())
            .map(|entry| {
                (
                    entry.path().to_owned(),
                    std::fs::read(entry.path()).unwrap(),
                )
            })
            .collect::<Vec<_>>();
        assert!(
            originals
                .iter()
                .any(|(path, _)| path.parent() == Some(directory.join("nodes").as_path()))
        );
        let receipt: serde_json::Value = serde_json::from_slice(
            &std::fs::read(directory.join("inspection-actions.json")).unwrap(),
        )
        .unwrap();
        let actions = receipt["actions"].as_object().unwrap();
        assert!(!actions.is_empty());
        let calls = bounded_calls(&fixture).len();
        let (_, unchanged) = fixture.run("knowledge");
        assert_eq!(unchanged["semantic_calls"], 0, "{unchanged}");
        assert_eq!(bounded_calls(&fixture).len(), calls);
        for node in actions.keys() {
            let arguments = [
                "work",
                "refine",
                work_id,
                node.as_str(),
                "--max-part-characters",
                "600",
            ];
            let mut preview_arguments = arguments.to_vec();
            preview_arguments.push("--dry-run");
            let preview = execution_json(&fixture, &preview_arguments);
            assert_eq!(preview["dry_run"], true);
            assert_eq!(bounded_calls(&fixture).len(), calls);
            for (path, raw) in &originals {
                assert_eq!(&std::fs::read(path).unwrap(), raw);
            }
            let refined = execution_json(&fixture, &arguments);
            assert_eq!(refined["dry_run"], false);
            assert!(refined["inspection_nodes"].as_array().unwrap().len() > 1);
            let before = std::fs::read(directory.join("inspection-refinements.json")).unwrap();
            let repeated = execution_json(&fixture, &arguments);
            assert_eq!(repeated["already_refined"], true);
            assert_eq!(
                before,
                std::fs::read(directory.join("inspection-refinements.json")).unwrap()
            );
        }
        std::fs::write(fixture.root().join("refinement-ready"), "").unwrap();
        let (output, resumed) = fixture.run("knowledge");
        assert!(
            output.status.success(),
            "{engine}: {resumed}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(resumed["halted"], false);
        assert!(resumed["semantic_calls"].as_u64().unwrap() <= 64);
        assert_eq!(resumed["execution"]["work"]["max_parallel"], 2);
        assert_eq!(
            resumed["audit"][0]["assessed_path"],
            "wiki/concepts/large.md"
        );
        for (path, raw) in &originals {
            assert_eq!(&std::fs::read(path).unwrap(), raw);
        }
        assert_eq!(std::fs::read_to_string(&fixture.config).unwrap(), config);
        assert!(directory.join("seal.json").is_file());
        let calls = bounded_calls(&fixture);
        assert!(calls.iter().any(|call| call["kind"] == "reduce"));
        assert_eq!(
            calls
                .iter()
                .filter(|call| call["action"] == "finish")
                .count(),
            1
        );
        assert!(
            std::fs::read_dir(directory.join("nodes"))
                .unwrap()
                .any(|entry| std::fs::read_to_string(entry.unwrap().path())
                    .unwrap()
                    .contains("work-output-refinement-v1"))
        );
    }
}

#[test]
fn typed_native_refinement_is_bounded_and_resumes_both_hosts_without_replaying_holds() {
    let behavior = build_extraction_behavior("")
        .replace("'value':{'concepts':[]}", "'value':{'synthesis':'The source retains its declared conditions and attribution.','related':''}")
        .replace("output={'status':'complete','observation':output,'action':None}", r#"output={'status':'complete','observation':output,'action':None}
        if request['kind']=='inspect':
            count=sum(len(item['text']) for item in request['input']['ranges'])
            assert count<=80000
            if request['inspection']['can_refine']:
                try:
                    with (root/'native-refinement-requested').open('x') as flag: flag.write(node_id)
                    output={'status':'needs-smaller-inspection','observation':None,'action':{'reason':'A smaller complete assignment is required for this inspection.'}}
                except FileExistsError: pass
            elif (root/'hold-children').exists():
                output={'status':'needs-action','observation':None,'action':{'reason':'Smaller reading units are not evidence that this processing blocker is resolved.'}}"#)
        .replace("    row['output_bytes']=", r#"    row['input_revision']=request['input_revision']
    row['response_status']=output['status']
    raw=output if '--safe-mode' not in sys.argv else {'type':'result','subtype':'success','is_error':False,'structured_output':output}
    if output['status']=='needs-smaller-inspection':
        (root/('captured-'+node_id+'.json')).write_text(json.dumps(raw)+'\n')
    row['output_bytes']="#)
        .replace("\"$TEST_EXECUTION_ROOT/native-request.txt\"", "\"$TEST_EXECUTION_ROOT/native-request-$$.txt\"")
        .replace("(root/'native-request.txt')", "(root/('native-request-'+str(os.getpid())+'.txt'))");
    let section = WHOLE_SECTION
        .replace("/bin/cat > \"$TEST_EXECUTION_ROOT/whole-section-request.txt\"",
            "export TEST_REFINEMENT_SECTION_INPUT=\"$TEST_EXECUTION_ROOT/whole-section-request-$$.txt\"\n    /bin/cat > \"$TEST_REFINEMENT_SECTION_INPUT\"")
        .replace("(root/'whole-section-request.txt')", "Path(os.environ['TEST_REFINEMENT_SECTION_INPUT'])")
        .replace("exec < \"$TEST_EXECUTION_ROOT/whole-section-request.txt\"", "exec < \"$TEST_REFINEMENT_SECTION_INPUT\"");
    for engine in ["codex", "claude-code"] {
        for parallel in [1, 6] {
            let fixture = Fixture::new(engine, 30, &format!("{section}\n{behavior}"));
            let config = std::fs::read_to_string(&fixture.config).unwrap()
                .replace("concept_policy: supported-new", "concept_policy: disabled")
                .replace("  default_profile: primary\n", &format!(
                    "  default_profile: primary\n  work: {{max_input_bytes: 65536, max_result_bytes: 4096, max_calls: 64, max_parallel: {parallel}}}\n"));
            std::fs::write(&fixture.config, &config).unwrap();
            if parallel == 6 {
                std::fs::write(fixture.root().join("hold-children"), "").unwrap();
            }
            fixture.ingest();
            write_synthesis_pair(&fixture, "typed-refinement", None, 1500);
            let (_, result) = fixture.run("process");
            assert_eq!(result["halted"], false, "{engine}/{parallel}: {result}");
            assert_eq!(result["cancelled"], false);
            assert!(result["semantic_calls"].as_u64().unwrap() <= 64);
            assert_eq!(result["execution"]["work"]["max_parallel"], parallel);
            let steps = result["work_steps"].as_array().unwrap();
            let refinements = steps
                .iter()
                .filter(|step| step["outcome"] == "refined")
                .collect::<Vec<_>>();
            assert_eq!(refinements.len(), 1, "{engine}/{parallel}: {result}");
            let step = refinements[0];
            let work_id = step["work_id"].as_str().unwrap();
            let node_id = step["node_id"].as_str().unwrap();
            let directory = fixture.root().join("vault/.lorekeeper/work").join(work_id);
            let sidecar: serde_json::Value = serde_json::from_slice(
                &std::fs::read(directory.join("inspection-refinements.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(sidecar["schema"], "inspection-refinements-v3");
            assert_eq!(sidecar["entries"].as_object().unwrap().len(), 1);
            let action = sidecar["actions"]
                .as_object()
                .unwrap()
                .values()
                .find(|entry| entry["node_id"] == node_id)
                .unwrap();
            let calls = bounded_calls(&fixture);
            let call = calls
                .iter()
                .find(|call| {
                    call["node_id"] == node_id
                        && call["response_status"] == "needs-smaller-inspection"
                })
                .unwrap();
            assert_eq!(action["action"]["input_revision"], call["input_revision"]);
            assert_eq!(
                action["action"]["refinement_request"]["response_json_bytes"],
                step["result_json_bytes"]
            );
            let response =
                std::fs::read(fixture.root().join(format!("captured-{node_id}.json"))).unwrap();
            assert_eq!(
                action["action"]["refinement_request"]["response_revision"],
                lk_core::rss_original::digest(&response)
            );
            let status = execution_json(&fixture, &["queue", "status", "--json"]);
            let tasks = status["data"]["tasks"].as_array().unwrap();
            assert!(
                tasks
                    .iter()
                    .any(|task| task["kind"] == "summarize" && task["status"] == "done"),
                "{status}"
            );
            assert!(
                tasks.iter().any(|task| task["kind"] == "synthesize-concept"
                    && task["status"] == if parallel == 1 { "done" } else { "current" }),
                "{status}"
            );
            if parallel == 6 {
                let originals = walkdir::WalkDir::new(&directory)
                    .into_iter()
                    .map(Result::unwrap)
                    .filter(|entry| entry.file_type().is_file())
                    .map(|entry| {
                        (
                            entry.path().to_owned(),
                            std::fs::read(entry.path()).unwrap(),
                        )
                    })
                    .collect::<Vec<_>>();
                let (_, repeated) = fixture.run("process");
                assert_eq!(repeated["semantic_calls"], 0, "{repeated}");
                assert_eq!(repeated["halted"], false);
                assert!(!repeated["complete"].as_bool().unwrap_or(false));
                assert_eq!(bounded_calls(&fixture).len(), calls.len());
                for (path, bytes) in originals {
                    assert_eq!(bytes, std::fs::read(path).unwrap());
                }
            }
            assert_eq!(std::fs::read_to_string(&fixture.config).unwrap(), config);
        }
    }
}

#[test]
fn typed_native_audit_refinement_completes_under_both_hosts_shared_budget() {
    let behavior = RECORD_BOUNDED_AUDIT.replace(
        "output={'status':'complete','observation':output,'action':None}",
        r#"output={'status':'complete','observation':output,'action':None}
        if request['kind']=='inspect' and request['inspection']['can_refine']:
            try:
                with (root/'native-refinement-requested').open('x') as flag: flag.write(node_id)
                output={'status':'needs-smaller-inspection','observation':None,'action':{'reason':'A smaller complete assignment is required for this inspection.'}}
            except FileExistsError: pass"#);
    for engine in ["codex", "claude-code"] {
        let fixture = bounded_audit_fixture(engine, 64, &behavior);
        let (output, result) = fixture.run("knowledge");
        assert!(
            output.status.success(),
            "{engine}: {result}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(result["halted"], false);
        assert!(result["semantic_calls"].as_u64().unwrap() <= 64);
        let steps = result["work_steps"].as_array().unwrap();
        assert_eq!(
            steps
                .iter()
                .filter(|step| step["outcome"] == "refined")
                .count(),
            1
        );
        assert_eq!(result["audit"].as_array().unwrap().len(), 1);
        let work_id = steps
            .iter()
            .find(|step| step["outcome"] == "refined")
            .unwrap()["work_id"]
            .as_str()
            .unwrap();
        let status = execution_json(&fixture, &["work", "status", work_id]);
        assert_eq!(status["complete"], true);
        assert!(
            fixture
                .root()
                .join("vault/.lorekeeper/work")
                .join(work_id)
                .join("inspection-actions.json")
                .is_file()
        );
    }
}
