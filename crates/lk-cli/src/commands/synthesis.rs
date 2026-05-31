use std::sync::Arc;

use super::{build_llm_client, find_config, load_config, parse_date};

/// Synthesis periods. This is the SINGLE clap surface for `lore synthesis <period>` —
/// there is no parallel CLI enum to keep in sync. `--date`/`--year` and `--previous`
/// are mutually exclusive (a period is targeted either by an explicit date or as
/// "the just-completed one", never both).
#[derive(clap::Subcommand)]
pub enum Period {
    /// Cross-source themes + personal weekly review
    Weekly {
        #[arg(long, conflicts_with = "previous")]
        date: Option<String>,
        /// Synthesize the just-completed period (last week)
        #[arg(long)]
        previous: bool,
    },
    /// Personal monthly performance review
    Monthly {
        #[arg(long, conflicts_with = "previous")]
        date: Option<String>,
        /// Synthesize the just-completed period (last month)
        #[arg(long)]
        previous: bool,
    },
    /// Personal quarterly performance review
    Quarterly {
        #[arg(long, conflicts_with = "previous")]
        date: Option<String>,
        /// Synthesize the just-completed period (last quarter)
        #[arg(long)]
        previous: bool,
    },
    /// Personal annual performance review
    Annual {
        #[arg(long, conflicts_with = "previous")]
        year: Option<i32>,
        /// Synthesize the just-completed period (last year)
        #[arg(long)]
        previous: bool,
    },
}

pub async fn run(opts: &super::GlobalOptions, period: Period) -> miette::Result<()> {
    let config = load_config(&find_config(opts)?)?;
    let vault_root = config.vault.root_path();
    let llm = build_llm_client(&config, &vault_root)?;

    let ctx = Arc::new(
        lk_pipeline::PipelineContext::new(opts.template_dir.as_deref(), llm.clone(), &config)
            .map_err(|e| miette::miette!("{e}"))?,
    );
    let synth = lk_pipeline::Synthesizer::new(&vault_root, ctx, &config);
    let writer = lk_vault::VaultWriter::new(&vault_root);

    let tz = config.vault.timezone();
    let today = jiff::Timestamp::now().to_zoned(tz).date();

    // Plan every period's page upfront so all queue-mode LLM tasks land in the
    // buffer before ANY write happens. This decouples buffering from page writes:
    // if a write fails partway, we abort BEFORE flushing, so the buffered tasks are
    // dropped consistently — the same recovery story as `lore ingest`.
    let perf_on = config.performance.enabled;
    let outputs: Vec<lk_pipeline::RenderResult> = match period {
        // The personal-review periods are the performance subsystem; report the real
        // reason rather than letting the Synthesizer's empty result read as "no data".
        Period::Monthly { .. } | Period::Quarterly { .. } | Period::Annual { .. } if !perf_on => {
            eprintln!("Performance reviews are disabled (performance.enabled: false).");
            Vec::new()
        }
        Period::Weekly { date, previous } => {
            let target = resolve_weekly_target(date.as_deref(), previous, today)?;
            let mut outs = Vec::new();
            if let Some(out) = synth
                .try_weekly_synthesis(target)
                .await
                .map_err(|e| miette::miette!("{e}"))?
            {
                outs.push(out);
            }
            if let Some(out) = synth
                .try_weekly_personal(target)
                .await
                .map_err(|e| miette::miette!("{e}"))?
            {
                outs.push(out);
            }
            if outs.is_empty() {
                if !perf_on && config.synthesis.weekly.include_sources.is_empty() {
                    eprintln!(
                        "Weekly synthesis produced nothing: include_sources is empty and \
                         performance.enabled is false — nothing is configured to run."
                    );
                } else {
                    eprintln!("No source data found for week of {target}.");
                }
            }
            outs
        }
        Period::Monthly { date, previous } => {
            let (year, month) = resolve_monthly_target(date.as_deref(), previous, today)?;
            match synth
                .try_monthly_personal(year, month)
                .await
                .map_err(|e| miette::miette!("{e}"))?
            {
                Some(out) => vec![out],
                None => {
                    eprintln!("No work-log data found for {year}-{month:02}.");
                    vec![]
                }
            }
        }
        Period::Quarterly { date, previous } => {
            let (year, quarter) = resolve_quarterly_target(date.as_deref(), previous, today)?;
            match synth
                .try_quarterly_personal(year, quarter)
                .await
                .map_err(|e| miette::miette!("{e}"))?
            {
                Some(out) => vec![out],
                None => {
                    eprintln!("No data found for {year}-Q{quarter}.");
                    vec![]
                }
            }
        }
        Period::Annual { year, previous } => {
            let target_year = if previous {
                today.year() - 1
            } else {
                year.map(|y| y as i16).unwrap_or_else(|| today.year())
            };
            match synth
                .try_annual_personal(target_year)
                .await
                .map_err(|e| miette::miette!("{e}"))?
            {
                Some(out) => vec![out],
                None => {
                    eprintln!("No quarterly data found for {target_year}.");
                    vec![]
                }
            }
        }
    };

    for out in &outputs {
        writer
            .write_page(out.path.as_ref(), &out.content)
            .await
            .map_err(|e| miette::miette!("write {}: {e}", out.path))?;
        eprintln!("✓ wrote: {}", out.path);
    }

    // Flush only after every page write succeeded. Same atomicity guarantee as
    // `lore ingest`: the JSONL is only published if its target pages all exist.
    llm.flush()
        .await
        .map_err(|e| miette::miette!("queue flush: {e}"))?;

    Ok(())
}

fn resolve_weekly_target(
    date: Option<&str>,
    previous: bool,
    today: jiff::civil::Date,
) -> miette::Result<jiff::civil::Date> {
    if previous {
        today
            .checked_sub(jiff::Span::new().weeks(1))
            .map_err(|e| miette::miette!("date arithmetic: {e}"))
    } else {
        parse_date(date, today)
    }
}

fn resolve_monthly_target(
    date: Option<&str>,
    previous: bool,
    today: jiff::civil::Date,
) -> miette::Result<(i16, u8)> {
    let target = if previous {
        today
            .checked_sub(jiff::Span::new().months(1))
            .map_err(|e| miette::miette!("date arithmetic: {e}"))?
    } else {
        parse_date(date, today)?
    };
    Ok((target.year(), target.month() as u8))
}

fn resolve_quarterly_target(
    date: Option<&str>,
    previous: bool,
    today: jiff::civil::Date,
) -> miette::Result<(i16, u8)> {
    let target = if previous {
        today
            .checked_sub(jiff::Span::new().months(3))
            .map_err(|e| miette::miette!("date arithmetic: {e}"))?
    } else {
        parse_date(date, today)?
    };
    let quarter = lk_core::vault_path::quarter_of_month(target.month() as u8);
    Ok((target.year(), quarter))
}
