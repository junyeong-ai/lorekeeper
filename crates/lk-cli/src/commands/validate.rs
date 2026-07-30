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

    for (label, value, sibling) in folded_vault_dirs(&config).await {
        match sibling {
            Some(sibling) => eprintln!(
                "  warning: vault.dirs.{label} is '{value}' and does not exist, but the vault \
                 holds '{sibling}'. This filesystem does not fold case, so the next run \
                 creates '{value}' beside it and writes there — every page already under \
                 '{sibling}' then goes unscanned, invisible to the index, the graph and \
                 `doctor`"
            ),
            None => eprintln!(
                "  warning: vault.dirs.{label} is '{value}' but the vault holds no directory \
                 of that name — the filesystem is folding the spelling for you. Every link \
                 that crosses into it carries the configured spelling, so the vault resolves \
                 here and breaks the moment it is read where case is not folded"
            ),
        }
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
async fn folded_vault_dirs(
    config: &lk_core::config::Config,
) -> Vec<(&'static str, String, Option<String>)> {
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
                // Absent is normally the first run. But on a filesystem that does NOT fold
                // case, a misspelling is simply a different directory — and if the name it
                // was meant to be is sitting right there, `lore ingest` will create the
                // misspelled one beside it and write there, while every page under the real
                // one goes unscanned: invisible to the index, to the graph, and to `doctor`,
                // which reports no defect because it never sees them. Reproduced on a
                // case-sensitive APFS volume. Absence alone is silent; absence next to the
                // name it differs from only in case is not.
                if let Some(sibling) = case_variant_sibling(&parent, segment).await {
                    found.push((label, value.clone(), Some(sibling)));
                }
                break;
            }
            match directory_lists(&parent, segment).await {
                Some(false) => {
                    found.push((label, value.clone(), None));
                    break;
                }
                // Unreadable: the listing is unknown, which is not evidence of a fold.
                None => break,
                Some(true) => parent = child,
            }
        }
    }
    found
}

/// A directory in `parent` that case-folds onto `name` AND holds pages this tool wrote — the
/// name the configured one was probably meant to be.
///
/// The case fold alone is not evidence, and taking it as such was a false positive on the
/// realistic case: this tool writes into an existing Obsidian vault, where `daily`, `wiki`,
/// `me` and `synthesis` are all plausible folder names a person already used. A first run
/// beside a pre-existing unrelated `Daily/` on a case-sensitive volume was told its own pages
/// were about to go invisible, when there were no pages of its own at all.
///
/// What separates the two is whether the sibling holds Lorekeeper's own output, and a page says
/// so itself: the `type` frontmatter naming one of the formats this tool writes. A folder of
/// someone's own notes does not carry those. Only asked when the configured name is ABSENT, so
/// a vault legitimately holding both spellings is never examined.
///
/// A bound worth stating: two names differing only by Unicode normalization are not compared,
/// so a normalization-SENSITIVE filesystem could hide that pairing here. On the volumes this
/// ships to, normalization is folded, so the configured name resolves and the fold check above
/// sees it instead.
async fn case_variant_sibling(parent: &std::path::Path, name: &std::ffi::OsStr) -> Option<String> {
    let target = name.to_string_lossy();
    let mut entries = tokio::fs::read_dir(parent).await.ok()?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let found = entry.file_name();
        let found = found.to_string_lossy();
        if found == target || !found.eq_ignore_ascii_case(&target) {
            continue;
        }
        if holds_managed_pages(&parent.join(found.as_ref())) {
            return Some(found.into_owned());
        }
    }
    None
}

/// Whether any page under `dir` declares one of the formats this tool writes.
///
/// The page states its own provenance, so this needs no registry and no guessing at content.
/// Bounded: it stops at the first page that answers yes.
fn holds_managed_pages(dir: &std::path::Path) -> bool {
    walkdir::WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .flatten()
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "md"))
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .filter_map(|raw| {
            lk_core::frontmatter::parse_page(&raw)
                .ok()?
                .frontmatter
                .get("type")?
                .as_str()
                .map(str::to_owned)
        })
        .any(|declared| lk_core::vault_path::PAGE_FORMATS.contains(&declared.as_str()))
}

/// Whether `parent`'s listing contains `name` byte for byte — the question `Path::exists`
/// cannot answer, since it is the filesystem's folded answer that is wanted here. `None` when
/// the listing could not be read.
///
/// The three outcomes are distinct and collapsing them is a wrong warning, not a missing one:
/// a directory can be traversable without being readable (`chmod 111` — `exists` succeeds on a
/// child while `read_dir` fails), and reading "could not list" as "not listed" reports a
/// spelling the filesystem folded when nothing was folded at all.
async fn directory_lists(parent: &std::path::Path, name: &std::ffi::OsStr) -> Option<bool> {
    let mut entries = tokio::fs::read_dir(parent).await.ok()?;
    loop {
        match entries.next_entry().await {
            Ok(Some(entry)) if entry.file_name() == name => return Some(true),
            Ok(Some(_)) => {}
            Ok(None) => return Some(false),
            Err(_) => return None,
        }
    }
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
/// Reported, not refused — but do not read that as harmless, which an earlier version of this
/// comment did. Two roots on one directory put two page formats on one path:
/// `<daily>/{source-id}/{date}.md` and `<wiki>/concepts/{slug}.md` collide when a source is
/// named `concepts`, and a daily render then replaces a curated concept page wholesale. The
/// write itself is refused now (`VaultWriter` will not change a page's format), so the loss
/// cannot happen — which is why this stays a warning and not a refusal. What remains is that a
/// page under both roots is classified by whichever the scan reaches first, and which one that
/// is is not something the vault states.
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
    use super::{case_variant_sibling, folded_vault_dirs, overlapping_vault_dirs};

    /// Whether this filesystem folds case. It decides WHICH branch a misspelled root takes:
    /// where case folds the name resolves and the fold branch answers, where it does not the
    /// name is simply absent and the sibling branch does. Probed rather than assumed — CI runs
    /// on both, and a test that writes down one platform's answer passes on it and fails on the
    /// other, which is how these two tests were first written.
    fn filesystem_folds_case(dir: &std::path::Path) -> bool {
        let probe = dir.join("fold-probe");
        std::fs::create_dir(&probe).unwrap();
        let folds = dir.join("FOLD-PROBE").is_dir();
        std::fs::remove_dir(&probe).unwrap();
        folds
    }

    /// A root named differently from the directory it means is reported either way, and the two
    /// ways are different findings: where case folds, the name resolves under a spelling the
    /// vault does not hold, and every link crossing into it carries the configured one; where it
    /// does not, the name is absent beside the real directory, and the next run creates it and
    /// writes there while the pages already filed go unscanned.
    #[tokio::test]
    async fn a_root_named_differently_from_its_directory_is_reported_either_way() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("wiki");
        std::fs::create_dir(&real).unwrap();
        // A page of ours, so the sibling branch has its evidence on a case-sensitive volume.
        std::fs::write(
            real.join("rag.md"),
            "---\nid: rag\ntype: concept\ntitle: RAG\n---\n",
        )
        .unwrap();

        let found =
            folded_vault_dirs(&write_config(tmp.path(), "    wiki: Wiki\n", "variant")).await;
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].1, "Wiki");
        if filesystem_folds_case(tmp.path()) {
            assert_eq!(found[0].2, None, "a fold names no sibling: {found:?}");
        } else {
            assert_eq!(
                found[0].2.as_deref(),
                Some("wiki"),
                "an absence names the directory it differs from: {found:?}"
            );
        }
    }

    /// What must stay silent, on every filesystem: the name as given is real; nothing of that
    /// name or near it exists yet; a deliberate symlink, whose own name is a real entry.
    #[tokio::test]
    async fn a_root_that_names_its_own_directory_is_not_reported() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("wiki")).unwrap();

        assert!(
            folded_vault_dirs(&write_config(tmp.path(), "    wiki: wiki\n", "exact"))
                .await
                .is_empty()
        );
        assert!(
            folded_vault_dirs(&write_config(tmp.path(), "    wiki: absent\n", "absent"))
                .await
                .is_empty(),
            "a directory that does not exist yet is the first run"
        );
        if std::os::unix::fs::symlink(tmp.path().join("wiki"), tmp.path().join("notes")).is_ok() {
            assert!(
                folded_vault_dirs(&write_config(tmp.path(), "    wiki: notes\n", "link"))
                    .await
                    .is_empty(),
                "a symlink's own name is a real entry"
            );
        }
    }

    /// A case fold alone is not evidence. This tool writes into an existing Obsidian vault,
    /// where `daily`, `wiki`, `me` and `synthesis` are all names a person may already have
    /// used — so a first run beside a pre-existing unrelated folder was told its own pages were
    /// about to go invisible when it had none. What separates the two is whether the sibling
    /// holds pages this tool wrote, which a page states itself.
    ///
    /// Exercised directly, because reaching it through `folded_vault_dirs` needs a
    /// case-SENSITIVE filesystem: where case folds, the configured name resolves and the fold
    /// branch answers first — correctly, since links would then carry a spelling the vault does
    /// not hold.
    #[tokio::test]
    async fn a_case_variant_sibling_of_someone_elses_notes_is_not_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let theirs = tmp.path().join("Wiki");
        std::fs::create_dir(&theirs).unwrap();
        std::fs::write(theirs.join("grocery list.md"), "# Milk\n").unwrap();
        std::fs::write(
            theirs.join("journal.md"),
            "---\ntags: [personal]\n---\n\n# Tuesday\n",
        )
        .unwrap();

        let name = std::ffi::OsString::from("wiki");
        assert!(
            case_variant_sibling(tmp.path(), &name).await.is_none(),
            "someone's own folder is not this tool's misspelled output"
        );

        // One page declaring a format this tool writes, and it is our own data after all.
        std::fs::write(
            theirs.join("rag.md"),
            "---\nid: rag\ntype: concept\ntitle: RAG\n---\n",
        )
        .unwrap();
        assert_eq!(
            case_variant_sibling(tmp.path(), &name).await.as_deref(),
            Some("Wiki")
        );
    }

    /// A directory can be traversable without being listable, so `exists` on a child succeeds
    /// while `read_dir` on its parent fails. Reading that as "the name is not listed" reports
    /// a fold where nothing was folded — a warning that is not merely useless but says
    /// something false about a correct config.
    #[tokio::test]
    async fn an_unlistable_parent_is_not_reported_as_a_fold() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("vault");
        std::fs::create_dir_all(root.join("wiki")).unwrap();
        let config = write_config_at(&root, tmp.path(), "    wiki: wiki\n", "perm");

        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o111)).unwrap();
        let found = folded_vault_dirs(&config).await;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(
            found.is_empty(),
            "an unreadable listing is unknown, not evidence: {found:?}"
        );
    }

    fn write_config(root: &std::path::Path, dirs: &str, tag: &str) -> lk_core::config::Config {
        write_config_at(root, root, dirs, tag)
    }

    /// The config file and the vault root are separate here, so a test can make the vault
    /// unreadable without also hiding the config from the loader.
    fn write_config_at(
        vault_root: &std::path::Path,
        beside: &std::path::Path,
        dirs: &str,
        tag: &str,
    ) -> lk_core::config::Config {
        let path = beside.join(format!("{tag}.config.yaml"));
        std::fs::write(
            &path,
            format!(
                "vault:\n  root: {}\n  dirs:\n{dirs}identity:\n  name: t\n  \
                 email: t@t.com\nsources:\n  s1:\n    type: gmail\n",
                vault_root.display()
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
