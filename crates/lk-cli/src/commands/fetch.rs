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
                "no readable body could be read from {url}. For browser-rendered pages, \
                 save the page as HTML and drop it in {} instead.",
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
    let path = publish_article(&inbox, &title, content.as_bytes())
        .map_err(|e| miette::miette!("preserve article in {}: {e}", inbox.display()))?;

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

    let inbox = lk_source::manual_inbox_dir(
        &source.params,
        &config.vault.root_path(),
        &config.vault.dirs,
    )
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

/// Preserve this article in `inbox`, testing every candidate through exclusive
/// publication rather than choosing a path and subsequently overwriting it.
///
/// The title names the file so the inbox stays readable. A name already taken by DIFFERENT
/// content takes a suffix from this article's own bytes, which keeps both properties that
/// matter: with the prior entries retained, re-fetching reuses its existing bytes, and
/// two articles sharing a title never overwrite each other. Longer digest prefixes
/// and finally a counter also handle occupied suffixes; every step reuses exact
/// existing bytes, so repeated fetches do not mint another file.
/// A name is taken by anything that is not this same article as a regular file. The inbox
/// scanner refuses a symlink, so writing under one would leave an article nothing ever
/// ingests — and the comparison itself must not read through one, since what it would compare
/// is a file somewhere else entirely.
fn publish_article(inbox: &Path, title: &str, content: &[u8]) -> std::io::Result<PathBuf> {
    use lk_core::fs::{PublishOutcome, publish_exclusive};

    let stem = bounded(&slugify(title).unwrap_or_else(|| "article".into()));
    let fingerprint = blake3::hash(content).to_hex().to_string();
    let names = [
        article_name(&stem, ""),
        article_name(&stem, &format!("-{}", &fingerprint[..8])),
        article_name(&stem, &format!("-{}", &fingerprint[..16])),
        article_name(&stem, &format!("-{fingerprint}")),
    ];
    for name in names {
        let path = inbox.join(name);
        match publish_exclusive(&path, content, None)? {
            PublishOutcome::Created | PublishOutcome::Reused => return Ok(path),
            PublishOutcome::Occupied => {}
        }
    }
    for counter in 1..=u64::MAX {
        let path = inbox.join(article_name(&stem, &format!("-{fingerprint}-{counter}")));
        match publish_exclusive(&path, content, None)? {
            PublishOutcome::Created | PublishOutcome::Reused => return Ok(path),
            PublishOutcome::Occupied => {}
        }
    }
    Err(std::io::Error::other("article name space exhausted"))
}

fn article_name(stem: &str, suffix: &str) -> String {
    let mut cut = stem.len().min(255 - suffix.len() - ".md".len());
    while !stem.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{suffix}.md", stem[..cut].trim_end_matches('-'))
}

/// A filename a filesystem will accept. The title is the page's, not the user's, and Linux
/// bounds one name at 255 BYTES — a long Korean or Japanese headline reaches that in a third
/// of its characters, so a fetch would succeed and the write fail. Cut on a character
/// boundary. `article_name` reserves any additional digest/counter suffix budget.
fn bounded(stem: &str) -> String {
    const MAX_STEM_BYTES: usize = 200;

    if stem.len() <= MAX_STEM_BYTES {
        return stem.to_owned();
    }
    let mut cut = MAX_STEM_BYTES;
    while cut > 0 && !stem.is_char_boundary(cut) {
        cut -= 1;
    }
    stem[..cut].trim_end_matches('-').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_checks_every_suffix_and_reuses_the_existing_article() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = b"new article";
        let fingerprint = blake3::hash(bytes).to_hex().to_string();
        let occupied = [
            "headline.md".to_owned(),
            format!("headline-{}.md", &fingerprint[..8]),
            format!("headline-{}.md", &fingerprint[..16]),
            format!("headline-{fingerprint}.md"),
            format!("headline-{fingerprint}-1.md"),
        ];
        for name in &occupied {
            std::fs::write(dir.path().join(name), name.as_bytes()).unwrap();
        }
        let path = publish_article(dir.path(), "Headline", bytes).unwrap();
        assert_eq!(
            path,
            dir.path().join(format!("headline-{fingerprint}-2.md"))
        );
        assert_eq!(
            publish_article(dir.path(), "Headline", bytes).unwrap(),
            path
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        for name in &occupied {
            assert_eq!(
                std::fs::read(dir.path().join(name)).unwrap(),
                name.as_bytes()
            );
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 6);
    }

    #[cfg(unix)]
    #[test]
    fn publication_skips_symlinks_and_directories_at_suffixes_too() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = b"article";
        let fingerprint = blake3::hash(bytes).to_hex().to_string();
        let outside = dir.path().join("outside");
        std::fs::write(&outside, bytes).unwrap();
        let base = dir.path().join("headline.md");
        std::os::unix::fs::symlink(&outside, &base).unwrap();
        let short = dir
            .path()
            .join(format!("headline-{}.md", &fingerprint[..8]));
        std::fs::create_dir(&short).unwrap();
        let medium = dir
            .path()
            .join(format!("headline-{}.md", &fingerprint[..16]));
        std::os::unix::fs::symlink(&outside, &medium).unwrap();
        let path = publish_article(dir.path(), "Headline", bytes).unwrap();
        assert_eq!(path, dir.path().join(format!("headline-{fingerprint}.md")));
        assert!(base.symlink_metadata().unwrap().file_type().is_symlink());
        assert!(medium.symlink_metadata().unwrap().file_type().is_symlink());
        assert!(short.is_dir());
        assert_eq!(std::fs::read(outside).unwrap(), bytes);
    }

    #[test]
    fn long_multibyte_titles_fit_even_the_full_digest_and_counter_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let title = "긴제목".repeat(100);
        let stem = bounded(&slugify(&title).unwrap());
        let bytes = b"article";
        let fingerprint = blake3::hash(bytes).to_hex().to_string();
        for suffix in [
            "".to_owned(),
            format!("-{}", &fingerprint[..8]),
            format!("-{}", &fingerprint[..16]),
            format!("-{fingerprint}"),
        ] {
            std::fs::write(dir.path().join(article_name(&stem, &suffix)), b"occupied").unwrap();
        }
        let path = publish_article(dir.path(), &title, bytes).unwrap();
        assert!(path.file_name().unwrap().len() <= 255);
        assert!(
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("-1.md")
        );
        assert_eq!(publish_article(dir.path(), &title, bytes).unwrap(), path);
        assert!(article_name(&stem, &format!("-{fingerprint}-{}", u64::MAX)).len() <= 255);
    }

    #[test]
    fn publication_propagates_unreadable_parent_errors() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().join("not-a-directory");
        std::fs::write(&inbox, b"keep").unwrap();
        assert!(publish_article(&inbox, "headline", b"article").is_err());
        assert_eq!(std::fs::read(inbox).unwrap(), b"keep");
    }

    #[test]
    fn concurrent_same_title_fetches_preserve_both_originals() {
        use std::sync::{Arc, Barrier};
        let dir = tempfile::tempdir().unwrap();
        let barrier = Arc::new(Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|number| {
                let inbox = dir.path().to_owned();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let bytes = vec![b'a' + number; 16_384];
                    barrier.wait();
                    let path = publish_article(&inbox, "same title", &bytes).unwrap();
                    assert_eq!(std::fs::read(&path).unwrap(), bytes);
                    assert_eq!(publish_article(&inbox, "same title", &bytes).unwrap(), path);
                    path
                })
            })
            .collect();
        let paths: std::collections::HashSet<_> =
            handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(paths.len(), 8);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 8);
    }
}
