use super::{find_config, load_config};

pub async fn run(opts: &super::GlobalOptions, strict: bool) -> miette::Result<()> {
    let config = load_config(&find_config(opts)?)?;
    let vault_root = config.vault.root_path();
    let tz = config.vault.timezone();
    let log = lk_vault::IngestLog::new(vault_root.join(".lorekeeper").join("ingest.jsonl"));

    let now = jiff::Timestamp::now();
    // A source is stale once TWO ingest fires have come due since its last success
    // (one missed run of grace) — anchored at the last success, NOT at `now`, so the window
    // follows the real schedule sequence including weekend/off-day gaps. For `0 9 * * 1-5`
    // a Friday-morning success is not stale over the weekend (the next fire is Monday);
    // it only goes stale once Tuesday's fire has also passed unfilled. Ingestion is a
    // single all-source run (`ingest.schedule`), so every source shares one cadence; when
    // it is unset, a flat 48h window applies.
    const DEFAULT_STALE_AFTER_SECS: i64 = 48 * 3600;
    let ingest_schedule = config.ingest.schedule.as_deref();
    let is_stale = |last: jiff::Timestamp| -> bool {
        match ingest_schedule {
            Some(expr) => {
                match lk_core::cron::next_fire_after(expr, last, &tz)
                    .and_then(|first| lk_core::cron::next_fire_after(expr, first, &tz))
                {
                    Some(second_due) => now > second_due,
                    // Unparseable schedule (shouldn't happen post-validation) → flat window.
                    None => now.as_second() - last.as_second() > DEFAULT_STALE_AFTER_SECS,
                }
            }
            None => now.as_second() - last.as_second() > DEFAULT_STALE_AFTER_SECS,
        }
    };

    let mut fresh = 0u32;
    let mut stale = 0u32;
    let mut never = 0u32;

    for (id, sc) in config.enabled_sources() {
        let last = log
            .find_last_success(id)
            .await
            .map_err(|e| miette::miette!("read ingest log: {e}"))?;
        match last {
            Some(entry) => {
                let hours = (now.as_second() - entry.timestamp.as_second()) / 3600;
                if is_stale(entry.timestamp) {
                    eprintln!("⚠ {id} ({}) — {}h ago, STALE", sc.source_type, hours);
                    stale += 1;
                } else {
                    eprintln!("✓ {id} ({}) — {}h ago", sc.source_type, hours);
                    fresh += 1;
                }
            }
            None => {
                eprintln!("✗ {id} ({}) — never ingested", sc.source_type);
                never += 1;
            }
        }
    }

    eprintln!("\n{fresh} fresh, {stale} stale, {never} never");

    if stale > 0 || (strict && never > 0) {
        std::process::exit(1);
    }
    Ok(())
}
