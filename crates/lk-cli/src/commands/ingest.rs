use std::sync::Arc;

use super::{build_llm_client, find_config, load_config_with_revision, parse_date};

mod capture;
pub mod recovery;

/// The number of events the page states about itself, as ` (N events)`.
///
///
/// A daily page is a materialized view: a re-render REPLACES its event list with whatever the
/// source returns now, and a source whose window has passed returns fewer. This count is the
/// whole difference, so a dry run has to report it — otherwise "check with --dry-run first"
/// names a comparison the output does not offer. Read from the page that would be written, so
/// it is the number itself rather than a second count of the same events.
fn event_count(content: &str) -> String {
    match lk_core::frontmatter::parse_page(content)
        .ok()
        .as_ref()
        .and_then(|page| page.frontmatter.get("event_count"))
        .and_then(|v| v.as_u64())
    {
        Some(n) => format!(" ({n} events)"),
        // Said, not omitted. `doctor` tells a reader to compare this number before letting a
        // re-render replace a page's event list, and a `--template-dir` that drops the field
        // would otherwise print the same line as a page with no events to lose.
        None => " (event count unavailable)".to_string(),
    }
}

/// Say what writing this page costs, immediately before writing it.
///
/// A section whose answer nothing recorded is replaced with an empty one. The pipeline
/// re-enqueues it, so for its own output this is routine — but the same state describes a body
/// written by hand or by a drain that never stamped, and the write is not reversible. Said at
/// the write, so a page a later failure never reaches is never reported as lost, and under
/// `--dry-run` in the same conditional mood as every other line that run prints.
pub(super) fn report_discards(page: &lk_pipeline::RenderResult, dry_run: bool) {
    for heading in &page.discarded {
        if dry_run {
            eprintln!(
                "  [dry-run] would empty an unanswered section: {}: {heading}",
                page.path
            );
        } else {
            eprintln!(
                "  ! emptying an unanswered section: {}: {heading}",
                page.path
            );
        }
    }
}

struct SourceCapture {
    observation: lk_source::SourceObservation,
    originals: Vec<lk_core::manual_original::ManualBundleCapture>,
    retirement_blocked: bool,
}

async fn collect_with_budget<T>(
    started_at: std::time::Instant,
    seconds: std::num::NonZeroU64,
    collection: impl std::future::Future<Output = Result<T, lk_source::SourceError>>,
) -> Result<T, lk_source::SourceError> {
    let deadline = tokio::time::Instant::from_std(started_at)
        .checked_add(std::time::Duration::from_secs(seconds.get()))
        .ok_or_else(|| {
            lk_source::SourceError::InvalidParams(
                "collection timeout exceeds the platform clock range".into(),
            )
        })?;
    let result = tokio::time::timeout_at(deadline, collection).await;
    match result {
        Ok(Ok(value)) if tokio::time::Instant::now() < deadline => Ok(value),
        Ok(Err(error)) => Err(error),
        Ok(Ok(_)) | Err(_) => Err(lk_source::SourceError::CollectionTimeout {
            seconds: seconds.get(),
        }),
    }
}

fn ensure_config_current(path: &std::path::Path, expected: &str) -> miette::Result<()> {
    if load_config_with_revision(path)?.1 != expected {
        return Err(miette::miette!(
            "selected configuration changed during collection; captured observations were not published; rerun with the intended configuration"
        ));
    }
    Ok(())
}

pub async fn run(
    opts: &super::GlobalOptions,
    source: Option<String>,
    date_str: Option<String>,
    dry_run: bool,
    wait_for_vault: bool,
) -> miette::Result<()> {
    let config_path = find_config(opts)?;
    let (config, config_revision) = load_config_with_revision(&config_path)?;
    let vault_root = config.vault.root_path();

    let credential_store = lk_source::credentials::CredentialStore::new(&config.credentials_path())
        .map_err(|e| miette::miette!("{e}"))?;
    let creds = credential_store
        .load()
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
        if sc.source_type == lk_core::config::SourceType::Manual {
            lk_source::validate_manual_processing(&sc.params, config.ingest.documents.as_ref())
                .map_err(|error| miette::miette!("sources.{id} ({}): {error}", sc.source_type))?;
        }
    }

    if dry_run {
        eprintln!("[dry-run] no vault writes will be performed");
    }
    if let Some(td) = target_date {
        eprintln!(
            "[date] event windows are anchored to {td}; document snapshots preserve authored dates"
        );
    }

    let http = lk_source::build_http_client().map_err(|e| miette::miette!("{e}"))?;
    // Share each instance's in-memory access-token cache. Store-wide locking also
    // serializes refresh and successor persistence across cooperating processes.
    let atlassian =
        lk_source::build_atlassian_registry_with_store(&creds, &http, &credential_store);
    let extract_ctx = lk_source::ExtractContext {
        target_date: extract_target,
        timezone: tz,
        locale: config.vault.locale(),
        identity: config.identity.clone(),
        vault_root: vault_root.clone(),
        vault_dirs: config.vault.dirs.clone(),
        documents: config.ingest.documents.clone(),
        projection_cache: Some(lk_source::content::ProjectionCache::for_vault(
            &vault_root,
            if dry_run {
                lk_source::content::CacheAccess::ReadOnly
            } else {
                lk_source::content::CacheAccess::ReadWrite
            },
        )),
        originals: config.ingest.originals.clone(),
        max_input_bytes: config.ingest.inputs.max_source_bytes.get(),
    };

    // Capture source observations before acquiring the vault writer.
    let collection_started_at = jiff::Timestamp::now();
    struct Planned {
        id: String,
        result: lk_pipeline::IngestResult,
        started_at: std::time::Instant,
        manual_originals: Vec<lk_core::manual_original::ManualBundleCapture>,
        manual_retirement_blocked: bool,
        observation_outcome: lk_source::ObservationOutcome,
    }
    struct Acquired {
        id: String,
        started_at: std::time::Instant,
        result: Result<(lk_source::Selection, capture::CapturedSource), lk_source::SourceError>,
    }
    let mut acquired = Vec::new();
    let mut planned: Vec<Planned> = Vec::new();
    let mut manual_capture_bytes = 0usize;
    let mut rss_capture_bytes = 0usize;
    let mut acquired_inputs =
        lk_core::input::InputBudget::new(config.ingest.inputs.max_run_bytes.get());
    // Set on any source/extract/pipeline/write failure so the process exits non-zero —
    // a cron or CI run must be able to detect a partial failure, not see exit 0.
    let mut had_failure = false;
    let mut observations = std::collections::BTreeMap::new();

    for (id, sc) in &sources {
        let started_at = std::time::Instant::now();
        let mut source_ctx = extract_ctx.clone();
        source_ctx.max_input_bytes = source_ctx.max_input_bytes.min(acquired_inputs.remaining());
        eprintln!("▸ collect {id} ({})", sc.source_type);
        let adapter = match lk_source::build_source(
            sc.source_type,
            http.clone(),
            &creds,
            &atlassian,
            sc.instance.as_deref(),
        )
        .and_then(|adapter| {
            adapter
                .selection(&sc.params)
                .map(|selection| (adapter, selection))
        }) {
            Ok(adapter) => adapter,
            Err(error) => {
                acquired.push(Acquired {
                    id: id.clone(),
                    started_at,
                    result: Err(error),
                });
                continue;
            }
        };
        let (adapter, selection) = adapter;
        let collection = async {
            if sc.source_type == lk_core::config::SourceType::Manual {
                lk_source::capture_manual(&sc.params, &source_ctx)
                .await
                .and_then(|mut capture| {
                    capture.project_links(
                        &std::path::PathBuf::from(&config.vault.dirs.wiki)
                            .join(lk_core::vault_path::DOCUMENTS_SUBDIR),
                    )?;
                    for failure in &capture.failures {
                        eprintln!("  ✗ {failure}");
                    }
                    if !capture.unprocessed.is_empty() {
                        eprintln!("  retained: {} binary input(s) without a configured standalone decoder; configure ingest.documents/OCR for independent processing", capture.unprocessed.len());
                    }
                    if !capture.failures.is_empty() {
                        had_failure = true;
                    }
                    if manual_capture_bytes.saturating_add(capture.byte_len()) > 512 * 1024 * 1024 {
                        return Err(lk_source::SourceError::Parse(
                        "manual captures exceed the ingest-wide 512 MiB limit; split the ingest"
                            .into(),
                    ));
                    }
                    let retirement_blocked = !capture.failures.is_empty();
                    Ok(SourceCapture {
                        observation: lk_source::SourceObservation {
                            items: capture.items,
                            scope: lk_source::ObservationScope::Unverified,
                            outcome: if retirement_blocked {
                                lk_source::ObservationOutcome::Partial
                            } else {
                                lk_source::ObservationOutcome::Unverified
                            },
                        },
                        originals: capture.originals,
                        retirement_blocked,
                    })
                })
            } else {
                adapter
                    .observe(&sc.params, &source_ctx)
                    .await
                    .map(|observation| SourceCapture {
                        observation,
                        originals: Vec::new(),
                        retirement_blocked: false,
                    })
            }
        };
        let timeout = sc
            .collection_timeout_seconds
            .unwrap_or(config.ingest.source_timeout_seconds);
        let result = collect_with_budget(started_at, timeout, collection).await;
        let mut next_inputs = acquired_inputs.clone();
        let result = result.and_then(|capture| {
            // Recheck final projections (including manual link relocation) before
            // retaining the source. Failed sources do not reserve run capacity.
            let mut source_inputs = lk_core::input::InputBudget::new(source_ctx.max_input_bytes);
            source_inputs.admit_all(&capture.observation.items)?;
            next_inputs.admit_all(&capture.observation.items)?;
            Ok(capture)
        });
        let captured_manual_bytes = result.as_ref().map_or(0, |capture| {
            capture
                .originals
                .iter()
                .map(lk_core::manual_original::ManualBundleCapture::byte_len)
                .sum::<usize>()
        });
        let result = match result {
            Ok(capture) => {
                collect_with_budget(started_at, timeout, async move {
                    tokio::task::spawn_blocking(move || capture::CapturedSource::capture(capture))
                        .await
                        .map_err(|error| {
                            lk_source::SourceError::Parse(format!("capture source: {error}"))
                        })
                        .and_then(std::convert::identity)
                })
                .await
            }
            Err(error) => Err(error),
        };
        if result.is_ok() {
            acquired_inputs = next_inputs;
            manual_capture_bytes += captured_manual_bytes;
        }
        acquired.push(Acquired {
            id: id.clone(),
            started_at,
            result: result.map(|capture| (selection, capture)),
        });
    }

    ensure_config_current(&config_path, &config_revision)?;
    let _mutation = if dry_run {
        None
    } else {
        std::fs::create_dir_all(&vault_root).map_err(|e| miette::miette!("create vault: {e}"))?;
        Some(
            if wait_for_vault {
                eprintln!("Waiting for exclusive vault access...");
                lk_vault::MutationSession::acquire_waiting(&vault_root)
            } else {
                lk_vault::MutationSession::acquire(&vault_root)
            }
            .map_err(|e| miette::miette!("vault mutation: {e}"))?,
        )
    };

    ensure_config_current(&config_path, &config_revision)?;

    // Surface pending (undrained) queue files — never silently ignore them. The risk
    // differs by provider, so the message does too:
    //   - queue mode: exact requests reuse pending work; changed requests still need a drain.
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
                "pending queue files exist; run /lore-process to complete semantic work",
            );
            eprintln!(
                "! {pending} pending queue file(s) under {}/.lorekeeper/queue/. Run \
                 /lore-process to complete semantic work; identical pending requests are reused.",
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

    for acquisition in acquired {
        let id = &acquisition.id;
        let sc = &config.sources[id];
        let started_at = acquisition.started_at;
        observations.insert(
            id.clone(),
            super::synthesis::pending::ObservedSource::failed(id),
        );
        let (
            selection,
            SourceCapture {
                observation,
                originals: manual_originals,
                retirement_blocked: manual_retirement_blocked,
            },
        ) = match acquisition
            .result
            .and_then(|(selection, capture)| capture.restore().map(|source| (selection, source)))
        {
            Ok(capture) => capture,
            Err(error) => {
                eprintln!("  ✗ {id} extract: {error}");
                had_failure = true;
                if !dry_run {
                    record_failure(&log, id, &started_at, &error.to_string(), &mut had_failure)
                        .await;
                }
                continue;
            }
        };
        eprintln!("▸ plan {id} ({})", sc.source_type);
        let mut observed =
            super::synthesis::pending::ObservedSource::from_observation(id, &observation);
        let observation_outcome = observation.outcome;
        if observation_outcome == lk_source::ObservationOutcome::Partial {
            eprintln!(
                "  ! partial observation: source reach is incomplete; rerun the same ingest command with RUST_LOG=warn and --dry-run for provider diagnostics"
            );
            had_failure = true;
            let descriptor = sc.source_type.descriptor();
            if descriptor.unit == lk_core::config::ObservationUnit::Day && !descriptor.streaming {
                observations.insert(id.clone(), observed);
                let error = "incomplete source observation cannot replace a daily snapshot; existing pages and task observations were preserved";
                eprintln!("  ✗ {error}");
                if !dry_run {
                    record_failure(&log, id, &started_at, error, &mut had_failure).await;
                }
                continue;
            }
        }
        let raw = observation.items;
        eprintln!("  extracted: {} items", raw.len());

        // Open a per-source queue transaction: if plan fails partway it may have already
        // buffered LLM tasks for pages it never finished writing — roll those back so the
        // publication never retains requests for an abandoned target.
        llm.begin_source().await;
        let source_options = lk_pipeline::IngestOptions {
            target_date: if selection == lk_source::Selection::DocumentSnapshot {
                None
            } else {
                options.target_date
            },
            today: options.today,
            dry_run: options.dry_run,
        };
        let result = match pipeline.plan(id, sc, raw, &source_options).await {
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

        observed.personal_inputs = config.personal.as_ref().map(|personal| {
            lk_pipeline::work_log_inputs(id, &result.events, personal, config.vault.locale(), today)
        });
        observations.insert(id.clone(), observed);

        // Keep ALL captures for dependency/retirement guards, including entries the
        // pipeline excluded. Publication below selects only actually rendered events.
        // Written BEFORE the empty-pages skip, and written even when the list is empty: a
        // source can be open-work-only in a run that renders nothing, and an empty answer is
        // what retires proposals whose work is now finished. `--dry-run` writes nothing at all.
        let records_current_intent = !dry_run
            && target_date.is_none_or(|date| date == today)
            && config.personal.as_ref().is_some_and(|p| p.tasks.is_some());
        if records_current_intent {
            // Held for the write and no longer. These are the intent plane's files, so the
            // command writing them holds the plane whoever it is — scoped to `lore task`, the
            // guard was named for the first writer that needed it and left every other one
            // beside it unguarded. Held across the whole ingest instead, a fetch that takes
            // minutes would lock a person out of their own board for those minutes.
            if let Err(e) = lk_task::PlaneLock::hold(&vault_root)
                .map_err(|why| lk_task::TaskError::Malformed(why.to_string()))
                .and_then(|_held| lk_task::Candidates::new(&vault_root).record(id, &result.work))
            {
                eprintln!("  ✗ proposals: {e}");
                had_failure = true;
            }
        }

        // A scheduled source's items are appointments, which the agenda reports beside the
        // day's tasks and nothing ever proposes. One SNAPSHOT per source, replaced whole and
        // written even when EMPTY — that is the answer that clears a day whose every meeting
        // was cancelled, which a file per date could not say.
        if records_current_intent && sc.source_type.descriptor().scheduled {
            let appointments: Vec<_> = result
                .appointments
                .iter()
                .map(|(at, title)| lk_task::Appointment {
                    at: *at,
                    title: title.clone(),
                })
                .collect();
            if let Err(e) = lk_task::PlaneLock::hold(&vault_root)
                .map_err(|why| lk_task::TaskError::Malformed(why.to_string()))
                .and_then(|_held| lk_task::Schedule::new(&vault_root).record(id, &appointments))
            {
                eprintln!("  ✗ agenda: {e}");
                had_failure = true;
            }
        }

        // Skip only when there are no pages to write. (A manual source with an empty
        // inbox lands here too; with no events its archive step is a no-op.)
        if result.is_empty() {
            let (status, error) = source_log_outcome(observation_outcome, false, true);
            if observation_outcome == lk_source::ObservationOutcome::Partial {
                eprintln!("  ✗ no events retained from incomplete source reach");
            } else {
                eprintln!("  — skipped (no events)");
            }
            if !dry_run {
                record_log(
                    &log,
                    &lk_vault::LogEntry {
                        timestamp: jiff::Timestamp::now(),
                        source_id: id.clone(),
                        status,
                        event_count: 0,
                        duration_ms: started_at.elapsed().as_millis() as u64,
                        error: error.map(str::to_owned),
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

        let rss_bytes = result
            .daily_pages
            .iter()
            .filter_map(|p| p.rss_original.as_ref())
            .flat_map(|c| &c.payloads)
            .map(Vec::len)
            .sum::<usize>();
        rss_capture_bytes = rss_capture_bytes.saturating_add(rss_bytes);
        if rss_capture_bytes > 256 * 1024 * 1024 {
            return Err(miette::miette!(
                "RSS originals exceed ingest-wide 256 MiB limit; split the ingest"
            ));
        }
        if dry_run {
            for capture in &manual_originals {
                if !result.events.iter().any(|event| {
                    event.metadata.get("source_file").and_then(|v| v.as_str())
                        == Some(capture.source_file.as_str())
                }) {
                    continue;
                }
                let shared = manual_originals.iter().any(|other| {
                    other.manifest.entry != capture.manifest.entry
                        && other
                            .manifest
                            .members
                            .iter()
                            .any(|member| member.path == capture.manifest.entry)
                });
                let retirement = if sc
                    .params
                    .get("archive_after_ingest")
                    .and_then(|v| v.as_bool())
                    == Some(false)
                {
                    "disabled"
                } else if manual_retirement_blocked
                    || shared
                    || !capture.manifest.unsupported.is_empty()
                {
                    "blocked"
                } else {
                    "eligible only after publication and unchanged-byte check"
                };
                let reference = capture
                    .manifest
                    .reference()
                    .map_err(|e| miette::miette!("{e}"))?;
                eprintln!(
                    "  [dry-run] original: {} ({} bytes, {} members, {} unsupported constructs; retirement {})",
                    reference.manifest_path,
                    capture.byte_len(),
                    capture.manifest.members.len(),
                    capture.manifest.unsupported.len(),
                    retirement
                );
            }
            for out in &result.daily_pages {
                report_discards(out, dry_run);
                eprintln!(
                    "  [dry-run] would write: {}{}",
                    out.path,
                    event_count(&out.content)
                );
            }
            for out in &result.document_pages {
                report_discards(out, dry_run);
                eprintln!("  [dry-run] would write: {} (document)", out.path);
            }
            continue;
        }

        planned.push(Planned {
            observation_outcome,
            manual_originals,
            manual_retirement_blocked,
            id: id.clone(),
            result,
            started_at,
        });
    }

    ensure_config_current(&config_path, &config_revision)?;
    if dry_run {
        // Concept pages are accumulated across all sources, so preview them once.
        let concept_pages = pipeline
            .render_concept_pages()
            .await
            .map_err(|e| miette::miette!("concept render: {e}"))?;
        for out in &concept_pages {
            eprintln!("  [dry-run] would write: {}", out.path);
            super::report_refusals(out, true);
        }
        if had_failure {
            return Err(miette::miette!(
                "dry-run completed with source failures; see output above"
            ));
        }
        return Ok(());
    }

    let mut publication = super::publication::Publication::new(
        _mutation
            .as_ref()
            .expect("writing ingest holds mutation session"),
        llm.clone(),
    );

    let mut total_pages = 0usize;
    let mut any_write_failed = false;
    let mut all_personal: Vec<lk_core::event::Event> = Vec::new();

    for p in &planned {
        for capture in &p.manual_originals {
            if !p.result.events.iter().any(|event| {
                event.metadata.get("source_file").and_then(|v| v.as_str())
                    == Some(capture.source_file.as_str())
            }) {
                continue;
            }
            if let Err(error) = lk_vault::publish_manual_original(
                _mutation
                    .as_ref()
                    .expect("writing ingest holds mutation session"),
                capture,
            ) {
                eprintln!("  ✗ original publication: {error}");
                any_write_failed = true;
                break;
            }
        }
        if any_write_failed {
            break;
        }
        let outputs = p.result.daily_pages.iter().chain(&p.result.document_pages);
        for out in outputs.clone() {
            report_discards(out, false);
        }
        if let Err(error) = publication.publish(outputs.clone()).await {
            eprintln!("  ✗ source publication {}: {error}", p.id);
            any_write_failed = true;
            break;
        }
        for out in outputs {
            total_pages += 1;
            eprintln!("  ✓ wrote: {} ({})", out.path, p.id);
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
        if let Err(error) = publication.publish(&concept_pages).await {
            eprintln!("  ✗ concept publication: {error}");
            any_write_failed = true;
        } else {
            for out in &concept_pages {
                total_pages += 1;
                eprintln!("  ✓ wrote: {}", out.path);
                super::report_refusals(out, false);
            }
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
    let mut published_work_logs = std::collections::BTreeMap::new();
    if !any_write_failed && source.is_none() && !all_personal.is_empty() {
        let work_logs = pipeline
            .render_work_log(&all_personal, today)
            .await
            .map_err(|e| miette::miette!("work-log: {e}"))?;
        for out in &work_logs {
            report_discards(out, false);
        }
        let mut bindings = std::collections::BTreeMap::new();
        for out in &work_logs {
            let page = lk_core::frontmatter::parse_page(&out.content)
                .map_err(|e| miette::miette!("work-log publication binding: {e}"))?;
            let day = out
                .path
                .as_ref()
                .file_stem()
                .and_then(|name| name.to_str())
                .and_then(|name| name.parse::<jiff::civil::Date>().ok())
                .ok_or_else(|| miette::miette!("work-log publication has no exact day"))?;
            let input = page
                .frontmatter
                .get(lk_core::frontmatter::field::LLM_INPUTS)
                .and_then(|value| {
                    value.get(lk_queue::TargetKind::WorkLogSynthesis.llm_inputs_key())
                })
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    miette::miette!("work-log publication lacks its typed synthesis input")
                })?;
            bindings.insert(day, input.to_owned());
        }
        if let Err(error) = publication.publish(&work_logs).await {
            eprintln!("✗ work-log publication: {error}");
            any_write_failed = true;
        } else {
            published_work_logs = bindings;
            for out in &work_logs {
                total_pages += 1;
                eprintln!("▸ work-log → {}", out.path);
            }
        }
    } else if source.is_some() && !all_personal.is_empty() {
        eprintln!(
            "  work-log not refreshed: partial ingest of '{}'. Run `lore ingest` (all sources) to rebuild the day's work-log.",
            source.as_deref().unwrap_or_default()
        );
    }

    if !any_write_failed {
        publication.finish().await?;
        super::synthesis::pending::record_collection(
            &config,
            _mutation
                .as_ref()
                .expect("writing ingest holds mutation session"),
            super::synthesis::pending::CollectionPublication {
                requested_day: extract_target,
                full_ingest: source.is_none(),
                started_at: collection_started_at,
                targeted: target_date.is_some(),
                work_logs: published_work_logs,
            },
            observations.into_values().collect(),
        )?;
    }

    // Phase 5: Record the per-source ingest-log entry. The run is idempotent — daily
    // pages are materialized views re-rendered in full each run — so a write failure
    // anywhere just leaves the affected pages for the next run to reproduce; there is no
    // commit point to roll back.
    for p in &planned {
        let (status, error) = source_log_outcome(p.observation_outcome, any_write_failed, false);
        record_log(
            &log,
            &lk_vault::LogEntry {
                timestamp: jiff::Timestamp::now(),
                source_id: p.id.clone(),
                status,
                event_count: p.result.events.len(),
                duration_ms: p.started_at.elapsed().as_millis() as u64,
                error: error.map(str::to_owned),
            },
            &mut had_failure,
        )
        .await;
    }

    // Retire inbox entries only after all page/request publications succeeded.
    // Independent collection failures do not strand a successfully published source.
    if !any_write_failed {
        for p in &planned {
            if p.manual_retirement_blocked {
                tracing::warn!(source=%p.id,"manual: entry capture failures; retaining this source's inbox entries");
                continue;
            }
            let sc = sources.iter().find(|(id, _)| id == &p.id).map(|(_, sc)| sc);
            if let Some(sc) = sc
                && sc.source_type == lk_core::config::SourceType::Manual
                && let Err(e) = lk_source::archive_manual_originals(
                    &sc.params,
                    &p.result.events,
                    &p.manual_originals,
                    &extract_ctx,
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
            // Not "safe to re-run", and not "re-running reproduces the page" either. Two things
            // stand between the two: the cause may persist (a refused page-format change does
            // until the file or the config is dealt with), and a bare re-run refetches the
            // CURRENT window rather than replaying a log — only the manual inbox and RSS keep
            // the events on disk, so for every remote source a page for an earlier date is
            // reproduced only by asking for that date. The per-write line above names the page,
            // whose date is the one to pass.
            " (some writes failed — see the errors above; once the cause is dealt with, \
             re-running reproduces a page for the current window, and an earlier date needs \
             `lore ingest <source> --date <date>` because remote sources are refetched rather \
             than replayed)"
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

/// Health reads these persisted statuses: publication success cannot turn partial
/// provider reach into a successful collection, including an empty partial result.
fn source_log_outcome(
    outcome: lk_source::ObservationOutcome,
    write_failed: bool,
    quiet: bool,
) -> (lk_vault::LogStatus, Option<&'static str>) {
    use lk_source::ObservationOutcome;
    use lk_vault::LogStatus;
    match (write_failed, outcome == ObservationOutcome::Partial, quiet) {
        (true, true, _) => (
            LogStatus::Failed,
            Some("vault write failed; source reach is incomplete (partial observation)"),
        ),
        (true, false, _) => (LogStatus::Failed, Some("vault write failed")),
        (false, true, _) => (
            LogStatus::Failed,
            Some(
                "source reach is incomplete (partial observation); retained events do not represent complete collection",
            ),
        ),
        (false, false, true) => (LogStatus::Skipped, None),
        (false, false, false) => (LogStatus::Success, None),
    }
}

#[cfg(test)]
mod observation_log_tests {
    use super::source_log_outcome;
    use lk_source::ObservationOutcome;
    use lk_vault::LogStatus;

    #[tokio::test]
    async fn collection_deadline_cancels_waiting_work_and_retains_source_errors() {
        let seconds = std::num::NonZeroU64::new(1).unwrap();
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        struct PendingGuard(std::sync::Arc<std::sync::atomic::AtomicBool>);
        impl Drop for PendingGuard {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let started = std::time::Instant::now() - std::time::Duration::from_millis(980);
        let guard = PendingGuard(dropped.clone());
        let result = super::collect_with_budget(started, seconds, async move {
            let _guard = guard;
            std::future::pending::<Result<(), lk_source::SourceError>>().await
        })
        .await;
        assert!(matches!(
            result,
            Err(lk_source::SourceError::CollectionTimeout { seconds: 1 })
        ));
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
        let failed = super::collect_with_budget(std::time::Instant::now(), seconds, async {
            Err::<(), _>(lk_source::SourceError::Parse("provider failure".into()))
        })
        .await;
        assert!(
            matches!(failed, Err(lk_source::SourceError::Parse(message)) if message == "provider failure")
        );
    }

    #[tokio::test]
    async fn blocking_return_after_the_collection_budget_is_not_success() {
        let started = std::time::Instant::now() - std::time::Duration::from_millis(900);
        let result =
            super::collect_with_budget(started, std::num::NonZeroU64::new(1).unwrap(), async {
                std::thread::sleep(std::time::Duration::from_millis(150));
                Ok(())
            })
            .await;
        assert!(matches!(
            result,
            Err(lk_source::SourceError::CollectionTimeout { seconds: 1 })
        ));
    }

    #[test]
    fn partial_reach_never_becomes_success_or_a_quiet_skip() {
        for quiet in [false, true] {
            let (status, error) = source_log_outcome(ObservationOutcome::Partial, false, quiet);
            assert_eq!(status, LogStatus::Failed);
            assert!(error.unwrap().contains("source reach is incomplete"));
        }
        assert_eq!(
            source_log_outcome(ObservationOutcome::Observed, false, true),
            (LogStatus::Skipped, None)
        );
        assert_eq!(
            source_log_outcome(ObservationOutcome::Observed, false, false),
            (LogStatus::Success, None)
        );
        let (status, error) = source_log_outcome(ObservationOutcome::Partial, true, false);
        assert_eq!(status, LogStatus::Failed);
        let error = error.unwrap();
        assert!(
            error.contains("vault write failed") && error.contains("source reach is incomplete")
        );
    }
}
