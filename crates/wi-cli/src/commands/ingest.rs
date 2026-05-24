use std::sync::Arc;

use super::{build_llm_client, find_config, load_config, parse_date, resolve_template_dir};

pub async fn run(
    opts: &super::GlobalOpts,
    source: Option<String>,
    date_str: Option<String>,
    dry_run: bool,
    force: bool,
) -> miette::Result<()> {
    let config = load_config(&find_config(opts)?)?;
    let vault_root = config.vault.root_path();

    let creds = wi_source::credentials::Credentials::load(&vault_root)
        .map_err(|e| miette::miette!("{e}"))?;

    // Dry-run must NOT mutate filesystem. In queue mode the LLM client itself writes
    // queue files at the start of pipeline.plan() — well before the CLI's write
    // suppression logic kicks in — so we force NoopLlmClient for dry-run regardless
    // of the configured provider.
    let llm: std::sync::Arc<dyn wi_llm::LlmClient> = if dry_run {
        std::sync::Arc::new(wi_llm::NoopLlmClient)
    } else {
        build_llm_client(&config, &vault_root)
    };

    // Guard against silent data loss when provider is switched away from `queue`
    // while pending queue files exist. Without this check, the new run would dedup
    // events whose previous queue tasks were never drained by `/wi-process`,
    // permanently stranding them with empty semantic sections.
    let pending = pending_queue_count(&vault_root)?;
    match config.llm.provider {
        wi_core::config::LlmProvider::Anthropic if pending > 0 => {
            return Err(miette::miette!(
                "{pending} pending queue file(s) under {}/.wiki-ingest/queue/. Drain via \
                 /wi-process (or switch back to provider: queue) before running with \
                 provider: anthropic.",
                vault_root.display(),
            ));
        }
        wi_core::config::LlmProvider::Queue if pending > 0 => {
            // Not an error in queue mode — re-running just emits a new file that
            // /wi-process will drain alongside the existing ones. Warn so the user
            // can run /wi-process first if they want to avoid duplicate LLM work,
            // since each pending file already has a target page on disk.
            tracing::warn!(
                pending,
                queue_dir = %vault_root.join(".wiki-ingest").join("queue").display(),
                "pending queue files exist; run /wi-process first to avoid duplicate LLM work",
            );
            eprintln!(
                "! {pending} pending queue file(s) under {}/.wiki-ingest/queue/. Run \
                 /wi-process first to avoid duplicate LLM work on the same target pages.",
                vault_root.display(),
            );
        }
        _ => {}
    }

    // Sweep stranded `.jsonl.tmp` files from previous crashed runs. The queue
    // flush is sub-second, so any tmp older than an hour is from a process that
    // died mid-flush (or was killed). Each ingest writes its own PID-suffixed
    // tmp, so a concurrent ingest's just-created tmp is too young to be swept.
    // Skipped under --dry-run, which must not mutate the vault at all.
    if !dry_run {
        sweep_stale_tmps(&vault_root).await?;
    }

    let tz = config.vault.timezone();
    let today = jiff::Timestamp::now().to_zoned(tz.clone()).date();
    let target_date = if date_str.is_some() {
        Some(parse_date(date_str.as_deref(), today)?)
    } else {
        None
    };
    let extract_target = target_date.unwrap_or(today);

    let ctx = Arc::new(
        wi_pipeline::PipelineContext::new(
            &resolve_template_dir(opts, &vault_root),
            llm.clone(),
            &config,
        )
        .map_err(|e| miette::miette!("{e}"))?,
    );
    let pipeline = Arc::new(
        if dry_run {
            wi_pipeline::Pipeline::new_dry_run(&vault_root, ctx, &config)
        } else {
            wi_pipeline::Pipeline::new(&vault_root, ctx, &config)
        }
        .map_err(|e| miette::miette!("{e}"))?,
    );
    let writer = wi_vault::VaultWriter::new(&vault_root);
    let log = wi_vault::IngestLog::new(vault_root.join(".wiki-ingest").join("ingest.jsonl"));
    let options = wi_pipeline::IngestOptions {
        dry_run,
        force,
        target_date,
    };

    let sources: Vec<(String, wi_core::config::SourceConfig)> = match source {
        Some(ref id) => {
            let sc = config
                .sources
                .get(id)
                .ok_or_else(|| miette::miette!("source '{id}' not found"))?;
            if !sc.enabled {
                return Err(miette::miette!("source '{id}' is disabled"));
            }
            vec![(id.clone(), sc.clone())]
        }
        None => config
            .enabled_sources()
            .map(|(id, sc)| (id.to_string(), sc.clone()))
            .collect(),
    };

    if dry_run {
        eprintln!("[dry-run] no vault writes will be performed");
    }
    if let Some(td) = target_date {
        eprintln!("[date] only events on {td} will be processed");
    }

    let http = reqwest::Client::new();
    let extract_ctx = wi_source::ExtractContext {
        target_date: extract_target,
        timezone: tz,
    };

    // Phase 1: Plan all sources (no commits, no writes yet).
    struct Planned {
        id: String,
        result: wi_pipeline::IngestResult,
        started_at: std::time::Instant,
    }
    let mut planned: Vec<Planned> = Vec::new();
    // Set on any source/extract/pipeline/write failure so the process exits non-zero —
    // a cron or CI run must be able to detect a partial failure, not see exit 0.
    let mut had_failure = false;

    for (id, sc) in &sources {
        let started_at = std::time::Instant::now();
        eprintln!("▸ {id} ({})", sc.source_type);

        let adapter = match wi_source::create_source(sc.source_type, http.clone(), &creds) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("  ✗ {e}");
                if !dry_run {
                    record_failure(&log, id, &started_at, &e.to_string()).await;
                }
                had_failure = true;
                continue;
            }
        };

        let raw = match adapter.extract(&sc.params, &extract_ctx).await {
            Ok(items) => items,
            Err(e) => {
                eprintln!("  ✗ extract: {e}");
                if !dry_run {
                    record_failure(&log, id, &started_at, &e.to_string()).await;
                }
                had_failure = true;
                continue;
            }
        };
        eprintln!("  extracted: {} items", raw.len());

        let result = match pipeline.plan(id, sc, raw, &options).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("  ✗ pipeline: {e}");
                if !dry_run {
                    record_failure(&log, id, &started_at, &e.to_string()).await;
                }
                had_failure = true;
                continue;
            }
        };

        if result.is_empty() {
            eprintln!("  — skipped (no new events)");
            if !dry_run {
                log.record(&wi_vault::LogEntry {
                    timestamp: jiff::Timestamp::now(),
                    source_id: id.clone(),
                    status: wi_vault::LogStatus::Skipped,
                    events_count: 0,
                    duration_ms: started_at.elapsed().as_millis() as u64,
                    error: None,
                })
                .await
                .ok();
            }
            continue;
        }

        if !result.concepts.is_empty() {
            eprintln!("  concepts: {}", result.concepts.len());
        }

        if dry_run {
            for out in &result.daily_pages {
                eprintln!("  [dry-run] would write: {}", out.path);
            }
            continue;
        }

        planned.push(Planned {
            id: id.clone(),
            result,
            started_at,
        });
    }

    if dry_run {
        // Concept pages are accumulated across all sources, so preview them once.
        let concept_pages = pipeline
            .render_concept_pages()
            .await
            .map_err(|e| miette::miette!("concept render: {e}"))?;
        for out in &concept_pages {
            eprintln!("  [dry-run] would write: {}", out.path);
        }
        if had_failure {
            return Err(miette::miette!(
                "dry-run completed with source failures; see output above"
            ));
        }
        return Ok(());
    }

    // Phase 2: Write daily + concept pages.
    let mut total_pages = 0usize;
    let mut any_write_failed = false;
    let mut all_personal: Vec<wi_core::event::Event> = Vec::new();

    for p in &planned {
        for out in &p.result.daily_pages {
            if let Err(e) = writer.write_page(out.path.as_ref(), &out.content).await {
                eprintln!("  ✗ vault write {}: {e}", out.path);
                any_write_failed = true;
                break;
            }
            total_pages += 1;
            eprintln!("  ✓ wrote: {} ({})", out.path, p.id);
        }
        if any_write_failed {
            break;
        }
        for e in &p.result.events {
            if e.is_personal {
                all_personal.push(e.clone());
            }
        }
        if !all_personal.is_empty() {
            eprintln!(
                "  personal items for {}: {}",
                p.id,
                p.result.events.iter().filter(|e| e.is_personal).count()
            );
        }
    }

    // Concept pages are a cross-source aggregate, rendered once from the run-level
    // accumulator so two sources mentioning the same concept merge into one page.
    if !any_write_failed {
        let concept_pages = pipeline
            .render_concept_pages()
            .await
            .map_err(|e| miette::miette!("concept render: {e}"))?;
        for out in &concept_pages {
            if let Err(e) = writer.write_page(out.path.as_ref(), &out.content).await {
                eprintln!("  ✗ vault write {}: {e}", out.path);
                any_write_failed = true;
                break;
            }
            total_pages += 1;
            eprintln!("  ✓ wrote: {}", out.path);
        }
    }

    // Phase 3: Aggregate and write work-log.
    if !any_write_failed && !all_personal.is_empty() {
        let work_logs = pipeline
            .aggregate_work_log(&all_personal)
            .map_err(|e| miette::miette!("work-log: {e}"))?;
        for wl in &work_logs {
            if let Err(e) = writer.write_page(wl.path.as_ref(), &wl.content).await {
                eprintln!("✗ work-log write {}: {e}", wl.path);
                any_write_failed = true;
                break;
            }
            total_pages += 1;
            eprintln!("▸ work-log → {}", wl.path);
        }
    }

    // Phase 4: Persist queued LLM tasks atomically. Runs BEFORE dedup commit so the
    // queue file is the durability anchor for semantic work: if a crash strands the
    // flush, dedup is not yet committed and a re-run will re-extract the events and
    // re-queue the tasks. The temp + fsync + rename inside the queue client ensures
    // we never observe a half-written JSONL on disk.
    if !any_write_failed {
        llm.flush()
            .await
            .map_err(|e| miette::miette!("queue flush: {e}"))?;
    }

    // Phase 5: Commit dedup ONLY if every write AND the queue flush succeeded. Each
    // source committed independently so that future runs that re-extract these events
    // know they're already persisted.
    for p in &planned {
        let status = if any_write_failed {
            wi_vault::LogStatus::Failed
        } else {
            pipeline
                .commit(&p.result.events)
                .map_err(|e| miette::miette!("dedup commit for {}: {e}", p.id))?;
            wi_vault::LogStatus::Success
        };
        log.record(&wi_vault::LogEntry {
            timestamp: jiff::Timestamp::now(),
            source_id: p.id.clone(),
            status,
            events_count: p.result.events.len(),
            duration_ms: p.started_at.elapsed().as_millis() as u64,
            error: if any_write_failed {
                Some("vault write failed; dedup not committed".into())
            } else {
                None
            },
        })
        .await
        .ok();
    }

    had_failure |= any_write_failed;

    eprintln!(
        "\nDone. {} pages written, {} personal items tracked.{}",
        total_pages,
        all_personal.len(),
        if any_write_failed {
            " (some writes failed; dedup not committed — safe to re-run)"
        } else {
            ""
        }
    );

    if had_failure {
        return Err(miette::miette!(
            "ingest completed with failures; see {}/.wiki-ingest/ingest.jsonl",
            vault_root.display()
        ));
    }
    Ok(())
}

async fn sweep_stale_tmps(vault_root: &std::path::Path) -> miette::Result<()> {
    let dir = vault_root.join(".wiki-ingest").join("queue");
    if !dir.exists() {
        return Ok(());
    }
    const STALE_AFTER_SECS: i64 = 3600;
    let cutoff = jiff::Timestamp::now().as_second() - STALE_AFTER_SECS;
    let mut entries = tokio::fs::read_dir(&dir)
        .await
        .map_err(|e| miette::miette!("read queue dir: {e}"))?;
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|e| miette::miette!("queue entry: {e}"))?
    {
        let path = entry.path();
        if !path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|n| n.ends_with(".jsonl.tmp"))
        {
            continue;
        }
        let metadata = entry
            .metadata()
            .await
            .map_err(|e| miette::miette!("metadata: {e}"))?;
        let mtime_secs = metadata.modified().ok().and_then(|t| {
            t.duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_secs() as i64)
        });
        if mtime_secs.is_some_and(|m| m < cutoff) {
            tokio::fs::remove_file(&path)
                .await
                .map_err(|e| miette::miette!("remove stale tmp {}: {e}", path.display()))?;
            tracing::info!(path = %path.display(), "swept stale queue tmp");
        }
    }
    Ok(())
}

fn pending_queue_count(vault_root: &std::path::Path) -> miette::Result<usize> {
    let dir = vault_root.join(".wiki-ingest").join("queue");
    if !dir.exists() {
        return Ok(0);
    }
    let entries = std::fs::read_dir(&dir).map_err(|e| miette::miette!("read queue dir: {e}"))?;
    let mut n = 0;
    for entry in entries {
        let entry = entry.map_err(|e| miette::miette!("queue entry: {e}"))?;
        let path = entry.path();
        if path.is_file() && path.extension().is_some_and(|ext| ext == "jsonl") {
            n += 1;
        }
    }
    Ok(n)
}

async fn record_failure(
    log: &wi_vault::IngestLog,
    source_id: &str,
    start: &std::time::Instant,
    error: &str,
) {
    log.record(&wi_vault::LogEntry {
        timestamp: jiff::Timestamp::now(),
        source_id: source_id.into(),
        status: wi_vault::LogStatus::Failed,
        events_count: 0,
        duration_ms: start.elapsed().as_millis() as u64,
        error: Some(error.into()),
    })
    .await
    .ok();
}
