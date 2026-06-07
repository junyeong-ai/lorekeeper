use std::sync::Arc;

use tempfile::TempDir;

use lk_core::concept::ExtractedConcept;
use lk_core::config::{
    Config, Identity, PerformanceConfig, SourceConfig, SourceType, SynthesisConfig, VaultConfig,
    VaultDirs,
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
            params: serde_json::Value::Object(Default::default()),
            classify: Default::default(),
            labels: vec!["test".into()],
            extract_concepts: true,
            focus: None,
            track_personal: false,
        },
    );

    Config {
        ingest: Default::default(),
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
        },
        sources,
        performance: PerformanceConfig::default(),
        synthesis: SynthesisConfig::default(),
        llm: Default::default(),
        concepts: Default::default(),
        graph: Default::default(),
        maintenance: Default::default(),
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
        is_self: false,
        metadata: serde_json::Value::Null,
    }
}

fn make_ctx(config: &Config, llm: Arc<dyn LlmClient>) -> Arc<PipelineContext> {
    Arc::new(PipelineContext::new(None, llm, config).unwrap())
}

/// A `today` far enough ahead that no test event is a forecast, so the realized/forecast
/// gate never fires in tests that don't deliberately exercise it.
fn far_future() -> jiff::civil::Date {
    jiff::civil::date(2099, 1, 1)
}

#[tokio::test]
async fn concept_pages_written_with_merge() {
    let dir = TempDir::new().unwrap();
    let vault = dir.path();
    let config = base_config(vault);

    let concepts = vec![ExtractedConcept {
        name: "Claude Code".into(),
        category: None,
    }];

    let llm: Arc<dyn LlmClient> = Arc::new(MockLlmClient::with_concepts(concepts));
    let ctx = make_ctx(&config, llm);
    let mut pipeline = Pipeline::new(vault, ctx);

    let ts: jiff::Timestamp = "2026-05-23T10:00:00Z".parse().unwrap();
    let items = vec![raw_item("Anthropic releases new model", "...", "MSG-1", ts)];

    let options = IngestOptions {
        target_date: None,
        today: far_future(),
        dry_run: false,
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
    // on-disk concept page (exercising the created/updated round-trip).
    drop(pipeline);
    let llm2: Arc<dyn LlmClient> = Arc::new(MockLlmClient::with_concepts(vec![ExtractedConcept {
        name: "Claude Code".into(),
        category: None,
    }]));
    let mut pipeline2 = Pipeline::new(vault, make_ctx(&config, llm2));
    let ts2: jiff::Timestamp = "2026-05-24T10:00:00Z".parse().unwrap();
    let items2 = vec![raw_item("Anthropic releases v2", "...", "MSG-2", ts2)];
    pipeline2
        .plan(
            "test-source",
            config.sources.get("test-source").unwrap(),
            items2,
            &IngestOptions {
                target_date: None,
                today: far_future(),
                dry_run: false,
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
        concept_output2.content.contains("source_count: 0"),
        "ingest never counts citations — backlinks-sync is the sole owner of \
         source_count and re-derives it from the wikilink graph:\n{}",
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
    let mut pipeline = Pipeline::new(vault, ctx);

    // 2026-05-22 23:00 UTC == 2026-05-23 08:00 KST
    let ts: jiff::Timestamp = "2026-05-22T23:00:00Z".parse().unwrap();
    let items = vec![raw_item("KST morning email", "body", "M1", ts)];

    let result = pipeline
        .plan(
            "test-source",
            config.sources.get("test-source").unwrap(),
            items,
            &IngestOptions {
                target_date: None,
                today: far_future(),
                dry_run: false,
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
    let mut pipeline = Pipeline::new(vault, ctx);

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
                target_date: Some(jiff::civil::date(2026, 5, 23)),
                today: far_future(),
                dry_run: false,
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
    let mut pipeline = Pipeline::new(vault, ctx);

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
                target_date: None,
                today: far_future(),
                dry_run: false,
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
    // must merge into ONE page — not be overwritten by whichever source is written last.
    let dir = TempDir::new().unwrap();
    let vault = dir.path();
    let mut config = base_config(vault);
    config.sources.insert(
        "second-source".to_string(),
        SourceConfig {
            source_type: SourceType::Gmail,
            enabled: true,
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
        category: None,
    }]));
    let mut pipeline = Pipeline::new(vault, make_ctx(&config, llm));

    let ts: jiff::Timestamp = "2026-05-23T10:00:00Z".parse().unwrap();
    let opts = IngestOptions {
        target_date: None,
        today: far_future(),
        dry_run: false,
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
    // Citation counting belongs to `backlinks-sync` (covered in lk-graph), which
    // re-derives both `source_count` and the `## 출처` ref list from the wikilink
    // graph. Ingest writes neither — the merged page leaves the count at 0.
    assert!(
        shared.content.contains("source_count: 0"),
        "ingest must not count citations; backlinks-sync owns source_count:\n{}",
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
    let mut pipeline = Pipeline::new(vault, ctx);

    let ts: jiff::Timestamp = "2026-05-23T10:00:00Z".parse().unwrap();
    let items = vec![raw_item("Subject", "Body", "M1", ts)];

    let result = pipeline
        .plan(
            "test-source",
            config.sources.get("test-source").unwrap(),
            items,
            &IngestOptions {
                target_date: None,
                today: far_future(),
                dry_run: false,
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
    let mut pipeline = Pipeline::new(vault, ctx);
    let sc = config.sources.get("test-source").unwrap().clone();

    let ts: jiff::Timestamp = "2026-05-23T10:00:00Z".parse().unwrap();
    let make_items = || vec![raw_item("Subject", "Body", "M1", ts)];

    let opts = IngestOptions {
        target_date: None,
        today: far_future(),
        dry_run: false,
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

    // The run is idempotent: a daily page is a projection of its event log, re-rendered
    // in full each run. A write that failed is simply reproduced on the next plan.
    let result2 = pipeline
        .plan("test-source", &sc, make_items(), &opts)
        .await
        .unwrap();
    assert_eq!(
        result2.events.len(),
        1,
        "re-plan reproduces the event for retry — the page is a materialized view"
    );
}

#[tokio::test]
async fn re_plan_reproduces_the_same_events_idempotently() {
    // No persisted dedup: re-planning the same window always yields the same events, so a
    // deleted page self-heals and a re-run is byte-identical.
    let dir = TempDir::new().unwrap();
    let vault = dir.path();
    let config = base_config(vault);
    let llm: Arc<dyn LlmClient> = Arc::new(NoopLlmClient);
    let ctx = make_ctx(&config, llm);
    let mut pipeline = Pipeline::new(vault, ctx);
    let sc = config.sources.get("test-source").unwrap().clone();

    let ts: jiff::Timestamp = "2026-05-23T10:00:00Z".parse().unwrap();
    let items = || vec![raw_item("Subject", "Body", "M1", ts)];
    let opts = IngestOptions {
        target_date: None,
        today: far_future(),
        dry_run: false,
    };

    let r1 = pipeline
        .plan("test-source", &sc, items(), &opts)
        .await
        .unwrap();
    assert_eq!(r1.events.len(), 1);

    let r2 = pipeline
        .plan("test-source", &sc, items(), &opts)
        .await
        .unwrap();
    assert_eq!(r2.events.len(), 1, "re-plan reproduces the event");
    assert_eq!(
        r1.daily_pages[0].content, r2.daily_pages[0].content,
        "re-render is byte-identical"
    );
}

#[tokio::test]
async fn intra_batch_duplicates_collapse_in_one_plan() {
    // The literal same item surfaced twice in one fetch (same external_id → same EventId)
    // is one observation. Distinct ids survive — even when title/body match, since only the
    // EventId is a merge signal.
    let dir = TempDir::new().unwrap();
    let vault = dir.path();
    let config = base_config(vault);
    let llm: Arc<dyn LlmClient> = Arc::new(NoopLlmClient);
    let ctx = make_ctx(&config, llm);
    let mut pipeline = Pipeline::new(vault, ctx);
    let sc = config.sources.get("test-source").unwrap().clone();

    let ts: jiff::Timestamp = "2026-05-23T10:00:00Z".parse().unwrap();
    // Same external_id twice (pagination overlap) collapses; a same-titled item with a
    // DISTINCT id is a distinct observation and survives.
    let dup_a = raw_item("First", "Body", "M1", ts);
    let dup_b = raw_item("First", "Body", "M1", ts);
    let other = raw_item("First", "Body", "M2", ts);
    let opts = IngestOptions {
        target_date: None,
        today: far_future(),
        dry_run: false,
    };

    let r = pipeline
        .plan("test-source", &sc, vec![dup_a, dup_b, other], &opts)
        .await
        .unwrap();
    assert_eq!(
        r.events.len(),
        2,
        "same-id pair collapses, distinct-id item survives"
    );
}

#[tokio::test]
async fn daily_page_accumulates_across_feed_depletion() {
    // The core streaming fix: an item observed on day N must survive a later re-render
    // where the fetch no longer returns it (an RSS item scrolled out of the feed). The
    // per-date event log makes the page a projection that only ever grows.
    let dir = TempDir::new().unwrap();
    let vault = dir.path();
    let config = base_config(vault);
    let mut sc = config.sources.get("test-source").unwrap().clone();
    sc.source_type = SourceType::Rss; // streaming → projects from the event log
    let ts: jiff::Timestamp = "2026-05-23T10:00:00Z".parse().unwrap();
    let opts = IngestOptions {
        target_date: None,
        today: far_future(),
        dry_run: false,
    };

    // Run 1: the feed carries A and B.
    let mut p1 = Pipeline::new(vault, make_ctx(&config, Arc::new(NoopLlmClient)));
    let r1 = p1
        .plan(
            "test-source",
            &sc,
            vec![raw_item("A", "", "A", ts), raw_item("B", "", "B", ts)],
            &opts,
        )
        .await
        .unwrap();
    assert_eq!(r1.events.len(), 2);
    drop(p1);

    // Run 2: the feed now returns ONLY A (B scrolled off). The page must still carry B.
    let mut p2 = Pipeline::new(vault, make_ctx(&config, Arc::new(NoopLlmClient)));
    let r2 = p2
        .plan("test-source", &sc, vec![raw_item("A", "", "A", ts)], &opts)
        .await
        .unwrap();
    assert_eq!(
        r2.events.len(),
        2,
        "B is preserved from the event log, not depleted by the partial fetch"
    );
    assert!(r2.events.iter().any(|e| e.title == "B"));
}

#[tokio::test]
async fn dry_run_does_not_write_the_event_log() {
    let dir = TempDir::new().unwrap();
    let vault = dir.path();
    let config = base_config(vault);
    let mut sc = config.sources.get("test-source").unwrap().clone();
    sc.source_type = SourceType::Rss; // streaming → would write the log if not dry-run
    let ts: jiff::Timestamp = "2026-05-23T10:00:00Z".parse().unwrap();

    let mut pipeline = Pipeline::new(vault, make_ctx(&config, Arc::new(NoopLlmClient)));
    pipeline
        .plan(
            "test-source",
            &sc,
            vec![raw_item("A", "", "A", ts)],
            &IngestOptions {
                target_date: None,
                today: far_future(),
                dry_run: true,
            },
        )
        .await
        .unwrap();

    assert!(
        !vault.join(".lorekeeper").join("events").exists(),
        "a dry-run must not create the event log"
    );
}

#[tokio::test]
async fn queue_mode_emits_jsonl_tasks_with_targets() {
    let dir = TempDir::new().unwrap();
    let vault = dir.path();
    let mut config = base_config(vault);
    config
        .sources
        .get_mut("test-source")
        .unwrap()
        .extract_concepts = true;

    let queue_dir = vault.join(".lorekeeper").join("queue");
    let llm: Arc<dyn LlmClient> = Arc::new(lk_queue::QueueLlmClient::new(queue_dir.clone()));
    let ctx = make_ctx(&config, llm.clone());
    let mut pipeline = Pipeline::new(vault, ctx);
    let sc = config.sources.get("test-source").unwrap();

    let ts: jiff::Timestamp = "2026-05-23T10:00:00Z".parse().unwrap();
    let items = vec![raw_item("Test event", "Body", "M1", ts)];

    let result = pipeline
        .plan(
            "test-source",
            sc,
            items,
            &IngestOptions {
                target_date: None,
                today: far_future(),
                dry_run: false,
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
    let synth = Synthesizer::new(vault, make_ctx(&config, llm.clone()), &config, far_future());
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
    let synth = Synthesizer::new(vault, make_ctx(&config, llm), &config, far_future());
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
async fn empty_weekly_themes_is_cached_not_re_enqueued() {
    // Themes extraction can legitimately find no cross-source theme. A stamped
    // `themes_done` marks the empty section done; a re-render must NOT re-enqueue and
    // must carry the marker through weekly-synthesis.md.jinja (guards the template
    // emission). Were completion inferred from a non-empty body it would re-enqueue
    // every ingest forever.
    let dir = TempDir::new().unwrap();
    let vault = dir.path();
    let daily = vault.join("daily").join("test-source");
    std::fs::create_dir_all(&daily).unwrap();
    std::fs::write(
        daily.join("2026-05-20.md"),
        "---\nid: test-source-2026-05-20\n---\n\n# News\n\nbody\n",
    )
    .unwrap();
    let mut config = base_config(vault);
    config.synthesis.weekly.include_sources = vec!["test-source".into()];
    let week = jiff::civil::date(2026, 5, 23);
    let queue_dir = vault.join(".lorekeeper").join("queue");
    let themes_heading = config.vault.locale().strings().key_themes_this_week;

    let llm1: Arc<dyn LlmClient> = Arc::new(lk_queue::QueueLlmClient::new(queue_dir.clone()));
    let synth1 = Synthesizer::new(
        vault,
        make_ctx(&config, llm1.clone()),
        &config,
        far_future(),
    );
    let out1 = synth1
        .try_weekly_synthesis(week)
        .await
        .unwrap()
        .expect("themes page produced");
    let page_path = vault.join(out1.path.as_ref());
    std::fs::create_dir_all(page_path.parent().unwrap()).unwrap();
    std::fs::write(&page_path, &out1.content).unwrap();
    llm1.flush().await.unwrap();

    // Simulate the skill finding NO theme: leave the section empty, stamp themes_done.
    let themes_hash = out1
        .content
        .lines()
        .find_map(|l| l.trim().strip_prefix("themes: "))
        .map(|v| v.trim().trim_matches('"').to_string())
        .expect("pipeline pre-stamps themes hash");
    let stamped = out1.content.replace(
        &format!("themes: \"{themes_hash}\""),
        &format!("themes: \"{themes_hash}\"\n  themes_done: \"{themes_hash}\""),
    );
    std::fs::write(&page_path, &stamped).unwrap();
    for entry in std::fs::read_dir(&queue_dir).unwrap().flatten() {
        if entry.path().extension().and_then(|s| s.to_str()) == Some("jsonl") {
            std::fs::remove_file(entry.path()).unwrap();
        }
    }

    let llm2: Arc<dyn LlmClient> = Arc::new(lk_queue::QueueLlmClient::new(queue_dir.clone()));
    let synth2 = Synthesizer::new(
        vault,
        make_ctx(&config, llm2.clone()),
        &config,
        far_future(),
    );
    let out2 = synth2
        .try_weekly_synthesis(week)
        .await
        .unwrap()
        .expect("themes page still produced");
    llm2.flush().await.unwrap();

    assert_eq!(
        std::fs::read_dir(&queue_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("jsonl"))
            .count(),
        0,
        "an empty-but-done themes section must stay cached, not re-enqueue",
    );
    assert!(
        out2.content.contains("themes_done"),
        "the re-rendered themes page must carry themes_done through the template:\n{}",
        out2.content
    );
    let _ = themes_heading;
}

#[tokio::test]
async fn performance_enabled_gates_review_narratives() {
    let dir = TempDir::new().unwrap();
    let vault = dir.path();

    // A work-log page exists in the target ISO week.
    let work_log = vault.join("me").join(lk_core::vault_path::WORK_LOG_SUBDIR);
    std::fs::create_dir_all(&work_log).unwrap();
    std::fs::write(
        work_log.join("2026-05-20.md"),
        "---\nid: work-log-2026-05-20\ncategories: [project-delivery]\n---\n\n# Work\n\nbody\n",
    )
    .unwrap();

    let llm: Arc<dyn LlmClient> = Arc::new(NoopLlmClient);

    // performance.enabled: true → the personal weekly review is produced.
    let config = base_config(vault);
    let synth = Synthesizer::new(vault, make_ctx(&config, llm.clone()), &config, far_future());
    assert!(
        synth
            .try_weekly_review(jiff::civil::date(2026, 5, 23))
            .await
            .unwrap()
            .is_some(),
        "personal review should be produced when performance is enabled"
    );

    // performance.enabled: false → no review, even though the work-log page exists.
    let mut config = base_config(vault);
    config.performance.enabled = false;
    let synth = Synthesizer::new(vault, make_ctx(&config, llm), &config, far_future());
    assert!(
        synth
            .try_weekly_review(jiff::civil::date(2026, 5, 23))
            .await
            .unwrap()
            .is_none(),
        "disabling performance must suppress personal reviews"
    );
}

#[tokio::test]
async fn synthesis_excludes_pages_dated_after_today() {
    // Defense at the read boundary: a performance review must reflect only realized days,
    // so a work-log page dated after `today` (a forecast day that hasn't happened) is never
    // summed into a review — even if it somehow exists on disk.
    let dir = TempDir::new().unwrap();
    let vault = dir.path();

    // The ONLY work-log page in the target ISO week (Mon 2026-05-18 .. Sun 2026-05-24) is
    // dated Sunday 2026-05-24.
    let work_log = vault.join("me").join(lk_core::vault_path::WORK_LOG_SUBDIR);
    std::fs::create_dir_all(&work_log).unwrap();
    std::fs::write(
        work_log.join("2026-05-24.md"),
        "---\nid: work-log-2026-05-24\ncategories: [project-delivery]\n---\n\n# Work\n\nbody\n",
    )
    .unwrap();

    let llm: Arc<dyn LlmClient> = Arc::new(NoopLlmClient);
    let config = base_config(vault);

    // today = 2026-05-20: the 05-24 page is a forecast day, so the week has no realized
    // work-log and the review produces nothing.
    let synth = Synthesizer::new(
        vault,
        make_ctx(&config, llm.clone()),
        &config,
        jiff::civil::date(2026, 5, 20),
    );
    assert!(
        synth
            .try_weekly_review(jiff::civil::date(2026, 5, 23))
            .await
            .unwrap()
            .is_none(),
        "a work-log page dated after today must not feed a performance review"
    );

    // Once today has moved past that date the same page is realized and the review appears.
    let synth = Synthesizer::new(vault, make_ctx(&config, llm), &config, far_future());
    assert!(
        synth
            .try_weekly_review(jiff::civil::date(2026, 5, 23))
            .await
            .unwrap()
            .is_some(),
        "once realized, the same work-log page feeds the review"
    );
}

#[tokio::test]
async fn synthesis_page_is_a_materialized_view() {
    let dir = TempDir::new().unwrap();
    let vault = dir.path();

    let work_log = vault.join("me").join(lk_core::vault_path::WORK_LOG_SUBDIR);
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
    let llm1: Arc<dyn LlmClient> = Arc::new(lk_queue::QueueLlmClient::new(queue_dir.clone()));
    let synth1 = Synthesizer::new(
        vault,
        make_ctx(&config, llm1.clone()),
        &config,
        far_future(),
    );
    let out1 = synth1
        .try_weekly_review(week_date)
        .await
        .unwrap()
        .expect("weekly review produced");
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

    // Simulate /lore-process filling the narrative section and stamping its
    // completion marker (narratives are marker-signalled like every LLM section).
    // The pipeline already pre-stamped `narrative: "<hash>"`; stamp `narrative_done`
    // equal to it so the next ingest is a cache hit.
    let narrative_hash = out1
        .content
        .lines()
        .find_map(|l| l.trim().strip_prefix("narrative: "))
        .map(|v| v.trim().trim_matches('"').to_string())
        .expect("pipeline pre-stamps narrative hash");
    let narrative_filled = lk_vault::replace_section(
        &out1.content,
        strings.key_summary,
        "REAL-SYNTHESIS-NARRATIVE",
    );
    let filled = narrative_filled.replace(
        &format!("narrative: \"{narrative_hash}\""),
        &format!("narrative: \"{narrative_hash}\"\n  narrative_done: \"{narrative_hash}\""),
    );
    std::fs::write(&page_path, &filled).unwrap();
    for entry in std::fs::read_dir(&queue_dir).unwrap().flatten() {
        if entry.path().extension().and_then(|s| s.to_str()) == Some("jsonl") {
            std::fs::remove_file(entry.path()).unwrap();
        }
    }

    // Second run, identical input: cache hit → no new task, narrative preserved.
    let llm2: Arc<dyn LlmClient> = Arc::new(lk_queue::QueueLlmClient::new(queue_dir.clone()));
    let synth2 = Synthesizer::new(
        vault,
        make_ctx(&config, llm2.clone()),
        &config,
        far_future(),
    );
    let out2 = synth2
        .try_weekly_review(week_date)
        .await
        .unwrap()
        .expect("weekly review produced on re-run");
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
async fn quarterly_review_includes_latest_unsummarized_month_via_weekly_fallback() {
    use lk_queue::QueueLlmClient;

    // Q2 2026 = Apr/May/Jun. Apr and May have monthly reviews; Jun does NOT (it's the
    // current month, not yet summarized) but has a weekly review. The per-month
    // fallback must pull June from its weekly review — a whole-quarter fallback would
    // silently omit June because monthly reviews already exist for the other two months.
    let dir = TempDir::new().unwrap();
    let vault = dir.path();
    let config = base_config(vault);
    let summary = lk_core::i18n::Locale::Ko.strings().key_summary;

    let monthly = vault.join("me").join("monthly");
    std::fs::create_dir_all(&monthly).unwrap();
    for (m, marker) in [("04", "APR-MONTHLY"), ("05", "MAY-MONTHLY")] {
        std::fs::write(
            monthly.join(format!("2026-{m}.md")),
            format!("---\nid: m-2026-{m}\n---\n\n## {summary}\n\n{marker}\n"),
        )
        .unwrap();
    }

    // A weekly review inside June (ISO week of Jun 15, 2026).
    let jun = jiff::civil::date(2026, 6, 15).iso_week_date();
    let weekly = vault.join("me").join("weekly");
    std::fs::create_dir_all(&weekly).unwrap();
    std::fs::write(
        weekly.join(format!("{}-W{:02}.md", jun.year(), jun.week())),
        format!("---\nid: w\n---\n\n## {summary}\n\nJUNE-WEEKLY-MARKER\n"),
    )
    .unwrap();

    let llm: Arc<dyn LlmClient> =
        Arc::new(QueueLlmClient::new(vault.join(".lorekeeper").join("queue")));
    let synth = Synthesizer::new(vault, make_ctx(&config, llm), &config, far_future());
    let out = synth
        .try_quarterly_review(2026, 2)
        .await
        .unwrap()
        .expect("quarterly review produced");

    assert!(
        out.content.contains("### 2026-06"),
        "June must appear in the quarterly breakdown via weekly fallback:\n{}",
        out.content
    );
    assert!(
        out.content.contains("JUNE-WEEKLY-MARKER"),
        "June's weekly narrative must be the fallback content:\n{}",
        out.content
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
        timestamp: jiff::Timestamp::UNIX_EPOCH,
        date: jiff::civil::date(2026, 5, 20),
        title: "did a thing".into(),
        body: "details".into(),
        url: None,
        author: None,
        labels: vec!["personal".into()],
        category: None,
        performance_category: None,
        is_self: true,
        is_personal: true,
        metadata: serde_json::Value::Null,
    };

    let llm: Arc<dyn LlmClient> = Arc::new(NoopLlmClient);

    // Enabled → the personal event yields a work-log page.
    let config = base_config(vault);
    let pipeline = Pipeline::new(vault, make_ctx(&config, llm.clone()));
    assert!(
        !pipeline
            .render_work_log(std::slice::from_ref(&event), jiff::civil::date(2026, 6, 4))
            .await
            .unwrap()
            .is_empty(),
        "work-log should be produced when performance is enabled"
    );
    drop(pipeline); // one Pipeline owns one ingest run; finish it before reopening

    // Disabled → no work-log at the mechanism boundary, regardless of caller.
    let mut config = base_config(vault);
    config.performance.enabled = false;
    let pipeline = Pipeline::new(vault, make_ctx(&config, llm));
    assert!(
        pipeline
            .render_work_log(std::slice::from_ref(&event), jiff::civil::date(2026, 6, 4))
            .await
            .unwrap()
            .is_empty(),
        "disabling performance must suppress work-log generation"
    );
}

#[tokio::test]
async fn work_log_excludes_future_dated_contribution() {
    let dir = TempDir::new().unwrap();
    let vault = dir.path();
    let today = jiff::civil::date(2026, 6, 4);

    let make_event = |date: jiff::civil::Date, title: &str| lk_core::event::Event {
        id: lk_core::event::EventId::new("my-schedule", date, title),
        source_id: "my-schedule".into(),
        source_type: SourceType::GoogleCalendar,
        timestamp: jiff::Timestamp::UNIX_EPOCH,
        date,
        title: title.into(),
        body: "details".into(),
        url: None,
        author: None,
        labels: vec!["personal".into()],
        category: None,
        performance_category: None,
        is_self: true,
        is_personal: true,
        metadata: serde_json::Value::Null,
    };

    let llm: Arc<dyn LlmClient> = Arc::new(NoopLlmClient);
    let config = base_config(vault);
    let pipeline = Pipeline::new(vault, make_ctx(&config, llm));

    // A future-dated personal event (a calendar look-ahead meeting) is a commitment, not
    // work performed — it must never produce a work-log page.
    let future = make_event(jiff::civil::date(2026, 6, 5), "tomorrow's meeting");
    assert!(
        pipeline
            .render_work_log(std::slice::from_ref(&future), today)
            .await
            .unwrap()
            .is_empty(),
        "a future-dated event must not produce a work-log page"
    );

    // Mixed batch: only the date <= today entry yields a page; the future one is dropped.
    let past = make_event(today, "today's standup");
    let pages = pipeline
        .render_work_log(&[past, future], today)
        .await
        .unwrap();
    assert_eq!(
        pages.len(),
        1,
        "only the non-future date produces a work-log"
    );
    assert!(
        pages[0].path.to_string().contains("2026-06-04"),
        "the produced page is for today, not the future date: {}",
        pages[0].path
    );
}

#[tokio::test]
async fn gmail_daily_page_renders_category_highlights() {
    // Gmail's daily page surfaces an email-triage highlight section for a curated set of
    // categories ABOVE the full event list. A `classify` rule routes a matching event into
    // a highlight bucket; the event must appear BOTH in its highlight section AND in the
    // full Key Events list — the buckets are an additive highlight, never a replacement,
    // so an event is never hidden by its category.
    let dir = TempDir::new().unwrap();
    let vault = dir.path();
    let mut config = base_config(vault);
    config.sources.get_mut("test-source").unwrap().classify = vec![lk_core::config::ClassifyRule {
        category: "action_required".into(),
        keywords: vec!["deadline".into()],
        performance_category: None,
    }];

    let strings = config.vault.locale().strings();
    let action_heading = strings.action_required;
    let key_events_heading = strings.key_events;

    let llm: Arc<dyn LlmClient> = Arc::new(NoopLlmClient);
    let mut pipeline = Pipeline::new(vault, make_ctx(&config, llm));

    let ts: jiff::Timestamp = "2026-05-23T10:00:00Z".parse().unwrap();
    let result = pipeline
        .plan(
            "test-source",
            config.sources.get("test-source").unwrap(),
            vec![raw_item(
                "Project deadline moved",
                "Ship by Friday.",
                "MSG-1",
                ts,
            )],
            &IngestOptions {
                target_date: None,
                today: far_future(),
                dry_run: false,
            },
        )
        .await
        .unwrap();

    let content = result
        .daily_pages
        .first()
        .map(|p| p.content.as_str())
        .expect("gmail daily page rendered");

    assert!(
        content.contains(&format!("## {action_heading}")),
        "the action_required highlight section must render for a Gmail page:\n{content}"
    );
    assert!(
        content.contains("action_required: 1"),
        "the frontmatter action_required count must reflect the one bucketed event:\n{content}"
    );
    assert!(
        content.contains("Project deadline moved"),
        "the bucketed event must appear in the highlight:\n{content}"
    );
    assert!(
        content.contains(&format!("## {key_events_heading}")),
        "the full Key Events list must always render regardless of category:\n{content}"
    );
}

#[tokio::test]
async fn forecast_date_is_not_materialized() {
    // The vault is realized-only: an event dated after `today` is a calendar look-ahead
    // FORECAST — not knowledge yet — so the pipeline materializes NO page, no concepts, and
    // no citations for it. It becomes a page only once its date arrives. A realized event in
    // the same batch is unaffected, so a forecast can never suppress real work.
    let dir = TempDir::new().unwrap();
    let vault = dir.path();
    let config = base_config(vault); // test-source: Gmail, extract_concepts: true

    // The mock WOULD return a concept for any extraction — proving the date skip, not the
    // LLM, is what suppresses the forecast date.
    let llm: Arc<dyn LlmClient> = Arc::new(MockLlmClient::with_concepts(vec![ExtractedConcept {
        name: "Future Topic".into(),
        category: None,
    }]));
    let mut pipeline = Pipeline::new(vault, make_ctx(&config, llm));

    let today = jiff::civil::date(2026, 6, 4);
    let realized_ts: jiff::Timestamp = "2026-06-04T09:00:00Z".parse().unwrap();
    let forecast_ts: jiff::Timestamp = "2026-06-06T10:00:00Z".parse().unwrap(); // two days ahead
    let result = pipeline
        .plan(
            "test-source",
            config.sources.get("test-source").unwrap(),
            vec![
                raw_item("Today standup", "notes", "MSG-NOW", realized_ts),
                raw_item(
                    "Upcoming planning meeting",
                    "agenda",
                    "MSG-FUT",
                    forecast_ts,
                ),
            ],
            &IngestOptions {
                target_date: None,
                today,
                dry_run: false,
            },
        )
        .await
        .unwrap();

    // Exactly one daily page — today's. The forecast date is never materialized.
    assert_eq!(
        result.daily_pages.len(),
        1,
        "only the realized date produces a page; the forecast date is skipped"
    );
    assert!(
        result.daily_pages[0]
            .path
            .to_string()
            .contains("2026-06-04"),
        "the produced page is today's, not the forecast date: {}",
        result.daily_pages[0].path
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
        let llm1: Arc<dyn LlmClient> = Arc::new(lk_queue::QueueLlmClient::new(queue_dir.clone()));
        let result1 = {
            let mut pipeline1 = Pipeline::new(vault, make_ctx(&config, llm1.clone()));
            let r = pipeline1
                .plan(
                    "test-source",
                    sc,
                    items.clone(),
                    &IngestOptions {
                        target_date: None,
                        today: far_future(),
                        dry_run: false,
                    },
                )
                .await
                .unwrap();
            write_to_vault(vault, &r).await;
            llm1.flush().await.unwrap();
            r
        };

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
        let llm2: Arc<dyn LlmClient> = Arc::new(lk_queue::QueueLlmClient::new(queue_dir.clone()));
        let mut pipeline2 = Pipeline::new(vault, make_ctx(&config, llm2.clone()));
        let result2 = pipeline2
            .plan(
                "test-source",
                sc,
                items,
                &IngestOptions {
                    target_date: None,
                    today: far_future(),
                    dry_run: false,
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
    async fn work_log_topic_synthesis_is_a_materialized_view() {
        // The work-log's topic section follows the same cache discipline as a daily
        // page: an unchanged personal-event set re-renders the page WITHOUT enqueueing
        // a new synthesis task, and the skill-authored topic body survives the
        // re-render. Daily and synthesis pages already pin this; the work-log is the
        // third page family with an LLM-owned section.
        let dir = TempDir::new().unwrap();
        let vault = dir.path();
        let config = base_config(vault);
        let today = jiff::civil::date(2026, 6, 4);
        let topic_heading = config.vault.locale().strings().topic_summary;

        let event = lk_core::event::Event {
            id: lk_core::event::EventId::new("test-source", jiff::civil::date(2026, 5, 20), "x"),
            source_id: "test-source".into(),
            source_type: SourceType::Gmail,
            timestamp: jiff::Timestamp::UNIX_EPOCH,
            date: jiff::civil::date(2026, 5, 20),
            title: "did a thing".into(),
            body: "details".into(),
            url: None,
            author: None,
            labels: vec!["personal".into()],
            category: None,
            performance_category: None,
            is_self: true,
            is_personal: true,
            metadata: serde_json::Value::Null,
        };

        let queue_dir = vault.join(".lorekeeper").join("queue");

        // First run: the topic-synthesis task is enqueued and the rendered page
        // pre-stamps the current input hash.
        let llm1: Arc<dyn LlmClient> = Arc::new(lk_queue::QueueLlmClient::new(queue_dir.clone()));
        let pages1 = {
            let pipeline = Pipeline::new(vault, make_ctx(&config, llm1.clone()));
            pipeline
                .render_work_log(std::slice::from_ref(&event), today)
                .await
                .unwrap()
        };
        assert_eq!(pages1.len(), 1, "one work-log page for the event's date");
        llm1.flush().await.unwrap();
        assert!(
            queue_task_kinds_for_source(&queue_dir).contains("summarize"),
            "first run enqueues the topic synthesis"
        );

        // Persist the page and simulate `/lore-process` filling the topic section and
        // stamping its completion marker (topic synthesis is marker-signalled).
        let abs = vault.join(pages1[0].path.as_ref());
        tokio::fs::create_dir_all(abs.parent().unwrap())
            .await
            .unwrap();
        let mut filled =
            lk_vault::replace_section(&pages1[0].content, topic_heading, "REAL-TOPIC-BODY");
        if let Some(h) = task_hash_for_page(&queue_dir, &pages1[0].path.to_string(), "summarize") {
            filled = stamp_completion(&filled, "topic_summary", &h);
        }
        tokio::fs::write(&abs, filled).await.unwrap();
        clear_queue_dir(&queue_dir).await;

        // Second run, same events: cache hit — nothing enqueued, body preserved.
        let llm2: Arc<dyn LlmClient> = Arc::new(lk_queue::QueueLlmClient::new(queue_dir.clone()));
        let pages2 = {
            let pipeline = Pipeline::new(vault, make_ctx(&config, llm2.clone()));
            pipeline
                .render_work_log(std::slice::from_ref(&event), today)
                .await
                .unwrap()
        };
        llm2.flush().await.unwrap();
        let kinds = queue_task_kinds_for_source(&queue_dir);
        assert!(
            kinds.is_empty(),
            "an unchanged event set must not re-enqueue the topic synthesis; got {kinds:?}"
        );
        assert!(
            pages2[0].content.contains("REAL-TOPIC-BODY"),
            "the skill-authored topic body must survive the re-render:\n{}",
            pages2[0].content
        );
        // The marker must round-trip through the work-log template, or an empty-but-done
        // topic summary (a day of only trivial events) would re-enqueue forever.
        assert!(
            pages2[0].content.contains("topic_summary_done"),
            "the re-rendered work-log must carry topic_summary_done through:\n{}",
            pages2[0].content
        );
    }

    #[tokio::test]
    async fn empty_work_log_topic_summary_is_cached_not_re_enqueued() {
        // A day of only trivial events (calendar accepts, approvals) groups into
        // categories — so the page exists — but the skill skips them all, leaving an
        // empty topic summary. A stamped `topic_summary_done` marks it done; a re-render
        // must NOT re-enqueue. Were completion inferred from a non-empty body, the empty section would
        // re-enqueue every ingest forever.
        let dir = TempDir::new().unwrap();
        let vault = dir.path();
        let config = base_config(vault);
        let today = jiff::civil::date(2026, 6, 4);

        let event = lk_core::event::Event {
            id: lk_core::event::EventId::new("test-source", jiff::civil::date(2026, 5, 20), "x"),
            source_id: "test-source".into(),
            source_type: SourceType::Gmail,
            timestamp: jiff::Timestamp::UNIX_EPOCH,
            date: jiff::civil::date(2026, 5, 20),
            title: "calendar accept".into(),
            body: "trivial".into(),
            url: None,
            author: None,
            labels: vec!["personal".into()],
            category: None,
            performance_category: None,
            is_self: true,
            is_personal: true,
            metadata: serde_json::Value::Null,
        };

        let queue_dir = vault.join(".lorekeeper").join("queue");
        let llm1: Arc<dyn LlmClient> = Arc::new(lk_queue::QueueLlmClient::new(queue_dir.clone()));
        let pages1 = {
            let pipeline = Pipeline::new(vault, make_ctx(&config, llm1.clone()));
            pipeline
                .render_work_log(std::slice::from_ref(&event), today)
                .await
                .unwrap()
        };
        llm1.flush().await.unwrap();

        // Simulate the skill skipping every event as trivial: leave the topic section
        // EMPTY but stamp topic_summary_done.
        let abs = vault.join(pages1[0].path.as_ref());
        tokio::fs::create_dir_all(abs.parent().unwrap())
            .await
            .unwrap();
        let mut content = pages1[0].content.clone();
        if let Some(h) = task_hash_for_page(&queue_dir, &pages1[0].path.to_string(), "summarize") {
            content = stamp_completion(&content, "topic_summary", &h);
        }
        tokio::fs::write(&abs, content).await.unwrap();
        clear_queue_dir(&queue_dir).await;

        let llm2: Arc<dyn LlmClient> = Arc::new(lk_queue::QueueLlmClient::new(queue_dir.clone()));
        let _pages2 = {
            let pipeline = Pipeline::new(vault, make_ctx(&config, llm2.clone()));
            pipeline
                .render_work_log(std::slice::from_ref(&event), today)
                .await
                .unwrap()
        };
        llm2.flush().await.unwrap();
        assert!(
            !queue_task_kinds_for_source(&queue_dir).contains("summarize"),
            "an empty-but-done work-log topic summary must stay cached, not re-enqueue",
        );
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
        let llm1: Arc<dyn LlmClient> = Arc::new(lk_queue::QueueLlmClient::new(queue_dir.clone()));
        let result1 = {
            let mut pipeline1 = Pipeline::new(vault, make_ctx(&config, llm1.clone()));
            let r = pipeline1
                .plan(
                    "test-source",
                    sc,
                    initial,
                    &IngestOptions {
                        target_date: None,
                        today: far_future(),
                        dry_run: false,
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
        let llm2: Arc<dyn LlmClient> = Arc::new(lk_queue::QueueLlmClient::new(queue_dir.clone()));
        let mut pipeline2 = Pipeline::new(vault, make_ctx(&config, llm2.clone()));
        let _ = pipeline2
            .plan(
                "test-source",
                sc,
                changed,
                &IngestOptions {
                    target_date: None,
                    today: far_future(),
                    dry_run: false,
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
        let llm1: Arc<dyn LlmClient> = Arc::new(lk_queue::QueueLlmClient::new(queue_dir.clone()));
        let result1 = {
            let mut pipeline1 = Pipeline::new(vault, make_ctx(&config, llm1.clone()));
            let r = pipeline1
                .plan(
                    "test-source",
                    sc,
                    vec![raw_item("Event A", "Body A", "E1", ts)],
                    &IngestOptions {
                        target_date: None,
                        today: far_future(),
                        dry_run: false,
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
        let llm2: Arc<dyn LlmClient> = Arc::new(lk_queue::QueueLlmClient::new(queue_dir.clone()));
        let result2 = {
            let mut pipeline2 = Pipeline::new(vault, make_ctx(&config, llm2.clone()));
            let r = pipeline2
                .plan(
                    "test-source",
                    sc,
                    vec![
                        raw_item("Event A", "Body A", "E1", ts),
                        raw_item("Event B", "Body B (new!)", "E2", ts),
                    ],
                    &IngestOptions {
                        target_date: None,
                        today: far_future(),
                        dry_run: false,
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
    async fn deleting_completion_marker_forces_re_enqueue() {
        // Mechanism-free override path: deleting a section's `*_done` completion marker
        // re-queues on the next ingest, no flag needed. (Wiping the body alone does NOT,
        // by design — completion is the marker, never the body; see
        // `wiping_section_body_does_not_re_enqueue`.)
        let dir = TempDir::new().unwrap();
        let vault = dir.path();
        let config = base_config(vault);
        let sc = config.sources.get("test-source").unwrap();

        let ts: jiff::Timestamp = "2026-05-23T10:00:00Z".parse().unwrap();
        let items = vec![raw_item("Event A", "Body A", "E1", ts)];

        let queue_dir = vault.join(".lorekeeper").join("queue");
        let llm1: Arc<dyn LlmClient> = Arc::new(lk_queue::QueueLlmClient::new(queue_dir.clone()));
        let result1 = {
            let mut pipeline1 = Pipeline::new(vault, make_ctx(&config, llm1.clone()));
            let r = pipeline1
                .plan(
                    "test-source",
                    sc,
                    items.clone(),
                    &IngestOptions {
                        target_date: None,
                        today: far_future(),
                        dry_run: false,
                    },
                )
                .await
                .unwrap();
            write_to_vault(vault, &r).await;
            llm1.flush().await.unwrap();
            r
        };
        fill_llm_sections_with_dummy_bodies(vault, &result1).await;

        // Delete the summary completion marker line (body and input hash stay).
        let page_path = vault.join(result1.daily_pages[0].path.as_ref());
        let body = tokio::fs::read_to_string(&page_path).await.unwrap();
        let edited: String = body
            .lines()
            .filter(|l| !l.trim_start().starts_with("summary_done:"))
            .collect::<Vec<_>>()
            .join("\n");
        tokio::fs::write(&page_path, edited).await.unwrap();

        clear_queue_dir(&queue_dir).await;

        let llm2: Arc<dyn LlmClient> = Arc::new(lk_queue::QueueLlmClient::new(queue_dir.clone()));
        let mut pipeline2 = Pipeline::new(vault, make_ctx(&config, llm2.clone()));
        let _ = pipeline2
            .plan(
                "test-source",
                sc,
                items.clone(),
                &IngestOptions {
                    target_date: None,
                    today: far_future(),
                    dry_run: false,
                },
            )
            .await
            .unwrap();
        llm2.flush().await.unwrap();
        assert!(
            queue_task_kinds_for_source(&queue_dir).contains("summarize"),
            "deleting the completion marker must trigger re-enqueue",
        );
    }

    #[tokio::test]
    async fn wiping_section_body_does_not_re_enqueue() {
        // Counterpart to the marker-deletion test: completion is tracked by the marker,
        // not the body, so blanking the body (an empty result is valid for many kinds)
        // leaves the section DONE — no re-enqueue. The user must delete the marker.
        let dir = TempDir::new().unwrap();
        let vault = dir.path();
        let config = base_config(vault);
        let sc = config.sources.get("test-source").unwrap();

        let ts: jiff::Timestamp = "2026-05-23T10:00:00Z".parse().unwrap();
        let items = vec![raw_item("Event A", "Body A", "E1", ts)];

        let queue_dir = vault.join(".lorekeeper").join("queue");
        let llm1: Arc<dyn LlmClient> = Arc::new(lk_queue::QueueLlmClient::new(queue_dir.clone()));
        let result1 = {
            let mut pipeline1 = Pipeline::new(vault, make_ctx(&config, llm1.clone()));
            let r = pipeline1
                .plan(
                    "test-source",
                    sc,
                    items.clone(),
                    &IngestOptions {
                        target_date: None,
                        today: far_future(),
                        dry_run: false,
                    },
                )
                .await
                .unwrap();
            write_to_vault(vault, &r).await;
            llm1.flush().await.unwrap();
            r
        };
        fill_llm_sections_with_dummy_bodies(vault, &result1).await;

        let strings = lk_core::i18n::Locale::Ko.strings();
        let page_path = vault.join(result1.daily_pages[0].path.as_ref());
        let body = tokio::fs::read_to_string(&page_path).await.unwrap();
        let cleared = lk_vault::replace_section(&body, strings.summary, "");
        tokio::fs::write(&page_path, cleared).await.unwrap();

        clear_queue_dir(&queue_dir).await;

        let llm2: Arc<dyn LlmClient> = Arc::new(lk_queue::QueueLlmClient::new(queue_dir.clone()));
        let mut pipeline2 = Pipeline::new(vault, make_ctx(&config, llm2.clone()));
        pipeline2
            .plan(
                "test-source",
                sc,
                items,
                &IngestOptions {
                    target_date: None,
                    today: far_future(),
                    dry_run: false,
                },
            )
            .await
            .unwrap();
        llm2.flush().await.unwrap();
        assert!(
            !queue_task_kinds_for_source(&queue_dir).contains("summarize"),
            "an emptied body with its marker intact stays done; only marker deletion re-enqueues",
        );
    }

    /// Simulate `/lore-process`: fill each LLM-owned section AND stamp its
    /// `<key>_done` completion marker (the skill owns the markers; the pipeline only
    /// pre-stamps the input keys). Each stamp must equal the enqueued task's
    /// `cache_hash` so the next ingest is a cache hit.
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
            // The pipeline pre-stamped the input keys; the skill owns the completion
            // markers. Stamp each equal to its input hash so the next ingest is a cache
            // hit (completion is uniformly marker-signalled).
            let page_path = page.path.to_string();
            if let Some(hash) = task_hash_for_page(&queue_dir, &page_path, "summarize") {
                content = stamp_completion(&content, "summary", &hash);
            }
            if let Some(hash) = task_hash_for_page(&queue_dir, &page_path, "refine-events") {
                content = stamp_completion(&content, "refine_events", &hash);
            }
            if let Some(hash) = task_hash_for_page(&queue_dir, &page_path, "extract-concepts") {
                content = stamp_completion(&content, "concepts", &hash);
            }
            tokio::fs::write(&abs, content).await.unwrap();
        }
    }

    /// The `cache_hash` of the enqueued `refine-events` task targeting `page_path`.
    fn refine_hash_for_page(queue_dir: &std::path::Path, page_path: &str) -> Option<String> {
        task_hash_for_page(queue_dir, page_path, "refine-events")
    }

    /// The `cache_hash` of the enqueued task of `kind` targeting `page_path`.
    fn task_hash_for_page(
        queue_dir: &std::path::Path,
        page_path: &str,
        kind: &str,
    ) -> Option<String> {
        let entries = std::fs::read_dir(queue_dir).ok()?;
        for entry in entries.flatten() {
            if entry.path().extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            let content = std::fs::read_to_string(entry.path()).ok()?;
            for line in content.lines() {
                let task: serde_json::Value = serde_json::from_str(line).ok()?;
                if task["kind"] == kind && task["target"]["vault_path"].as_str() == Some(page_path)
                {
                    return task["cache_hash"].as_str().map(str::to_string);
                }
            }
        }
        None
    }

    /// Simulate `/lore-process` stamping a marker-signalled task's completion field:
    /// `concepts`/`refine_events` are pre-stamped by the pipeline, and the skill sets
    /// `<key>_done` equal to that hash once it has filled the section (even when the
    /// result is empty). Without this stamp the task re-enqueues on every ingest.
    fn stamp_completion(content: &str, input_key: &str, hash: &str) -> String {
        let done_key = format!("{input_key}_done");
        content.replace(
            &format!("{input_key}: \"{hash}\""),
            &format!("{input_key}: \"{hash}\"\n  {done_key}: \"{hash}\""),
        )
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
    async fn deleting_one_completion_marker_re_enqueues_only_that_section() {
        // Marker deletion is per-section: dropping `summary_done` re-enqueues the
        // summary alone, while sections whose markers are untouched stay cached.
        let dir = TempDir::new().unwrap();
        let vault = dir.path();
        let config = base_config(vault);
        let sc = config.sources.get("test-source").unwrap();

        let ts: jiff::Timestamp = "2026-05-23T10:00:00Z".parse().unwrap();
        let items = vec![raw_item("Event A", "Body A", "E1", ts)];

        let queue_dir = vault.join(".lorekeeper").join("queue");
        let llm1: Arc<dyn LlmClient> = Arc::new(lk_queue::QueueLlmClient::new(queue_dir.clone()));
        let result1 = {
            let mut pipeline1 = Pipeline::new(vault, make_ctx(&config, llm1.clone()));
            let r = pipeline1
                .plan(
                    "test-source",
                    sc,
                    items.clone(),
                    &IngestOptions {
                        target_date: None,
                        today: far_future(),
                        dry_run: false,
                    },
                )
                .await
                .unwrap();
            write_to_vault(vault, &r).await;
            llm1.flush().await.unwrap();
            r
        };
        fill_llm_sections_with_dummy_bodies(vault, &result1).await;

        // Strip only the `summary_done` marker from `llm_inputs:`. The body and the
        // refine/concepts markers stay intact — the lookup should re-enqueue summary
        // alone.
        let page_path = vault.join(result1.daily_pages[0].path.as_ref());
        let body = tokio::fs::read_to_string(&page_path).await.unwrap();
        let edited: String = body
            .lines()
            .filter(|line| !line.trim_start().starts_with("summary_done:"))
            .collect::<Vec<_>>()
            .join("\n");
        tokio::fs::write(&page_path, edited).await.unwrap();

        clear_queue_dir(&queue_dir).await;

        let llm2: Arc<dyn LlmClient> = Arc::new(lk_queue::QueueLlmClient::new(queue_dir.clone()));
        let mut pipeline2 = Pipeline::new(vault, make_ctx(&config, llm2.clone()));
        let _ = pipeline2
            .plan(
                "test-source",
                sc,
                items,
                &IngestOptions {
                    target_date: None,
                    today: far_future(),
                    dry_run: false,
                },
            )
            .await
            .unwrap();
        llm2.flush().await.unwrap();
        let kinds = queue_task_kinds_for_source(&queue_dir);
        assert!(
            kinds.contains("summarize"),
            "removing summary_done must trigger re-enqueue: got {kinds:?}",
        );
        // The OTHER markers are still present, so their tasks should NOT re-enqueue.
        assert!(
            !kinds.contains("refine-events"),
            "refine_events_done was untouched; must remain cached: got {kinds:?}",
        );
        assert!(
            !kinds.contains("extract-concepts"),
            "concepts_done was untouched; must remain cached: got {kinds:?}",
        );
    }

    #[tokio::test]
    async fn empty_concept_result_is_cached_not_re_enqueued() {
        // Regression guard: concept extraction can legitimately find NOTHING (a
        // low-signal page). The concepts section then stays empty — but a stamped
        // `concepts_done` marks it done, so a re-ingest of identical input must NOT
        // re-enqueue. Were completion inferred from a non-empty body, the empty section would
        // re-enqueue forever, burning LLM work every ingest for every concept-less page.
        let dir = TempDir::new().unwrap();
        let vault = dir.path();
        let config = base_config(vault);
        let sc = config.sources.get("test-source").unwrap();

        let ts: jiff::Timestamp = "2026-05-23T10:00:00Z".parse().unwrap();
        let items = vec![raw_item("Event A", "Body A", "E1", ts)];

        let queue_dir = vault.join(".lorekeeper").join("queue");
        let llm1: Arc<dyn LlmClient> = Arc::new(lk_queue::QueueLlmClient::new(queue_dir.clone()));
        let result1 = {
            let mut pipeline1 = Pipeline::new(vault, make_ctx(&config, llm1.clone()));
            let r = pipeline1
                .plan(
                    "test-source",
                    sc,
                    items.clone(),
                    &IngestOptions {
                        target_date: None,
                        today: far_future(),
                        dry_run: false,
                    },
                )
                .await
                .unwrap();
            write_to_vault(vault, &r).await;
            llm1.flush().await.unwrap();
            r
        };

        // Simulate `/lore-process` finding NO concepts: stamp `concepts_done` but
        // leave the `## Related Concepts` body empty. (Summary + refine are stamped so
        // they don't muddy the assertion.)
        let page_path_rel = result1.daily_pages[0].path.to_string();
        let abs = vault.join(&page_path_rel);
        let strings = lk_core::i18n::Locale::Ko.strings();
        let mut content = tokio::fs::read_to_string(&abs).await.unwrap();
        // Fill + complete summary and refine to isolate the concepts assertion.
        content = lk_vault::replace_section(&content, strings.summary, "REAL-SUMMARY");
        content = lk_vault::replace_section(&content, strings.key_events, "REAL-REFINED");
        if let Some(h) = task_hash_for_page(&queue_dir, &page_path_rel, "summarize") {
            content = stamp_completion(&content, "summary", &h);
        }
        if let Some(h) = task_hash_for_page(&queue_dir, &page_path_rel, "refine-events") {
            content = stamp_completion(&content, "refine_events", &h);
        }
        if let Some(h) = task_hash_for_page(&queue_dir, &page_path_rel, "extract-concepts") {
            content = stamp_completion(&content, "concepts", &h);
        }
        tokio::fs::write(&abs, content).await.unwrap();

        clear_queue_dir(&queue_dir).await;

        let llm2: Arc<dyn LlmClient> = Arc::new(lk_queue::QueueLlmClient::new(queue_dir.clone()));
        let mut pipeline2 = Pipeline::new(vault, make_ctx(&config, llm2.clone()));
        let result2 = pipeline2
            .plan(
                "test-source",
                sc,
                items,
                &IngestOptions {
                    target_date: None,
                    today: far_future(),
                    dry_run: false,
                },
            )
            .await
            .unwrap();
        llm2.flush().await.unwrap();

        let kinds = queue_task_kinds_for_source(&queue_dir);
        assert!(
            !kinds.contains("extract-concepts"),
            "an empty-but-done concepts section must stay cached, not re-enqueue: got {kinds:?}",
        );
        // The materialized view re-renders every ingest, so the marker only survives if
        // the TEMPLATE emits it. Assert it round-trips through the render — a template
        // that dropped `concepts_done` would re-enqueue on the next ingest forever.
        assert!(
            result2.daily_pages[0].content.contains("concepts_done"),
            "the re-rendered page must carry concepts_done through; the daily template \
             dropped the marker:\n{}",
            result2.daily_pages[0].content
        );
    }

    #[tokio::test]
    async fn changed_input_drops_the_stale_completion_marker() {
        // A completion marker is valid only for the input it was stamped against. When
        // the input changes (concept extraction misses), the OLD `concepts_done` must
        // NOT ride the re-render forward — otherwise a later revert to the original input
        // would false-hit the stale marker against a body left empty by the interim
        // render. The render emits the marker only on a cache hit, so a miss drops it.
        let dir = TempDir::new().unwrap();
        let vault = dir.path();
        let config = base_config(vault);
        let sc = config.sources.get("test-source").unwrap();

        let ts: jiff::Timestamp = "2026-05-23T10:00:00Z".parse().unwrap();
        let queue_dir = vault.join(".lorekeeper").join("queue");

        // Ingest A, simulate the skill stamping concepts_done for input A.
        let llm1: Arc<dyn LlmClient> = Arc::new(lk_queue::QueueLlmClient::new(queue_dir.clone()));
        let result1 = {
            let mut p = Pipeline::new(vault, make_ctx(&config, llm1.clone()));
            let r = p
                .plan(
                    "test-source",
                    sc,
                    vec![raw_item("Event A", "Body A", "E1", ts)],
                    &IngestOptions {
                        target_date: None,
                        today: far_future(),
                        dry_run: false,
                    },
                )
                .await
                .unwrap();
            write_to_vault(vault, &r).await;
            llm1.flush().await.unwrap();
            r
        };
        let page_rel = result1.daily_pages[0].path.to_string();
        let abs = vault.join(&page_rel);
        let mut content = tokio::fs::read_to_string(&abs).await.unwrap();
        if let Some(h) = task_hash_for_page(&queue_dir, &page_rel, "summarize") {
            content = stamp_completion(&content, "summary", &h);
        }
        if let Some(h) = task_hash_for_page(&queue_dir, &page_rel, "extract-concepts") {
            content = stamp_completion(&content, "concepts", &h);
        }
        tokio::fs::write(&abs, content).await.unwrap();
        clear_queue_dir(&queue_dir).await;

        // Ingest B with DIFFERENT content → concepts input hash changes → miss.
        let llm2: Arc<dyn LlmClient> = Arc::new(lk_queue::QueueLlmClient::new(queue_dir.clone()));
        let mut p2 = Pipeline::new(vault, make_ctx(&config, llm2.clone()));
        let result2 = p2
            .plan(
                "test-source",
                sc,
                vec![raw_item("Event B", "Totally different body", "E2", ts)],
                &IngestOptions {
                    target_date: None,
                    today: far_future(),
                    dry_run: false,
                },
            )
            .await
            .unwrap();
        llm2.flush().await.unwrap();

        assert!(
            queue_task_kinds_for_source(&queue_dir).contains("extract-concepts"),
            "changed input must re-enqueue concept extraction",
        );
        assert!(
            !result2.daily_pages[0].content.contains("concepts_done"),
            "a stale completion marker must be dropped on a miss, not ride the \
             re-render forward:\n{}",
            result2.daily_pages[0].content
        );
    }

    #[tokio::test]
    async fn growing_concept_registry_does_not_invalidate_other_caches() {
        // The first run populates the vault with a concept page; the second run with
        // identical input must still hit the cache. The concept registry never enters
        // the task (dedup is skill-side), so a growing vault can't perturb cache hits.
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
        let llm1: Arc<dyn LlmClient> = Arc::new(lk_queue::QueueLlmClient::new(queue_dir.clone()));
        let result1 = {
            let mut pipeline1 = Pipeline::new(vault, make_ctx(&config, llm1.clone()));
            let r = pipeline1
                .plan(
                    "test-source",
                    sc,
                    items.clone(),
                    &IngestOptions {
                        target_date: None,
                        today: far_future(),
                        dry_run: false,
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
            "---\nid: concept-a\ntitle: \"Concept A\"\naliases: [\"Concept A\", \"CA\"]\ncreated: 2026-05-23\n---\n\n# Concept A\n",
        )
        .await
        .unwrap();
        fill_llm_sections_with_dummy_bodies(vault, &result1).await;
        clear_queue_dir(&queue_dir).await;

        // Second run with QueueLlmClient and IDENTICAL input. Vault now has one
        // concept on disk — the cache identity is the input text/source/date/categories
        // only, so nothing fires.
        let llm_queue: Arc<dyn LlmClient> =
            Arc::new(lk_queue::QueueLlmClient::new(queue_dir.clone()));
        let mut pipeline2 = Pipeline::new(vault, make_ctx(&config, llm_queue.clone()));
        let _ = pipeline2
            .plan(
                "test-source",
                sc,
                items,
                &IngestOptions {
                    target_date: None,
                    today: far_future(),
                    dry_run: false,
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
        let llm: Arc<dyn LlmClient> = Arc::new(lk_queue::QueueLlmClient::new(queue_dir.clone()));
        {
            let mut pipeline = Pipeline::new(vault, make_ctx(&config, llm.clone()));
            let r = pipeline
                .plan(
                    "test-source",
                    sc,
                    items,
                    &IngestOptions {
                        target_date: None,
                        today: far_future(),
                        dry_run: false,
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
                "weekly-synthesis-themes" => "themes",
                "weekly-review-narrative"
                | "monthly-review-narrative"
                | "quarterly-review-narrative"
                | "annual-review-narrative" => "narrative",
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

    /// Two documents that share a title must not collide onto one slug-derived page — the
    /// second would silently overwrite the first. The first keeps the clean slug; the
    /// collision is disambiguated by the document's own stable id, so both survive.
    #[tokio::test]
    async fn same_titled_documents_get_distinct_pages() {
        let dir = TempDir::new().unwrap();
        let vault = dir.path();
        let mut config = base_config(vault);
        config.sources.get_mut("test-source").unwrap().source_type = SourceType::Manual;
        let sc = config.sources.get("test-source").unwrap().clone();

        let ts: jiff::Timestamp = "2026-05-23T10:00:00Z".parse().unwrap();
        // Three documents share a title but are distinct files (distinct external ids) —
        // the 3+ case the disambiguation must keep collision-free, not merely improbable.
        let items = vec![
            raw_item("Meeting Notes", "First file body", "DOC-A", ts),
            raw_item("Meeting Notes", "Second file body", "DOC-B", ts),
            raw_item("Meeting Notes", "Third file body", "DOC-C", ts),
        ];

        let queue_dir = vault.join(".lorekeeper").join("queue");
        let llm: Arc<dyn LlmClient> = Arc::new(QueueLlmClient::new(queue_dir));
        let mut pipeline = Pipeline::new(vault, make_ctx(&config, llm.clone()));
        let r = pipeline
            .plan(
                "test-source",
                &sc,
                items,
                &IngestOptions {
                    target_date: None,
                    today: far_future(),
                    dry_run: false,
                },
            )
            .await
            .unwrap();

        assert_eq!(
            r.document_pages.len(),
            3,
            "same-titled documents must not collide into a single page"
        );
        let paths: std::collections::HashSet<std::path::PathBuf> = r
            .document_pages
            .iter()
            .map(|p| p.path.as_ref().to_path_buf())
            .collect();
        assert_eq!(
            paths.len(),
            3,
            "all same-titled documents must occupy distinct page paths"
        );
    }

    /// A later, DIFFERENT document that happens to share a title with one already in the
    /// vault (e.g. a prior run's archived manual note) must not claim the existing page's
    /// slug and overwrite it — the cross-run collision a batch-only check would miss. The
    /// existing page's identity (`source_file`/`source_url`) is compared, and a mismatch
    /// forces disambiguation.
    #[tokio::test]
    async fn document_does_not_overwrite_a_different_existing_doc_with_same_title() {
        let dir = TempDir::new().unwrap();
        let vault = dir.path();
        let mut config = base_config(vault);
        config.sources.get_mut("test-source").unwrap().source_type = SourceType::Manual;
        let sc = config.sources.get("test-source").unwrap().clone();

        // Pre-seed an existing document page owned by a DIFFERENT document.
        let docs_dir = vault.join("wiki").join("documents");
        tokio::fs::create_dir_all(&docs_dir).await.unwrap();
        let existing_path = docs_dir.join("meeting-notes.md");
        tokio::fs::write(
            &existing_path,
            "---\nid: meeting-notes\ntitle: \"Meeting Notes\"\nsource_file: \"old-file.md\"\n---\n\n# Meeting Notes\n\nOld content.\n",
        )
        .await
        .unwrap();

        let ts: jiff::Timestamp = "2026-05-23T10:00:00Z".parse().unwrap();
        // A new, distinct document (its own external id → distinct identity) sharing the title.
        let items = vec![raw_item("Meeting Notes", "New content", "NEW-DOC", ts)];

        let queue_dir = vault.join(".lorekeeper").join("queue");
        let llm: Arc<dyn LlmClient> = Arc::new(QueueLlmClient::new(queue_dir));
        let mut pipeline = Pipeline::new(vault, make_ctx(&config, llm.clone()));
        let r = pipeline
            .plan(
                "test-source",
                &sc,
                items,
                &IngestOptions {
                    target_date: None,
                    today: far_future(),
                    dry_run: false,
                },
            )
            .await
            .unwrap();

        assert_eq!(r.document_pages.len(), 1);
        let new_path = r.document_pages[0].path.as_ref().to_path_buf();
        assert_ne!(
            new_path,
            std::path::Path::new("wiki/documents/meeting-notes.md"),
            "a different same-titled document must not claim the existing page's slug"
        );
        // The pre-existing page is left untouched.
        let old = tokio::fs::read_to_string(&existing_path).await.unwrap();
        assert!(
            old.contains("source_file: \"old-file.md\"") && old.contains("Old content."),
            "the existing different document must be preserved, not overwritten:\n{old}"
        );
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
        let llm1: Arc<dyn LlmClient> = Arc::new(lk_queue::QueueLlmClient::new(queue_dir.clone()));
        let result1 = {
            let mut pipeline = Pipeline::new(vault, make_ctx(&config, llm1.clone()));
            let r = pipeline
                .plan(
                    "test-source",
                    &sc,
                    items.clone(),
                    &IngestOptions {
                        target_date: None,
                        today: far_future(),
                        dry_run: false,
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
        // Completion is uniformly marker-signalled; stamp each marker like the skill does.
        let doc_page_path = result1.document_pages[0].path.to_string();
        if let Some(hash) = task_hash_for_page(&queue_dir, &doc_page_path, "summarize") {
            content = stamp_completion(&content, "summary", &hash);
        }
        if let Some(hash) = task_hash_for_page(&queue_dir, &doc_page_path, "extract-concepts") {
            content = stamp_completion(&content, "concepts", &hash);
        }
        tokio::fs::write(&doc_path, content).await.unwrap();
        clear_queue_dir(&queue_dir).await;

        // Re-ingest identical input → cache hit: zero re-enqueue, bodies preserved.
        let llm2: Arc<dyn LlmClient> = Arc::new(lk_queue::QueueLlmClient::new(queue_dir.clone()));
        let mut pipeline2 = Pipeline::new(vault, make_ctx(&config, llm2.clone()));
        let result2 = pipeline2
            .plan(
                "test-source",
                &sc,
                items,
                &IngestOptions {
                    target_date: None,
                    today: far_future(),
                    dry_run: false,
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
