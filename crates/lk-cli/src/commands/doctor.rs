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
use lk_core::markdown::{CredentialForm, TextDefect, scan_credentials, scan_defects};
use walkdir::WalkDir;

use super::{find_config, load_config};

pub async fn run(opts: &super::GlobalOptions) -> miette::Result<()> {
    let config = load_config(&find_config(opts)?)?;
    let vault_root = config.vault.root_path();
    // A section a pending task can still fill is work in flight, not a defect — and the queue
    // is the only thing that knows which. Asking it is what separates "nobody is going to do
    // this" from "the drain has not run yet", and the remediation below is destructive if
    // followed for the second one.
    let in_flight = super::queue::work_in_flight(&vault_root);
    let report = audit(
        &managed_roots(&vault_root, &config.vault.dirs),
        &vault_root,
        &in_flight.keys,
    );

    for page in &report.pages {
        eprintln!("✗ {}", rel(&page.path, &vault_root));
        for (line, defect) in &page.defects {
            eprintln!("    L{line}: {}", defect.description());
        }
        for section in &page.unanswered {
            eprintln!("    `{section}` records an input nothing has answered");
        }
        for (line, credential) in &page.credentials {
            eprintln!(
                "    L{line}: matches the form of — {}",
                credential.description()
            );
        }
    }
    // Counted apart from the defects, because they are not one: a defect is a page this tool
    // wrote wrong, and a credential is a string a source sent. Summing them under one word
    // would tell an operator a number that answers neither question.
    let defects = report.defects();
    let credentials = report.credentials();
    let flagged = if credentials == 0 {
        format!("{defects} defect(s)")
    } else {
        format!("{defects} defect(s), {credentials} credential form(s)")
    };
    eprintln!(
        "\n{} pages scanned, {} flagged ({flagged}){}",
        report.scanned,
        report.pages.len(),
        if report.errors > 0 {
            format!(", {} unreadable", report.errors)
        } else {
            String::new()
        }
    );
    if report.scanned == 0 {
        // Which roots are absent is the answer to "check the config", and it is on disk — so it
        // is handed over rather than asked for. A vault before its first ingest legitimately has
        // none of them, which is why this reports and does not gate: an absent directory is a
        // config question, not the vault contradicting itself.
        let missing: Vec<String> = managed_roots(&vault_root, &config.vault.dirs)
            .iter()
            .filter(|root| !root.is_dir())
            .map(|root| rel(root, &vault_root).into_owned())
            .collect();
        eprintln!("\nNo pages found under the configured vault.dirs.");
        if !missing.is_empty() {
            eprintln!(
                "  These do not exist under {}: {}",
                vault_root.display(),
                missing.join(", ")
            );
        }
    }
    if !report.in_flight.is_empty() {
        eprintln!(
            "\n{} section(s) are queued for a drain and not counted above — run `/lore-process`. \
             A re-render empties each of these too, so they are named:",
            report.in_flight.len()
        );
        for (path, section) in &report.in_flight {
            eprintln!("    {}: `{section}`", path.display());
        }
    }
    if report.pages.iter().any(|page| !page.defects.is_empty()) {
        eprintln!(
            "\nA cleanliness defect predates a tightened contract, and WHERE the line sits\n\
             decides the repair. In a section the pipeline renders — frontmatter, headings, the\n\
             event list — re-ingesting that page's source reproduces it clean, because the\n\
             contract is upheld at conversion. In a section an LLM or a human wrote, a\n\
             re-render splices the existing body through unchanged and the defect survives: fix\n\
             the line in place, or delete that section's `llm_inputs.<key>_done` marker so the\n\
             next re-render enqueues it to be written again. A page with no ingestible source\n\
             behind it at all — a concept, an exploration, a review narrative — is only ever\n\
             fixed in place."
        );
    }
    if report.pages.iter().any(|page| !page.credentials.is_empty()) {
        eprintln!(
            "\nThese are SHAPES, not verdicts. A string matching an issuer's credential form may\n\
             be a key or may be that issuer's own documented example — AWS publishes\n\
             `AKIAIOSFODNN7EXAMPLE`, and a page quoting it matches. Only the issuer knows\n\
             which, so decide there.\n\
             \n\
             Where it IS a key it is live wherever it was issued, and the page is not where it\n\
             is fixed. ROTATE at the issuer first — every copy stops working at once, and\n\
             until then editing the page only makes the leak harder to find. The page is a\n\
             record of what a source said, so this reports and never rewrites: what to do with\n\
             the text, and with the git history holding it, is a decision about the record\n\
             rather than about the key.\n\
             \n\
             Read the clean case honestly too. Only forms whose ISSUER publishes a grammar are\n\
             named — a reserved prefix followed by that issuer's own alphabet — because\n\
             nothing in free text distinguishes an unprefixed key from a hash. No finding here\n\
             means no page carries one of THOSE, never that no page carries a secret."
        );
    }
    if report.pages.iter().any(|page| !page.unanswered.is_empty()) {
        eprintln!(
            "\nThese sections record an input hash with no matching answer. A section is enqueued\n\
             again only when its page is RE-RENDERED, and a scheduled run re-renders the current\n\
             date alone, so this does not come back on its own.\n\
             \n\
             Read the marker before choosing a repair:\n\
             \x20 `<key>_done` DIFFERENT   a later render superseded the answer. Deleting that one\n\
             \x20                          line is the whole fix — the next re-render enqueues it.\n\
             \x20 `<key>_done` ABSENT      nothing recorded an answer. Whether one EXISTS is the\n\
             \x20                          question: stamping the hash beside it claims the\n\
             \x20                          section's content was produced from THIS input, and a\n\
             \x20                          filled body never establishes that — which is the whole\n\
             \x20                          reason the marker exists. Stamp only what you can show.\n\
             \x20 the input is not a string  nothing can equal it; only a re-render restores the pair.\n\
             \n\
             A re-render EMPTIES the section. Nothing recorded an answer for the input the page\n\
             now carries, so there is nothing to splice back — the task is enqueued again and a\n\
             drain writes the section from scratch. If the section holds something already, that\n\
             is exactly the case this report cannot tell from an unanswered one, and rewriting\n\
             the page is not reversible: copy the body out first. `lore ingest` and `lore\n\
             synthesis` each name the sections they are about to empty before they write —\n\
             a synthesis page is re-rendered by the latter, never by an ingest.\n\
             \n\
             Re-rendering costs more than it looks. `lore ingest --date <date>` re-renders EVERY\n\
             daily page for that date from a fresh fetch BEFORE it reaches the work-log, and a\n\
             source whose window has passed returns fewer events than the page holds — so\n\
             repairing a work-log truncates the daily pages it aggregates. Measured on this\n\
             author's vault: a date the RSS event log covers came back 8 of 8 events; one older\n\
             than the log, 6 of 18. `--dry-run` reports the event count it would write for each\n\
             daily page — compare it with the count the page states, and read `extracted: N\n\
             items` above it as the whole fetch window rather than that page.\n\
             \x20 daily, document   `lore ingest <source> --date <date>`. A manual document whose\n\
             \x20                   inbox file was archived after ingest has no input left to\n\
             \x20                   re-read, so it is only ever filled in place.\n\
             \x20 work-log          `lore ingest --date <date>` — every source, hence the cost\n\
             \x20                   above; and a date whose sources return no personal events\n\
             \x20                   re-renders nothing at all, silently.\n\
             \x20 synthesis, review `lore synthesis <period>` (`--date`, or `--year` for annual)\n\
             \x20                   — reads persisted pages, fetches nothing, so it is safe.\n\
             \n\
             `concepts` is the exception, and it needs no re-fetch at all. Never stamp its marker\n\
             by hand — that claims an empty section is answered and loses the extraction for\n\
             good. Write a result file to `<vault>/.lorekeeper/queue/results/<name>.json` and run\n\
             `lore queue apply`: it writes the concept pages, the links and the marker in one\n\
             edit, and needs no queue task behind it. The file must be a COMPLETE result — the\n\
             schema is `/lore-process`'s `references/processing-kinds.md`, under\n\
             `kind: extract-concepts` — because a partial one is quarantined, and one whose\n\
             `anchor` or `cache_hash` does not match the page is dropped as dead with exit 0."
        );
    }
    if !in_flight.unread.is_empty() {
        // Said last, because it qualifies everything above: an unanswered section is only a
        // finding if no pending task can fill it, and that is unknown for a queue not fully read.
        eprintln!(
            "\nThe queue could not be read in full, so `no pending task` is not established:"
        );
        for problem in &in_flight.unread {
            eprintln!("  {problem}");
        }
    }
    // Non-zero on a real defect OR on a page that could not be verified — "clean" must
    // never be claimed for content that was skipped.
    //
    // A credential does NOT gate it. Every finding this exits on names a repair inside the
    // vault, and a credential's is at the issuer: the page records what a source sent, so the
    // guidance above says not to edit it, and a run that stayed red until someone did would
    // be red forever. That is the same verdict-carries-no-information failure `lore graph
    // lint` keeps its observation channel out of the exit code to avoid. It is printed first
    // and loudest instead.
    let gating = report
        .pages
        .iter()
        .filter(|page| !page.defects.is_empty() || !page.unanswered.is_empty())
        .count();
    if gating > 0 || report.errors > 0 {
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
pub(crate) fn managed_roots(vault_root: &Path, dirs: &VaultDirs) -> Vec<PathBuf> {
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
    /// Credentials the page carries verbatim. Separate from `defects` because the repair is
    /// not this tool's: a rendering defect is fixed by re-rendering, while a live key is
    /// fixed at its issuer and nowhere else.
    credentials: Vec<(usize, CredentialForm)>,
}

/// The `llm_inputs` sections this page records an input for and no answer to.
///
/// The pipeline stamps `llm_inputs.<key>` when it enqueues the work and whoever does the work
/// stamps `<key>_done` with the same hash; a cache hit is the two being equal, and emptiness
/// signals neither way. So a page carrying the input without a matching answer holds work that
/// was enqueued and lost — `lore queue count` reports nothing pending, and nothing re-enqueues
/// it on its own, because `llm_cache::lookup` runs when a page is RE-RENDERED and a scheduled
/// run only re-renders the current date. `lore ingest --date <that day>` is the recovery: the
/// lookup keys on the `_done` marker, so an absent or mismatched one is a miss.
///
/// Three states are outstanding, and each is a way the marker cannot answer the input: absent,
/// carrying a different hash (an input the page no longer has), or an input recorded as anything
/// but a string, which the pipeline never writes and nothing can equal.
///
/// Read as frontmatter, never as text: the pair is a frontmatter record, so a body discussing the
/// format is prose. `parse_page` owns that boundary — the first line is exactly `---`, a
/// delimiter stands alone on its line, CRLF and a BOM are handled — and is the only reader of it.
///
/// Both keys come from `TargetKind`: `llm_inputs_key` and the `completion_key` derived from it,
/// so a new task kind is covered without being listed here and the `_done` suffix rule stays
/// with the type that owns it.
fn unanswered_sections(page: &lk_core::frontmatter::VaultPage) -> Vec<&'static str> {
    use strum::IntoEnumIterator;

    let inputs = page
        .frontmatter
        .get(lk_core::frontmatter::field::LLM_INPUTS);
    let recorded = |key: &str| inputs.and_then(|v| v.get(key)).and_then(|v| v.as_str());
    let mut found: Vec<&'static str> = lk_queue::TargetKind::iter()
        .filter(|kind| {
            inputs
                .and_then(|v| v.get(kind.llm_inputs_key()))
                .is_some_and(|input| {
                    input
                        .as_str()
                        .is_none_or(|hash| recorded(&kind.completion_key()) != Some(hash))
                })
        })
        .map(|kind| kind.llm_inputs_key())
        .collect();
    found.sort_unstable();
    found.dedup();
    found
}

/// The outcome of a vault audit: pages scanned, which ones carry defects, and how many
/// were unreadable (so the caller can refuse to report "clean" when some were skipped).
pub(crate) struct AuditReport {
    pub(crate) scanned: u32,
    errors: u32,
    pages: Vec<PageDefects>,
    /// Sections a pending task can still fill, as `(page, section)`. Reported, never counted as
    /// a defect: the work is queued and the drain has simply not run yet.
    ///
    /// NAMED, not merely counted: the generated AGENTS.md sends an agent here to find the pages
    /// whose unstamped body the next render will empty, and a queued section is emptied by that
    /// render just the same. A number answers a question nobody asked.
    in_flight: Vec<(PathBuf, &'static str)>,
}

impl AuditReport {
    /// A claim a page makes that does not hold: a defect this tool wrote, or a section whose
    /// input nothing has answered. Counted apart from a credential, which is a string a SOURCE
    /// sent — summing them under one word would report a number that answers neither question.
    pub(crate) fn defects(&self) -> usize {
        self.pages
            .iter()
            .map(|page| page.defects.len() + page.unanswered.len())
            .sum()
    }

    pub(crate) fn credentials(&self) -> usize {
        self.pages.iter().map(|page| page.credentials.len()).sum()
    }
}

/// Walk `roots`, scanning every `.md` against the cleanliness contract. Pure with
/// respect to its return value — no printing of results, no process exit — so it is
/// unit-testable. Symlinks are NOT followed (`follow_links(false)`): pipeline pages are
/// real files, and following links could escape the vault or loop. A missing root is
/// skipped (a vault may not have produced a synthesis yet); an unreadable or non-UTF-8
/// file is counted as an error (not as clean), since pipeline output is always valid
/// UTF-8 — this only ever fires on a foreign file a user dropped under a managed root.
pub(crate) fn audit(
    roots: &[PathBuf],
    vault_root: &Path,
    in_flight_keys: &std::collections::HashSet<(PathBuf, &'static str)>,
) -> AuditReport {
    let mut scanned = 0u32;
    let mut errors = 0u32;
    let mut in_flight: Vec<(PathBuf, &'static str)> = Vec::new();
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
            // `scan_defects` reads the bytes as written — an inlined `data:` URI is a fact about
            // the text — while the marker pair is a frontmatter record and is read as one. A
            // page whose frontmatter will not parse is unverifiable rather than clean.
            let defects = scan_defects(&text);
            let credentials = scan_credentials(&text);
            let unanswered = match lk_core::frontmatter::parse_page(&text) {
                Ok(page) => {
                    let rel = path.strip_prefix(vault_root).unwrap_or(path).to_path_buf();
                    let (queued, outstanding): (Vec<_>, Vec<_>) = unanswered_sections(&page)
                        .into_iter()
                        .partition(|key| in_flight_keys.contains(&(rel.clone(), *key)));
                    in_flight.extend(queued.into_iter().map(|key| (rel.clone(), key)));
                    outstanding
                }
                Err(e) => {
                    eprintln!("⚠ {}: frontmatter: {e}", path.display());
                    errors += 1;
                    Vec::new()
                }
            };
            if !defects.is_empty() || !unanswered.is_empty() || !credentials.is_empty() {
                pages.push(PageDefects {
                    path: path.to_path_buf(),
                    defects,
                    unanswered,
                    credentials,
                });
            }
        }
    }

    AuditReport {
        scanned,
        errors,
        pages,
        in_flight,
    }
}

/// Path relative to the vault root for compact output, falling back to the full
/// path if the page somehow sits outside it.
fn rel<'a>(path: &'a Path, root: &Path) -> std::borrow::Cow<'a, str> {
    path.strip_prefix(root).unwrap_or(path).to_string_lossy()
}

#[cfg(test)]
mod tests {
    /// Most cases are about a page's own contents, with no queue behind them: an empty in-flight
    /// set is "no task can fill this", which is what makes an unanswered section a finding.
    fn audit_with_empty_queue(roots: &[PathBuf]) -> AuditReport {
        audit(roots, Path::new(""), &std::collections::HashSet::new())
    }

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
    /// Tests state a page as its file contents, the way the walk finds it, and go through the
    /// same parse the walk uses.
    /// A credential is a finding of its own, not a text defect: the two have nothing in
    /// common but the page they sit on, and everything the report says about repairing one
    /// is wrong for the other.
    #[test]
    fn a_credential_is_reported_apart_from_a_rendering_defect() {
        let dir = tempfile::tempdir().expect("tempdir");
        let daily = dir.path().join("daily").join("slack");
        std::fs::create_dir_all(&daily).expect("mkdir");
        // Assembled rather than written, so this file carries no literal a secret scanner
        // reads as a live key — the same reason the scanner's own fixtures are built.
        let token = ["ghp", "_0123456789abcdefghij", "klmnopqrstuvwxyz"].concat();
        std::fs::write(
            daily.join("2026-05-23.md"),
            format!(
                "---\nid: d\ntype: daily\n---\n\n## Key Messages\n\n\
                 - deploy with GITHUB_TOKEN={token}\n"
            ),
        )
        .expect("page");

        let report = audit(&[dir.path().join("daily")], dir.path(), &Default::default());
        assert_eq!(report.pages.len(), 1, "{:?}", report.pages.len());
        let page = &report.pages[0];
        assert_eq!(
            page.credentials,
            vec![(8, lk_core::markdown::CredentialForm::GitHubToken)]
        );
        assert!(page.defects.is_empty(), "a key is not a rendering defect");
        assert!(page.unanswered.is_empty());
    }

    /// The ordinary vault is silent. A page full of links, hashes and prose names nothing,
    /// which is what makes a finding worth reading.
    #[test]
    fn an_ordinary_page_names_no_credential() {
        let dir = tempfile::tempdir().expect("tempdir");
        let daily = dir.path().join("daily").join("slack");
        std::fs::create_dir_all(&daily).expect("mkdir");
        std::fs::write(
            daily.join("2026-05-23.md"),
            "---\nid: d\ntype: daily\nllm_inputs:\n---\n\n## Key Messages\n\n\
             - [RAG](../../wiki/concepts/rag.md) at commit 0f1e2d3c4b5a6978\n\
             - keys beginning ghp_ must be rotated\n",
        )
        .expect("page");

        let report = audit(&[dir.path().join("daily")], dir.path(), &Default::default());
        assert!(report.pages.is_empty(), "{:?}", report.pages.len());
    }

    fn unanswered(text: &str) -> Vec<&'static str> {
        unanswered_sections(&lk_core::frontmatter::parse_page(text).expect("frontmatter"))
    }

    #[test]
    fn audit_flags_a_section_whose_work_was_enqueued_and_never_answered() {
        let answered = "---\nid: a\nllm_inputs:\n  summary: \"h1\"\n  summary_done: \"h1\"\n---\n\n## Summary\n";
        assert!(
            unanswered(answered).is_empty(),
            "an answered section is not a defect, even with an empty body"
        );

        let pending = "---\nid: a\nllm_inputs:\n  summary: \"h1\"\n  concepts: \"h2\"\n  concepts_done: \"h2\"\n---\n\n## Summary\n";
        assert_eq!(
            unanswered(pending),
            vec!["summary"],
            "the input without its answer is the one reported"
        );

        assert!(
            unanswered("---\nid: a\n---\n\n# no llm_inputs at all\n").is_empty(),
            "a page the pipeline never enqueued work for has nothing outstanding"
        );

        // Through the walk, so the finding reaches the report the CLI prints.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("daily");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("answered.md"), answered).unwrap();
        fs::write(root.join("pending.md"), pending).unwrap();
        let report = audit_with_empty_queue(&[root]);
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

    /// A page that WRITES ABOUT the contract is not a page that violates it. The marker pair is
    /// a frontmatter record; prose is prose. A vault whose concepts include its own tooling is
    /// the ordinary case, so this shape is reachable wherever the contract is documented.
    #[test]
    fn a_page_whose_body_explains_the_marker_contract_is_not_a_defect() {
        let page = "---\nid: markers\ntype: concept\ntitle: \"Completion markers\"\n---\n\n\
                    ## Synthesis\n\n\
                    A page records llm_inputs: entries such as\n\
                    \x20 summary: \"deadbeef\"\n\
                    and the drain stamps the companion marker.\n\n\
                    ---\n\n\
                    ## Sources\n";
        assert!(
            unanswered(page).is_empty(),
            "prose about the format is not a record: {:?}",
            unanswered(page)
        );
    }

    /// A marker left over from a superseded input is not an answer to the input on the page:
    /// `llm_cache::lookup` caches on the two being EQUAL and the drain copies the hash verbatim,
    /// so a mismatch is outstanding work.
    #[test]
    fn a_marker_from_a_superseded_input_is_not_an_answer() {
        let stale = "---\nid: a\nllm_inputs:\n  summary: \"new\"\n  summary_done: \"old\"\n---\n\n## Summary\n";
        assert_eq!(unanswered(stale), vec!["summary"]);

        let matching = "---\nid: a\nllm_inputs:\n  summary: \"new\"\n  summary_done: \"new\"\n---\n\n## Summary\n";
        assert!(unanswered(matching).is_empty());
    }

    /// A key that merely LOOKS like a record is not one: a top-level `summary:` is a sibling of
    /// `llm_inputs` rather than an entry in it, and an `llm_inputs` that is a scalar records
    /// nothing at all.
    #[test]
    fn only_entries_inside_the_llm_inputs_mapping_count() {
        let sibling = "---\nid: a\nsummary: \"h1\"\nllm_inputs:\n  concepts: \"h2\"\n  concepts_done: \"h2\"\n---\n\n## Summary\n";
        assert!(
            unanswered(sibling).is_empty(),
            "a top-level key is not an llm_inputs entry: {:?}",
            unanswered(sibling)
        );

        let scalar = "---\nid: a\nllm_inputs: \"summary: h1\"\n---\n\n## Summary\n";
        assert!(unanswered(scalar).is_empty(), "a scalar records nothing");
    }

    /// An input hash recorded as anything but a string is malformed — the pipeline writes hex —
    /// and nothing can equal it, so the section is unanswerABLE rather than answered.
    #[test]
    fn an_input_recorded_as_a_non_string_can_never_be_answered() {
        for value in ["[\"h1\"]", "{a: 1}", "123", "true"] {
            let page = format!("---\nid: a\nllm_inputs:\n  summary: {value}\n---\n\n## Summary\n");
            assert_eq!(
                unanswered(&page),
                vec!["summary"],
                "`summary: {value}` records an input nothing can answer"
            );
        }
    }

    /// A flow-style mapping is the same record written inline, and the page carrying one is the
    /// worst case to miss: `lk_vault::set_llm_input` REFUSES that shape rather than writing past
    /// it, so such a page can never receive a `_done` marker and is unanswered by construction.
    #[test]
    fn an_inline_mapping_records_the_same_inputs_as_a_block_one() {
        let flow = "---\nid: a\nllm_inputs: {summary: \"h1\"}\n---\n\n## Summary\n";
        assert_eq!(unanswered(flow), vec!["summary"]);

        let answered =
            "---\nid: a\nllm_inputs: {summary: \"h1\", summary_done: \"h1\"}\n---\n\n## Summary\n";
        assert!(unanswered(answered).is_empty());
    }

    /// A page whose frontmatter will not parse is unverifiable, not clean — `doctor` must never
    /// report a page it could not read as having nothing outstanding.
    #[test]
    fn a_page_with_unparseable_frontmatter_is_counted_as_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("daily");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("broken.md"),
            "---\nid: a\nllm_inputs:\n  summary: \"h\"\n",
        )
        .unwrap();

        let report = audit_with_empty_queue(&[root]);
        assert_eq!(
            report.errors, 1,
            "an unclosed frontmatter block is an error"
        );
        assert!(
            report.pages.is_empty(),
            "nothing is claimed about a page that could not be read"
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
                unanswered(&page),
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

        let report = audit_with_empty_queue(&[root]);
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

        let report = audit_with_empty_queue(&[managed]);
        assert_eq!(report.scanned, 1);
        assert!(
            report.pages.is_empty(),
            "content outside the managed roots must never be scanned"
        );
    }

    #[test]
    fn audit_skips_a_missing_root_without_error() {
        let dir = tempfile::tempdir().unwrap();
        let report = audit_with_empty_queue(&[dir.path().join("synthesis-that-does-not-exist")]);
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
