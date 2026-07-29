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

    for (a, b, shared) in overlapping_vault_dirs(&config) {
        eprintln!(
            "  warning: vault.dirs.{a} and vault.dirs.{b} overlap on disk at '{shared}'; a \
             page under both is classified by whichever the scan reaches first, so which page \
             type it has depends on nothing the vault states"
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

/// Pairs of configured vault roots that overlap ON DISK — one inside the other, or the two
/// naming one directory.
///
/// TWO roots is the whole condition. A single root spelled differently from the directory it
/// resolves to is harmless and was briefly warned about here in error: the scan joins the
/// CONFIGURED spelling onto the vault root and the classification predicates read that same
/// string, so `wiki: Wiki` over a `wiki/` directory is self-consistent — citations derive,
/// the catalog and the map link correctly, `index-sync` reports in sync. Warning about it
/// also fired on a correct vault, since a case-sensitive volume may hold `Wiki` and `wiki` as
/// two real directories with the config naming one of them exactly.
///
/// `VaultDirs::validate` rejects roots that overlap as path strings, which is all it can do
/// without I/O, and the filesystem's notion of one directory is coarser than any string
/// comparison: `Pages` and `pages` on a case-insensitive volume, the NFC and NFD spellings of
/// a Korean name on APFS, a symlink. Canonicalizing first asks the filesystem its own
/// question and answers all three at once; containment is then checked the same way the
/// string rule checks it, since a root inside another is the overlap that matters and equality
/// is only its degenerate case.
///
/// Reported, not refused: `scan_vault` no longer derives citations from a page reached twice,
/// so nothing is corrupted — but it has to pick one classification for a page that has two,
/// and which one it picks is not something the vault states.
fn overlapping_vault_dirs(
    config: &lk_core::config::Config,
) -> Vec<(&'static str, &'static str, String)> {
    let dirs = &config.vault.dirs;
    let root = config.vault.root_path();
    let roots = [
        ("daily", &dirs.daily),
        ("personal", &dirs.personal),
        ("synthesis", &dirs.synthesis),
        ("wiki", &dirs.wiki),
    ];
    let resolve = |value: &str| std::fs::canonicalize(root.join(value)).ok();
    let contains =
        |outer: &std::path::Path, inner: &std::path::Path| inner.strip_prefix(outer).is_ok();
    let mut found = Vec::new();
    for (index, (a_label, a)) in roots.iter().enumerate() {
        for (b_label, b) in &roots[index + 1..] {
            if let (Some(a_path), Some(b_path)) = (resolve(a), resolve(b)) {
                let shared = if contains(&a_path, &b_path) {
                    Some(&a_path)
                } else if contains(&b_path, &a_path) {
                    Some(&b_path)
                } else {
                    None
                };
                if let Some(shared) = shared {
                    found.push((*a_label, *b_label, shared.display().to_string()));
                }
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::overlapping_vault_dirs;

    fn write_config(root: &std::path::Path, dirs: &str, tag: &str) -> lk_core::config::Config {
        let path = root.join(format!("{tag}.config.yaml"));
        std::fs::write(
            &path,
            format!(
                "vault:\n  root: {}\n  dirs:\n{dirs}identity:\n  name: t\n  \
                 email: t@t.com\nsources:\n  s1:\n    type: gmail\n",
                root.display()
            ),
        )
        .unwrap();
        super::load_config(&path).unwrap()
    }

    /// The string rule in `VaultDirs::validate` is all that can be decided without I/O, and
    /// the filesystem's notion of one directory is coarser: a symlink here, and case or
    /// Unicode normalization on the volumes this ships to. Both shapes count — two roots
    /// naming one directory, and one root sitting inside another — because a page under both
    /// has two page types either way.
    #[test]
    fn roots_that_overlap_on_disk_are_reported() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("wiki")).unwrap();
        if std::os::unix::fs::symlink(tmp.path().join("wiki"), tmp.path().join("notes")).is_err() {
            return;
        }

        let found = overlapping_vault_dirs(&write_config(
            tmp.path(),
            "    wiki: wiki\n    daily: notes\n",
            "collide",
        ));
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!((found[0].0, found[0].1), ("daily", "wiki"));

        // One root INSIDE another, reached only through the alias: the string rule sees
        // `notes/inner` and `wiki` as unrelated, and on disk the first is under the second.
        std::fs::create_dir(tmp.path().join("wiki/inner")).unwrap();
        let nested = overlapping_vault_dirs(&write_config(
            tmp.path(),
            "    wiki: wiki\n    daily: notes/inner\n",
            "nested",
        ));
        assert_eq!(nested.len(), 1, "{nested:?}");

        // Separate directories, and a root that does not exist yet, are both fine.
        assert!(
            overlapping_vault_dirs(&write_config(
                tmp.path(),
                "    wiki: wiki\n    daily: absent\n",
                "ok"
            ))
            .is_empty()
        );
    }
}
