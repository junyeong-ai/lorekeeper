use std::sync::Arc;

use tempfile::TempDir;

use lk_core::concept::ExtractedConcept;
use lk_core::config::{
    Config, DedupConfig, Identity, PerformanceConfig, SourceConfig, SourceType, SynthesisConfig,
    VaultConfig, VaultDirs,
};
use lk_core::event::RawItem;
use lk_pipeline::{IngestOptions, Pipeline, PipelineContext, Synthesizer};
use lk_queue::{LlmClient, MockLlmClient, NoopLlmClient};

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
        concepts: Default::default(),
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
        category: None,
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
        category: None,
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
            labels: vec![],
            extract_concepts: true,
            focus: None,
            track_personal: false,
        },
    );

    let llm: Arc<dyn LlmClient> = Arc::new(MockLlmClient::with_concepts(vec![ExtractedConcept {
        name: "Shared Concept".into(),
        slug: "shared-concept".into(),
        category: None,
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
    // Citations accumulate as the count here; the `## 출처` ref list itself is
    // re-derived from the wikilink graph by `backlinks-sync` (covered in lk-graph),
    // not written into concept frontmatter at ingest.
    assert!(
        shared.content.contains("source_count: 2"),
        "both sources must accumulate into the count:\n{}",
        shared.content
    );
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
    pipeline.commit(&r2.events, &r2.duplicates).unwrap();
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
    pipeline.commit(&r1.events, &r1.duplicates).unwrap();
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
    use lk_queue::QueueLlmClient;

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
async fn synthesis_page_is_a_materialized_view() {
    use lk_queue::QueueLlmClient;

    let dir = TempDir::new().unwrap();
    let vault = dir.path();

    let work_log = vault.join("me").join("work-log");
    std::fs::create_dir_all(&work_log).unwrap();
    std::fs::write(
        work_log.join("2026-05-20.md"),
        "---\nid: work-log-2026-05-20\ncategories: [project-delivery]\n---\n\n# Work\n\nbody\n",
    )
    .unwrap();

    let queue_dir = vault.join(".lorekeeper").join("queue");
    let config = base_config(vault);
    let strings = lk_core::i18n::Locale::Ko.strings();
    let week_date = jiff::civil::date(2026, 5, 23);

    // First run: empty narrative + a queued task + llm_inputs.narrative hash stamped.
    let llm1: Arc<dyn LlmClient> = Arc::new(QueueLlmClient::new(queue_dir.clone()));
    let synth1 = Synthesizer::new(vault, make_ctx(&config, llm1.clone()), &config);
    let out1 = synth1
        .try_weekly_personal(week_date)
        .await
        .unwrap()
        .expect("weekly personal produced");
    let page_path = vault.join(out1.path.as_ref());
    std::fs::create_dir_all(page_path.parent().unwrap()).unwrap();
    std::fs::write(&page_path, &out1.content).unwrap();
    llm1.flush().await.unwrap();
    assert!(
        out1.content.contains("llm_inputs:"),
        "synthesis page must carry an llm_inputs hash:\n{}",
        out1.content
    );
    let queued_first = std::fs::read_dir(&queue_dir).unwrap().count();
    assert!(queued_first > 0, "first run must enqueue a synthesis task");

    // Simulate /lore-process filling the narrative section.
    let filled = lk_vault::replace_section(
        &out1.content,
        strings.key_summary,
        "REAL-SYNTHESIS-NARRATIVE",
    );
    std::fs::write(&page_path, &filled).unwrap();
    for entry in std::fs::read_dir(&queue_dir).unwrap().flatten() {
        if entry.path().extension().and_then(|s| s.to_str()) == Some("jsonl") {
            std::fs::remove_file(entry.path()).unwrap();
        }
    }

    // Second run, identical input: cache hit → no new task, narrative preserved.
    let llm2: Arc<dyn LlmClient> = Arc::new(QueueLlmClient::new(queue_dir.clone()));
    let synth2 = Synthesizer::new(vault, make_ctx(&config, llm2.clone()), &config);
    let out2 = synth2
        .try_weekly_personal(week_date)
        .await
        .unwrap()
        .expect("weekly personal produced on re-run");
    llm2.flush().await.unwrap();
    assert!(
        out2.content.contains("REAL-SYNTHESIS-NARRATIVE"),
        "cached synthesis narrative must be preserved across re-render:\n{}",
        out2.content
    );
    let queued_second = std::fs::read_dir(&queue_dir)
        .map(|d| {
            d.flatten()
                .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("jsonl"))
                .count()
        })
        .unwrap_or(0);
    assert_eq!(
        queued_second, 0,
        "unchanged synthesis input must not re-enqueue any task",
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

mod materialized_view {
    //! Daily pages are materialized views: structural fields (frontmatter, raw events,
    //! headings) come from the deterministic render; semantic fields (summary, refined
    //! events, concept wiki-links) are filled by `/lore-process` and preserved across
    //! re-renders via the `llm_inputs` BLAKE3-128 hash recorded in frontmatter.
    //!
    //! These tests exercise the three observable behaviors:
    //! 1. Re-ingesting unchanged input does not re-enqueue LLM tasks (token-cache hit).
    //! 2. Re-ingesting with a real LLM-filled section preserves the body across renders.
    //! 3. Mutating the input invalidates the cache and the tasks fire again.
    use super::*;
    use lk_queue::QueueLlmClient;

    fn first_daily_page(result: &lk_pipeline::IngestResult) -> &str {
        result
            .daily_pages
            .first()
            .map(|p| p.content.as_str())
            .expect("daily page rendered")
    }

    async fn write_to_vault(vault: &std::path::Path, result: &lk_pipeline::IngestResult) {
        for page in &result.daily_pages {
            let abs = vault.join(page.path.as_ref());
            tokio::fs::create_dir_all(abs.parent().unwrap())
                .await
                .unwrap();
            tokio::fs::write(&abs, &page.content).await.unwrap();
        }
    }

    fn queue_task_kinds_for_source(
        queue_dir: &std::path::Path,
    ) -> std::collections::HashSet<String> {
        let mut kinds = std::collections::HashSet::new();
        let Ok(entries) = std::fs::read_dir(queue_dir) else {
            return kinds;
        };
        for entry in entries.flatten() {
            if entry.path().extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            let content = std::fs::read_to_string(entry.path()).unwrap();
            for line in content.lines() {
                let task: serde_json::Value = serde_json::from_str(line).unwrap();
                kinds.insert(task["kind"].as_str().unwrap().to_string());
            }
        }
        kinds
    }

    #[tokio::test]
    async fn unchanged_input_skips_llm_enqueue() {
        let dir = TempDir::new().unwrap();
        let vault = dir.path();
        let config = base_config(vault);
        let sc = config.sources.get("test-source").unwrap();

        let ts: jiff::Timestamp = "2026-05-23T10:00:00Z".parse().unwrap();
        let items = vec![raw_item("Event A", "Body A", "E1", ts)];

        // First ingest: empty vault, queue receives the full task set.
        let queue_dir = vault.join(".lorekeeper").join("queue");
        let llm1: Arc<dyn LlmClient> = Arc::new(QueueLlmClient::new(queue_dir.clone()));
        let result1 = {
            let pipeline1 = Pipeline::new(vault, make_ctx(&config, llm1.clone()), &config).unwrap();
            let r = pipeline1
                .plan(
                    "test-source",
                    sc,
                    items.clone(),
                    &IngestOptions {
                        dry_run: false,
                        force: false,
                        target_date: None,
                    },
                )
                .await
                .unwrap();
            write_to_vault(vault, &r).await;
            llm1.flush().await.unwrap();
            r
        }; // pipeline1 dropped here → redb file lock released

        let kinds_first = queue_task_kinds_for_source(&queue_dir);
        assert!(
            kinds_first.contains("summarize"),
            "first run enqueues summary"
        );
        assert!(
            kinds_first.contains("refine-events"),
            "first run enqueues refine"
        );
        assert!(
            kinds_first.contains("extract-concepts"),
            "first run enqueues concepts"
        );

        // Simulate the skill filling each section so the cache is hot.
        fill_llm_sections_with_dummy_bodies(vault, &result1).await;

        // Clear the queue archive so the second flush is observed in isolation.
        clear_queue_dir(&queue_dir).await;

        // Second ingest: same input, page exists with filled sections + matching hashes.
        // No tasks should be enqueued.
        let llm2: Arc<dyn LlmClient> = Arc::new(QueueLlmClient::new(queue_dir.clone()));
        let pipeline2 = Pipeline::new(vault, make_ctx(&config, llm2.clone()), &config).unwrap();
        let result2 = pipeline2
            .plan(
                "test-source",
                sc,
                items,
                &IngestOptions {
                    dry_run: false,
                    force: true, // force triggers the path that previously re-queued unconditionally
                    target_date: None,
                },
            )
            .await
            .unwrap();
        llm2.flush().await.unwrap();
        let kinds_second = queue_task_kinds_for_source(&queue_dir);
        assert!(
            kinds_second.is_empty(),
            "cached re-ingest must not enqueue any task; got {kinds_second:?}",
        );

        // The freshly-rendered page must preserve the previously-filled bodies.
        let page = first_daily_page(&result2);
        assert!(
            page.contains("REAL-SUMMARY-BODY"),
            "summary body preserved across re-render"
        );
        assert!(
            page.contains("REAL-REFINED-EVENT-BODY"),
            "refined events preserved"
        );
        assert!(page.contains("REAL-CONCEPT-WIKILINK"), "concepts preserved");
    }

    #[tokio::test]
    async fn changed_input_invalidates_cache_and_re_enqueues() {
        let dir = TempDir::new().unwrap();
        let vault = dir.path();
        let config = base_config(vault);
        let sc = config.sources.get("test-source").unwrap();

        let ts: jiff::Timestamp = "2026-05-23T10:00:00Z".parse().unwrap();
        let initial = vec![raw_item("Event A", "Body A", "E1", ts)];

        let queue_dir = vault.join(".lorekeeper").join("queue");
        let llm1: Arc<dyn LlmClient> = Arc::new(QueueLlmClient::new(queue_dir.clone()));
        let result1 = {
            let pipeline1 = Pipeline::new(vault, make_ctx(&config, llm1.clone()), &config).unwrap();
            let r = pipeline1
                .plan(
                    "test-source",
                    sc,
                    initial,
                    &IngestOptions {
                        dry_run: false,
                        force: false,
                        target_date: None,
                    },
                )
                .await
                .unwrap();
            write_to_vault(vault, &r).await;
            llm1.flush().await.unwrap();
            r
        };
        fill_llm_sections_with_dummy_bodies(vault, &result1).await;
        clear_queue_dir(&queue_dir).await;

        // Re-ingest with a NEW event body — the LLM input changes, so its hash changes,
        // and every task that takes that text in its prompt must fire again.
        let changed = vec![
            raw_item("Event A", "Body A", "E1", ts),
            raw_item("Event B", "Body B (new!)", "E2", ts),
        ];
        let llm2: Arc<dyn LlmClient> = Arc::new(QueueLlmClient::new(queue_dir.clone()));
        let pipeline2 = Pipeline::new(vault, make_ctx(&config, llm2.clone()), &config).unwrap();
        let _ = pipeline2
            .plan(
                "test-source",
                sc,
                changed,
                &IngestOptions {
                    dry_run: false,
                    force: true,
                    target_date: None,
                },
            )
            .await
            .unwrap();
        llm2.flush().await.unwrap();
        let kinds = queue_task_kinds_for_source(&queue_dir);
        assert!(
            kinds.contains("summarize"),
            "new event invalidates summary cache"
        );
        assert!(
            kinds.contains("refine-events"),
            "new event invalidates refine cache"
        );
        assert!(
            kinds.contains("extract-concepts"),
            "new event invalidates concepts cache"
        );
    }

    #[tokio::test]
    async fn refined_then_changed_input_keeps_page_hash_aligned_with_new_task() {
        // Regression for the in-place refine completion contract: after a page is
        // refined and its completion stamped, changing the events must
        //   (a) re-enqueue refine, and
        //   (b) leave page.refine_events == the NEW refine task's cache_hash, so the
        //       skill PROCESSES it (stale-guard matches) instead of dropping it as
        //       stale, and the stale `refine_events_done` must NOT equal the new
        //       refine_events (else the skill would wrongly skip).
        // Without the pre-stamped current-input hash this strands refinement forever.
        let dir = TempDir::new().unwrap();
        let vault = dir.path();
        let config = base_config(vault);
        let sc = config.sources.get("test-source").unwrap();
        let ts: jiff::Timestamp = "2026-05-23T10:00:00Z".parse().unwrap();
        let queue_dir = vault.join(".lorekeeper").join("queue");

        // Ingest 1 + simulate /lore-process (fill bodies + stamp refine_events_done).
        let llm1: Arc<dyn LlmClient> = Arc::new(QueueLlmClient::new(queue_dir.clone()));
        let result1 = {
            let pipeline1 = Pipeline::new(vault, make_ctx(&config, llm1.clone()), &config).unwrap();
            let r = pipeline1
                .plan(
                    "test-source",
                    sc,
                    vec![raw_item("Event A", "Body A", "E1", ts)],
                    &IngestOptions {
                        dry_run: false,
                        force: false,
                        target_date: None,
                    },
                )
                .await
                .unwrap();
            write_to_vault(vault, &r).await;
            llm1.flush().await.unwrap();
            r
        };
        fill_llm_sections_with_dummy_bodies(vault, &result1).await;
        clear_queue_dir(&queue_dir).await;

        // Ingest 2: add an event → the refine input changes.
        let llm2: Arc<dyn LlmClient> = Arc::new(QueueLlmClient::new(queue_dir.clone()));
        let result2 = {
            let pipeline2 = Pipeline::new(vault, make_ctx(&config, llm2.clone()), &config).unwrap();
            let r = pipeline2
                .plan(
                    "test-source",
                    sc,
                    vec![
                        raw_item("Event A", "Body A", "E1", ts),
                        raw_item("Event B", "Body B (new!)", "E2", ts),
                    ],
                    &IngestOptions {
                        dry_run: false,
                        force: true,
                        target_date: None,
                    },
                )
                .await
                .unwrap();
            write_to_vault(vault, &r).await;
            llm2.flush().await.unwrap();
            r
        };

        let kinds = queue_task_kinds_for_source(&queue_dir);
        assert!(
            kinds.contains("refine-events"),
            "changed input must re-enqueue refine"
        );

        let page_path = result2.daily_pages.first().unwrap().path.to_string();
        let new_refine_hash =
            refine_hash_for_page(&queue_dir, &page_path).expect("a refine task targets the page");
        let raw = tokio::fs::read_to_string(vault.join(&page_path))
            .await
            .unwrap();
        let page = lk_vault::parse_page(&raw).unwrap();
        let llm_inputs = page.frontmatter.get("llm_inputs");
        let page_refine = llm_inputs
            .and_then(|v| v.get("refine_events"))
            .and_then(|v| v.as_str());
        let page_done = llm_inputs
            .and_then(|v| v.get("refine_events_done"))
            .and_then(|v| v.as_str());

        assert_eq!(
            page_refine,
            Some(new_refine_hash.as_str()),
            "page.refine_events must equal the NEW task hash so the skill processes it"
        );
        assert_ne!(
            page_done, page_refine,
            "stale completion stamp must not equal the new refine_events (would wrongly skip)"
        );
        assert!(
            !raw.contains("REAL-REFINED-EVENT-BODY"),
            "stale refined body must not be preserved across an input change"
        );
    }

    #[tokio::test]
    async fn deleting_section_body_forces_re_enqueue() {
        // Mechanism-free override path: a vault editor wipes the section body and the
        // next ingest re-queues without any flag.
        let dir = TempDir::new().unwrap();
        let vault = dir.path();
        let config = base_config(vault);
        let sc = config.sources.get("test-source").unwrap();

        let ts: jiff::Timestamp = "2026-05-23T10:00:00Z".parse().unwrap();
        let items = vec![raw_item("Event A", "Body A", "E1", ts)];

        let queue_dir = vault.join(".lorekeeper").join("queue");
        let llm1: Arc<dyn LlmClient> = Arc::new(QueueLlmClient::new(queue_dir.clone()));
        let result1 = {
            let pipeline1 = Pipeline::new(vault, make_ctx(&config, llm1.clone()), &config).unwrap();
            let r = pipeline1
                .plan(
                    "test-source",
                    sc,
                    items.clone(),
                    &IngestOptions {
                        dry_run: false,
                        force: false,
                        target_date: None,
                    },
                )
                .await
                .unwrap();
            write_to_vault(vault, &r).await;
            llm1.flush().await.unwrap();
            r
        };
        fill_llm_sections_with_dummy_bodies(vault, &result1).await;

        // Wipe the summary section body manually (frontmatter hash stays).
        let strings = lk_core::i18n::Locale::Ko.strings();
        let page_path = vault.join(result1.daily_pages[0].path.as_ref());
        let body = tokio::fs::read_to_string(&page_path).await.unwrap();
        let cleared = lk_vault::replace_section(&body, strings.summary, "");
        tokio::fs::write(&page_path, cleared).await.unwrap();

        clear_queue_dir(&queue_dir).await;

        let llm2: Arc<dyn LlmClient> = Arc::new(QueueLlmClient::new(queue_dir.clone()));
        let pipeline2 = Pipeline::new(vault, make_ctx(&config, llm2.clone()), &config).unwrap();
        let _ = pipeline2
            .plan(
                "test-source",
                sc,
                items,
                &IngestOptions {
                    dry_run: false,
                    force: true,
                    target_date: None,
                },
            )
            .await
            .unwrap();
        llm2.flush().await.unwrap();
        let kinds = queue_task_kinds_for_source(&queue_dir);
        assert!(
            kinds.contains("summarize"),
            "emptied section must trigger re-enqueue without any flag",
        );
    }

    /// Simulate `/lore-process`: fill each LLM-owned section AND stamp the
    /// `refine_events` frontmatter (which the pipeline deliberately leaves `null`
    /// for an in-place rewrite — the skill owns that completion marker). The
    /// stamp must equal the enqueued refine-events task's `cache_hash`.
    async fn fill_llm_sections_with_dummy_bodies(
        vault: &std::path::Path,
        result: &lk_pipeline::IngestResult,
    ) {
        // Default locale is Ko, so the section anchors the pipeline emits are Korean.
        // Gmail-typed source uses 주요 이벤트 (not 주요 메시지).
        let strings = lk_core::i18n::Locale::Ko.strings();
        let queue_dir = vault.join(".lorekeeper").join("queue");
        for page in &result.daily_pages {
            let abs = vault.join(page.path.as_ref());
            let mut content = tokio::fs::read_to_string(&abs).await.unwrap();
            content = lk_vault::replace_section(&content, strings.summary, "REAL-SUMMARY-BODY");
            content =
                lk_vault::replace_section(&content, strings.key_events, "REAL-REFINED-EVENT-BODY");
            content = lk_vault::replace_section(
                &content,
                strings.related_concepts,
                "- [[REAL-CONCEPT-WIKILINK]]",
            );
            // The pipeline already pre-stamped `refine_events` with this hash; the
            // skill owns the completion marker `refine_events_done`. Stamp it equal
            // so the next ingest is a cache hit (done == refine_events).
            if let Some(hash) = refine_hash_for_page(&queue_dir, &page.path.to_string()) {
                content = content.replace(
                    &format!("refine_events: \"{hash}\""),
                    &format!("refine_events: \"{hash}\"\n  refine_events_done: \"{hash}\""),
                );
            }
            tokio::fs::write(&abs, content).await.unwrap();
        }
    }

    /// The `cache_hash` of the enqueued `refine-events` task targeting `page_path`.
    fn refine_hash_for_page(queue_dir: &std::path::Path, page_path: &str) -> Option<String> {
        let entries = std::fs::read_dir(queue_dir).ok()?;
        for entry in entries.flatten() {
            if entry.path().extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            let content = std::fs::read_to_string(entry.path()).ok()?;
            for line in content.lines() {
                let task: serde_json::Value = serde_json::from_str(line).ok()?;
                if task["kind"] == "refine-events"
                    && task["target"]["vault_path"].as_str() == Some(page_path)
                {
                    return task["cache_hash"].as_str().map(str::to_string);
                }
            }
        }
        None
    }

    async fn clear_queue_dir(queue_dir: &std::path::Path) {
        if let Ok(mut entries) = tokio::fs::read_dir(queue_dir).await {
            while let Some(entry) = entries.next_entry().await.unwrap_or(None) {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                    tokio::fs::remove_file(&path).await.unwrap();
                }
            }
        }
    }

    #[tokio::test]
    async fn deleting_llm_inputs_frontmatter_line_forces_re_enqueue() {
        // Twin of `deleting_section_body_forces_re_enqueue`. Both override paths must
        // work: body deletion AND frontmatter-hash deletion. The user picks whichever
        // is more convenient — only one is needed to invalidate the cache.
        let dir = TempDir::new().unwrap();
        let vault = dir.path();
        let config = base_config(vault);
        let sc = config.sources.get("test-source").unwrap();

        let ts: jiff::Timestamp = "2026-05-23T10:00:00Z".parse().unwrap();
        let items = vec![raw_item("Event A", "Body A", "E1", ts)];

        let queue_dir = vault.join(".lorekeeper").join("queue");
        let llm1: Arc<dyn LlmClient> = Arc::new(QueueLlmClient::new(queue_dir.clone()));
        let result1 = {
            let pipeline1 = Pipeline::new(vault, make_ctx(&config, llm1.clone()), &config).unwrap();
            let r = pipeline1
                .plan(
                    "test-source",
                    sc,
                    items.clone(),
                    &IngestOptions {
                        dry_run: false,
                        force: false,
                        target_date: None,
                    },
                )
                .await
                .unwrap();
            write_to_vault(vault, &r).await;
            llm1.flush().await.unwrap();
            r
        };
        fill_llm_sections_with_dummy_bodies(vault, &result1).await;

        // Strip the `summary:` line from `llm_inputs:`. The body stays intact, the
        // hash is gone — the lookup should see "no cached hash" and re-enqueue.
        let page_path = vault.join(result1.daily_pages[0].path.as_ref());
        let body = tokio::fs::read_to_string(&page_path).await.unwrap();
        let edited: String = body
            .lines()
            .filter(|line| !line.trim_start().starts_with("summary:"))
            .collect::<Vec<_>>()
            .join("\n");
        tokio::fs::write(&page_path, edited).await.unwrap();

        clear_queue_dir(&queue_dir).await;

        let llm2: Arc<dyn LlmClient> = Arc::new(QueueLlmClient::new(queue_dir.clone()));
        let pipeline2 = Pipeline::new(vault, make_ctx(&config, llm2.clone()), &config).unwrap();
        let _ = pipeline2
            .plan(
                "test-source",
                sc,
                items,
                &IngestOptions {
                    dry_run: false,
                    force: true,
                    target_date: None,
                },
            )
            .await
            .unwrap();
        llm2.flush().await.unwrap();
        let kinds = queue_task_kinds_for_source(&queue_dir);
        assert!(
            kinds.contains("summarize"),
            "removing the summary hash must trigger re-enqueue: got {kinds:?}",
        );
        // The OTHER hashes are still present, so their tasks should NOT re-enqueue.
        assert!(
            !kinds.contains("refine-events"),
            "refine-events hash was untouched; must remain cached: got {kinds:?}",
        );
        assert!(
            !kinds.contains("extract-concepts"),
            "concepts hash was untouched; must remain cached: got {kinds:?}",
        );
    }

    #[tokio::test]
    async fn growing_concept_registry_does_not_invalidate_other_caches() {
        // Regression guard for the existing_concepts cache-poisoning bug. The first
        // run populates the vault with a concept page; the second run with identical
        // input must still hit the cache, even though `load_existing_concept_refs`
        // now returns one more entry. Hash identity is the cache_identity subset.
        let dir = TempDir::new().unwrap();
        let vault = dir.path();
        let config = base_config(vault);
        let sc = config.sources.get("test-source").unwrap();

        let ts: jiff::Timestamp = "2026-05-23T10:00:00Z".parse().unwrap();
        let items = vec![raw_item("Event A", "Body A", "E1", ts)];

        // Bootstrap the daily page through the QueueLlmClient (so the queue carries the
        // refine-events task whose cache_hash `/lore-process` would stamp), then seed a
        // concept page on disk directly so the on-disk registry is non-empty on run 2.
        let queue_dir = vault.join(".lorekeeper").join("queue");
        let llm1: Arc<dyn LlmClient> = Arc::new(QueueLlmClient::new(queue_dir.clone()));
        let result1 = {
            let pipeline1 = Pipeline::new(vault, make_ctx(&config, llm1.clone()), &config).unwrap();
            let r = pipeline1
                .plan(
                    "test-source",
                    sc,
                    items.clone(),
                    &IngestOptions {
                        dry_run: false,
                        force: false,
                        target_date: None,
                    },
                )
                .await
                .unwrap();
            write_to_vault(vault, &r).await;
            llm1.flush().await.unwrap();
            r
        };
        let concept_abs = vault.join("wiki/concepts/concept-a.md");
        tokio::fs::create_dir_all(concept_abs.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(
            &concept_abs,
            "---\ntitle: \"Concept A\"\ncreated: 2026-05-23\n---\n\n# Concept A\n",
        )
        .await
        .unwrap();
        fill_llm_sections_with_dummy_bodies(vault, &result1).await;
        clear_queue_dir(&queue_dir).await;

        // Second run with QueueLlmClient and IDENTICAL input. Vault now has one
        // concept on disk — if existing_concepts were part of the cache identity,
        // every cache lookup would miss and re-enqueue. With existing_concepts
        // excluded by design, nothing fires.
        let llm_queue: Arc<dyn LlmClient> = Arc::new(QueueLlmClient::new(queue_dir.clone()));
        let pipeline2 =
            Pipeline::new(vault, make_ctx(&config, llm_queue.clone()), &config).unwrap();
        let _ = pipeline2
            .plan(
                "test-source",
                sc,
                items,
                &IngestOptions {
                    dry_run: false,
                    force: true,
                    target_date: None,
                },
            )
            .await
            .unwrap();
        llm_queue.flush().await.unwrap();

        let kinds = queue_task_kinds_for_source(&queue_dir);
        assert!(
            kinds.is_empty(),
            "growing concept registry must not invalidate cached tasks; got {kinds:?}",
        );
    }

    #[tokio::test]
    async fn queue_task_cache_hash_equals_page_frontmatter_hash() {
        // The stale-task guard's entire correctness rests on the queue task's
        // cache_hash matching the page's llm_inputs.<key> frontmatter. This test
        // closes that link end to end: for every enqueued task, parse the target
        // page's frontmatter and assert llm_inputs[<key>] == task.cache_hash.
        let dir = TempDir::new().unwrap();
        let vault = dir.path();
        let config = base_config(vault);
        let sc = config.sources.get("test-source").unwrap();

        let ts: jiff::Timestamp = "2026-05-23T10:00:00Z".parse().unwrap();
        let items = vec![raw_item("Event A", "Body A", "E1", ts)];

        let queue_dir = vault.join(".lorekeeper").join("queue");
        let llm: Arc<dyn LlmClient> = Arc::new(QueueLlmClient::new(queue_dir.clone()));
        {
            let pipeline = Pipeline::new(vault, make_ctx(&config, llm.clone()), &config).unwrap();
            let r = pipeline
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
            write_to_vault(vault, &r).await;
        }
        llm.flush().await.unwrap();

        // The frontmatter key per target.kind — must mirror TargetKind::llm_inputs_key.
        let key_for = |kind: &str| -> &'static str {
            match kind {
                "daily-summary" | "document-summary" => "summary",
                "daily-refine-events" => "refine_events",
                "daily-concepts" | "document-concepts" => "concepts",
                "work-log-synthesis" => "topic_summary",
                "weekly-synthesis-narrative" => "themes",
                "weekly-personal-narrative"
                | "monthly-personal-narrative"
                | "quarterly-personal-narrative"
                | "annual-personal-narrative" => "narrative",
                other => panic!("unmapped target.kind in test: {other}"),
            }
        };

        let mut entries = tokio::fs::read_dir(&queue_dir).await.unwrap();
        let entry = entries.next_entry().await.unwrap().unwrap();
        let content = tokio::fs::read_to_string(entry.path()).await.unwrap();
        let mut checked = 0;
        for line in content.lines() {
            let task: serde_json::Value = serde_json::from_str(line).unwrap();
            let cache_hash = task["cache_hash"].as_str().expect("cache_hash present");
            assert_eq!(
                cache_hash.len(),
                32,
                "cache_hash must be 32 hex (BLAKE3-128)"
            );
            assert!(cache_hash.chars().all(|c| c.is_ascii_hexdigit()));

            let page_path = vault.join(task["target"]["vault_path"].as_str().unwrap());
            let raw = tokio::fs::read_to_string(&page_path).await.unwrap();
            let page = lk_vault::parse_page(&raw).unwrap();
            let kind = task["target"]["kind"].as_str().unwrap();
            let key = key_for(kind);
            let frontmatter_hash = page
                .frontmatter
                .get("llm_inputs")
                .and_then(|v| v.get(key))
                .and_then(|v| v.as_str());

            // Uniform invariant for EVERY kind (including the in-place refine, whose
            // `refine_events` is pre-stamped with the current-input hash): the page's
            // stale reference key equals the task's cache_hash at enqueue time. For
            // refine, completion is tracked separately by `refine_events_done`, which
            // is correctly absent here (the skill hasn't run yet).
            assert_eq!(
                frontmatter_hash,
                Some(cache_hash),
                "stale-guard invariant: page llm_inputs.{key} must equal task.cache_hash",
            );
            if kind == "daily-refine-events" {
                assert!(
                    page.frontmatter
                        .get("llm_inputs")
                        .and_then(|v| v.get("refine_events_done"))
                        .and_then(|v| v.as_str())
                        .is_none(),
                    "refine_events_done must be unset before /lore-process runs",
                );
            }
            checked += 1;
        }
        assert!(checked >= 3, "expected several tasks, checked {checked}");
    }

    /// The document/manual path (`plan_documents`) is a materialized view exactly like
    /// the daily path, but every other test exercises only `SourceType::Gmail`. This
    /// guards the document preservation contract directly: re-ingesting an unchanged
    /// manual document is a cache hit (zero re-enqueue) and the skill-filled
    /// `## Summary` / `## Related Concepts` survive the re-render.
    #[tokio::test]
    async fn manual_document_is_a_materialized_view() {
        let dir = TempDir::new().unwrap();
        let vault = dir.path();
        let mut config = base_config(vault);
        // Reuse the single configured source but flip it to the document path.
        config.sources.get_mut("test-source").unwrap().source_type = SourceType::Manual;
        let sc = config.sources.get("test-source").unwrap().clone();

        let ts: jiff::Timestamp = "2026-05-23T10:00:00Z".parse().unwrap();
        let items = vec![raw_item("My Research Note", "Body of the note", "DOC1", ts)];

        let queue_dir = vault.join(".lorekeeper").join("queue");
        let strings = lk_core::i18n::Locale::Ko.strings();

        // First ingest: empty document page + queued summary/concepts tasks.
        let llm1: Arc<dyn LlmClient> = Arc::new(QueueLlmClient::new(queue_dir.clone()));
        let result1 = {
            let pipeline = Pipeline::new(vault, make_ctx(&config, llm1.clone()), &config).unwrap();
            let r = pipeline
                .plan(
                    "test-source",
                    &sc,
                    items.clone(),
                    &IngestOptions {
                        dry_run: false,
                        force: false,
                        target_date: None,
                    },
                )
                .await
                .unwrap();
            for page in &r.document_pages {
                let abs = vault.join(page.path.as_ref());
                tokio::fs::create_dir_all(abs.parent().unwrap())
                    .await
                    .unwrap();
                tokio::fs::write(&abs, &page.content).await.unwrap();
            }
            llm1.flush().await.unwrap();
            r
        };
        assert_eq!(
            result1.document_pages.len(),
            1,
            "manual source yields a document page"
        );
        let kinds_first = queue_task_kinds_for_source(&queue_dir);
        assert!(
            kinds_first.contains("summarize"),
            "first run enqueues document summary"
        );
        assert!(
            kinds_first.contains("extract-concepts"),
            "first run enqueues document concepts"
        );

        // Simulate /lore-process filling the document's LLM sections.
        let doc_path = vault.join(result1.document_pages[0].path.as_ref());
        let mut content = tokio::fs::read_to_string(&doc_path).await.unwrap();
        content = lk_vault::replace_section(&content, strings.summary, "REAL-DOC-SUMMARY");
        content =
            lk_vault::replace_section(&content, strings.related_concepts, "- [[REAL-DOC-CONCEPT]]");
        tokio::fs::write(&doc_path, content).await.unwrap();
        clear_queue_dir(&queue_dir).await;

        // Re-ingest identical input → cache hit: zero re-enqueue, bodies preserved.
        let llm2: Arc<dyn LlmClient> = Arc::new(QueueLlmClient::new(queue_dir.clone()));
        let pipeline2 = Pipeline::new(vault, make_ctx(&config, llm2.clone()), &config).unwrap();
        let result2 = pipeline2
            .plan(
                "test-source",
                &sc,
                items,
                &IngestOptions {
                    dry_run: false,
                    force: true,
                    target_date: None,
                },
            )
            .await
            .unwrap();
        llm2.flush().await.unwrap();
        let kinds_second = queue_task_kinds_for_source(&queue_dir);
        assert!(
            kinds_second.is_empty(),
            "cached document re-ingest must not re-enqueue any task; got {kinds_second:?}",
        );
        let page = result2.document_pages[0].content.as_str();
        assert!(
            page.contains("REAL-DOC-SUMMARY"),
            "document summary preserved across re-render"
        );
        assert!(
            page.contains("REAL-DOC-CONCEPT"),
            "document concepts preserved across re-render"
        );
    }
}
