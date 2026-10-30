use super::*;

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
