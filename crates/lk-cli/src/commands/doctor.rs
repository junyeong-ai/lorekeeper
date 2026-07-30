//! `lore doctor` — audit materialized vault pages against the contracts they must satisfy: the
//! text-cleanliness contract (`lk_core::markdown::scan_defects`) and the completion-marker
//! contract that says a section whose input was recorded has been answered.
//!
//! The pipeline guarantees every page it writes is clean by construction (the
//! rich-text converters strip the offending content at conversion time). A
//! materialized view, though, persists on disk: a page written before a contract was
//! tightened keeps its defect until re-ingested. This command makes that drift
//! observable — deterministic, offline, zero network — so a stale page surfaces here
//! instead of being eyeballed later.
//!
//! Scope is the four pipeline-managed roots (`vault.dirs.{daily,personal,synthesis,
//! wiki}`) ONLY. User-authored content elsewhere in the vault (Excalidraw drawings,
//! which legitimately embed base64, hand-written notes) is never the pipeline's
//! output and so is never judged by the pipeline's contract.
//!
//! `run` is a thin shell: load config → `managed_roots` → `audit` → print → exit.
//! The two pure functions hold all the logic so they are unit-testable without the
//! `process::exit` the CLI needs.

use std::path::{Path, PathBuf};

use lk_core::config::VaultDirs;
use lk_core::markdown::{TextDefect, scan_defects};
use walkdir::WalkDir;

use super::{find_config, load_config};

pub async fn run(opts: &super::GlobalOptions) -> miette::Result<()> {
    let config = load_config(&find_config(opts)?)?;
    let vault_root = config.vault.root_path();
    let report = audit(&managed_roots(&vault_root, &config.vault.dirs));

    for page in &report.pages {
        eprintln!("✗ {}", rel(&page.path, &vault_root));
        for (line, defect) in &page.defects {
            eprintln!("    L{line}: {}", defect.description());
        }
        for section in &page.unanswered {
            eprintln!("    `{section}` was enqueued and never answered");
        }
    }
    let total: usize = report
        .pages
        .iter()
        .map(|p| p.defects.len() + p.unanswered.len())
        .sum();
    eprintln!(
        "\n{} pages scanned, {} with defects ({total} total){}",
        report.scanned,
        report.pages.len(),
        if report.errors > 0 {
            format!(", {} unreadable", report.errors)
        } else {
            String::new()
        }
    );
    if report.scanned == 0 {
        eprintln!(
            "\nNo pages found under the configured vault.dirs — check the config if unexpected."
        );
    }
    if report.pages.iter().any(|page| !page.defects.is_empty()) {
        eprintln!(
            "\nPages with a cleanliness defect predate a tightened contract. Re-ingest the\n\
             affected page's source with the current binary — the contract is upheld at\n\
             conversion, so a fresh ingest reproduces each page clean."
        );
    }
    if report.pages.iter().any(|page| !page.unanswered.is_empty()) {
        eprintln!(
            "\nAn unanswered section's work was enqueued and lost: the queue file it belonged\n\
             to has been archived or pruned, so nothing pending remains to fill it. Re-ingest\n\
             that page's date (`lore ingest <source> --date <date>`) to enqueue it again, then\n\
             drain with /lore-process."
        );
    }
    // Non-zero on a real defect OR on a page that could not be verified — "clean" must
    // never be claimed for content that was skipped.
    if !report.pages.is_empty() || report.errors > 0 {
        std::process::exit(1);
    }
    Ok(())
}

/// The pipeline-managed roots under `vault_root`. `weekly`/`monthly`/`quarterly`/
/// `annual` are subdirectories within `personal` and `synthesis`, so those two roots
/// cover every personal review and team synthesis; `wiki` covers concepts, documents,
/// explorations, and the generated catalogs. Everything else in the vault is
/// user-authored and deliberately out of scope.
///
/// Deduplicated by CANONICAL identity, so an unusual config pointing two of the four at the
/// same directory does not scan it twice — and the comparison has to be canonical, because the
/// spellings that reach here differ by construction: the filesystem folds case, or Unicode
/// normalization, or a symlink, and equal paths were never the interesting case. Comparing the
/// joined text reported one physical file as two defective pages, each told to be re-ingested,
/// and doubled the scanned count.
fn managed_roots(vault_root: &Path, dirs: &VaultDirs) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::with_capacity(4);
    let mut seen: Vec<PathBuf> = Vec::with_capacity(4);
    for dir in [&dirs.daily, &dirs.personal, &dirs.synthesis, &dirs.wiki] {
        let path = vault_root.join(dir);
        let identity = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if !seen.contains(&identity) {
            seen.push(identity);
            roots.push(path);
        }
    }
    roots
}

/// One page that violated a contract. Clean pages are never recorded.
struct PageDefects {
    path: PathBuf,
    defects: Vec<(usize, TextDefect)>,
    /// `llm_inputs` keys the page records an input hash for and no answer to.
    unanswered: Vec<&'static str>,
}

/// The `llm_inputs` sections this page was told to have filled and never was.
///
/// The pipeline stamps `llm_inputs.<key>` when it enqueues the work and whoever does the work
/// stamps `<key>_done`; a cache hit is the two being equal, and emptiness never signals either
/// way. So a page carrying the input and not the answer has work that was enqueued and lost —
/// the queue file it belonged to has since been archived or pruned, `lore queue count` reports
/// nothing pending, and the section stays empty for good.
///
/// Nothing else reports this. The one visible trace is indirect and only for concepts: a
/// concept page whose origin never completed its extraction has no citation, so it surfaces in
/// `lore graph lint`'s observation channel as an orphan — mixed in with concepts that simply
/// have not been cited yet, which is not a defect at all and is why that channel does not gate.
/// On the vault this was measured against, 38 pages carried
/// unanswered sections while `doctor` reported no defects and the queue reported nothing to do.
///
/// The keys come from `TargetKind`, so a new task kind is covered without being listed here.
fn unanswered_sections(text: &str) -> Vec<&'static str> {
    use strum::IntoEnumIterator;

    let Some(inputs) = text
        .split_once("llm_inputs:")
        .and_then(|(_, rest)| rest.split_once("\n---").map(|(block, _)| block))
    else {
        return Vec::new();
    };
    let records = |key: &str| {
        inputs
            .lines()
            .any(|line| line.trim_start().starts_with(&format!("{key}: ")))
    };
    let mut found: Vec<&'static str> = lk_queue::TargetKind::iter()
        .map(|kind| kind.llm_inputs_key())
        .filter(|key| records(key) && !records(&format!("{key}_done")))
        .collect();
    found.sort_unstable();
    found.dedup();
    found
}

/// The outcome of a vault audit: pages scanned, which ones carry defects, and how many
/// were unreadable (so the caller can refuse to report "clean" when some were skipped).
struct AuditReport {
    scanned: u32,
    errors: u32,
    pages: Vec<PageDefects>,
}

/// Walk `roots`, scanning every `.md` against the cleanliness contract. Pure with
/// respect to its return value — no printing of results, no process exit — so it is
/// unit-testable. Symlinks are NOT followed (`follow_links(false)`): pipeline pages are
/// real files, and following links could escape the vault or loop. A missing root is
/// skipped (a vault may not have produced a synthesis yet); an unreadable or non-UTF-8
/// file is counted as an error (not as clean), since pipeline output is always valid
/// UTF-8 — this only ever fires on a foreign file a user dropped under a managed root.
fn audit(roots: &[PathBuf]) -> AuditReport {
    let mut scanned = 0u32;
    let mut errors = 0u32;
    let mut pages = Vec::new();

    for root in roots {
        if !root.exists() {
            continue;
        }
        for entry in WalkDir::new(root).follow_links(false).sort_by_file_name() {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("⚠ walk error under {}: {e}", root.display());
                    errors += 1;
                    continue;
                }
            };
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let text = match std::fs::read_to_string(path) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("⚠ {}: read failed: {e}", path.display());
                    errors += 1;
                    continue;
                }
            };
            scanned += 1;
            let defects = scan_defects(&text);
            let unanswered = unanswered_sections(&text);
            if !defects.is_empty() || !unanswered.is_empty() {
                pages.push(PageDefects {
                    path: path.to_path_buf(),
                    defects,
                    unanswered,
                });
            }
        }
    }

    AuditReport {
        scanned,
        errors,
        pages,
    }
}

/// Path relative to the vault root for compact output, falling back to the full
/// path if the page somehow sits outside it.
fn rel<'a>(path: &'a Path, root: &Path) -> std::borrow::Cow<'a, str> {
    path.strip_prefix(root).unwrap_or(path).to_string_lossy()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// The completion-marker contract, which nothing reported until now: the pipeline stamps the
    /// input hash when it enqueues the work, whoever does the work stamps `<key>_done`, and a page
    /// carrying the first without the second has work that was enqueued and then lost with its
    /// queue file. Measured on a live vault: 38 pages, 75 sections, while `doctor` said no defects
    /// and `queue count` said nothing pending.
    ///
    /// Emptiness is deliberately not the test — a focus-filtered summary, an extraction that found
    /// nothing and a trivial-only work-log are all legitimately empty AND answered, which is why
    /// the marker exists.
    #[test]
    fn audit_flags_a_section_whose_work_was_enqueued_and_never_answered() {
        let answered = "---\nid: a\nllm_inputs:\n  summary: \"h1\"\n  summary_done: \"h1\"\n---\n\n## Summary\n";
        assert!(
            unanswered_sections(answered).is_empty(),
            "an answered section is not a defect, even with an empty body"
        );

        let pending = "---\nid: a\nllm_inputs:\n  summary: \"h1\"\n  concepts: \"h2\"\n  concepts_done: \"h2\"\n---\n\n## Summary\n";
        assert_eq!(
            unanswered_sections(pending),
            vec!["summary"],
            "the input without its answer is the one reported"
        );

        assert!(
            unanswered_sections("---\nid: a\n---\n\n# no llm_inputs at all\n").is_empty(),
            "a page the pipeline never enqueued work for has nothing outstanding"
        );

        // Through the walk, so the finding reaches the report the CLI prints.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("daily");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("answered.md"), answered).unwrap();
        fs::write(root.join("pending.md"), pending).unwrap();
        let report = audit(&[root]);
        assert_eq!(report.scanned, 2);
        assert_eq!(
            report.pages.len(),
            1,
            "only the unanswered page is reported"
        );
        assert_eq!(report.pages[0].unanswered, vec!["summary"]);
        assert!(
            report.pages[0].defects.is_empty(),
            "an unanswered section is not a text defect"
        );
    }

    /// Every task kind's key is covered, because the keys come from `TargetKind` rather than a
    /// list here — a new kind would otherwise be enqueued and never audited.
    #[test]
    fn every_task_kinds_key_is_audited() {
        use strum::IntoEnumIterator;

        for kind in lk_queue::TargetKind::iter() {
            let key = kind.llm_inputs_key();
            let page = format!("---\nid: a\nllm_inputs:\n  {key}: \"h\"\n---\n\n# x\n");
            assert_eq!(
                unanswered_sections(&page),
                vec![key],
                "{kind:?}'s key is not audited"
            );
        }
    }

    #[test]
    fn audit_flags_defective_pages_and_scans_clean_ones() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("daily");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("clean.md"), "# ok\n\n![x](https://x.io/a.png)\n").unwrap();
        fs::write(
            root.join("dirty.md"),
            "# bad\n\n![x](data:image/png;base64,AAAA)\n",
        )
        .unwrap();

        let report = audit(&[root]);
        assert_eq!(report.scanned, 2, "both .md files are scanned");
        assert_eq!(report.pages.len(), 1, "only the defective page is reported");
        assert!(report.pages[0].path.ends_with("dirty.md"));
        assert_eq!(
            report.pages[0].defects,
            vec![(3, TextDefect::InlineDataUri)]
        );
    }

    #[test]
    fn audit_ignores_pages_outside_the_given_roots() {
        // A defective page in a directory NOT passed as a root — user content such as
        // an Excalidraw drawing that legitimately embeds base64 — is never scanned.
        // The scoping is exactly what keeps the doctor from false-flagging it.
        let dir = tempfile::tempdir().unwrap();
        let managed = dir.path().join("wiki");
        let user = dir.path().join("Excalidraw");
        fs::create_dir_all(&managed).unwrap();
        fs::create_dir_all(&user).unwrap();
        fs::write(managed.join("concept.md"), "# clean\n").unwrap();
        fs::write(user.join("drawing.md"), "![](data:image/png;base64,ZZZZ)\n").unwrap();

        let report = audit(&[managed]);
        assert_eq!(report.scanned, 1);
        assert!(
            report.pages.is_empty(),
            "content outside the managed roots must never be scanned"
        );
    }

    #[test]
    fn audit_skips_a_missing_root_without_error() {
        let dir = tempfile::tempdir().unwrap();
        let report = audit(&[dir.path().join("synthesis-that-does-not-exist")]);
        assert_eq!(report.scanned, 0);
        assert!(report.pages.is_empty());
    }

    #[test]
    fn managed_roots_are_the_four_pipeline_dirs_not_user_content() {
        let roots = managed_roots(Path::new("/vault"), &VaultDirs::default());
        assert_eq!(
            roots,
            vec![
                PathBuf::from("/vault/daily"),
                PathBuf::from("/vault/me"),
                PathBuf::from("/vault/synthesis"),
                PathBuf::from("/vault/wiki"),
            ],
            "scope is exactly the four pipeline-managed roots"
        );
    }
}
