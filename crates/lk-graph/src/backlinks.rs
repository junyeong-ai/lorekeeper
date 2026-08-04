//! Re-derive every concept page's evidence-dependent state from the actual link graph —
//! this module is the single source-of-truth for all of it. A concept's evidence is the set
//! of pages that cite it, and three things on the page answer to that set: the `##
//! <Sources>` body, the frontmatter `source_count`, and the `llm_inputs.synthesis` input
//! its `## <Synthesis>` is owed against. Ingest writes none of them — it renders the sources
//! heading empty, preserves the on-disk count verbatim (0 for a new page), and carries the
//! recorded input through unchanged — so this sync is what makes them correct. Manual edits
//! (a daily page gains or loses a concept link) and source-page deletions are reflected on
//! the next run.
//!
//! Pure set comparison: no heuristics, no LLM, no per-source rules. A page is in the sources
//! list iff it has an outgoing link that resolves to the concept.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use lk_core::concept::citation_digest;
use lk_core::config::VaultDirs;
use lk_core::frontmatter;
use lk_core::link;
use lk_vault::{
    VaultWriter, record_llm_input, replace_section, resolve_section, section_body,
    set_frontmatter_field,
};
use serde::Serialize;

use crate::GraphError;
use crate::scan::{ScannedPage, VaultExistence, is_concept_page, is_valid_source, path_slug};

/// Outcome of one concept page's reconciliation.
#[derive(Debug, Clone, Serialize)]
pub struct ConceptUpdate {
    /// Vault-relative path of the concept page that was (or would be) updated.
    pub path: PathBuf,
    /// Sources that were missing from the page and would be added.
    pub added: Vec<String>,
    /// Sources that were on the page but no longer have an incoming link.
    pub removed: Vec<String>,
}

/// A concept page whose synthesis was written against a citation set the page no longer
/// carries — or against none at all. The work this sweep hands to the LLM queue.
#[derive(Debug, Clone, Serialize)]
pub struct ConceptResynthesis {
    /// Vault-relative path of the concept page.
    pub path: PathBuf,
    /// The synthesis heading as the page spells it, so a result lands where the page can
    /// receive it even when `vault.locale` has moved since the page was written.
    pub anchor: String,
    /// The pages citing this concept, sorted — the evidence the rewritten synthesis
    /// answers to.
    pub citations: Vec<String>,
    /// The body the rewrite will REPLACE, when the section holds one.
    ///
    /// Every other LLM-owned section names what a re-render is about to discard
    /// (`llm_cache::SectionDecision::discarding`), because a body somebody wrote without
    /// recording it is not recoverable once the page is rewritten — and a concept's synthesis
    /// is the section most likely to hold exactly that, since a human or `/lore-wiki add`
    /// authors it at creation. Reported rather than left to be noticed afterwards.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discarding: Option<String>,
}

/// Outcome of a backlinks-sync run (a mutating operation), mirroring `MergeResult`:
/// domain-module operation outcomes are `*Result`; the CLI presentation wrapper is
/// `output::BacklinksSyncReport`.
#[derive(Debug, Default, Serialize)]
pub struct BacklinksSyncResult {
    /// Concept pages whose derived state differed and was rewritten (or would be,
    /// under `--dry-run`).
    pub updated: Vec<ConceptUpdate>,
    /// Count of concept pages whose derived state was already correct.
    pub unchanged: usize,
    /// Concept pages whose written synthesis was adopted as the answer for the evidence they
    /// carry, because they had recorded no input before this run. Reported so an upgrade says
    /// what it decided rather than leaving it to be discovered.
    pub adopted: Vec<PathBuf>,
    /// Concept pages whose `## <Synthesis>` has not been written against the citation set
    /// the page now carries. Reported rather than acted on here: this crate computes what
    /// is true of the vault and never calls an LLM, so the caller is what turns the list
    /// into queued work.
    ///
    /// Independent of `updated` — a page whose sources were already correct can still be
    /// owed a synthesis, because writing the section is a separate act from deriving the
    /// evidence it answers to.
    pub resynthesize: Vec<ConceptResynthesis>,
    /// Concept pages that could not record their derived state: they carry no frontmatter
    /// block, or one that will not parse, or no heading to hold one of the sections. Reported
    /// rather than silently skipped — their citation counts are stale until a human repairs
    /// the page — and the command exits non-zero while any remain. Skipped rather than fatal,
    /// because aborting mid-sweep leaves every page already written synced and every page
    /// after it not, and one corrupt page blocks it forever.
    ///
    /// Always serialized. This list IS the command's verdict, so omitting it when empty hides
    /// the field on exactly the clean runs a consumer sees most, where `undefined` reads as
    /// neither empty nor absent.
    pub skipped: Vec<PathBuf>,
    /// Cited concept pages carrying no synthesis heading. Their citations and count ARE
    /// written — the two facts are independent — but nothing can be owed against evidence the
    /// page has nowhere to answer.
    pub headless: Vec<PathBuf>,
    /// Whether this was a dry run (no writes were performed).
    pub dry_run: bool,
}

/// Recompute the `## <Sources>` section on every `{wiki}/concepts/{slug}.md` page from
/// the incoming links observed in `pages`, and rewrite any page whose section
/// differs. Returns the diff per page plus a count of pages that needed no change.
///
/// `dry_run = true` skips all writes but still reports what would change. Re-running
/// the command after a real run is a no-op: the sources block is generated by the
/// same routine that compares against it.
///
/// Takes no locale: every heading here is READ, under any locale's spelling, and a page
/// carrying none is skipped rather than given one — so there is no write for a locale to decide.
/// Whether this sweep records the input a concept's synthesis is owed against.
///
/// The input is a promise that something will answer it: `lore doctor` reports a recorded
/// input with no matching answer, and nothing re-renders a concept page's markers from
/// scratch, so an input written where no drain can ever run is a finding that never clears.
/// A run with no LLM plane behind it therefore keeps the citation bookkeeping and writes no
/// promise — the same reason a concept nothing cites records nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SynthesisPolicy {
    Record,
    Skip,
}

pub fn sync_concept_backlinks(
    pages: &[ScannedPage],
    vault_root: &Path,
    dry_run: bool,
    dirs: &VaultDirs,
    synthesis: SynthesisPolicy,
) -> Result<BacklinksSyncResult, GraphError> {
    // Reverse index: concept page id → sorted set of source page ids that cite it.
    // Each link already carries its resolved page id (scan resolves every destination
    // against its page's location), so crediting is a set-membership check — `## Sources`
    // and `source_count` can never diverge from the link graph. Only non-concept content
    // pages count as sources (`is_valid_source`); a concept→concept link belongs in
    // `## Related`, and navigation pages aren't sources. BTreeMap/BTreeSet keep the
    // rendered body deterministic.
    // `pages` here is a full-vault scan of every page dir, so it IS the vault.
    let existence = VaultExistence::build(pages, dirs);
    let by_id: HashMap<&str, &ScannedPage> = pages.iter().map(|p| (p.id.as_str(), p)).collect();
    let mut incoming: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for page in pages {
        if !is_valid_source(&page.path, dirs) {
            continue;
        }
        for target in page
            .outgoing
            .iter()
            .filter_map(|link| existence.reached(link))
        {
            // Self-references are excluded for the same reason the graph excludes
            // self-edges.
            if target != page.id && existence.is_knowledge(target) {
                incoming
                    .entry(target.to_owned())
                    .or_default()
                    .insert(page.id.clone());
            }
        }
    }

    let writer = VaultWriter::new(vault_root);
    let mut report = BacklinksSyncResult {
        dry_run,
        ..BacklinksSyncResult::default()
    };

    for page in pages {
        if !is_concept_page(&page.path, dirs) {
            continue;
        }

        if page.id.is_empty() {
            continue;
        }

        // Sources for this concept = every page that links it.
        let mut sources: Vec<String> = incoming
            .get(&page.id)
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default();
        sources.sort();

        let full_path = vault_root.join(&page.path);
        let raw = std::fs::read_to_string(&full_path)
            .map_err(|e| GraphError::Io(format!("failed to read {}: {e}", full_path.display())))?;

        // The page was read successfully; a frontmatter that will not parse here is real
        // corruption, and every write below would have to guess what it is replacing.
        // Recorded and skipped rather than fatal, because aborting mid-sweep leaves every
        // page already written synced and every page after it not, and one corrupt page
        // would block the janitor forever.
        let Ok(parsed) = frontmatter::parse_page(&raw) else {
            report.skipped.push(page.path.clone());
            continue;
        };

        // Headings are found under ANY locale (a page authored before a `vault.locale` switch
        // keeps its old-language spelling) and rewritten under the one the page actually
        // carries — mirrors the pipeline's all-locale `capture_section`.
        //
        // A page with no sources heading has nowhere to hold its citations. The list and the
        // count are one fact, so neither is written: falling back to the current locale's
        // spelling would leave `replace_section` nothing to replace while the count was
        // written anyway, and the page would assert evidence that appears nowhere on it.
        let Some(sources_section) = resolve_section(&raw, |s| s.concept_sources) else {
            report.skipped.push(page.path.clone());
            continue;
        };

        let existing = parse_existing_sources(&raw, sources_section.heading, &page.path);
        let existing_set: BTreeSet<&str> = existing.iter().map(String::as_str).collect();
        let desired_set: BTreeSet<&str> = sources.iter().map(String::as_str).collect();

        // `source_count` is re-derived here too (ingest preserves the on-disk value but never
        // computes it): the authoritative count is the number of incoming citations, the same
        // set that drives the `## Sources` body.
        let desired_count = sources.len() as u64;

        // The digest of that same set is what the page's synthesis is owed against, and
        // recording it is this sweep's job for the same reason the count is: it is the one
        // place that knows which pages cite this concept.
        //
        // A concept NOTHING cites has no evidence for a synthesis to answer to, so it records
        // no input and is owed nothing — there would be nothing for a drain to read. A concept
        // whose last citation was deleted keeps the input it already carries: its synthesis
        // still states what the vault learned, and no rewrite could improve on it from a set
        // that is now empty.
        //
        // The synthesis heading is required only where a synthesis is OWED. Requiring it
        // unconditionally would let a page missing that one section keep a citation count the
        // graph contradicts, forever, and hold the sweep's exit code non-zero with it — a
        // page nothing cites owes no synthesis and is still owed a correct count of zero.
        let synthesis_section = resolve_section(&raw, |s| s.concept_synthesis);
        let owed_input = (synthesis == SynthesisPolicy::Record
            && !sources.is_empty()
            && synthesis_section.is_some())
        .then(|| citation_digest(&sources));
        let completion_key = frontmatter::field::completion(frontmatter::field::SYNTHESIS);
        let recorded_answer = llm_input(&parsed, &completion_key);

        // A page that carries a written synthesis and has never recorded an input is one
        // somebody WROTE — by hand, by `/lore-wiki add`, or as the grounding sentence of the
        // extraction that created it — before anything tracked what it answers to. Its prose
        // is adopted as the answer for the evidence the page has today, not queued for
        // replacement.
        //
        // The alternative is what an upgrade would otherwise do: every cited concept is owed
        // at once, the drain REWRITES rather than appends, and the whole authored corpus is
        // replaced in the first unattended run — irreversibly, for a vault not in git. No
        // amount of warning printed to a scheduler's log undoes that, because the rewrite
        // follows seconds later in the same run.
        //
        // Adoption costs nothing that is not recovered: the next time the citation set MOVES
        // the page is owed a rewrite like any other, and forcing one sooner is the lever every
        // other section already has — delete the `_done` marker.
        let adopting = owed_input.is_some()
            && recorded_answer.is_none()
            && llm_input(&parsed, frontmatter::field::SYNTHESIS).is_none()
            && synthesis_section.is_some_and(|section| !section.body.trim().is_empty());

        // Deriving the evidence and writing the section that answers to it are separate acts,
        // so the two questions are asked separately: a page whose sources list is already
        // correct can still be owed a synthesis, and asking them together would leave it owed
        // forever.
        if let (Some(digest), Some(section)) = (&owed_input, &synthesis_section)
            && !adopting
            && recorded_answer != Some(digest.as_str())
        {
            report.resynthesize.push(ConceptResynthesis {
                path: page.path.clone(),
                anchor: format!("## {}", section.heading),
                citations: sources.clone(),
                discarding: (!section.body.trim().is_empty())
                    .then(|| section.body.trim().to_string()),
            });
        }

        // A cited page with nowhere to put a synthesis is a defect of its own: it can record
        // its citations and cannot record what they are owed. Reported so the count still
        // lands and the missing section is named, rather than one blocking the other.
        if !sources.is_empty() && synthesis_section.is_none() {
            report.headless.push(page.path.clone());
        }
        if adopting {
            report.adopted.push(page.path.clone());
        }

        // A page with no frontmatter block, or none carrying the `llm_inputs:` mapping, has
        // nowhere to record what this sweep derives. Skipped like the corrupt case above, and
        // the command still exits non-zero — including for the synthesis it can no longer be
        // owed, since a task naming a page that cannot receive the result would fail every
        // drain forever.
        let new_body = render_sources_body(&sources, &page.path, &by_id);
        let Some(updated_content) = set_source_count(
            &replace_section(&raw, sources_section.heading, &new_body),
            desired_count,
        )
        .and_then(|content| match &owed_input {
            None => Some(content),
            Some(digest) => {
                let stamp = serde_json::to_string(digest).expect("a hex digest always serializes");
                let recorded = record_llm_input(&content, frontmatter::field::SYNTHESIS, &stamp)?;
                match adopting {
                    // Adoption stamps BOTH: the input the prose is taken to answer, and the
                    // answer itself. One without the other is the unanswered state `lore
                    // doctor` reports.
                    true => record_llm_input(&recorded, &completion_key, &stamp),
                    false => Some(recorded),
                }
            }
        }) else {
            report.skipped.push(page.path.clone());
            report.resynthesize.retain(|owed| owed.path != page.path);
            report.headless.retain(|path| path != &page.path);
            continue;
        };

        // The verdict is the page this sweep would write against the page on disk, not a
        // field-by-field comparison: everything derived is rendered into the candidate, so
        // whatever differs is by definition drift. A source page RETITLED changes its
        // citation's display text without changing which page is cited — the set is the
        // same, so the synthesis stays answered while the line naming it is corrected.
        if updated_content == raw {
            report.unchanged += 1;
            continue;
        }

        if !dry_run {
            writer
                .write_page_sync(&page.path, &updated_content)
                .map_err(|e| {
                    GraphError::Io(format!("failed to write {}: {e}", page.path.display()))
                })?;
        }

        report.updated.push(ConceptUpdate {
            path: page.path.clone(),
            added: desired_set
                .difference(&existing_set)
                .map(|s| (*s).to_owned())
                .collect(),
            removed: existing_set
                .difference(&desired_set)
                .map(|s| (*s).to_owned())
                .collect(),
        });
    }

    Ok(report)
}

/// The value recorded under `llm_inputs.<key>`, if the page carries one.
fn llm_input<'a>(page: &'a frontmatter::VaultPage, key: &str) -> Option<&'a str> {
    page.frontmatter
        .get(frontmatter::field::LLM_INPUTS)
        .and_then(|v| v.get(key))
        .and_then(|v| v.as_str())
}

/// Extract the resolved page ids of the links in the existing `## <heading>` section,
/// in their on-disk order. Used only to compute the diff (added/removed) — the
/// rewritten body is canonicalised through [`render_sources_body`].
pub(crate) fn parse_existing_sources(
    content: &str,
    heading: &str,
    concept_path: &Path,
) -> Vec<String> {
    // Read the section body through the same fence-aware boundary `replace_section`
    // rewrites against, so the diff and the rewrite never disagree about where the
    // section ends (a fenced `## ` inside the body is content, not a boundary).
    let Some(body) = section_body(content, heading) else {
        return Vec::new();
    };

    link::extract_dests(body)
        .into_iter()
        .filter_map(|dest| link::resolve_dest(concept_path, &dest))
        .map(|resolved| path_slug(&resolved))
        .filter(|id| !id.is_empty())
        .collect()
}

/// Set the frontmatter `source_count` — a thin wrapper over the single-sourced
/// `set_frontmatter_field` so backlinks and the audit marker share one writer.
fn set_source_count(content: &str, count: u64) -> Option<String> {
    set_frontmatter_field(
        content,
        frontmatter::field::SOURCE_COUNT,
        &count.to_string(),
    )
}

/// Render the sources list: `- [title](relative-path)` per line, newline-separated,
/// no trailing newline (the section helper owns separators). Each destination is
/// relative to the concept page's own directory, the address form every emitter
/// writes; the display text is the source page's title.
pub(crate) fn render_sources_body(
    sources: &[String],
    concept_path: &Path,
    by_id: &HashMap<&str, &ScannedPage>,
) -> String {
    sources
        .iter()
        .map(|id| {
            let (title, dest) = match by_id.get(id.as_str()) {
                Some(source) => (
                    source.title.as_str(),
                    link::relative_dest(concept_path, &source.path),
                ),
                // A source id always comes from the same scan `by_id` was built from,
                // so this arm is unreachable in practice; addressing by id keeps the
                // entry meaningful if it ever isn't.
                None => (id.as_str(), link::encode_dest(&format!("/{id}.md"))),
            };
            format!("- {}", link::md_link(title, &dest))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::Link;
    use tempfile::TempDir;

    fn build_page(id: &str, rel: &str, outgoing: &[&str]) -> ScannedPage {
        ScannedPage {
            id: id.to_owned(),
            path: PathBuf::from(rel),
            title: id.to_owned(),
            outgoing: outgoing
                .iter()
                .map(|s| Link::to(&format!("{s}.md")))
                .collect(),
        }
    }

    /// A concept page as the template renders one: every section the sweep reads, and the
    /// `llm_inputs:` mapping it records the synthesis input in.
    fn write_concept(dir: &TempDir, stem: &str, source_lines: &[&str]) {
        write_concept_page(dir, stem, source_lines, "");
    }

    fn write_concept_page(dir: &TempDir, stem: &str, source_lines: &[&str], llm_inputs: &str) {
        let path = dir.path().join("wiki/concepts").join(format!("{stem}.md"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let count = source_lines.len();
        // The sources body as `replace_section` renders it, so a fixture nothing has changed
        // about compares equal to what the sweep would write.
        let sources = match source_lines {
            [] => String::new(),
            lines => format!("{}\n\n", lines.join("\n")),
        };
        std::fs::write(
            &path,
            format!(
                "---\nid: {stem}\nsource_count: {count}\nllm_inputs:\n{llm_inputs}---\n\n\
                 # {stem}\n\n## 핵심\n\nWhat {stem} is.\n\n## 출처\n\n{sources}\
                 ## 메타\n\n- key: value\n"
            ),
        )
        .unwrap();
    }

    /// The `llm_inputs` block of a page whose synthesis was written from `citations`.
    fn synthesized_from(citations: &[&str]) -> String {
        let digest = citation_digest(
            &citations
                .iter()
                .map(|s| (*s).to_owned())
                .collect::<Vec<_>>(),
        );
        format!("  synthesis: \"{digest}\"\n  synthesis_done: \"{digest}\"\n")
    }

    #[test]
    fn inserts_missing_backlink() {
        let dir = TempDir::new().unwrap();
        write_concept(&dir, "oy365", &[]);

        let pages = vec![
            build_page("wiki/concepts/oy365", "wiki/concepts/oy365.md", &[]),
            // A daily page that links the concept.
            build_page(
                "daily/slack/2026-05-20",
                "daily/slack/2026-05-20.md",
                &["wiki/concepts/oy365"],
            ),
        ];

        let report = sync_concept_backlinks(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert_eq!(report.updated.len(), 1);
        assert_eq!(report.unchanged, 0);
        assert_eq!(report.updated[0].added, vec!["daily/slack/2026-05-20"]);
        assert!(report.updated[0].removed.is_empty());

        // The entry links the source page relative to the concept's own directory,
        // titled by the source page's title.
        let content = std::fs::read_to_string(dir.path().join("wiki/concepts/oy365.md")).unwrap();
        assert!(content.contains(
            "## 출처\n\n- [daily/slack/2026-05-20](../../daily/slack/2026-05-20.md)\n\n## 메타"
        ));
        assert!(content.contains("source_count: 1"));
    }

    /// `source_count` is re-derived, not merely kept consistent with the section beside it.
    /// A page whose entries are already right but whose count is wrong — a hand edit, a run
    /// that died between the two writes — is exactly the drift this command exists to
    /// repair, and it is the one shape that looks unchanged if the two facts are checked as
    /// alternatives rather than together.
    #[test]
    fn a_correct_source_list_with_a_wrong_count_is_still_repaired() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("wiki/concepts/oy365.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "---\nid: oy365\nsource_count: 7\nllm_inputs:\n---\n\n# oy365\n\n## 핵심\n\nWhat oy365 is.\n\n## 출처\n\n- [daily/slack/2026-05-20](../../daily/slack/2026-05-20.md)\n\n## 메타\n\n- key: value\n",
        )
        .unwrap();

        let pages = vec![
            build_page("wiki/concepts/oy365", "wiki/concepts/oy365.md", &[]),
            build_page(
                "daily/slack/2026-05-20",
                "daily/slack/2026-05-20.md",
                &["wiki/concepts/oy365"],
            ),
        ];

        let report = sync_concept_backlinks(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert_eq!(report.unchanged, 0, "a wrong count is not 'unchanged'");
        assert_eq!(report.updated.len(), 1);
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("source_count: 1"), "{content}");
    }

    #[test]
    fn removes_stale_backlink() {
        let dir = TempDir::new().unwrap();
        // Disk says the concept is referenced by an old daily page that no longer
        // links to it.
        write_concept(&dir, "oy365", &["- [old](../../daily/slack/2026-01-01.md)"]);

        let pages = vec![build_page(
            "wiki/concepts/oy365",
            "wiki/concepts/oy365.md",
            &[],
        )];

        let report = sync_concept_backlinks(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert_eq!(report.updated.len(), 1);
        assert!(report.updated[0].added.is_empty());
        assert_eq!(report.updated[0].removed, vec!["daily/slack/2026-01-01"]);

        let content = std::fs::read_to_string(dir.path().join("wiki/concepts/oy365.md")).unwrap();
        // Body collapsed to empty — section heading remains, no list items.
        assert!(content.contains("## 출처\n\n## 메타"), "{content}");
    }

    #[test]
    fn preserves_other_sections() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("wiki/concepts/oy365.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "---\nid: oy365\nllm_inputs:\n---\n\n# oy365\n\n## 핵심\n\nThis is the synthesis paragraph.\n\n## 출처\n\n- [old](../../daily/x/old.md)\n\n## 메타\n\n- references: 1\n",
        )
        .unwrap();

        let pages = vec![
            build_page("wiki/concepts/oy365", "wiki/concepts/oy365.md", &[]),
            build_page(
                "daily/x/2026-05-20",
                "daily/x/2026-05-20.md",
                &["wiki/concepts/oy365"],
            ),
        ];

        sync_concept_backlinks(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("## 핵심\n\nThis is the synthesis paragraph."));
        assert!(content.contains("## 메타\n\n- references: 1"));
        assert!(content.contains("## 출처\n\n- [daily/x/2026-05-20](../../daily/x/2026-05-20.md)"));
        assert!(!content.contains("(../../daily/x/old.md)"));
    }

    #[test]
    fn dry_run_does_not_write() {
        let dir = TempDir::new().unwrap();
        write_concept(&dir, "oy365", &[]);
        let before = std::fs::read_to_string(dir.path().join("wiki/concepts/oy365.md")).unwrap();

        let pages = vec![
            build_page("wiki/concepts/oy365", "wiki/concepts/oy365.md", &[]),
            build_page(
                "daily/x/2026-05-20",
                "daily/x/2026-05-20.md",
                &["wiki/concepts/oy365"],
            ),
        ];

        let report = sync_concept_backlinks(
            &pages,
            dir.path(),
            true,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert!(report.dry_run);
        assert_eq!(report.updated.len(), 1);

        let after = std::fs::read_to_string(dir.path().join("wiki/concepts/oy365.md")).unwrap();
        assert_eq!(before, after, "dry-run must not write");
    }

    #[test]
    fn idempotent_on_second_run() {
        let dir = TempDir::new().unwrap();
        write_concept(&dir, "oy365", &[]);

        let pages = vec![
            build_page("wiki/concepts/oy365", "wiki/concepts/oy365.md", &[]),
            build_page(
                "daily/x/2026-05-20",
                "daily/x/2026-05-20.md",
                &["wiki/concepts/oy365"],
            ),
        ];

        // First run rewrites the page.
        let first = sync_concept_backlinks(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert_eq!(first.updated.len(), 1);

        // Second run: every concept page is already in sync.
        let second = sync_concept_backlinks(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert!(second.updated.is_empty());
        assert_eq!(second.unchanged, 1);
    }

    #[test]
    fn english_locale_uses_sources_heading() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("wiki/concepts/oy365.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "---\nid: oy365\nllm_inputs:\n---\n\n# oy365\n\n## Synthesis\n\nWhat oy365 is.\n\n## Sources\n\n\n## Metadata\n",
        )
        .unwrap();

        let pages = vec![
            build_page("wiki/concepts/oy365", "wiki/concepts/oy365.md", &[]),
            build_page(
                "daily/x/2026-05-20",
                "daily/x/2026-05-20.md",
                &["wiki/concepts/oy365"],
            ),
        ];

        sync_concept_backlinks(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains(
            "## Sources\n\n- [daily/x/2026-05-20](../../daily/x/2026-05-20.md)\n\n## Metadata"
        ));
    }

    #[test]
    fn self_link_to_concept_is_ignored() {
        // A concept page that links itself should not list itself as a source.
        let dir = TempDir::new().unwrap();
        write_concept_page(&dir, "oy365", &[], &synthesized_from(&[]));

        let pages = vec![build_page(
            "wiki/concepts/oy365",
            "wiki/concepts/oy365.md",
            &["wiki/concepts/oy365"],
        )];

        let report = sync_concept_backlinks(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert_eq!(report.updated.len(), 0);
        assert_eq!(report.unchanged, 1);
    }

    #[test]
    fn concept_to_concept_link_is_not_a_source() {
        // Only content pages qualify as sources; a citation from another concept
        // belongs in `## Related` and must not be credited here.
        let dir = TempDir::new().unwrap();
        write_concept_page(&dir, "target", &[], &synthesized_from(&[]));
        write_concept_page(&dir, "citer", &[], &synthesized_from(&[]));

        let pages = vec![
            build_page("wiki/concepts/target", "wiki/concepts/target.md", &[]),
            build_page(
                "wiki/concepts/citer",
                "wiki/concepts/citer.md",
                &["wiki/concepts/target"],
            ),
        ];

        let report = sync_concept_backlinks(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert!(report.updated.is_empty());
    }

    /// A concept whose evidence has moved is owed a rewritten synthesis, and the sweep that
    /// derives the evidence is what says so. The anchor travels with it: a page keeps the
    /// heading it was written under, so a result must land there rather than under whatever
    /// `vault.locale` says today.
    #[test]
    fn a_concept_whose_citations_moved_is_owed_a_synthesis() {
        let dir = TempDir::new().unwrap();
        write_concept_page(&dir, "oy365", &[], &synthesized_from(&[]));

        let pages = vec![
            build_page("wiki/concepts/oy365", "wiki/concepts/oy365.md", &[]),
            build_page(
                "daily/slack/2026-05-20",
                "daily/slack/2026-05-20.md",
                &["wiki/concepts/oy365"],
            ),
        ];

        let report = sync_concept_backlinks(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert_eq!(report.resynthesize.len(), 1, "{report:?}");
        let owed = &report.resynthesize[0];
        assert_eq!(owed.anchor, "## 핵심");
        assert_eq!(owed.citations, vec!["daily/slack/2026-05-20"]);

        // The page now records the input its synthesis is owed against, so the drain that
        // writes the section has something to stamp against.
        let content = std::fs::read_to_string(dir.path().join("wiki/concepts/oy365.md")).unwrap();
        let digest = citation_digest(&["daily/slack/2026-05-20".to_owned()]);
        assert!(
            content.contains(&format!("synthesis: \"{digest}\"")),
            "{content}"
        );
    }

    /// A synthesis already written from the set the page carries is owed nothing, so a
    /// re-run of the janitor never spends an LLM session on a concept nothing has changed
    /// about — the same self-perpetuating cache every other section has.
    #[test]
    fn a_synthesis_matching_its_evidence_is_owed_nothing() {
        let dir = TempDir::new().unwrap();
        write_concept_page(
            &dir,
            "oy365",
            &["- [daily/slack/2026-05-20](../../daily/slack/2026-05-20.md)"],
            &synthesized_from(&["daily/slack/2026-05-20"]),
        );

        let pages = vec![
            build_page("wiki/concepts/oy365", "wiki/concepts/oy365.md", &[]),
            build_page(
                "daily/slack/2026-05-20",
                "daily/slack/2026-05-20.md",
                &["wiki/concepts/oy365"],
            ),
        ];

        let report = sync_concept_backlinks(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert!(report.resynthesize.is_empty(), "{report:?}");
        assert_eq!(report.unchanged, 1);
    }

    /// A source page that is RETITLED changes how a citation reads and not what it is. The
    /// input is the citation set, so the sources body is rewritten with the new display text
    /// while the synthesis stays answered — a digest taken over the rendered list would
    /// re-enqueue every concept the retitled page cites.
    #[test]
    fn retitling_a_source_does_not_owe_a_new_synthesis() {
        let dir = TempDir::new().unwrap();
        write_concept_page(
            &dir,
            "oy365",
            &["- [Old Title](../../wiki/documents/spec.md)"],
            &synthesized_from(&["wiki/documents/spec"]),
        );

        let mut source = build_page(
            "wiki/documents/spec",
            "wiki/documents/spec.md",
            &["wiki/concepts/oy365"],
        );
        source.title = "New Title".into();
        let pages = vec![
            build_page("wiki/concepts/oy365", "wiki/concepts/oy365.md", &[]),
            source,
        ];

        let report = sync_concept_backlinks(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert!(
            report.resynthesize.is_empty(),
            "the evidence is the same page: {report:?}"
        );
        let content = std::fs::read_to_string(dir.path().join("wiki/concepts/oy365.md")).unwrap();
        assert!(
            content.contains("[New Title]"),
            "the citation names the page it points at as that page is titled today:\n{content}"
        );
    }

    /// The two questions are independent. A page whose sources list is already correct can
    /// still be owed a synthesis, and asking them together would leave it owed forever.
    #[test]
    fn a_page_in_sync_can_still_be_owed_a_synthesis() {
        let dir = TempDir::new().unwrap();
        write_concept_page(
            &dir,
            "oy365",
            &["- [daily/slack/2026-05-20](../../daily/slack/2026-05-20.md)"],
            &format!(
                "  synthesis: \"{}\"\n",
                citation_digest(&["daily/slack/2026-05-20".to_owned()])
            ),
        );

        let pages = vec![
            build_page("wiki/concepts/oy365", "wiki/concepts/oy365.md", &[]),
            build_page(
                "daily/slack/2026-05-20",
                "daily/slack/2026-05-20.md",
                &["wiki/concepts/oy365"],
            ),
        ];

        let report = sync_concept_backlinks(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert_eq!(report.unchanged, 1, "nothing to rewrite: {report:?}");
        assert_eq!(
            report.resynthesize.len(),
            1,
            "the input is recorded and unanswered: {report:?}"
        );
    }

    /// A page that already holds written prose and has recorded no input is ADOPTED: the
    /// prose becomes the answer for the evidence the page carries. Queueing it instead is an
    /// upgrade that replaces every authored synthesis in the vault in one unattended run.
    #[test]
    fn a_written_synthesis_with_no_recorded_input_is_adopted_not_replaced() {
        let dir = TempDir::new().unwrap();
        write_concept_page(&dir, "legacy", &[], "");

        let pages = vec![
            build_page("wiki/concepts/legacy", "wiki/concepts/legacy.md", &[]),
            build_page(
                "daily/slack/2026-05-20",
                "daily/slack/2026-05-20.md",
                &["wiki/concepts/legacy"],
            ),
        ];

        let report = sync_concept_backlinks(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert_eq!(
            report.adopted,
            vec![PathBuf::from("wiki/concepts/legacy.md")]
        );
        assert!(report.resynthesize.is_empty(), "{report:?}");

        // Both markers, so the page reads as answered rather than as work nothing did.
        let content = std::fs::read_to_string(dir.path().join("wiki/concepts/legacy.md")).unwrap();
        let digest = citation_digest(&["daily/slack/2026-05-20".to_owned()]);
        assert!(
            content.contains(&format!("synthesis: \"{digest}\"")),
            "{content}"
        );
        assert!(
            content.contains(&format!("synthesis_done: \"{digest}\"")),
            "{content}"
        );
    }

    /// Adoption is about PROSE, not about the marker being absent. A page whose synthesis is
    /// empty has nothing to adopt, so it is owed one — otherwise a concept created without a
    /// grounding sentence would be marked answered while its section stayed blank.
    #[test]
    fn an_empty_synthesis_with_no_recorded_input_is_owed_one() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("wiki/concepts/blank.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "---\nid: blank\nsource_count: 0\nllm_inputs:\n---\n\n# Blank\n\n\
             ## 핵심\n\n## 출처\n\n## 메타\n",
        )
        .unwrap();

        let pages = vec![
            build_page("wiki/concepts/blank", "wiki/concepts/blank.md", &[]),
            build_page(
                "daily/slack/2026-05-20",
                "daily/slack/2026-05-20.md",
                &["wiki/concepts/blank"],
            ),
        ];

        let report = sync_concept_backlinks(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert!(report.adopted.is_empty(), "{report:?}");
        assert_eq!(report.resynthesize.len(), 1, "{report:?}");
    }

    /// A concept page written before the input existed carries no `llm_inputs:` mapping. The
    /// sweep DERIVES that input, so it establishes the mapping rather than skipping every
    /// page in an existing vault.
    #[test]
    fn a_page_with_no_llm_inputs_mapping_gains_one() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("wiki/concepts/legacy.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "---\nid: legacy\nsource_count: 0\n---\n\n# Legacy\n\n\
             ## 핵심\n\nWritten before the input existed.\n\n## 출처\n\n\n## 메타\n",
        )
        .unwrap();

        let pages = vec![
            build_page("wiki/concepts/legacy", "wiki/concepts/legacy.md", &[]),
            build_page(
                "daily/slack/2026-05-20",
                "daily/slack/2026-05-20.md",
                &["wiki/concepts/legacy"],
            ),
        ];

        let report = sync_concept_backlinks(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert!(report.skipped.is_empty(), "{report:?}");
        assert_eq!(report.adopted.len(), 1, "{report:?}");

        let content = std::fs::read_to_string(&path).unwrap();
        let digest = citation_digest(&["daily/slack/2026-05-20".to_owned()]);
        assert!(
            content.contains(&format!("llm_inputs:\n  synthesis: \"{digest}\"")),
            "{content}"
        );
    }

    /// A concept NOTHING cites is owed nothing: there is no evidence for a synthesis to
    /// answer to, so a queued task would send a drain to read an empty set. It records no
    /// input either, which is what keeps `lore doctor` from reporting an unanswered one
    /// forever.
    #[test]
    fn an_uncited_concept_is_owed_no_synthesis() {
        let dir = TempDir::new().unwrap();
        write_concept(&dir, "orphan", &[]);

        let pages = vec![build_page(
            "wiki/concepts/orphan",
            "wiki/concepts/orphan.md",
            &[],
        )];

        let report = sync_concept_backlinks(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert!(report.resynthesize.is_empty(), "{report:?}");
        assert_eq!(report.unchanged, 1, "{report:?}");

        let content = std::fs::read_to_string(dir.path().join("wiki/concepts/orphan.md")).unwrap();
        assert!(!content.contains("synthesis:"), "{content}");
    }

    /// A concept whose last citation was deleted keeps the input it already answered. Its
    /// synthesis still states what the vault learned, and no rewrite could improve on it from
    /// a set that is now empty — so it is neither owed work nor left looking unanswered.
    #[test]
    fn losing_every_citation_leaves_the_answered_input_alone() {
        let dir = TempDir::new().unwrap();
        write_concept_page(
            &dir,
            "oy365",
            &["- [daily/slack/2026-05-20](../../daily/slack/2026-05-20.md)"],
            &synthesized_from(&["daily/slack/2026-05-20"]),
        );

        // The citing page is gone from the vault entirely.
        let pages = vec![build_page(
            "wiki/concepts/oy365",
            "wiki/concepts/oy365.md",
            &[],
        )];

        let report = sync_concept_backlinks(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert!(report.resynthesize.is_empty(), "{report:?}");
        assert_eq!(report.updated.len(), 1, "the sources list is now empty");

        let content = std::fs::read_to_string(dir.path().join("wiki/concepts/oy365.md")).unwrap();
        let digest = citation_digest(&["daily/slack/2026-05-20".to_owned()]);
        assert!(
            content.contains(&format!("synthesis_done: \"{digest}\"")),
            "the answered input survives losing its evidence:\n{content}"
        );
    }

    /// A page with no frontmatter block at all has nowhere to record any of this, and is not
    /// reported as owing a synthesis: the task would name a page that cannot receive the
    /// result, and every drain would fail on it forever.
    #[test]
    fn a_page_with_no_frontmatter_is_skipped_not_queued() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("wiki/concepts/bare.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "# Bare\n\n## 핵심\n\nNo frontmatter at all.\n\n## 출처\n\n\n## 메타\n",
        )
        .unwrap();

        let pages = vec![
            build_page("wiki/concepts/bare", "wiki/concepts/bare.md", &[]),
            build_page(
                "daily/slack/2026-05-20",
                "daily/slack/2026-05-20.md",
                &["wiki/concepts/bare"],
            ),
        ];

        let report = sync_concept_backlinks(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert_eq!(report.skipped, vec![PathBuf::from("wiki/concepts/bare.md")]);
        assert!(report.resynthesize.is_empty(), "{report:?}");
        assert!(report.updated.is_empty(), "{report:?}");
    }

    /// A concept page with no sources heading at all is skipped, not half-written.
    ///
    /// The count and the body are one fact. Writing `source_count: 1` onto a page where no
    /// citation appears — while the report says `+1 source(s)` — is exactly the disagreement
    /// between a derived count and its evidence that this sweep exists to remove.
    #[test]
    fn a_page_with_nowhere_to_record_its_sources_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wiki/concepts/headless.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "---\nid: headless\ntype: concept\ntitle: \"Headless\"\nsource_count: 0\n---\n\n\
             # Headless\n\n## Synthesis\n\nNo sources heading anywhere.\n",
        )
        .unwrap();
        let pages = vec![
            build_page("wiki/concepts/headless", "wiki/concepts/headless.md", &[]),
            build_page(
                "daily/slack/2026-05-20",
                "daily/slack/2026-05-20.md",
                &["wiki/concepts/headless"],
            ),
        ];

        let report = sync_concept_backlinks(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert_eq!(
            report.skipped,
            vec![PathBuf::from("wiki/concepts/headless.md")]
        );
        assert!(report.updated.is_empty(), "{report:?}");

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("source_count: 0"),
            "the count must not claim a citation the page cannot show:\n{after}"
        );
    }
}
