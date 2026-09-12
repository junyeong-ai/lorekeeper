//! `lore brief` — the day's knowledge, for the person it was collected for.
//!
//! The vault fills every morning and nothing carried any of it back: `lore agenda` answers
//! for the board, and the pages themselves are read by whoever opens them, which is nobody.
//! This is the other half of a working day — what the sources taught, beside what the board
//! promised.
//!
//! A view, like the agenda. It writes nothing and derives nothing: the concept pages already
//! record when each entered the vault and when it was last cited, so this reads the day off
//! them rather than computing a second answer to the same question.

use super::{GlobalOptions, find_config, load_config};

pub async fn run(opts: &GlobalOptions, date: Option<String>, json: bool) -> miette::Result<()> {
    let config = load_config(&find_config(opts)?)?;
    let zone = config.vault.timezone();
    let today = jiff::Zoned::now().with_time_zone(zone).date();
    let date = super::parse_date(date.as_deref(), today)?;

    let brief = lk_vault::build_brief(&config.vault.root_path(), &config.vault.dirs, date)
        .map_err(|e| miette::miette!("{e}"))?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&brief).map_err(|e| miette::miette!("{e}"))?
        );
        return Ok(());
    }

    eprintln!(
        "{date}  {} learned · {} revisited",
        brief.learned.len(),
        brief.revisited.len()
    );
    if brief.learned.is_empty() && brief.revisited.is_empty() {
        eprintln!(
            "\nno concept page records this day — `lore health` says whether the sources ran"
        );
        return Ok(());
    }

    let mut category = None;
    for entry in &brief.learned {
        if category.as_ref() != Some(&entry.category) {
            eprintln!("\n{}", entry.category.as_deref().unwrap_or("—"));
            category = Some(entry.category.clone());
        }
        println!(
            "  {}  {}",
            super::pad(&entry.id, 40),
            entry.statement.as_deref().unwrap_or_default()
        );
    }

    // Names only. A concept the vault already held is the day confirming what it knows, and
    // restating each one is the flood this exists to reduce.
    if !brief.revisited.is_empty() {
        eprintln!("\nrevisited");
        println!(
            "  {}",
            brief
                .revisited
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}
