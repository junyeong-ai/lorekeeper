use std::sync::Arc;

use super::{build_llm_client, find_config, load_config, parse_date};

pub async fn run(
    opts: &super::GlobalOptions,
    source: Option<String>,
    date_str: Option<String>,
    dry_run: bool,
) -> miette::Result<()> {
    let config = load_config(&find_config(opts)?)?;
    let vault_root = config.vault.root_path();

    let creds = lk_source::credentials::Credentials::load(&vault_root)
        .map_err(|e| miette::miette!("{e}"))?;

    // Dry-run must NOT mutate filesystem. In queue mode the LLM client buffers tasks
    // during planning and writes a queue file on `flush()`; rather than thread dry-run
    // suppression through the flush path, we force NoopLlmClient for dry-run so no queue
    // file is ever produced, regardless of the configured provider.
    let llm: std::sync::Arc<dyn lk_queue::LlmClient> = if dry_run {
        std::sync::Arc::new(lk_queue::NoopLlmClient)
    } else {
        build_llm_client(&config, &vault_root)?
    };

    // Surface pending (undrained) queue files — never silently ignore them. The risk
    // differs by provider, so the message does too:
    //   - queue mode: re-running just emits another file that `/lore-process` drains
    //     alongside the existing ones; warn so the user can drain first to avoid
    //     duplicate LLM work on the same target pages.
    //   - any non-queue provider (e.g. noop): this run writes the pages but produces no
    //     tasks to fill the empty sections, so the pending tasks are stranded — a stronger
    //     warning, since draining means switching back to queue.
    let pending = pending_queue_count(&vault_root)?;
    if pending > 0 {
        let queue_dir = vault_root.join(".lorekeeper").join("queue");
        if config.llm.provider == lk_core::config::LlmProvider::Queue {
            tracing::warn!(
                pending,
                queue_dir = %queue_dir.display(),
                "pending queue files exist; run /lore-process first to avoid duplicate LLM work",
            );
            eprintln!(
                "! {pending} pending queue file(s) under {}/.lorekeeper/queue/. Run \
                 /lore-process first to avoid duplicate LLM work on the same target pages.",
                vault_root.display(),
            );
        } else {
            tracing::warn!(
                pending,
                provider = ?config.llm.provider,
                queue_dir = %queue_dir.display(),
                "pending queue files exist but provider is not `queue`; their tasks will be stranded",
            );
            eprintln!(
                "! {pending} pending queue file(s) under {}/.lorekeeper/queue/ will be STRANDED: \
                 provider is not `queue`, so this run creates no tasks to fill their target pages. \
                 Switch to `llm.provider: queue` and run /lore-process to drain them first.",
                vault_root.display(),
            );
        }
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
        lk_pipeline::PipelineContext::build(opts.template_dir.as_deref(), llm.clone(), &config)
            .map_err(|e| miette::miette!("{e}"))?,
    );
    let mut pipeline = lk_pipeline::Pipeline::new(&vault_root, ctx);
    let writer = lk_vault::VaultWriter::new(&vault_root);
    let log = lk_vault::IngestLog::new(vault_root.join(".lorekeeper").join("ingest.jsonl"));
    let options = lk_pipeline::IngestOptions {
        target_date,
        today,
        dry_run,
    };

    let sources: Vec<(String, lk_core::config::SourceConfig)> = match source {
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

    for (id, sc) in &sources {
        lk_source::validate_params(sc.source_type, &sc.params)
            .map_err(|e| miette::miette!("sources.{id} ({}): {e}", sc.source_type))?;
    }

    if dry_run {
        eprintln!("[dry-run] no vault writes will be performed");
    }
    if let Some(td) = target_date {
        eprintln!("[date] only events on {td} will be processed");
    }

    let http = lk_source::build_http_client().map_err(|e| miette::miette!("{e}"))?;
    // One auth provider per Atlassian instance for the whole run. OAuth refresh tokens
    // rotate, so two providers over one instance would invalidate each other mid-run.
    let atlassian = lk_source::build_atlassian_registry(&creds, &http, &vault_root);
    let extract_ctx = lk_source::ExtractContext {
        target_date: extract_target,
        timezone: tz,
        locale: config.vault.locale(),
        identity: config.identity.clone(),
        vault_root: vault_root.clone(),
    };

    // Phase 1: Plan all sources (no vault writes yet).
    struct Planned {
        id: String,
        result: lk_pipeline::IngestResult,
        started_at: std::time::Instant,
    }
    let mut planned: Vec<Planned> = Vec::new();
    // Set on any source/extract/pipeline/write failure so the process exits non-zero —
    // a cron or CI run must be able to detect a partial failure, not see exit 0.
    let mut had_failure = false;

    for (id, sc) in &sources {
        let started_at = std::time::Instant::now();
        eprintln!("▸ {id} ({})", sc.source_type);

        let adapter = match lk_source::build_source(
            sc.source_type,
            http.clone(),
            &creds,
            &atlassian,
            sc.instance.as_deref(),
        ) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("  ✗ {e}");
                had_failure = true;
                if !dry_run {
                    record_failure(&log, id, &started_at, &e.to_string(), &mut had_failure).await;
                }
                continue;
            }
        };

        let raw = match adapter.extract(&sc.params, &extract_ctx).await {
            Ok(items) => items,
            Err(e) => {
                eprintln!("  ✗ extract: {e}");
                had_failure = true;
                if !dry_run {
                    record_failure(&log, id, &started_at, &e.to_string(), &mut had_failure).await;
                }
                continue;
            }
        };
        eprintln!("  extracted: {} items", raw.len());

        // Open a per-source queue transaction: if plan fails partway it may have already
        // buffered LLM tasks for pages it never finished writing — roll those back so the
        // flushed queue file never references an unwritten page.
        llm.begin_source().await;
        let result = match pipeline.plan(id, sc, raw, &options).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("  ✗ pipeline: {e}");
                llm.rollback_source().await;
                had_failure = true;
                if !dry_run {
                    record_failure(&log, id, &started_at, &e.to_string(), &mut had_failure).await;
                }
                continue;
            }
        };

        // Skip only when there are no pages to write. (A manual source with an empty
        // inbox lands here too; with no events its archive step is a no-op.)
        if result.is_empty() {
            eprintln!("  — skipped (no events)");
            if !dry_run {
                record_log(
                    &log,
                    &lk_vault::LogEntry {
                        timestamp: jiff::Timestamp::now(),
                        source_id: id.clone(),
                        status: lk_vault::LogStatus::Skipped,
                        event_count: 0,
                        duration_ms: started_at.elapsed().as_millis() as u64,
                        error: None,
                    },
                    &mut had_failure,
                )
                .await;
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
            for out in &result.document_pages {
                eprintln!("  [dry-run] would write: {} (document)", out.path);
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
    let mut all_personal: Vec<lk_core::event::Event> = Vec::new();

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
        for out in &p.result.document_pages {
            if let Err(e) = writer.write_page(out.path.as_ref(), &out.content).await {
                eprintln!("  ✗ vault write {}: {e}", out.path);
                any_write_failed = true;
                break;
            }
            total_pages += 1;
            eprintln!("  ✓ wrote: {} (document)", out.path);
        }
        if any_write_failed {
            break;
        }
        // Only realized personal contribution feeds the work-log; a forecast (calendar
        // look-ahead) commitment is not work done, so it is neither collected nor counted.
        // `render_work_log` re-asserts this as the subsystem invariant, but gating here
        // keeps the run summary honest and avoids passing events it would only drop.
        let before = all_personal.len();
        all_personal.extend(
            p.result
                .events
                .iter()
                .filter(|e| e.is_personal && e.date <= today)
                .cloned(),
        );
        let added = all_personal.len() - before;
        if added > 0 {
            eprintln!("  personal items for {}: {added}", p.id);
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

    // Phase 3: Render the work-log — ONLY on a full ingest (no source filter). The work-log
    // is a cross-source daily aggregate; a filtered `lore ingest <id>` sees a subset of
    // personal events BY CONSTRUCTION, so rendering it would silently freeze a structural
    // partial into the page. A transient fetch failure inside a full run is different and
    // deliberately does NOT block this write: the failure is loud (non-zero exit, named in
    // the report) and the next full run re-renders the page complete — blocking here would
    // trade that self-healing one-day gap for a work-log frozen by any persistently
    // failing source, news feeds included.
    // (`render_work_log` separately gates the whole subsystem on `config.personal` being set.)
    if !any_write_failed && source.is_none() && !all_personal.is_empty() {
        let work_logs = pipeline
            .render_work_log(&all_personal, today)
            .await
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
    } else if source.is_some() && !all_personal.is_empty() {
        eprintln!(
            "  work-log not refreshed: partial ingest of '{}'. Run `lore ingest` (all sources) to rebuild the day's work-log.",
            source.as_deref().unwrap_or_default()
        );
    }

    // Phase 4: Persist queued LLM tasks atomically. The queue file is the durability
    // anchor for semantic work: the temp + fsync + rename inside the queue client ensures
    // we never observe a half-written JSONL on disk, and a crash that strands the flush is
    // recovered by the next run re-rendering the same pages and re-queueing the tasks.
    if !any_write_failed {
        llm.flush()
            .await
            .map_err(|e| miette::miette!("queue flush: {e}"))?;
    }

    // Phase 5: Record the per-source ingest-log entry. The run is idempotent — daily
    // pages are materialized views re-rendered in full each run — so a write failure
    // anywhere just leaves the affected pages for the next run to reproduce; there is no
    // commit point to roll back.
    for p in &planned {
        let status = if any_write_failed {
            lk_vault::LogStatus::Failed
        } else {
            lk_vault::LogStatus::Success
        };
        record_log(
            &log,
            &lk_vault::LogEntry {
                timestamp: jiff::Timestamp::now(),
                source_id: p.id.clone(),
                status,
                event_count: p.result.events.len(),
                duration_ms: p.started_at.elapsed().as_millis() as u64,
                error: if any_write_failed {
                    Some("vault write failed".into())
                } else {
                    None
                },
            },
            &mut had_failure,
        )
        .await;
    }

    // Phase 6: Archive consumed inbox files for manual sources. Reached only when every
    // vault write succeeded (`!any_write_failed`) AND the queue flush above succeeded (a
    // flush error `?`-returns before here) — by then the manual source's knowledge is
    // durably materialized, so another source's fetch failure must not strand its inbox.
    // Any write/flush failure leaves files in the inbox for safe retry.
    if !any_write_failed {
        for p in &planned {
            let sc = sources.iter().find(|(id, _)| id == &p.id).map(|(_, sc)| sc);
            if let Some(sc) = sc
                && sc.source_type == lk_core::config::SourceType::Manual
                && let Err(e) = lk_source::archive_consumed_files(
                    &sc.params,
                    &p.result.events,
                    extract_target,
                    &vault_root,
                )
            {
                tracing::warn!(source = %p.id, error = %e, "manual archive failed");
            }
        }
    }

    had_failure |= any_write_failed;

    eprintln!(
        "\nDone. {} pages written, {} personal items tracked.{}",
        total_pages,
        all_personal.len(),
        if any_write_failed {
            " (some writes failed — safe to re-run)"
        } else {
            ""
        }
    );

    if had_failure {
        return Err(miette::miette!(
            "ingest completed with failures; see {}/.lorekeeper/ingest.jsonl",
            vault_root.display()
        ));
    }
    Ok(())
}

async fn sweep_stale_tmps(vault_root: &std::path::Path) -> miette::Result<()> {
    let dir = vault_root.join(".lorekeeper").join("queue");
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
    let dir = vault_root.join(".lorekeeper").join("queue");
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

/// Append one ingest-log entry, surfacing a write failure as a run failure instead of
/// swallowing it: the log is the audit trail `lore status` / `lore health` read, so a
/// disk-full or permission error must reach the exit code, not just the tracing log.
async fn record_log(log: &lk_vault::IngestLog, entry: &lk_vault::LogEntry, had_failure: &mut bool) {
    if let Err(e) = log.record(entry).await {
        tracing::warn!(error = %e, "ingest-log write failed");
        *had_failure = true;
    }
}

async fn record_failure(
    log: &lk_vault::IngestLog,
    source_id: &str,
    start: &std::time::Instant,
    error: &str,
    had_failure: &mut bool,
) {
    record_log(
        log,
        &lk_vault::LogEntry {
            timestamp: jiff::Timestamp::now(),
            source_id: source_id.into(),
            status: lk_vault::LogStatus::Failed,
            event_count: 0,
            duration_ms: start.elapsed().as_millis() as u64,
            error: Some(error.into()),
        },
        had_failure,
    )
    .await;
}
