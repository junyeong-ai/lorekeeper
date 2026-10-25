use std::sync::Arc;

use super::{build_llm_client, find_config, parse_date};

pub mod pending;

/// Synthesis periods. This is the SINGLE clap surface for `lore synthesis <period>` —
/// there is no parallel CLI enum to keep in sync. `--date`/`--year` and `--previous`
/// are mutually exclusive (a period is targeted either by an explicit date or as
/// "the just-completed one", never both).
#[derive(clap::Subcommand)]
pub enum Period {
    /// Resume durable closed-period obligations, or inspect them without writes
    Pending {
        /// Resume only this dependency stage (default: all stages)
        #[arg(long, conflicts_with = "reconcile")]
        stage: Option<pending::ResumeStage>,
        /// Print the durable intent report as JSON
        #[arg(long)]
        json: bool,
        /// Inspect durable intent status without resuming or writing
        #[arg(long, conflicts_with = "reconcile")]
        status: bool,
        /// Register bounded missing closed periods from synthesis.recovery without processing them
        #[arg(long)]
        reconcile: bool,
    },
    /// Cross-source themes + personal weekly review
    Weekly {
        /// Any date inside the target week, `YYYY-MM-DD` (default: this week)
        #[arg(long, conflicts_with = "previous")]
        date: Option<String>,
        /// Synthesize the just-completed period (last week)
        #[arg(long)]
        previous: bool,
        /// Record this exact closed period for dependency-aware scheduled processing
        #[arg(long)]
        defer: bool,
    },
    /// Personal monthly performance review
    Monthly {
        /// Any date inside the target month, `YYYY-MM-DD` (default: this month)
        #[arg(long, conflicts_with = "previous")]
        date: Option<String>,
        /// Synthesize the just-completed period (last month)
        #[arg(long)]
        previous: bool,
        /// Record this exact closed period for dependency-aware scheduled processing
        #[arg(long)]
        defer: bool,
    },
    /// Personal quarterly performance review
    Quarterly {
        /// Any date inside the target quarter, `YYYY-MM-DD` (default: this quarter)
        #[arg(long, conflicts_with = "previous")]
        date: Option<String>,
        /// Synthesize the just-completed period (last quarter)
        #[arg(long)]
        previous: bool,
        /// Record this exact closed period for dependency-aware scheduled processing
        #[arg(long)]
        defer: bool,
    },
    /// Personal annual performance review
    Annual {
        /// The target calendar year (default: this year)
        // `i16` matches `jiff::civil::Date::year()` and `try_annual_review`'s domain, so an
        // out-of-range `--year` is rejected by clap at parse time rather than silently
        // truncated by an `as i16` downcast.
        #[arg(long, conflicts_with = "previous")]
        year: Option<i16>,
        /// Synthesize the just-completed period (last year)
        #[arg(long)]
        previous: bool,
        /// Record this exact closed period for dependency-aware scheduled processing
        #[arg(long)]
        defer: bool,
    },
}

pub async fn run(
    opts: &super::GlobalOptions,
    period: Period,
    wait_for_vault: bool,
) -> miette::Result<()> {
    let config_path = find_config(opts)?;
    let (config, revision) = super::load_config_with_revision(&config_path)?;
    let pin = super::ConfigurationPin::from_loaded(config_path, revision);
    let vault_root = config.vault.root_path();
    let tz = config.vault.timezone();
    let today = jiff::Timestamp::now().to_zoned(tz).date();

    if let Period::Pending {
        status: true, json, ..
    } = &period
    {
        return pending::report(&vault_root, *json);
    }

    // Hold the shared vault writer lease before reading evidence or existing pages.
    // External editors do not participate; publication still validates its own inputs.
    std::fs::create_dir_all(&vault_root).map_err(|e| miette::miette!("create vault: {e}"))?;
    let _mutation = super::publication::acquire_mutation(&vault_root, wait_for_vault, &pin).await?;
    let vault_root = _mutation.root().to_path_buf();
    if let Period::Pending {
        reconcile: true,
        json,
        ..
    } = period
    {
        return pending::reconcile(&config, &_mutation, today, json);
    }
    if let Period::Pending { stage, json, .. } = period {
        return pending::resume(&config, opts, &_mutation, stage, json, today).await;
    }
    if let Some(periods) = deferred_periods(&period, today, &config)? {
        pending::defer(&config, &_mutation, periods, today)?;
        eprintln!(
            "Closed-period intent retained; resume after collection and semantic processing."
        );
        return Ok(());
    }
    let llm = build_llm_client(&config, &vault_root)?;

    let ctx = Arc::new(
        lk_pipeline::PipelineContext::build(opts.template_dir.as_deref(), llm.clone(), &config)
            .map_err(|e| miette::miette!("{e}"))?,
    );

    let synth = lk_pipeline::Synthesizer::new(&vault_root, ctx, &config, today);
    let mut publication = super::publication::Publication::new(&_mutation, llm.clone());

    let personal_on = config.personal.is_some();
    let outputs: Vec<lk_pipeline::RenderResult> = match period {
        // The personal-review periods belong to the personal module; report the real
        // reason rather than letting the Synthesizer's empty result read as "no data".
        Period::Monthly { .. } | Period::Quarterly { .. } | Period::Annual { .. }
            if !personal_on =>
        {
            eprintln!("Personal reviews need a `personal:` section in config.yaml.");
            Vec::new()
        }
        Period::Weekly { date, previous, .. } => {
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
                .try_weekly_review(target)
                .await
                .map_err(|e| miette::miette!("{e}"))?
            {
                outs.push(out);
            }
            if outs.is_empty() {
                if !personal_on && config.synthesis.weekly.include_sources.is_empty() {
                    eprintln!(
                        "Weekly synthesis produced nothing: include_sources is empty and \
                         there is no `personal:` section — nothing is configured to run."
                    );
                } else {
                    eprintln!("No source data found for week of {target}.");
                }
            }
            outs
        }
        Period::Monthly { date, previous, .. } => {
            let (year, month) = resolve_monthly_target(date.as_deref(), previous, today)?;
            match synth
                .try_monthly_review(year, month)
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
        Period::Quarterly { date, previous, .. } => {
            let (year, quarter) = resolve_quarterly_target(date.as_deref(), previous, today)?;
            match synth
                .try_quarterly_review(year, quarter)
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
        Period::Annual { year, previous, .. } => {
            let target_year = if previous {
                today.year() - 1
            } else {
                year.unwrap_or_else(|| today.year())
            };
            match synth
                .try_annual_review(target_year)
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
        Period::Pending { .. } => unreachable!("pending processing returned before direct preview"),
    };

    for out in &outputs {
        super::ingest::report_discards(out, false);
    }
    publication.publish(&outputs).await?;
    publication.finish().await?;
    for out in &outputs {
        eprintln!("✓ wrote: {}", out.path);
    }

    Ok(())
}

fn deferred_periods(
    period: &Period,
    today: jiff::civil::Date,
    config: &lk_core::config::Config,
) -> miette::Result<Option<Vec<pending::ClosedPeriod>>> {
    use pending::{ClosedPeriod, Kind};
    let result = match period {
        Period::Weekly {
            date,
            previous,
            defer: true,
        } => {
            let date = resolve_weekly_target(date.as_deref(), *previous, today)?;
            let mut periods = vec![];
            if !config.synthesis.weekly.include_sources.is_empty() {
                periods.push(ClosedPeriod::containing(Kind::WeeklyThemes, date)?);
            }
            if config.personal.is_some() {
                periods.push(ClosedPeriod::containing(Kind::WeeklyReview, date)?);
            }
            periods
        }
        Period::Monthly {
            date,
            previous,
            defer: true,
        } => {
            let (year, month) = resolve_monthly_target(date.as_deref(), *previous, today)?;
            vec![ClosedPeriod::containing(
                Kind::Monthly,
                jiff::civil::Date::new(year, month as i8, 1)
                    .map_err(|e| miette::miette!("month: {e}"))?,
            )?]
        }
        Period::Quarterly {
            date,
            previous,
            defer: true,
        } => {
            let (year, quarter) = resolve_quarterly_target(date.as_deref(), *previous, today)?;
            vec![ClosedPeriod::containing(
                Kind::Quarterly,
                jiff::civil::Date::new(year, ((quarter - 1) * 3 + 1) as i8, 1)
                    .map_err(|e| miette::miette!("quarter: {e}"))?,
            )?]
        }
        Period::Annual {
            year,
            previous,
            defer: true,
        } => {
            let year = if *previous {
                today.year() - 1
            } else {
                year.unwrap_or_else(|| today.year())
            };
            vec![ClosedPeriod::containing(
                Kind::Annual,
                jiff::civil::Date::new(year, 1, 1).map_err(|e| miette::miette!("year: {e}"))?,
            )?]
        }
        _ => return Ok(None),
    };
    Ok(Some(result))
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
