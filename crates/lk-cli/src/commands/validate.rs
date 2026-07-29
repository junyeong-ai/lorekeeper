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

    for (label, value) in folded_vault_dirs(&config).await {
        eprintln!(
            "  warning: vault.dirs.{label} is '{value}' but the vault holds no directory of \
             that name — the filesystem is folding the spelling for you. Every link that \
             crosses into it carries the configured spelling, so the vault resolves here and \
             breaks the moment it is read where case or Unicode is not folded"
        );
    }

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

/// Configured vault roots that only resolve because the filesystem folds their spelling.
///
/// Every link a page writes into another root is built from the CONFIGURED name
/// (`render::concepts_dir_dest`), so `wiki: Wiki` over a `wiki/` directory publishes
/// `../../Wiki/concepts/x.md` into daily pages. That resolves on a case-insensitive volume
/// and is a broken link the moment the vault is read where case is not folded — a Linux
/// checkout, GitHub — which the OKF link invariant forbids. The NFC-against-NFD spelling of a
/// Korean root does the same on APFS. And on a genuinely case-sensitive volume the misspelled
/// root is simply a second directory: `lore ingest` creates it and populates it while every
/// page already under the real one becomes invisible to the index and the graph, with nothing
/// reporting a defect.
///
/// The condition is "resolves, but not under the name given": the path canonicalizes AND some
/// segment of it is absent from its parent's listing as a literal entry. Both halves are
/// needed — a root that does not exist yet is the first-run case, and a case-sensitive volume
/// may legitimately hold `Wiki` and `wiki` as two directories with the config naming one of
/// them exactly, which is what an earlier version of this check flagged wrongly. Comparing
/// literal entries rather than folding case also leaves a deliberate symlink alone: its own
/// name is a real entry.
async fn folded_vault_dirs(config: &lk_core::config::Config) -> Vec<(&'static str, String)> {
    let dirs = &config.vault.dirs;
    let mut found = Vec::new();
    for (label, value) in [
        ("daily", &dirs.daily),
        ("personal", &dirs.personal),
        ("synthesis", &dirs.synthesis),
        ("wiki", &dirs.wiki),
    ] {
        let mut parent = config.vault.root_path();
        for segment in std::path::Path::new(value.as_str()).components() {
            let std::path::Component::Normal(segment) = segment else {
                continue;
            };
            let child = parent.join(segment);
            if !child.exists() {
                break;
            }
            if !directory_lists(&parent, segment).await {
                found.push((label, value.clone()));
                break;
            }
            parent = child;
        }
    }
    found
}

/// Whether `parent`'s listing contains `name` byte for byte — the question `Path::exists`
/// cannot answer, since it is the filesystem's folded answer that is wanted here.
async fn directory_lists(parent: &std::path::Path, name: &std::ffi::OsStr) -> bool {
    let Ok(mut entries) = tokio::fs::read_dir(parent).await else {
        return false;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry.file_name() == name {
            return true;
        }
    }
    false
}

/// Pairs of configured vault roots that overlap ON DISK — one inside the other, or the two
/// naming one directory.
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
    use super::{folded_vault_dirs, overlapping_vault_dirs};

    /// Both halves of the condition, and the two shapes that must stay silent. An earlier
    /// version scanned for a case-fold near-match and flagged a correct vault; the version
    /// before that had no check at all and let a root publish links under a name the vault
    /// does not hold.
    #[tokio::test]
    async fn a_root_that_only_resolves_by_folding_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("wiki")).unwrap();

        // Resolves (this filesystem folds case) but the vault lists no `Wiki`.
        let found =
            folded_vault_dirs(&write_config(tmp.path(), "    wiki: Wiki\n", "folded")).await;
        assert_eq!(found, vec![("wiki", "Wiki".to_owned())], "{found:?}");

        // The name as given is a real entry — nothing is being folded.
        assert!(
            folded_vault_dirs(&write_config(tmp.path(), "    wiki: wiki\n", "exact"))
                .await
                .is_empty()
        );
        // Nothing there yet: the first-run case, not a misspelling.
        assert!(
            folded_vault_dirs(&write_config(tmp.path(), "    wiki: absent\n", "absent"))
                .await
                .is_empty()
        );
        // A deliberate symlink keeps its own name in the listing, so it is not a fold.
        if std::os::unix::fs::symlink(tmp.path().join("wiki"), tmp.path().join("notes")).is_ok() {
            assert!(
                folded_vault_dirs(&write_config(tmp.path(), "    wiki: notes\n", "link"))
                    .await
                    .is_empty()
            );
        }
    }

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
