use super::*;

#[test]
fn exact_packet_reuse_preserves_paid_bytes_and_defers_only_its_target_on_both_hosts() {
    for engine in ["codex", "claude-code"] {
        let screening = RECORD_PROGRESSIVE_EXTRACTION.replace(
            "'decision':'inspect' if candidate['candidate_key'].endswith('/candidate-000.md') else 'not-selected',",
            "'decision':'defer',",
        );
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
        let (_, first) = fixture.run("process");
        assert_eq!(first["semantic_calls"], 1, "{engine}: {first}");
        assert_eq!(first["halted"], false, "{engine}: {first}");
        assert_eq!(first["queue_application"]["applied"], 0);
        let original = execution_json(&fixture, &["work", "select", "--task-id", &task.task_id]);
        assert_eq!(original["status"], "deferred");
        let receipt_path = original["progress"]["receipts"][0]["path"]
            .as_str()
            .unwrap();
        let original_receipt = std::fs::read(root.join(receipt_path)).unwrap();
        let receipt: serde_json::Value = serde_json::from_slice(&original_receipt).unwrap();
        assert_eq!(receipt["schema"], "screening-computed-output-v1");
        assert!(receipt["reused_from"].is_null());
        let candidate = root.join("wiki/documents/candidate-003.md");
        let before = std::fs::read_to_string(&candidate).unwrap();
        std::fs::write(
            candidate,
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
        assert_eq!(
            next["queue_planning"]["reused_screening_packets"], 1,
            "{engine}: {next}"
        );
        assert!(
            next["work_steps"]
                .as_array()
                .unwrap()
                .iter()
                .all(|step| step["role"] != "screening")
        );
        let current = execution_json(&fixture, &["work", "select", "--task-id", &task.task_id]);
        assert_eq!(current["status"], "deferred");
        assert_ne!(
            current["progress"]["selection_id"],
            original["progress"]["selection_id"]
        );
        assert_eq!(current["progress"]["deferred"], 4);
        assert!(current["request"].is_null());
        assert_eq!(
            std::fs::read(root.join(receipt_path)).unwrap(),
            original_receipt
        );
        let (_, repeated) = fixture.run("process");
        assert_eq!(repeated["halted"], false, "{engine}: {repeated}");
        assert_eq!(repeated["semantic_calls"], 0, "{engine}: {repeated}");
        assert_eq!(repeated["queue_planning"]["reused_screening_packets"], 0);
        assert_eq!(
            std::fs::read(root.join(receipt_path)).unwrap(),
            original_receipt
        );
        std::fs::write(root.join(receipt_path), b"{").unwrap();
        let (_, corrupt) = fixture.run("process");
        assert_eq!(corrupt["halted"], true, "{engine}: {corrupt}");
        assert_eq!(corrupt["semantic_calls"], 0, "{engine}: {corrupt}");
        assert_eq!(std::fs::read(root.join(receipt_path)).unwrap(), b"{");
    }
}
