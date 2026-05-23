use std::sync::Arc;

use super::{build_llm_client, find_config, load_config, parse_date, resolve_template_dir};

pub enum Period {
    Weekly {
        date: Option<String>,
        previous: bool,
    },
    Monthly {
        date: Option<String>,
        previous: bool,
    },
    Quarterly {
        date: Option<String>,
        previous: bool,
    },
    Annual {
        year: Option<i32>,
        previous: bool,
    },
}

pub async fn run(opts: &super::GlobalOpts, period: Period) -> miette::Result<()> {
    let config = load_config(&find_config(opts)?)?;
    let vault_root = config.vault.root_path();
    let llm = build_llm_client(&config, &vault_root);

    let ctx = Arc::new(
        wi_pipeline::PipelineContext::new(
            &resolve_template_dir(opts, &vault_root),
            llm.clone(),
            &config,
        )
        .map_err(|e| miette::miette!("{e}"))?,
    );
    let synth = wi_pipeline::Synthesizer::new(&vault_root, ctx, &config);
    let writer = wi_vault::VaultWriter::new(&vault_root);

    let tz = config.vault.timezone();
    let today = jiff::Timestamp::now().to_zoned(tz).date();

    match period {
        Period::Weekly { date, previous } => {
            let target = resolve_weekly_target(date.as_deref(), previous, today)?;
            let mut wrote = 0u32;

            if let Some(out) = synth
                .weekly_synthesis(target)
                .await
                .map_err(|e| miette::miette!("{e}"))?
            {
                writer
                    .write_page(out.path.as_ref(), &out.content)
                    .await
                    .map_err(|e| miette::miette!("write: {e}"))?;
                eprintln!("✓ wrote: {}", out.path);
                wrote += 1;
            }

            if let Some(out) = synth
                .weekly_personal(target)
                .await
                .map_err(|e| miette::miette!("{e}"))?
            {
                writer
                    .write_page(out.path.as_ref(), &out.content)
                    .await
                    .map_err(|e| miette::miette!("write: {e}"))?;
                eprintln!("✓ wrote: {}", out.path);
                wrote += 1;
            }

            if wrote == 0 {
                eprintln!("No source data found for week of {target}.");
            }
        }
        Period::Monthly { date, previous } => {
            let (year, month) = resolve_monthly_target(date.as_deref(), previous, today)?;
            match synth
                .monthly(year, month)
                .await
                .map_err(|e| miette::miette!("{e}"))?
            {
                Some(out) => {
                    writer
                        .write_page(out.path.as_ref(), &out.content)
                        .await
                        .map_err(|e| miette::miette!("write: {e}"))?;
                    eprintln!("✓ wrote: {}", out.path);
                }
                None => eprintln!("No work-log data found for {year}-{month:02}."),
            }
        }
        Period::Quarterly { date, previous } => {
            let (year, quarter) = resolve_quarterly_target(date.as_deref(), previous, today)?;
            match synth
                .quarterly(year, quarter)
                .await
                .map_err(|e| miette::miette!("{e}"))?
            {
                Some(out) => {
                    writer
                        .write_page(out.path.as_ref(), &out.content)
                        .await
                        .map_err(|e| miette::miette!("write: {e}"))?;
                    eprintln!("✓ wrote: {}", out.path);
                }
                None => eprintln!("No data found for {year}-Q{quarter}."),
            }
        }
        Period::Annual { year, previous } => {
            let target_year = if previous {
                today.year() - 1
            } else {
                year.map(|y| y as i16).unwrap_or_else(|| today.year())
            };
            match synth
                .annual(target_year)
                .await
                .map_err(|e| miette::miette!("{e}"))?
            {
                Some(out) => {
                    writer
                        .write_page(out.path.as_ref(), &out.content)
                        .await
                        .map_err(|e| miette::miette!("write: {e}"))?;
                    eprintln!("✓ wrote: {}", out.path);
                }
                None => eprintln!("No quarterly data found for {target_year}."),
            }
        }
    }

    // Persist any buffered queue tasks emitted by the synthesizer. Without this,
    // queue-mode synthesis runs would write narrative pages with empty bodies and
    // drop the corresponding LLM tasks at process exit. Same ordering as `wi ingest`:
    // flush before exit so the queue file is durable.
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
    let quarter = wi_core::vault_path::quarter_of_month(target.month() as u8);
    Ok((target.year(), quarter))
}
