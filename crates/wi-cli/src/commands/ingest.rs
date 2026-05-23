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

    let llm = build_llm_client(&config, &vault_root);

    let tz = config.vault.timezone();
    let today = jiff::Timestamp::now().to_zoned(tz.clone()).date();
    let target_date = if date_str.is_some() {
        Some(parse_date(date_str.as_deref(), today)?)
    } else {
        None
    };
    let extract_target = target_date.unwrap_or(today);

    let ctx = Arc::new(
        wi_pipeline::PipelineContext::new(&resolve_template_dir(opts, &vault_root), llm, &config)
            .map_err(|e| miette::miette!("{e}"))?,
    );
    let pipeline = Arc::new(
        wi_pipeline::Pipeline::new(&vault_root, ctx, &config)
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

    for (id, sc) in &sources {
        let started_at = std::time::Instant::now();
        eprintln!("▸ {id} ({})", sc.source_type);

        let adapter = match wi_source::create_source(sc.source_type, http.clone(), &creds) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("  ✗ {e}");
                record_failure(&log, id, &started_at, &e.to_string()).await;
                continue;
            }
        };

        let raw = match adapter.extract(&sc.params, &extract_ctx).await {
            Ok(items) => items,
            Err(e) => {
                eprintln!("  ✗ extract: {e}");
                record_failure(&log, id, &started_at, &e.to_string()).await;
                continue;
            }
        };
        eprintln!("  extracted: {} items", raw.len());

        let result = match pipeline.plan(id, sc, raw, &options).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("  ✗ pipeline: {e}");
                record_failure(&log, id, &started_at, &e.to_string()).await;
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
            for out in result.daily_pages.iter().chain(result.concept_pages.iter()) {
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
        return Ok(());
    }

    // Phase 2: Write daily + concept pages.
    let mut total_pages = 0usize;
    let mut any_write_failed = false;
    let mut all_personal: Vec<wi_core::event::Event> = Vec::new();

    for p in &planned {
        for out in p
            .result
            .daily_pages
            .iter()
            .chain(p.result.concept_pages.iter())
        {
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

    // Phase 4: Commit dedup ONLY if every write succeeded. Each source committed independently
    // so that future runs that re-extract these events know they're already persisted.
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
    Ok(())
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
