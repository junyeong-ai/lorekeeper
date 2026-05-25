use std::sync::Arc;

use tempfile::TempDir;

use lk_core::concept::ExtractedConcept;
use lk_core::config::{
    Config, DedupConfig, Identity, PerformanceConfig, SourceConfig, SourceType, SynthesisConfig,
    VaultConfig, VaultDirs,
};
use lk_core::event::RawItem;
use lk_llm::{LlmClient, MockLlmClient, NoopLlmClient};
use lk_pipeline::{IngestOptions, Pipeline, PipelineContext, Synthesizer};

fn base_config(vault_root: &std::path::Path) -> Config {
    let mut sources = std::collections::BTreeMap::new();
    sources.insert(
        "test-source".to_string(),
        SourceConfig {
            source_type: SourceType::Gmail,
            enabled: true,
            schedule: None,
            params: serde_json::Value::Object(Default::default()),
            classify: Default::default(),
            classify_with_llm: false,
            labels: vec!["test".into()],
            extract_concepts: true,
            focus: None,
            track_personal: false,
        },
    );

    Config {
        vault: VaultConfig {
            root: vault_root.to_string_lossy().into(),
            dirs: VaultDirs::default(),
            timezone: Some("UTC".into()),
            locale: None,
        },
        identity: Identity {
            name: "Test User".into(),
            email: "test@example.com".into(),
            slack_id: None,
            jira_id: None,
        },
        sources,
        dedup: DedupConfig::default(),
        performance: PerformanceConfig::default(),
        synthesis: SynthesisConfig::default(),
        llm: Default::default(),
        graph: Default::default(),
    }
}

fn raw_item(title: &str, body: &str, external_id: &str, when: jiff::Timestamp) -> RawItem {
    RawItem {
        external_id: Some(external_id.into()),
        title: title.into(),
        body: body.into(),
        url: Some(format!("https://example.com/{external_id}")),
        author: Some("alice@example.com".into()),
        timestamp: when,
        metadata: serde_json::Value::Null,
    }
}

fn make_ctx(config: &Config, llm: Arc<dyn LlmClient>) -> Arc<PipelineContext> {
    Arc::new(PipelineContext::new(None, llm, config).unwrap())
}

#[tokio::test]
async fn concept_pages_written_with_merge() {
    let dir = TempDir::new().unwrap();
    let vault = dir.path();
    let config = base_config(vault);

    let concepts = vec![ExtractedConcept {
        name: "Claude Code".into(),
        slug: "claude-code".into(),
    }];

    let llm: Arc<dyn LlmClient> = Arc::new(MockLlmClient::with_concepts(concepts));
    let ctx = make_ctx(&config, llm);
    let pipeline = Pipeline::new(vault, ctx, &config).unwrap();

    let ts: jiff::Timestamp = "2026-05-23T10:00:00Z".parse().unwrap();
    let items = vec![raw_item("Anthropic releases new model", "...", "MSG-1", ts)];

    let options = IngestOptions {
        dry_run: false,
        force: false,
        target_date: None,
    };

    let result = pipeline
        .plan(
            "test-source",
            config.sources.get("test-source").unwrap(),
            items,
            &options,
        )
        .await
        .unwrap();

    assert_eq!(result.concepts.len(), 1);
    let concept_pages = pipeline.render_concept_pages().await.unwrap();
    let concept_output = concept_pages
        .iter()
        .find(|o| o.path.to_string().contains("wiki/concepts/claude-code"))
        .expect("concept page must be rendered");

    let writer = lk_vault::VaultWriter::new(vault);
    for out in result.daily_pages.iter().chain(concept_pages.iter()) {
        writer
            .write_page(out.path.as_ref(), &out.content)
            .await
            .unwrap();
    }

    // Re-ingest on a different date with a FRESH pipeline, so the merge reads the
    // on-disk concept page (exercising the created/updated round-trip). Drop the first
    // pipeline so its single-writer dedup lock is released.
    drop(pipeline);
    let llm2: Arc<dyn LlmClient> = Arc::new(MockLlmClient::with_concepts(vec![ExtractedConcept {
        name: "Claude Code".into(),
        slug: "claude-code".into(),
    }]));
    let pipeline2 = Pipeline::new(vault, make_ctx(&config, llm2), &config).unwrap();
    let ts2: jiff::Timestamp = "2026-05-24T10:00:00Z".parse().unwrap();
    let items2 = vec![raw_item("Anthropic releases v2", "...", "MSG-2", ts2)];
    pipeline2
        .plan(
            "test-source",
            config.sources.get("test-source").unwrap(),
            items2,
            &IngestOptions {
                dry_run: false,
                force: false,
                target_date: None,
            },
        )
        .await
        .unwrap();
    let concept_pages2 = pipeline2.render_concept_pages().await.unwrap();
    let concept_output2 = concept_pages2
        .iter()
        .find(|o| o.path.to_string().contains("claude-code"))
        .unwrap();

    assert!(
        concept_output2.content.contains("source_count: 2"),
        "expected merged count 2, content was:\n{}",
        concept_output2.content
    );
    assert!(
        concept_output2.content.contains("created: 2026-05-23"),
        "first_seen (created) must survive re-ingest, not reset to today:\n{}",
        concept_output2.content
    );
    assert!(
        concept_output2.content.contains("updated: 2026-05-24"),
        "last_seen (updated) must advance to the new date"
    );
    assert!(
        concept_output2
            .content
            .contains(r#"aliases: ["Claude Code"]"#),
        "concept page must carry an alias so [[Claude Code]] resolves:\n{}",
        concept_output2.content
    );
    let _ = concept_output;
}

#[tokio::test]
async fn timezone_affects_vault_date() {
    let dir = TempDir::new().unwrap();
    let vault = dir.path();
    let mut config = base_config(vault);
    config.vault.timezone = Some("Asia/Seoul".into());

    let llm: Arc<dyn LlmClient> = Arc::new(NoopLlmClient);
    let ctx = make_ctx(&config, llm);
    let pipeline = Pipeline::new(vault, ctx, &config).unwrap();

    // 2026-05-22 23:00 UTC == 2026-05-23 08:00 KST
    let ts: jiff::Timestamp = "2026-05-22T23:00:00Z".parse().unwrap();
    let items = vec![raw_item("KST morning email", "body", "M1", ts)];

    let result = pipeline
        .plan(
            "test-source",
            config.sources.get("test-source").unwrap(),
            items,
            &IngestOptions {
                dry_run: false,
                force: false,
                target_date: None,
            },
        )
        .await
        .unwrap();

    let daily_path = result
        .daily_pages
        .iter()
        .find(|o| o.path.to_string().contains("daily/test-source"))
        .unwrap()
        .path
        .to_string();

    assert!(
        daily_path.contains("2026-05-23"),
        "KST date should be 2026-05-23, got: {daily_path}"
    );
}

#[tokio::test]
async fn target_date_filters_events() {
    let dir = TempDir::new().unwrap();
    let vault = dir.path();
    let config = base_config(vault);

    let llm: Arc<dyn LlmClient> = Arc::new(NoopLlmClient);
    let ctx = make_ctx(&config, llm);
    let pipeline = Pipeline::new(vault, ctx, &config).unwrap();

    let day1: jiff::Timestamp = "2026-05-23T10:00:00Z".parse().unwrap();
    let day2: jiff::Timestamp = "2026-05-24T10:00:00Z".parse().unwrap();
    let items = vec![
        raw_item("Day 1 event", "", "D1", day1),
        raw_item("Day 2 event", "", "D2", day2),
    ];

    let result = pipeline
        .plan(
            "test-source",
            config.sources.get("test-source").unwrap(),
            items,
            &IngestOptions {
                dry_run: false,
                force: false,
                target_date: Some(jiff::civil::date(2026, 5, 23)),
            },
        )
        .await
        .unwrap();

    assert_eq!(result.events.len(), 1);
    assert_eq!(result.events[0].date, jiff::civil::date(2026, 5, 23));
}

#[tokio::test]
async fn multi_date_events_produce_multiple_daily_pages() {
    let dir = TempDir::new().unwrap();
    let vault = dir.path();
    let config = base_config(vault);

    let llm: Arc<dyn LlmClient> = Arc::new(NoopLlmClient);
    let ctx = make_ctx(&config, llm);
    let pipeline = Pipeline::new(vault, ctx, &config).unwrap();

    let day1: jiff::Timestamp = "2026-05-23T10:00:00Z".parse().unwrap();
    let day2: jiff::Timestamp = "2026-05-24T10:00:00Z".parse().unwrap();
    let items = vec![
        raw_item("Day 1 event", "", "D1", day1),
        raw_item("Day 2 event", "", "D2", day2),
    ];

    let result = pipeline
        .plan(
            "test-source",
            config.sources.get("test-source").unwrap(),
            items,
            &IngestOptions {
                dry_run: false,
                force: false,
                target_date: None,
            },
        )
        .await
        .unwrap();

    let daily_count = result.daily_pages.len();
    assert_eq!(daily_count, 2, "expected one daily page per date");
}

#[tokio::test]
async fn concept_accumulates_across_sources_in_one_run() {
    // Two different sources mention the same concept in a single run. The concept page
    // must merge into ONE page with source_count 2 and both source refs — not be
    // overwritten by whichever source is written last.
    let dir = TempDir::new().unwrap();
    let vault = dir.path();
    let mut config = base_config(vault);
    config.sources.insert(
        "second-source".to_string(),
        SourceConfig {
            source_type: SourceType::Gmail,
            enabled: true,
            schedule: None,
            params: serde_json::Value::Object(Default::default()),
            classify: Default::default(),
            classify_with_llm: false,
            labels: vec![],
            extract_concepts: true,
            focus: None,
            track_personal: false,
        },
    );

    let llm: Arc<dyn LlmClient> = Arc::new(MockLlmClient::with_concepts(vec![ExtractedConcept {
        name: "Shared Concept".into(),
        slug: "shared-concept".into(),
    }]));
    let pipeline = Pipeline::new(vault, make_ctx(&config, llm), &config).unwrap();

    let ts: jiff::Timestamp = "2026-05-23T10:00:00Z".parse().unwrap();
    let opts = IngestOptions {
        dry_run: false,
        force: false,
        target_date: None,
    };
    for sid in ["test-source", "second-source"] {
        pipeline
            .plan(
                sid,
                config.sources.get(sid).unwrap(),
                vec![raw_item("Anthropic ships", "...", &format!("{sid}-1"), ts)],
                &opts,
            )
            .await
            .unwrap();
    }

    let pages = pipeline.render_concept_pages().await.unwrap();
    let shared = pages
        .iter()
        .find(|o| o.path.to_string().contains("shared-concept"))
        .expect("one merged concept page");
    assert_eq!(
        pages
            .iter()
            .filter(|o| o.path.to_string().contains("shared-concept"))
            .count(),
        1,
        "concept must be a single merged page, not one per source"
    );
    assert!(
        shared.content.contains("source_count: 2"),
        "both sources must accumulate into the count:\n{}",
        shared.content
    );
    assert!(shared.content.contains("daily/test-source/2026-05-23"));
    assert!(shared.content.contains("daily/second-source/2026-05-23"));
}

#[tokio::test]
async fn llm_failure_does_not_break_pipeline() {
    let dir = TempDir::new().unwrap();
    let vault = dir.path();
    let config = base_config(vault);

    let llm: Arc<dyn LlmClient> = Arc::new(MockLlmClient::failing());
    let ctx = make_ctx(&config, llm);
    let pipeline = Pipeline::new(vault, ctx, &config).unwrap();

    let ts: jiff::Timestamp = "2026-05-23T10:00:00Z".parse().unwrap();
    let items = vec![raw_item("Subject", "Body", "M1", ts)];

    let result = pipeline
        .plan(
            "test-source",
            config.sources.get("test-source").unwrap(),
            items,
            &IngestOptions {
                dry_run: false,
                force: false,
                target_date: None,
            },
        )
        .await
        .unwrap();

    assert_eq!(result.events.len(), 1);
    assert!(result.concepts.is_empty());
    assert_eq!(
        result.daily_pages.len(),
        1,
        "daily page should still be produced"
    );
    assert!(
        pipeline.render_concept_pages().await.unwrap().is_empty(),
        "a failing LLM yields no concept pages"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn write_failure_keeps_events_novel_for_retry() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().unwrap();
    let vault = dir.path();
    let config = base_config(vault);

    let llm: Arc<dyn LlmClient> = Arc::new(NoopLlmClient);
    let ctx = make_ctx(&config, llm);
    let pipeline = Pipeline::new(vault, ctx, &config).unwrap();
    let sc = config.sources.get("test-source").unwrap().clone();

    let ts: jiff::Timestamp = "2026-05-23T10:00:00Z".parse().unwrap();
    let make_items = || vec![raw_item("Subject", "Body", "M1", ts)];

    let opts = IngestOptions {
        dry_run: false,
        force: false,
        target_date: None,
    };

    // Phase 1: Plan succeeds
    let result = pipeline
        .plan("test-source", &sc, make_items(), &opts)
        .await
        .unwrap();
    assert!(!result.daily_pages.is_empty());

    // Phase 2: Make the daily directory read-only to force a write failure
    let daily_dir = vault.join("daily").join("test-source");
    tokio::fs::create_dir_all(&daily_dir).await.unwrap();
    let original_perms = tokio::fs::metadata(&daily_dir).await.unwrap().permissions();
    let mut readonly = original_perms.clone();
    readonly.set_mode(0o555);
    tokio::fs::set_permissions(&daily_dir, readonly)
        .await
        .unwrap();

    let writer = lk_vault::VaultWriter::new(vault);
    let write_outcome = writer
        .write_page(
            result.daily_pages[0].path.as_ref(),
            &result.daily_pages[0].content,
        )
        .await;
    assert!(
        write_outcome.is_err(),
        "write should fail in read-only directory"
    );

    // Restore permissions so the test can clean up
    tokio::fs::set_permissions(&daily_dir, original_perms)
        .await
        .unwrap();

    // CLI contract: on write failure, dedup is NOT committed.
    // (pipeline.commit() is intentionally NOT called)

    // Re-plan with same input — event must still appear as novel because dedup never recorded it.
    let result2 = pipeline
        .plan("test-source", &sc, make_items(), &opts)
        .await
        .unwrap();
    assert_eq!(
        result2.events.len(),
        1,
        "event must remain novel: write failed → commit skipped → no dedup entry"
    );
}

#[tokio::test]
async fn dry_run_pipeline_creates_no_dedup_file() {
    let dir = TempDir::new().unwrap();
    let vault = dir.path();
    let config = base_config(vault);
    let llm: Arc<dyn LlmClient> = Arc::new(NoopLlmClient);
    let ctx = make_ctx(&config, llm);
    let pipeline = Pipeline::new_dry_run(vault, ctx, &config).unwrap();
    let sc = config.sources.get("test-source").unwrap().clone();

    let ts: jiff::Timestamp = "2026-05-23T10:00:00Z".parse().unwrap();
    let opts = IngestOptions {
        dry_run: true,
        force: false,
        target_date: None,
    };
    let r = pipeline
        .plan(
            "test-source",
            &sc,
            vec![raw_item("S", "B", "M1", ts)],
            &opts,
        )
        .await
        .unwrap();
    assert_eq!(r.events.len(), 1, "no prior cache → event is novel");

    // A dry-run must never create the dedup database file.
    assert!(
        !vault.join(".lorekeeper").join("dedup.redb").exists(),
        "dry-run must not create the dedup cache"
    );
}

#[tokio::test]
async fn plan_does_not_commit_dedup_until_commit_called() {
    let dir = TempDir::new().unwrap();
    let vault = dir.path();
    let config = base_config(vault);
    let llm: Arc<dyn LlmClient> = Arc::new(NoopLlmClient);
    let ctx = make_ctx(&config, llm);
    let pipeline = Pipeline::new(vault, ctx, &config).unwrap();
    let sc = config.sources.get("test-source").unwrap().clone();

    let ts: jiff::Timestamp = "2026-05-23T10:00:00Z".parse().unwrap();
    let items = || vec![raw_item("Subject", "Body", "M1", ts)];

    let opts = IngestOptions {
        dry_run: false,
        force: false,
        target_date: None,
    };

    let r1 = pipeline
        .plan("test-source", &sc, items(), &opts)
        .await
        .unwrap();
    assert_eq!(r1.events.len(), 1);

    // Without calling commit(), a second plan must still see the event as novel.
    let r2 = pipeline
        .plan("test-source", &sc, items(), &opts)
        .await
        .unwrap();
    assert_eq!(
        r2.events.len(),
        1,
        "event must still be novel — commit was never called"
    );

    // After commit, the event is marked seen.
    pipeline.commit(&r2.events).unwrap();
    let r3 = pipeline
        .plan("test-source", &sc, items(), &opts)
        .await
        .unwrap();
    assert!(r3.events.is_empty(), "event must be deduped after commit()");
}

#[tokio::test]
async fn force_bypasses_dedup() {
    let dir = TempDir::new().unwrap();
    let vault = dir.path();
    let config = base_config(vault);
    let llm: Arc<dyn LlmClient> = Arc::new(NoopLlmClient);
    let ctx = make_ctx(&config, llm);
    let pipeline = Pipeline::new(vault, ctx, &config).unwrap();
    let sc = config.sources.get("test-source").unwrap().clone();

    let ts: jiff::Timestamp = "2026-05-23T10:00:00Z".parse().unwrap();
    let items = || vec![raw_item("Subject", "Body", "M1", ts)];

    // Commit the event so dedup would normally filter it.
    let normal = IngestOptions {
        dry_run: false,
        force: false,
        target_date: None,
    };
    let r1 = pipeline
        .plan("test-source", &sc, items(), &normal)
        .await
        .unwrap();
    pipeline.commit(&r1.events).unwrap();
    let r2 = pipeline
        .plan("test-source", &sc, items(), &normal)
        .await
        .unwrap();
    assert!(r2.events.is_empty(), "already-seen event is deduped");

    // With force, the same event is re-processed despite being in the dedup cache.
    let forced = IngestOptions {
        dry_run: false,
        force: true,
        target_date: None,
    };
    let r3 = pipeline
        .plan("test-source", &sc, items(), &forced)
        .await
        .unwrap();
    assert_eq!(r3.events.len(), 1, "force must bypass dedup");
}

#[tokio::test]
async fn queue_mode_emits_jsonl_tasks_with_targets() {
    use lk_llm::QueueLlmClient;

    let dir = TempDir::new().unwrap();
    let vault = dir.path();
    let mut config = base_config(vault);
    config
        .sources
        .get_mut("test-source")
        .unwrap()
        .extract_concepts = true;

    let queue_dir = vault.join(".lorekeeper").join("queue");
    let llm: Arc<dyn LlmClient> = Arc::new(QueueLlmClient::new(queue_dir.clone()));
    let ctx = make_ctx(&config, llm.clone());
    let pipeline = Pipeline::new(vault, ctx, &config).unwrap();
    let sc = config.sources.get("test-source").unwrap();

    let ts: jiff::Timestamp = "2026-05-23T10:00:00Z".parse().unwrap();
    let items = vec![raw_item("Test event", "Body", "M1", ts)];

    let result = pipeline
        .plan(
            "test-source",
            sc,
            items,
            &IngestOptions {
                dry_run: false,
                force: false,
                target_date: None,
            },
        )
        .await
        .unwrap();

    // Queue mode: daily page exists but summary section is empty
    assert_eq!(result.daily_pages.len(), 1);
    assert!(result.concepts.is_empty(), "concepts deferred to skill");

    // Tasks buffer in memory until flush — the queue dir doesn't even exist yet.
    assert!(
        !queue_dir.exists(),
        "queue dir must not be created before flush"
    );

    // Simulate the CLI's end-of-run commit.
    llm.flush().await.expect("flush should succeed");

    // After flush: a single JSONL file with both tasks.
    let mut entries = tokio::fs::read_dir(&queue_dir).await.unwrap();
    let entry = entries
        .next_entry()
        .await
        .unwrap()
        .expect("queue file should exist");
    let content = tokio::fs::read_to_string(entry.path()).await.unwrap();
    let tasks: Vec<serde_json::Value> = content
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert!(
        tasks.iter().any(|t| t["kind"] == "summarize"),
        "should have a summarize task"
    );
    assert!(
        tasks.iter().any(|t| t["kind"] == "extract-concepts"),
        "should have an extract-concepts task"
    );
    for task in &tasks {
        let path = task["target"]["vault_path"].as_str().unwrap();
        assert!(
            path.contains("daily/test-source/2026-05-23"),
            "target should point at daily page: {path}"
        );
    }
}

#[tokio::test]
async fn weekly_synthesis_is_opt_in_via_include_sources() {
    let dir = TempDir::new().unwrap();
    let vault = dir.path();

    // A daily page exists in the target ISO week (Mon 2026-05-18 .. Sun 2026-05-24).
    let daily = vault.join("daily").join("test-source");
    std::fs::create_dir_all(&daily).unwrap();
    std::fs::write(
        daily.join("2026-05-20.md"),
        "---\nid: test-source-2026-05-20\n---\n\n# News\n\nbody\n",
    )
    .unwrap();

    let llm: Arc<dyn LlmClient> = Arc::new(NoopLlmClient);

    // Empty include_sources → the source is NOT swept into a cross-source themes page,
    // even though its daily page sits in range. Knowledge feeds stay out of the digest.
    let config = base_config(vault);
    let synth = Synthesizer::new(vault, make_ctx(&config, llm.clone()), &config);
    assert!(
        synth
            .try_weekly_synthesis(jiff::civil::date(2026, 5, 23))
            .await
            .unwrap()
            .is_none(),
        "empty include_sources must produce no cross-source themes page"
    );

    // Listing the source opts it in → a themes page is produced.
    let mut config = base_config(vault);
    config.synthesis.weekly.include_sources = vec!["test-source".into()];
    let synth = Synthesizer::new(vault, make_ctx(&config, llm), &config);
    assert!(
        synth
            .try_weekly_synthesis(jiff::civil::date(2026, 5, 23))
            .await
            .unwrap()
            .is_some(),
        "a source listed in include_sources opts into the weekly themes page"
    );
}

#[tokio::test]
async fn performance_enabled_gates_personal_narratives() {
    let dir = TempDir::new().unwrap();
    let vault = dir.path();

    // A work-log page exists in the target ISO week.
    let work_log = vault.join("me").join("work-log");
    std::fs::create_dir_all(&work_log).unwrap();
    std::fs::write(
        work_log.join("2026-05-20.md"),
        "---\nid: work-log-2026-05-20\ncategories: [project-delivery]\n---\n\n# Work\n\nbody\n",
    )
    .unwrap();

    let llm: Arc<dyn LlmClient> = Arc::new(NoopLlmClient);

    // performance.enabled: true → the personal weekly review is produced.
    let config = base_config(vault);
    let synth = Synthesizer::new(vault, make_ctx(&config, llm.clone()), &config);
    assert!(
        synth
            .try_weekly_personal(jiff::civil::date(2026, 5, 23))
            .await
            .unwrap()
            .is_some(),
        "personal review should be produced when performance is enabled"
    );

    // performance.enabled: false → no review, even though the work-log page exists.
    let mut config = base_config(vault);
    config.performance.enabled = false;
    let synth = Synthesizer::new(vault, make_ctx(&config, llm), &config);
    assert!(
        synth
            .try_weekly_personal(jiff::civil::date(2026, 5, 23))
            .await
            .unwrap()
            .is_none(),
        "disabling performance must suppress personal reviews"
    );
}

#[tokio::test]
async fn work_log_generation_is_gated_by_performance_enabled() {
    let dir = TempDir::new().unwrap();
    let vault = dir.path();

    let event = lk_core::event::Event {
        id: lk_core::event::EventId::new("test-source", jiff::civil::date(2026, 5, 20), "x"),
        source_id: "test-source".into(),
        source_type: SourceType::Gmail,
        date: jiff::civil::date(2026, 5, 20),
        title: "did a thing".into(),
        body: "details".into(),
        url: None,
        author: None,
        labels: vec!["personal".into()],
        work_category: None,
        is_personal: true,
        content_hash: lk_core::event::content_hash("did a thing", "details"),
        metadata: serde_json::Value::Null,
    };

    let llm: Arc<dyn LlmClient> = Arc::new(NoopLlmClient);

    // Enabled → the personal event yields a work-log page.
    let config = base_config(vault);
    let pipeline = Pipeline::new(vault, make_ctx(&config, llm.clone()), &config).unwrap();
    assert!(
        !pipeline
            .render_work_log(std::slice::from_ref(&event))
            .await
            .unwrap()
            .is_empty(),
        "work-log should be produced when performance is enabled"
    );
    drop(pipeline); // release the single-writer dedup lock before reopening

    // Disabled → no work-log at the mechanism boundary, regardless of caller.
    let mut config = base_config(vault);
    config.performance.enabled = false;
    let pipeline = Pipeline::new(vault, make_ctx(&config, llm), &config).unwrap();
    assert!(
        pipeline
            .render_work_log(std::slice::from_ref(&event))
            .await
            .unwrap()
            .is_empty(),
        "disabling performance must suppress work-log generation"
    );
}
