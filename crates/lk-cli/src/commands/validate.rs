use super::schema::render_agents_md;
use super::{find_config, load_config};

pub async fn run(opts: &super::GlobalOptions) -> miette::Result<()> {
    let path = find_config(opts)?;
    let config = load_config(&path)?;

    // Validate each enabled source's params against its adapter schema so config
    // errors (missing keys, wrong types, typos) surface here rather than at the
    // first scheduled ingest run.
    let enabled: Vec<&str> = config.enabled_sources().map(|(id, _)| id).collect();
    for (id, sc) in config.enabled_sources() {
        lk_source::validate_params(sc.source_type, &sc.params)
            .map_err(|e| miette::miette!("sources.{id} ({}): {e}", sc.source_type))?;
    }

    eprintln!("Config valid: {}", path.display());
    eprintln!("  vault: {}", config.vault.root);
    eprintln!("  timezone: {:?}", config.vault.timezone);
    eprintln!("  sources ({}): {}", enabled.len(), enabled.join(", "));

    for (label, spelled, on_disk) in misspelled_vault_dirs(&config).await {
        eprintln!(
            "  warning: vault.dirs.{label} is '{spelled}' but the vault holds '{on_disk}'; \
             every page under it is classified by the configured spelling, so pages are \
             scanned and then belong to no page type — rename one to match"
        );
    }

    // Check AGENTS.md drift — warn if missing or out of date.
    let locale = config.vault.locale();
    let expected = render_agents_md(locale, &config.vault.dirs, config.personal.is_some());
    let agents_path = config
        .vault
        .root_path()
        .join(&config.vault.dirs.wiki)
        .join("AGENTS.md");
    let needs_regen = match tokio::fs::read_to_string(&agents_path).await {
        Ok(on_disk) => on_disk != expected,
        Err(_) => true,
    };
    if needs_regen {
        eprintln!(
            "  warning: {}/AGENTS.md is missing or out of date; run `lore schema` to regenerate",
            config.vault.dirs.wiki
        );
    }

    Ok(())
}

/// Configured vault directories whose on-disk spelling differs from the config's.
///
/// A page's type is decided by which configured directory its path starts with, and the path
/// comes from walking the vault — so on a case-insensitive filesystem `wiki: Wiki` over a
/// `wiki/` directory reads every page under it and then matches none of them: concept pages
/// stop being concept pages, `backlinks-sync` skips them, and their citations freeze while
/// the only visible symptom is a longer orphan list. Folding case in the comparison instead
/// would be wrong — on a case-sensitive filesystem the two really are different directories —
/// so the mismatch is reported where a config is checked rather than resolved silently.
///
/// Only the ROOT segment is compared: nothing below it is named by config.
async fn misspelled_vault_dirs(
    config: &lk_core::config::Config,
) -> Vec<(&'static str, String, String)> {
    let dirs = &config.vault.dirs;
    let root = config.vault.root_path();
    let mut found = Vec::new();
    for (label, value) in [
        ("daily", &dirs.daily),
        ("personal", &dirs.personal),
        ("synthesis", &dirs.synthesis),
        ("wiki", &dirs.wiki),
    ] {
        let Some(first) = std::path::Path::new(value.as_str())
            .components()
            .find_map(|component| match component {
                std::path::Component::Normal(segment) => Some(segment.to_string_lossy()),
                _ => None,
            })
        else {
            continue;
        };
        let Ok(mut entries) = tokio::fs::read_dir(&root).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name != *first && name.eq_ignore_ascii_case(&first) {
                found.push((label, first.into_owned(), name));
                break;
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::misspelled_vault_dirs;

    fn config_at(root: &std::path::Path, wiki: &str) -> lk_core::config::Config {
        let path = root.join(format!("{wiki}.config.yaml"));
        std::fs::write(
            &path,
            format!(
                "vault:\n  root: {}\n  dirs:\n    wiki: {wiki}\nidentity:\n  name: t\n  \
                 email: t@t.com\nsources:\n  s1:\n    type: gmail\n",
                root.display()
            ),
        )
        .unwrap();
        super::load_config(&path).unwrap()
    }

    #[tokio::test]
    async fn a_configured_dir_spelled_differently_from_the_vaults_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("wiki")).unwrap();

        let found = misspelled_vault_dirs(&config_at(tmp.path(), "Wiki")).await;
        assert_eq!(
            found,
            vec![("wiki", "Wiki".to_owned(), "wiki".to_owned())],
            "a case-only difference is invisible on this filesystem and fatal to page typing"
        );

        assert!(
            misspelled_vault_dirs(&config_at(tmp.path(), "wiki"))
                .await
                .is_empty(),
            "the matching spelling is not a finding"
        );
        // A directory that simply does not exist yet is the first-run case, not a mismatch.
        assert!(
            misspelled_vault_dirs(&config_at(tmp.path(), "notes"))
                .await
                .is_empty(),
            "an absent directory is not a misspelling"
        );

        // Only the four roots are named by config at the vault's top level. A period name is
        // a leaf under `personal`/`synthesis`, so a top-level folder of the user's own that
        // happens to case-match one is unrelated — comparing it would raise a false alarm
        // about a directory Lorekeeper never addresses.
        std::fs::create_dir(tmp.path().join("Weekly")).unwrap();
        assert!(
            misspelled_vault_dirs(&config_at(tmp.path(), "wiki"))
                .await
                .is_empty(),
            "a folder case-matching a period name is not a configured root"
        );
    }
}
