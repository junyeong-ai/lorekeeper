use super::{find_config, load_config};

pub async fn run(opts: &super::GlobalOpts) -> miette::Result<()> {
    let config = load_config(&find_config(opts)?)?;
    let vault_root = config.vault.root_path();
    let reader = wi_vault::VaultReader::new(&vault_root);

    let work_log_dir = std::path::Path::new(&config.vault.dirs.personal).join("work-log");
    let files = reader
        .list_markdown(&work_log_dir)
        .await
        .map_err(|e| miette::miette!("read work-log dir {}: {e}", work_log_dir.display()))?;

    if files.is_empty() {
        eprintln!("No work-log data found at {}.", work_log_dir.display());
        return Ok(());
    }

    let recent: Vec<_> = files.iter().rev().take(30).collect();
    let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
    let mut total = 0usize;

    for file in &recent {
        let page = reader
            .read_page(file)
            .await
            .map_err(|e| miette::miette!("read {}: {e}", file.display()))?;
        if let Some(page) = page
            && let Some(cats) = page
                .frontmatter
                .get("categories")
                .and_then(|v| v.as_array())
        {
            for cat in cats {
                if let Some(s) = cat.as_str() {
                    *counts.entry(s.to_string()).or_insert(0) += 1;
                    total += 1;
                }
            }
        }
    }

    if total == 0 {
        eprintln!("No category data in recent {} work-logs.", recent.len());
        return Ok(());
    }

    eprintln!("Recent {} work-logs — category distribution:", recent.len());
    let max_count = counts.values().copied().max().unwrap_or(1);
    for (cat, count) in &counts {
        let pct = (*count as f64 / total as f64 * 100.0) as u32;
        let bar_len = (count * 30 / max_count).max(1);
        let bar = "█".repeat(bar_len);
        eprintln!("  {cat:30} {bar} {count} ({pct}%)");
    }
    Ok(())
}
