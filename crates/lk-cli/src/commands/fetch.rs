//! `lore fetch <url>` — the gesture a person actually makes.
//!
//! Someone reads something worth keeping and has a link. No feed reaches every publisher and
//! none ever will, so the vault's coverage is not what decides whether that link becomes
//! knowledge — whether there is a way to hand it over in one move is. There was not: the
//! inbox takes files, and nothing turned a URL into one.
//!
//! This writes the article into the inbox and stops. The next ingest reads it exactly as it
//! reads anything a person dropped there, so nothing downstream learns a new case.

use std::path::{Path, PathBuf};

use lk_core::concept::slugify;
use lk_core::config::{Config, SourceType};

use super::{GlobalOptions, find_config, load_config};

pub async fn run(opts: &GlobalOptions, url: &str, source: Option<String>) -> miette::Result<()> {
    let config_path = find_config(opts)?;
    let config = load_config(&config_path)?;
    let (source_id, inbox) = resolve_inbox(&config, source.as_deref())?;

    let http = lk_source::build_http_client().map_err(|e| miette::miette!("{e}"))?;
    let article = lk_source::fetch::readable(&http, url)
        .await
        .map_err(|e| miette::miette!("{e}"))?
        .ok_or_else(|| {
            miette::miette!(
                "no article body could be read from {url} — a listing page or one whose \
                 content is assembled in the browser has nothing to keep. Save the page as \
                 HTML and drop it in {} instead.",
                inbox.display()
            )
        })?;

    let title = article.title.unwrap_or_else(|| url.to_string());
    // The title and the address are frontmatter rather than prose so the ingest reads them as
    // fields: `source_url` becomes the document page's citation back to the original.
    let content = format!(
        "---\ntitle: {}\nsource_url: {}\n---\n\n{}\n",
        serde_json::to_string(&title).unwrap_or_else(|_| "\"\"".into()),
        serde_json::to_string(url).unwrap_or_else(|_| "\"\"".into()),
        article.markdown.trim()
    );
    tokio::fs::create_dir_all(&inbox)
        .await
        .map_err(|e| miette::miette!("create {}: {e}", inbox.display()))?;
    let path = free_path(&inbox, &title, &content);
    lk_core::fs::write_atomic(&path, content.as_bytes(), None)
        .map_err(|e| miette::miette!("write {}: {e}", path.display()))?;

    eprintln!("{title}");
    eprintln!("  {}", path.display());
    eprintln!("  `lore ingest {source_id}` turns it into a document page");
    Ok(())
}

/// The inbox a fetched article lands in, and the source that will read it.
///
/// Named rather than chosen where several exist: two inboxes mean two deliberately separate
/// streams, and picking one for the user would file the article under whichever happened to
/// be configured first.
fn resolve_inbox(config: &Config, requested: Option<&str>) -> miette::Result<(String, PathBuf)> {
    let manual: Vec<(&String, &lk_core::config::SourceConfig)> = config
        .sources
        .iter()
        .filter(|(_, sc)| sc.source_type == SourceType::Manual && sc.enabled)
        .collect();

    let (id, source) = match requested {
        Some(id) => manual
            .iter()
            .find(|(name, _)| name.as_str() == id)
            .copied()
            .ok_or_else(|| {
                miette::miette!(
                    "no enabled `manual` source is called `{id}`{}",
                    listing(&manual)
                )
            })?,
        None if manual.len() == 1 => manual[0],
        None if manual.is_empty() => {
            return Err(miette::miette!(
                "this config has no enabled `manual` source, so there is no inbox to fetch \
                 into — add one (see config.example.yaml) and the article has somewhere to land"
            ));
        }
        None => {
            return Err(miette::miette!(
                "several `manual` sources are enabled; name the one to fetch into with \
                 `--source <id>`{}",
                listing(&manual)
            ));
        }
    };

    let inbox = lk_source::manual_inbox_dir(&source.params, &config.vault.root_path())
        .map_err(|e| miette::miette!("{id}: {e}"))?;
    Ok((id.clone(), inbox))
}

fn listing(manual: &[(&String, &lk_core::config::SourceConfig)]) -> String {
    if manual.is_empty() {
        return String::new();
    }
    format!(
        " — enabled: {}",
        manual
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Where this article belongs in `inbox`.
///
/// The title names the file so the inbox stays readable. A name already taken by DIFFERENT
/// content takes a suffix from this article's own bytes, which keeps both properties that
/// matter: re-fetching one article always resolves to the one path it already occupies, and
/// two articles sharing a title never overwrite each other. A counter would satisfy the
/// second and break the first, minting a file per re-fetch.
fn free_path(inbox: &Path, title: &str, content: &str) -> PathBuf {
    let stem = slugify(title).unwrap_or_else(|| "article".into());
    let candidate = inbox.join(format!("{stem}.md"));
    match std::fs::read_to_string(&candidate) {
        Ok(held) if held != content => {
            let fingerprint = &blake3::hash(content.as_bytes()).to_hex()[..8];
            inbox.join(format!("{stem}-{fingerprint}.md"))
        }
        _ => candidate,
    }
}
