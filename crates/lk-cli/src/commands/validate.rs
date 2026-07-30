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

    for (label, value, finding) in inspect_vault_dirs(&config).await {
        match finding {
            RootFinding::Folded => eprintln!(
                "  warning: vault.dirs.{label} is '{value}' but the vault holds no directory \
                 of that name — the filesystem is folding the spelling for you. Every link \
                 that crosses into it carries the configured spelling, so the vault resolves \
                 here and breaks the moment it is read where case is not folded"
            ),
            RootFinding::VariantSibling(sibling) => eprintln!(
                "  warning: vault.dirs.{label} is '{value}' and does not exist, but the vault \
                 holds '{sibling}', a name this filesystem keeps separate, holding pages that \
                 declare Lorekeeper page formats. If those pages are this vault's, the next run \
                 creates '{value}' beside them and writes there while they go unscanned — \
                 invisible to the index, the graph and `doctor`; point vault.dirs.{label} at \
                 '{sibling}'. If they are another tool's, the two are unrelated and only the \
                 name is a coincidence"
            ),
            RootFinding::NotADirectory => eprintln!(
                "  warning: vault.dirs.{label} is '{value}', which exists and is not a \
                 directory — `lore ingest` fails when it tries to create pages under it"
            ),
            RootFinding::Unverifiable(parent) => eprintln!(
                "  warning: vault.dirs.{label} is '{value}' and could not be verified: \
                 '{parent}' cannot be listed, so whether the filesystem is folding the \
                 spelling is unknown rather than answered no"
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

/// What inspecting a configured vault root on disk found. Kept apart because the remedies
/// differ and so does the certainty: two of these are defects, one is a config error, and one
/// is the honest absence of an answer.
enum RootFinding {
    /// The path resolves, but not under the name given — the filesystem folds the spelling.
    Folded,
    /// The name is absent, and a name this filesystem keeps separate from it sits beside it
    /// holding pages that declare Lorekeeper formats.
    VariantSibling(String),
    /// A segment exists and is not a directory.
    NotADirectory,
    /// A parent could not be listed, so neither of the first two could be decided.
    Unverifiable(String),
}

/// Inspect each configured vault root against the directory it actually names.
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
/// [`RootFinding::Folded`] is "resolves, but not under the name given": the segment exists AND
/// is absent from its parent's listing as a literal entry. Both halves are needed — a root that
/// does not exist yet is the first-run case, and a case-sensitive volume may legitimately hold
/// `Wiki` and `wiki` as two directories with the config naming one of them exactly, which is
/// what an earlier version of this check flagged wrongly. Comparing literal entries rather than
/// folding names also leaves a deliberate symlink alone: its own name is a real entry.
async fn inspect_vault_dirs(
    config: &lk_core::config::Config,
) -> Vec<(&'static str, String, RootFinding)> {
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
            let finding = if !child.is_dir() {
                if std::fs::symlink_metadata(&child).is_ok() {
                    // An entry that is not a directory: a file, a socket, a dangling symlink.
                    // `lore ingest` fails on it, and it fails at the write rather than here.
                    Some(RootFinding::NotADirectory)
                } else {
                    // Absent is normally the first run. But where names are kept apart, a
                    // misspelling is simply a different directory — and if the name it was
                    // meant to be is sitting right there, `lore ingest` creates the misspelled
                    // one beside it and writes there, while every page under the other goes
                    // unscanned: invisible to the index, to the graph, and to `doctor`, which
                    // reports no defect because it never sees them. Reproduced on a
                    // case-sensitive APFS volume. Absence alone is silent; absence next to a
                    // name that folds onto it is not.
                    variant_sibling(&parent, segment)
                        .await
                        .map(RootFinding::VariantSibling)
                }
            } else {
                match directory_lists(&parent, segment).await {
                    Some(false) => Some(RootFinding::Folded),
                    None => Some(RootFinding::Unverifiable(parent.display().to_string())),
                    Some(true) => {
                        parent = child;
                        None
                    }
                }
            };
            if let Some(finding) = finding {
                found.push((label, value.clone(), finding));
                break;
            }
        }
    }
    found
}

/// A directory in `parent` that another filesystem would fold onto `name` AND holds pages
/// declaring Lorekeeper formats — the name the configured one was probably meant to be.
///
/// The fold alone is not evidence, and taking it as such was a false positive on the realistic
/// case: this tool writes into an existing Obsidian vault, where `daily`, `wiki`, `me` and
/// `synthesis` are all plausible folder names a person already used. A first run beside a
/// pre-existing unrelated `Daily/` on a case-sensitive volume was told its own pages were about
/// to go invisible, when there were no pages of its own at all.
///
/// What narrows it is whether the sibling holds pages that declare one of this tool's formats.
/// That is evidence and not proof — a note from another tool may carry `type: daily` — so the
/// finding states what was seen and leaves the conclusion to the reader, rather than asserting
/// that pages are about to go unscanned. Only asked when the configured name is ABSENT, so a
/// vault legitimately holding both spellings is never examined.
///
/// Pairing is [`lk_core::fs::names_fold_together`], not ASCII case: an accented or Hangul root
/// differing only by Unicode normalization is exactly the pair a case-sensitive Linux volume
/// keeps apart and an earlier ASCII-only comparison could not see.
async fn variant_sibling(parent: &std::path::Path, name: &std::ffi::OsStr) -> Option<String> {
    let target = name.to_string_lossy();
    let mut entries = tokio::fs::read_dir(parent).await.ok()?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let found = entry.file_name();
        let found = found.to_string_lossy();
        if found == target || !lk_core::fs::names_fold_together(&found, &target) {
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
/// The page states its own provenance, so this needs no registry and no guessing at content —
/// though provenance is what the page CLAIMS, so this is evidence of Lorekeeper output rather
/// than proof of it, which is why its caller reports what it saw instead of concluding.
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
/// a Korean name on APFS, a symlink, a bind mount.
///
/// Identity and containment are asked separately because no one comparison answers both.
/// Identity is [`same_file::is_same_file`] — the filesystem's own answer (device and inode, file
/// index on Windows), which is what sees a bind mount, where two paths name one directory
/// without either being a link to the other. Containment needs paths, so it compares
/// [`lk_core::fs::canonical_prefix`]: the longest existing ancestor resolved and the remainder
/// re-attached, because a root whose leaf does not exist yet is the case where reporting the
/// overlap BEFORE the first write is the whole point — plain `canonicalize` fails there and
/// reported nothing until ingest had already created the ambiguity.
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
    let mut found = Vec::new();
    for (index, (a_label, a)) in roots.iter().enumerate() {
        for (b_label, b) in &roots[index + 1..] {
            let (a_path, b_path) = (root.join(a.as_str()), root.join(b.as_str()));
            if same_file::is_same_file(&a_path, &b_path).unwrap_or(false) {
                found.push((*a_label, *b_label, a_path.display().to_string()));
                continue;
            }
            let (a_real, b_real) = (
                lk_core::fs::canonical_prefix(&a_path),
                lk_core::fs::canonical_prefix(&b_path),
            );
            let outer = if b_real.strip_prefix(&a_real).is_ok() {
                Some(&a_real)
            } else if a_real.strip_prefix(&b_real).is_ok() {
                Some(&b_real)
            } else {
                None
            };
            if let Some(outer) = outer {
                found.push((*a_label, *b_label, outer.display().to_string()));
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::{RootFinding, inspect_vault_dirs, overlapping_vault_dirs, variant_sibling};

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
            inspect_vault_dirs(&write_config(tmp.path(), "    wiki: Wiki\n", "variant")).await;
        assert_eq!(found.len(), 1, "one finding for one root");
        assert_eq!(found[0].1, "Wiki");
        match (&found[0].2, filesystem_folds_case(tmp.path())) {
            (RootFinding::Folded, true) => {}
            (RootFinding::VariantSibling(sibling), false) => assert_eq!(sibling, "wiki"),
            (finding, folds) => panic!(
                "a filesystem that folds={folds} must take the other branch: {}",
                describe(finding)
            ),
        }
    }

    fn describe(finding: &RootFinding) -> String {
        match finding {
            RootFinding::Folded => "folded".into(),
            RootFinding::VariantSibling(name) => format!("sibling {name}"),
            RootFinding::NotADirectory => "not a directory".into(),
            RootFinding::Unverifiable(parent) => format!("unverifiable under {parent}"),
        }
    }

    /// What must stay silent, on every filesystem: the name as given is real; nothing of that
    /// name or near it exists yet; a deliberate symlink, whose own name is a real entry.
    #[tokio::test]
    async fn a_root_that_names_its_own_directory_is_not_reported() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("wiki")).unwrap();

        assert!(
            inspect_vault_dirs(&write_config(tmp.path(), "    wiki: wiki\n", "exact"))
                .await
                .is_empty()
        );
        assert!(
            inspect_vault_dirs(&write_config(tmp.path(), "    wiki: absent\n", "absent"))
                .await
                .is_empty(),
            "a directory that does not exist yet is the first run"
        );
        if std::os::unix::fs::symlink(tmp.path().join("wiki"), tmp.path().join("notes")).is_ok() {
            assert!(
                inspect_vault_dirs(&write_config(tmp.path(), "    wiki: notes\n", "link"))
                    .await
                    .is_empty(),
                "a symlink's own name is a real entry"
            );
        }
    }

    /// A root that is not a directory passed validation and failed at the first write with a
    /// bare `Not a directory (os error 20)` from somewhere inside the pipeline. It is the same
    /// class as a misspelling — the config names something the vault cannot hold pages in — so
    /// it belongs to the same check, and a dangling symlink is that condition too.
    #[tokio::test]
    async fn a_root_that_is_not_a_directory_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("wiki"), "not a directory\n").unwrap();
        let found = inspect_vault_dirs(&write_config(tmp.path(), "    wiki: wiki\n", "file")).await;
        assert_eq!(found.len(), 1, "{}", found.len());
        assert!(
            matches!(found[0].2, RootFinding::NotADirectory),
            "{}",
            describe(&found[0].2)
        );

        std::fs::remove_file(tmp.path().join("wiki")).unwrap();
        if std::os::unix::fs::symlink(tmp.path().join("gone"), tmp.path().join("wiki")).is_ok() {
            let found =
                inspect_vault_dirs(&write_config(tmp.path(), "    wiki: wiki\n", "dangling")).await;
            assert_eq!(found.len(), 1, "{}", found.len());
            assert!(
                matches!(found[0].2, RootFinding::NotADirectory),
                "a dangling symlink is an entry that is not a directory: {}",
                describe(&found[0].2)
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
            variant_sibling(tmp.path(), &name).await.is_none(),
            "someone's own folder is not this tool's misspelled output"
        );

        // One page declaring a format this tool writes, and it is our own data after all.
        std::fs::write(
            theirs.join("rag.md"),
            "---\nid: rag\ntype: concept\ntitle: RAG\n---\n",
        )
        .unwrap();
        assert_eq!(
            variant_sibling(tmp.path(), &name).await.as_deref(),
            Some("Wiki")
        );
    }

    /// Case is one of three ways a filesystem folds a name, and pairing on ASCII case answered
    /// for only the one this was written on. An accented or Hangul root spelled NFC in config
    /// against an NFD directory on disk is the pair a case-sensitive Linux volume keeps apart —
    /// verified there — and it is the vocabulary these vaults are actually named in.
    #[tokio::test]
    async fn a_sibling_differing_only_by_unicode_normalization_is_paired() {
        let tmp = tempfile::tempdir().unwrap();
        // `café` written NFD (e + U+0301) on disk, NFC (U+00E9) in config.
        let nfd = tmp.path().join("cafe\u{301}");
        if std::fs::create_dir(&nfd).is_err() {
            return;
        }
        std::fs::write(
            nfd.join("rag.md"),
            "---\nid: rag\ntype: concept\ntitle: RAG\n---\n",
        )
        .unwrap();

        // Asked directly, so the answer does not depend on whether THIS volume folds the
        // spelling — the pairing rule is what is under test, and it must be the union of what
        // any shipped filesystem collapses.
        for spelling in ["caf\u{e9}", "CAF\u{c9}", "Cafe\u{301}"] {
            assert_eq!(
                variant_sibling(tmp.path(), &std::ffi::OsString::from(spelling)).await,
                Some("cafe\u{301}".to_string()),
                "{spelling:?} must pair with the NFD directory on disk"
            );
        }
        assert!(
            variant_sibling(tmp.path(), &std::ffi::OsString::from("cafe"))
                .await
                .is_none(),
            "a name that no filesystem folds onto it is a different directory"
        );
    }

    /// A directory can be traversable without being listable, so `exists` on a child succeeds
    /// while `read_dir` on its parent fails. Reading that as "the name is not listed" reports
    /// a fold where nothing was folded — a warning that is not merely useless but says
    /// something false about a correct config. Saying nothing at all is also wrong, though:
    /// silence reads as "checked, and fine" for a check that did not run.
    #[tokio::test]
    async fn an_unlistable_parent_is_reported_as_unverified_not_as_a_fold() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("vault");
        std::fs::create_dir_all(root.join("wiki")).unwrap();
        let config = write_config_at(&root, tmp.path(), "    wiki: wiki\n", "perm");

        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o111)).unwrap();
        let found = inspect_vault_dirs(&config).await;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();

        // Running as root ignores directory permissions, so the listing succeeds and there is
        // nothing to report — the probe decides, rather than the test assuming an unprivileged
        // process.
        let listable = std::fs::read_dir(root.join("wiki")).is_ok()
            && std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o111))
                .and_then(|()| std::fs::read_dir(&root).map(|_| ()))
                .is_ok();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        if listable {
            assert!(found.is_empty(), "a listable parent has nothing to report");
            return;
        }
        assert_eq!(found.len(), 1, "{}", found.len());
        assert!(
            matches!(found[0].2, RootFinding::Unverifiable(_)),
            "an unreadable listing is unknown, not evidence of a fold: {}",
            describe(&found[0].2)
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

        // A root whose leaf does not exist YET, reached through an alias, is the case worth
        // reporting before the first write rather than after: `canonicalize` fails on the whole
        // path and answered nothing until ingest had already created the ambiguity.
        let future = overlapping_vault_dirs(&write_config(
            tmp.path(),
            "    wiki: wiki\n    daily: notes/later\n",
            "future",
        ));
        assert_eq!(future.len(), 1, "{future:?}");

        // Separate directories, and a root that does not exist yet under no alias, are fine.
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
