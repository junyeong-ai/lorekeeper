use super::*;

#[test]
fn standalone_screening_rejects_ambiguous_input_and_stored_state_without_mutation() {
    let (fixture, task) = progressive_extraction_fixture("codex", "exit 97", 1, 65536, 16384);
    let root = fixture.root().join("vault");
    let raw = std::fs::read_to_string(root.join(&task.target.vault_path)).unwrap();
    let discovery = fixture.root().join("discovery.json");
    std::fs::write(
        &discovery,
        serde_json::to_vec(&serde_json::json!({
            "target_revision":lk_core::concept::file_revision(&raw),
            "queries":[{"query":"transactional outbox","phrase":true,"spacing":false}]
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
    let turn = execution_json(&fixture, &["work", "select", "--task-id", &task.task_id]);
    let output = screening_decisions(&turn["request"], true);
    let encoded = serde_json::to_string(&output).unwrap();
    let response = fixture.root().join("response.json");
    let store = root.join(".lorekeeper/queue/screening");
    for (field, duplicate) in [
        ("\"task_id\":", "\"task_id\":\"other\",\"task_id\":"),
        (
            "\"decision\":\"defer\"",
            "\"decision\":\"not-selected\",\"decision\":\"defer\"",
        ),
    ] {
        let ambiguous = encoded.replacen(field, duplicate, 1);
        assert_ne!(ambiguous, encoded);
        std::fs::write(&response, &ambiguous).unwrap();
        let rejected = fixture
            .command()
            .args([
                "work",
                "select-record",
                "--task-id",
                &task.task_id,
                "--file",
                response.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(!rejected.status.success());
        assert!(String::from_utf8_lossy(&rejected.stderr).contains("duplicate field"));
        assert!(!store.exists(), "a refused reply records nothing");
        assert_eq!(std::fs::read_to_string(&response).unwrap(), ambiguous);
        assert!(!root.join(".lorekeeper/mutation/pending.json").exists());
    }
    std::fs::write(&response, serde_json::to_string_pretty(&output).unwrap()).unwrap();
    execution_json(
        &fixture,
        &[
            "work",
            "select-record",
            "--task-id",
            &task.task_id,
            "--file",
            response.to_str().unwrap(),
        ],
    );
    let recorded = execution_json(&fixture, &["work", "select", "--task-id", &task.task_id]);
    assert_eq!(recorded["progress"]["deferred"], 1);
    assert_eq!(recorded["progress"]["complete"], false);
    let ledger = store
        .join(recorded["progress"]["screening_id"].as_str().unwrap())
        .join("ledger.json");
    let raw = std::fs::read_to_string(&ledger).unwrap();
    let ambiguous = raw.replacen("\"row\":0", "\"row\":1,\"row\":0", 1);
    assert_ne!(ambiguous, raw);
    std::fs::write(&ledger, &ambiguous).unwrap();
    let rejected = fixture
        .command()
        .args(["work", "select", "--task-id", &task.task_id])
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("duplicate field"));
    assert_eq!(std::fs::read_to_string(&ledger).unwrap(), ambiguous);
    std::fs::write(ledger, raw).unwrap();
    assert_eq!(
        execution_json(&fixture, &["work", "select", "--task-id", &task.task_id])["progress"]["deferred"],
        1
    );
    assert!(
        !fixture.root().join("arguments").exists(),
        "no host invocation is needed"
    );
}

#[test]
fn recorded_decisions_survive_unread_edits_and_only_a_changed_card_is_screened_again() {
    for engine in ["codex", "claude-code"] {
        let screening = RECORD_PROGRESSIVE_EXTRACTION.replace(
            "'decision':'inspect' if candidate['path'].endswith('/candidate-000.md') else 'not-selected',",
            "'decision':'defer',",
        );
        assert_ne!(screening, RECORD_PROGRESSIVE_EXTRACTION);
        let (fixture, task) = progressive_extraction_fixture(
            engine,
            &format!(
                "{}\n{screening}",
                WHOLE_SECTION.replace("request['kind']", "request.get('kind')")
            ),
            4,
            65536,
            16384,
        );
        let config = std::fs::read_to_string(&fixture.config)
            .unwrap()
            .replace("max_calls: 64", "max_calls: 1");
        std::fs::write(&fixture.config, config).unwrap();
        let root = fixture.root().join("vault");
        let raw = std::fs::read_to_string(root.join(&task.target.vault_path)).unwrap();
        let discovery = fixture.root().join("discovery.json");
        std::fs::write(
            &discovery,
            serde_json::to_vec(&serde_json::json!({
                "target_revision":lk_core::concept::file_revision(&raw),
                "queries":[{"query":"transactional outbox","phrase":true,"spacing":false}]
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
        let select = || execution_json(&fixture, &["work", "select", "--task-id", &task.task_id]);
        let (_, first) = fixture.run("process");
        assert_eq!(first["semantic_calls"], 1, "{engine}: {first}");
        assert_eq!(first["halted"], false, "{engine}: {first}");
        assert_eq!(first["queue_application"]["applied"], 0);
        let original = select();
        assert_eq!(original["status"], "deferred");
        assert_eq!(original["progress"]["deferred"], 4);
        let store = root
            .join(".lorekeeper/queue/screening")
            .join(original["progress"]["screening_id"].as_str().unwrap());
        let stored = || {
            walkdir::WalkDir::new(&store)
                .into_iter()
                .map(Result::unwrap)
                .filter(|entry| entry.file_type().is_file())
                .map(|entry| {
                    (
                        entry.path().to_owned(),
                        std::fs::read(entry.path()).unwrap(),
                    )
                })
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let paid = stored();
        assert_eq!(paid.len(), 2, "one packet and the ledger");
        let candidate = root.join("wiki/documents/candidate-003.md");
        let before = std::fs::read_to_string(&candidate).unwrap();
        std::fs::write(
            &candidate,
            format!("{before}\nUnrelated trailing observation.\n"),
        )
        .unwrap();

        let queue = root.join(".lorekeeper/queue");
        let client = lk_queue::QueueLlmClient::new(queue.clone());
        let request: lk_queue::SummarizeRequest = serde_json::from_value(serde_json::json!({
            "text":"Independent source.","max_sentences":2,"locale":"en",
            "target":{"vault_path":"wiki/documents/zz-independent.md",
                "anchor":"## Summary","kind":"document-summary"},
        }))
        .unwrap();
        let summary = client.build_task("summary", lk_queue::TaskKind::Summarize, request);
        std::fs::write(root.join(&summary.target.vault_path), format!(
            "---\ntype: document\ntitle: Independent\nllm_inputs:\n  summary: {}\n---\n## Summary\n", summary.cache_hash
        )).unwrap();
        lk_queue::write_tasks_atomic(&queue.join("independent.jsonl"), &[summary]).unwrap();
        let (_, next) = fixture.run("process");
        assert_eq!(next["halted"], false, "{engine}: {next}");
        assert_eq!(next["semantic_calls"], 1, "{engine}: {next}");
        assert_eq!(next["queue_application"]["applied"], 1, "{engine}: {next}");
        assert!(
            next["work_steps"]
                .as_array()
                .unwrap()
                .iter()
                .all(|step| step["role"] != "screening"),
            "an edit the card does not show spends no screening call: {next}"
        );
        let current = select();
        assert_eq!(current["status"], "deferred");
        assert_eq!(current["progress"]["deferred"], 4);
        assert!(current["request"].is_null());
        assert_eq!(stored(), paid);
        let (_, repeated) = fixture.run("process");
        assert_eq!(repeated["halted"], false, "{engine}: {repeated}");
        assert_eq!(repeated["semantic_calls"], 0, "{engine}: {repeated}");
        assert_eq!(stored(), paid);

        std::fs::write(
            &candidate,
            before.replace("title: Candidate 003", "title: Candidate 003 revised"),
        )
        .unwrap();
        let changed = select();
        assert_eq!(changed["status"], "screening");
        assert_eq!(changed["progress"]["unknown"], 1);
        assert_eq!(changed["progress"]["deferred"], 3);
        let (_, rescreened) = fixture.run("process");
        assert_eq!(rescreened["semantic_calls"], 1, "{engine}: {rescreened}");
        let calls = std::fs::read_to_string(fixture.root().join("progressive-calls.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|call| call["action"] == "screen")
            .collect::<Vec<_>>();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["paths"].as_array().unwrap().len(), 4);
        assert_eq!(
            calls[1]["paths"],
            serde_json::json!(["wiki/documents/candidate-003.md"])
        );
        let after = stored();
        assert_eq!(after.len(), 3, "the changed card's packet joins the ledger");
        for (path, bytes) in &paid {
            if path.file_name().unwrap() != "ledger.json" {
                assert_eq!(after.get(path), Some(bytes), "paid packets are immutable");
            }
        }
        assert_eq!(select()["progress"]["deferred"], 4);

        let ledger = store.join("ledger.json");
        std::fs::write(&ledger, b"{").unwrap();
        let (_, corrupt) = fixture.run("process");
        assert_eq!(corrupt["halted"], true, "{engine}: {corrupt}");
        assert_eq!(corrupt["semantic_calls"], 0, "{engine}: {corrupt}");
        assert_eq!(std::fs::read(&ledger).unwrap(), b"{");
    }
}
