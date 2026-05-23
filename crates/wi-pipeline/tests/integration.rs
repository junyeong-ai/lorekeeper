use std::path::PathBuf;
use std::sync::Arc;

use tempfile::TempDir;

use wi_core::concept::{Confidence, ExtractedConcept};
use wi_core::config::{
    Config, DedupConfig, Identity, LabelConfig, PerformanceConfig, SourceConfig, SourceType,
    SynthesisConfig, VaultConfig, VaultDirs,
};
use wi_core::event::RawItem;
use wi_llm::{LlmClient, MockLlmClient, NoopLlmClient};
use wi_pipeline::{IngestOptions, Pipeline, PipelineContext, Synthesizer};

fn template_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../templates")
}

fn base_config(vault_root: &std::path::Path) -> Config {
    let mut sources = std::collections::BTreeMap::new();
    sources.insert(
        "test-source".to_string(),
        SourceConfig {
            source_type: SourceType::Gmail,
            enabled: true,
            schedule: None,
            params: serde_json::Value::Object(Default::default()),
            labels: vec!["test".into()],
            extract_concepts: true,
            track_personal: false,
        },
    );

    Config {
        vault: VaultConfig {
            root: vault_root.to_string_lossy().into(),
            dirs: VaultDirs::default(),
            timezone: Some("UTC".into()),
        },
        identity: Identity {
            name: "Test User".into(),
            email: "test@example.com".into(),
            slack_id: None,
            jira_id: None,
        },
        sources,
        dedup: DedupConfig::default(),
        labels: LabelConfig::default(),
        performance: PerformanceConfig::default(),
        concepts: Default::default(),
        synthesis: SynthesisConfig::default(),
        llm: Default::default(),
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
    Arc::new(PipelineContext::new(&template_dir(), llm, config).unwrap())
}

#[tokio::test]
async fn concept_pages_written_with_merge() {
    let dir = TempDir::new().unwrap();
    let vault = dir.path();
    let config = base_config(vault);

    let concepts = vec![ExtractedConcept {
        name: "Claude Code".into(),
        slug: "claude-code".into(),
        confidence: Confidence::Extracted,
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
    let concept_output = result
        .concept_pages
        .iter()
        .find(|o| o.path.to_string().contains("wiki/concepts/claude-code"))
        .expect("concept page must be in outputs");

    let writer = wi_vault::VaultWriter::new(vault);
    for out in result.daily_pages.iter().chain(result.concept_pages.iter()) {
        writer
            .write_page(out.path.as_ref(), &out.content)
            .await
            .unwrap();
    }

    // Re-ingest on a different date — should merge into existing concept page
    let ts2: jiff::Timestamp = "2026-05-24T10:00:00Z".parse().unwrap();
    let items2 = vec![raw_item("Anthropic releases v2", "...", "MSG-2", ts2)];
    let result2 = pipeline
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

    let concept_output2 = result2
        .concept_pages
        .iter()
        .find(|o| o.path.to_string().contains("claude-code"))
        .unwrap();

    assert!(
        concept_output2.content.contains("mention_count: 2"),
        "expected merged count 2, content was:\n{}",
        concept_output2.content
    );
    assert!(
        concept_output2.content.contains("2026-05-23"),
        "should retain first_seen reference"
    );
    assert!(
        concept_output2.content.contains("2026-05-24"),
        "should add second source reference"
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
    assert!(result.concept_pages.is_empty());
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

    let writer = wi_vault::VaultWriter::new(vault);
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
async fn queue_mode_emits_jsonl_tasks_with_targets() {
    use wi_llm::QueueLlmClient;

    let dir = TempDir::new().unwrap();
    let vault = dir.path();
    let mut config = base_config(vault);
    config
        .sources
        .get_mut("test-source")
        .unwrap()
        .extract_concepts = true;

    let queue_dir = vault.join(".wiki-ingest").join("queue");
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
async fn synthesis_defaults_to_enabled_sources_when_include_empty() {
    let dir = TempDir::new().unwrap();
    let vault = dir.path();
    let config = base_config(vault);

    let llm: Arc<dyn LlmClient> = Arc::new(NoopLlmClient);
    let ctx = make_ctx(&config, llm);
    let synth = Synthesizer::new(vault, ctx, &config);

    let synthesis = synth
        .weekly_synthesis(jiff::civil::date(2026, 5, 23))
        .await
        .unwrap();
    let personal = synth
        .weekly_personal(jiff::civil::date(2026, 5, 23))
        .await
        .unwrap();
    // No pages on disk → both None; the test verifies Synthesizer didn't refuse to run
    // due to empty include_sources.
    assert!(synthesis.is_none() && personal.is_none());
}
