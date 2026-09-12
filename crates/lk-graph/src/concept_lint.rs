//! Structural lints for `{wiki}/concepts/*.md` pages.
//!
//! Concept pages are the one page kind an LLM creates directly: the queue-mode path
//! defers creation to `/lore-process`, which writes frontmatter and body itself rather
//! than through a template. So the defects this module looks for are the ones a
//! generated page can carry past every earlier gate — a `category` outside the
//! configured slate, a name that already belongs to another page, a contradiction a
//! human has yet to resolve.
//!
//! Every check here is DECIDABLE from the pages themselves: it reports a fact about
//! the vault, never a guess about whether two ideas match. That is why `graph lint`
//! exiting non-zero can stay meaningful — a check that fires on judgment calls trains
//! its reader to ignore it. All three lints are pure functions over the single
//! `scan_concept_pages` pass; they report, they never repair.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use lk_core::concept::identity_key;
use lk_core::config::ConceptCategory;
use lk_core::frontmatter;
use lk_core::markdown::FenceState;
use serde::Serialize;

use crate::GraphError;

/// The callout type that marks an unresolved contradiction on a concept page.
/// `/lore-wiki audit` writes `> [!conflict]` into the synthesis section when two
/// cited sources make conflicting claims; the marker lives in the LLM-owned body
/// (so ingest re-render preserves it via `preserved_synthesis`) and `graph lint`
/// surfaces it until a human resolves the contradiction and removes the callout.
const CONFLICT_CALLOUT: &str = "conflict";

/// One concept page whose `category` frontmatter does not appear in the
/// configured `concepts.categories[].id` set.
#[derive(Debug, Clone, Serialize)]
pub struct InvalidCategoryConcept {
    /// Vault-relative path of the offending concept page.
    pub path: PathBuf,
    /// The slug that identifies the concept — the file stem (the graph's canonical
    /// page identity), so the lint can locate the page even when frontmatter is broken.
    pub slug: String,
    /// The `category` value as found on disk — the thing that fails to match
    /// any configured id.
    pub category: String,
}

/// One concept page, read from disk once and shared across every concept lint.
/// The lints are pure functions over `&[ConceptPage]`, so `{wiki}/concepts/` is
/// walked a single time per `graph lint` rather than once per check.
#[derive(Debug, Clone)]
pub struct ConceptPage {
    /// Canonical identity = the file stem, matching how the link graph identifies
    /// every page (`scan::ScannedPage.id` is path-derived). Concepts are written as
    /// `{slug}.md`, so the stem IS the slug — independent of frontmatter, so it holds
    /// even when frontmatter is absent or malformed.
    pub slug: String,
    /// Vault-relative path, for lint output.
    pub path: PathBuf,
    /// `category` frontmatter value, if present.
    pub category: Option<String>,
    /// Every name this page answers to: its slug, its `title`, and each `aliases`
    /// entry. `aliases` is the registry the pipeline's dedup resolves incoming concept
    /// names against, so this is the page's full claim on the name space — what
    /// `find_duplicate_concepts` compares. A page with unreadable frontmatter still
    /// contributes its slug, which it holds by owning the file.
    pub names: Vec<String>,
    /// Page body (frontmatter stripped), for conflict-callout scanning.
    pub body: String,
}

/// Read `{wiki}/concepts/*.md` once into [`ConceptPage`]s, sorted by slug (so every
/// lint's output is deterministic without re-sorting). The single disk pass behind
/// the concept lints. A page with malformed frontmatter still yields a page — slug
/// from the file stem, no category, empty body — so slug-only checks still see it
/// while content checks naturally skip it. A missing concepts dir is not an error.
pub fn scan_concept_pages(
    vault_root: &Path,
    wiki_dir: &str,
) -> Result<Vec<ConceptPage>, GraphError> {
    let concepts_dir = vault_root
        .join(wiki_dir)
        .join(lk_core::vault_path::CONCEPTS_SUBDIR);
    let entries = match std::fs::read_dir(&concepts_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(GraphError::Io(format!(
                "read {}: {e}",
                concepts_dir.display()
            )));
        }
    };

    let mut pages = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| GraphError::Io(format!("walk concepts: {e}")))?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => return Err(GraphError::Io(format!("read {}: {e}", path.display()))),
        };
        let file_stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_owned();
        let rel_path = path.strip_prefix(vault_root).unwrap_or(&path).to_path_buf();
        // Slug is always the file stem (the graph's canonical page identity); only
        // category, names and body come from parsing, and a malformed page degrades to
        // none/slug-only/empty.
        let mut names = vec![file_stem.clone()];
        let (category, body) = match frontmatter::parse_page(&raw) {
            Ok(page) => {
                // A PRESENT field is reported as written, whatever its YAML type: a value that
                // can match no configured id is exactly what this check is for, and reading only
                // strings would make `category: 123` indistinguishable from no category.
                let category = page.frontmatter.get("category").map(|v| match v.as_str() {
                    Some(s) => s.to_owned(),
                    None => v.to_string(),
                });
                names.extend(
                    page.frontmatter
                        .get("title")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned),
                );
                names.extend(
                    page.frontmatter
                        .get("aliases")
                        .and_then(|v| v.as_array())
                        .into_iter()
                        .flatten()
                        .filter_map(|v| v.as_str())
                        .map(str::to_owned),
                );
                (category, page.body)
            }
            Err(_) => (None, String::new()),
        };
        pages.push(ConceptPage {
            slug: file_stem,
            path: rel_path,
            category,
            names,
            body,
        });
    }
    pages.sort_by(|a, b| a.slug.cmp(&b.slug));
    Ok(pages)
}

/// Concept pages whose `category` frontmatter is set to a value not in `configured`.
/// Pages without a `category` field are not flagged — leaving the field unset is the
/// documented way to mark a concept as uncategorised. When `configured` is empty, the
/// categorisation feature is off and nothing is flagged.
pub fn find_invalid_categories(
    pages: &[ConceptPage],
    configured: &[ConceptCategory],
) -> Vec<InvalidCategoryConcept> {
    if configured.is_empty() {
        return Vec::new();
    }
    let valid_ids: std::collections::HashSet<&str> =
        configured.iter().map(|c| c.id.as_str()).collect();

    pages
        .iter()
        .filter_map(|page| {
            // An EMPTY value is the uncategorised state, not an invalid category — the reading
            // the rest of the vault already takes: `templates/concept.md.jinja` renders the field
            // under `{% if category %}`, and `wiki concepts` filters it out of the registry.
            let category = page.category.as_deref().filter(|c| !c.is_empty())?;
            if valid_ids.contains(category) {
                return None;
            }
            Some(InvalidCategoryConcept {
                path: page.path.clone(),
                slug: page.slug.clone(),
                category: category.to_owned(),
            })
        })
        .collect()
}

/// Two concept pages that answer to the SAME name — one page's slug, `title` or alias
/// reduces to the same identity as one of the other's (`doc-hub` / `docs-hub`).
#[derive(Debug, Clone, Serialize)]
pub struct DuplicateConcept {
    /// The two concept slugs, ordered lexicographically for deterministic output.
    pub a: String,
    pub b: String,
    /// The colliding name as written on `a`, and as written on `b` — so the reason a
    /// pair is reported is visible without opening either page (an alias claiming
    /// another page's title reads very differently from two variant slugs).
    pub a_name: String,
    pub b_name: String,
}

/// Concept page pairs whose NAME SETS intersect: some name — a slug, a `title`, an alias —
/// belongs to both pages at once. That is a defect about the vault, not a guess about the
/// ideas: one name reaching two pages fragments its citations by spelling, and the
/// pipeline's alias index has to pick one of them. Read-only; `lore graph merge` is the
/// remedy a human triggers — or, when the pages are genuinely different things that happen
/// to share a name, renaming one of them is.
///
/// Deliberately EXACT: no score, no threshold, and no morphology. Two names collide only
/// when `lk_core::concept::identity_key` reduces them to one identity — no similarity guess is
/// ever made.
///
/// `C`, `C++` and `C#` are reported as one name on three pages, and that IS a fact about the
/// vault: all three slugify to `c`, so they claim one address, and whichever page owns it is
/// where every extraction naming any of them lands. Reporting the pair is right, and the remedy
/// is the one `/lore-wiki audit` prescribes for a genuine collision — disambiguate the name.
/// `identity_key` records why the addressing cannot separate them.
///
/// A scored variant (Sørensen-Dice over slug character bigrams) preceded this and was
/// measured on a 1,599-concept vault: 298 findings, of which one was a real duplicate. The
/// signal is morphology, so it fires on every shared namespace prefix (`amazon-sagemaker-ai`
/// ~ `amazon-sagemaker-hyperpod`), every shared head noun (`robot-foundation-model` ~
/// `tabular-foundation-model`) and on plain character coincidence (`agentops` ~ `gentoo`),
/// while missing acronym pairs entirely. No cutoff separates those from real duplicates,
/// because the difference is meaning, not spelling distance — and a permanently-red lint is
/// worse than a silent one. Two softer keys were measured on the same vault and dropped for
/// the same reason: an order-insensitive token multiset found nothing the exact key did not
/// (no two slugs are permutations of each other) while assuming word order carries no
/// meaning, and per-token plural stripping bought exactly one finding at the cost of
/// collapsing `http` onto `https`. Everything they reached for — plurals, acronyms,
/// shorthand — is a question about meaning, and belongs to `/lore-wiki audit` layer 5.
///
/// Being exact is also why no version-variant escape hatch is needed: `gpt-4`/`gpt-5` and
/// `gemini-3-1-flash-lite`/`gemini-3-5-flash-lite` simply claim different names.
pub fn find_duplicate_concepts(pages: &[ConceptPage]) -> Vec<DuplicateConcept> {
    // key → the pages claiming it, each with the first name on that page that produced it.
    // BTreeMaps throughout: iteration order is the key order and then page order, so the
    // output is deterministic without a final sort.
    let mut claims: BTreeMap<String, BTreeMap<usize, &str>> = BTreeMap::new();
    for (i, page) in pages.iter().enumerate() {
        for name in &page.names {
            if let Some(key) = identity_key(name) {
                claims.entry(key).or_default().entry(i).or_insert(name);
            }
        }
    }

    // Two pages can claim one another through several names; report the PAIR once.
    // `pages` is slug-sorted, so `i < j` already yields lexicographically-ordered `(a, b)`.
    let mut pairs: BTreeMap<(usize, usize), (&str, &str)> = BTreeMap::new();
    for holders in claims.values() {
        let claimants: Vec<(usize, &str)> = holders.iter().map(|(i, name)| (*i, *name)).collect();
        for x in 0..claimants.len() {
            for y in (x + 1)..claimants.len() {
                pairs
                    .entry((claimants[x].0, claimants[y].0))
                    .or_insert((claimants[x].1, claimants[y].1));
            }
        }
    }

    pairs
        .into_iter()
        .map(|((i, j), (a_name, b_name))| DuplicateConcept {
            a: pages[i].slug.clone(),
            b: pages[j].slug.clone(),
            a_name: a_name.to_owned(),
            b_name: b_name.to_owned(),
        })
        .collect()
}

/// A concept page carrying an unresolved `> [!conflict]` callout — a contradiction
/// `/lore-wiki audit` flagged between cited sources that no human has resolved yet.
#[derive(Debug, Clone, Serialize)]
pub struct UnresolvedConflict {
    /// Vault-relative path of the concept page.
    pub path: PathBuf,
    /// The concept slug (frontmatter `id`, falling back to the file stem).
    pub slug: String,
    /// The callout title (text after `[!conflict]`), empty when the marker has none.
    pub note: String,
}

/// One blockquote line, read rather than flattened: how deep its quote nests, and the text
/// inside it. `None` where the line is not a blockquote — which is also where a callout ENDS.
///
/// A real blockquote marker is required and may carry only CommonMark's 0–3 spaces of leading
/// indent: 4 or more is an indented code block, so a callout copied into one is content rather
/// than a live marker, and a bare `[!conflict]` text line is neither.
///
/// Both fields exist because flattening them lost a callout's structure and the reader needed
/// it back. `depth` separates the callout's own lines from a callout NESTED inside it, whose
/// body was being reported as the outer one's statement. `text` keeps its own indentation —
/// CommonMark consumes ONE space after each marker and the rest is content — because trimming
/// made `>     ``` ` read as a fence opener where it is an indented code line, which swallowed
/// every statement after it.
struct Quoted<'a> {
    depth: usize,
    text: &'a str,
}

fn quoted(line: &str) -> Option<Quoted<'_>> {
    let indent = line.len() - line.trim_start().len();
    if indent > 3 {
        return None;
    }
    let mut text = line.trim_start().strip_prefix('>')?;
    let mut depth = 1;
    loop {
        text = text.strip_prefix(' ').unwrap_or(text);
        match text.strip_prefix('>') {
            Some(rest) => {
                depth += 1;
                text = rest;
            }
            None => return Some(Quoted { depth, text }),
        }
    }
}

/// Whether a callout's continuation line STATES something, as opposed to opening another
/// callout inside it.
///
/// The empty-title fallback below reports the first line that does. Three things it never has
/// to judge, because the walk answers them from structure before asking: a fenced line (the
/// walk carries its own `FenceState`), a line the quote indents four spaces or more, and a
/// line belonging to a callout NESTED in this one (it answers only at the callout's own
/// depth). What is left is a question about text, and this is the whole of it.
///
/// FALSE POSITIVES that remain, each of which reads as prose and none of which can be told
/// from a statement without parsing the block: an HTML comment, a table row, a list marker
/// carrying its item. Each surfaces as itself, so the report shows a line the page carries
/// rather than a wrong claim about the concept.
///
/// FALSE NEGATIVE, in the other direction: a real statement that opens by quoting bracket-bang
/// notation (`[!important] 를 쓰라는 지침과 충돌한다`) reads as a nested callout and is passed
/// over, and the line after it answers instead — emptily, where it was the only one. Accepted
/// because the shapes are identical in text and the cost is asymmetric: a skipped statement
/// leaves a conflict reported by its page alone, while a marker reported as a statement makes
/// `lore graph lint` assert something about the concept that nobody wrote.
fn states_something(text: &str) -> bool {
    !text.is_empty() && !text.starts_with("[!")
}

/// If `line` is an Obsidian conflict callout (`> [!conflict] <title>`, allowing
/// nested blockquote markers and an optional `-`/`+` fold flag), return its title
/// (possibly empty). The callout type is matched case-insensitively against
/// `conflict` exactly — a callout of any other type is `None`, so an ordinary
/// `> [!note]` never trips the lint.
fn parse_conflict_callout(line: &str) -> Option<(usize, &str)> {
    let quoted = quoted(line)?;
    let inner = quoted.text.trim().strip_prefix("[!")?;
    let close = inner.find(']')?;
    if !inner[..close].trim().eq_ignore_ascii_case(CONFLICT_CALLOUT) {
        return None;
    }
    let after = inner[close + 1..].trim_start();
    Some((
        quoted.depth,
        after.strip_prefix(['-', '+']).unwrap_or(after).trim(),
    ))
}

/// Concept pages whose body carries an unresolved `> [!conflict]` callout. The scan
/// is fence-aware (a callout quoted inside a code block is content, not a live marker)
/// and reports each page once, keyed on the first marker's title. Read-only; the lint
/// reports, a human resolves the contradiction and deletes the callout to clear it.
///
/// Fence-aware at BOTH layers, because a fence can be written outside the quote or inside it
/// and only the second is how a callout gets quoted for illustration. `parse_fence` refuses a
/// line beginning with `>`, so the raw-line state cannot see a fence a blockquote carries —
/// which is how a `[!conflict]` inside `> ``` … > ``` ` was reported as a disagreement the
/// page does not record, against this very sentence's promise.
pub fn find_unresolved_conflicts(pages: &[ConceptPage]) -> Vec<UnresolvedConflict> {
    pages
        .iter()
        .filter_map(|page| {
            let lines: Vec<&str> = page.body.lines().collect();
            let mut fence = FenceState::new();
            let mut quoted_fence = FenceState::new();
            for (at, line) in lines.iter().enumerate() {
                if fence.apply(line) || !fence.is_closed() {
                    continue;
                }
                let inside_quote = quoted(line).is_some_and(|q| quoted_fence.apply(q.text));
                if inside_quote || !quoted_fence.is_closed() {
                    continue;
                }
                let Some((depth, title)) = parse_conflict_callout(line) else {
                    continue;
                };
                // Obsidian's title is optional and it is what this report SHOWS, so a
                // callout written without one stated its disagreement in the body and said
                // nothing here — an open conflict invisible in the one report that exists to
                // surface it. The callout's own continuation lines ARE that statement, so
                // the first of them that states something stands in. Nothing is inferred:
                // what is reported is a line the page carries, and a callout that says
                // nothing anywhere still reports nothing. The walk ends at the first line
                // that is not a blockquote, which is also where the callout ends, so it can
                // never reach into the block that follows.
                let note = match title.is_empty() {
                    false => title.to_owned(),
                    true => {
                        // Two things the walk must not mistake for the callout's own statement.
                        // A fenced block inside its body is data, so the walk runs its own
                        // fence state over the quoted text. And a callout NESTED inside this
                        // one has a body of its own, which was being reported as the outer
                        // callout's statement while the outer callout's real statement sat on
                        // a later line, never reached — so only lines at THIS callout's depth
                        // answer, and a deeper line is skipped without ending the walk.
                        let mut inner = FenceState::new();
                        lines[at + 1..]
                            .iter()
                            .map_while(|line| quoted(line))
                            .filter(|q| !inner.apply(q.text) && inner.is_closed())
                            // A line the quote indents four spaces or more is an indented code
                            // block, the other way markdown writes data — so its content is no
                            // more the callout's statement than a fenced line's is. This is why
                            // `Quoted::text` keeps its indentation: trimmed, `>     ``` ` read
                            // as a fence opener, and the statement after it was never reached.
                            .filter(|q| q.text.len() - q.text.trim_start().len() < 4)
                            .filter(|q| q.depth == depth)
                            .map(|q| q.text.trim())
                            .find(|text| states_something(text))
                            .unwrap_or_default()
                            .to_owned()
                    }
                };
                return Some(UnresolvedConflict {
                    path: page.path.clone(),
                    slug: page.slug.clone(),
                    note,
                });
            }
            None
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_concept(root: &Path, slug: &str, frontmatter_body: &str) {
        let dir = root.join("wiki").join(lk_core::vault_path::CONCEPTS_SUBDIR);
        std::fs::create_dir_all(&dir).unwrap();
        let content = format!("---\n{frontmatter_body}\n---\n\n# {slug}\n");
        std::fs::write(dir.join(format!("{slug}.md")), content).unwrap();
    }

    fn cats(ids: &[&str]) -> Vec<ConceptCategory> {
        ids.iter()
            .map(|id| ConceptCategory {
                id: (*id).into(),
                label: (*id).into(),
            })
            .collect()
    }

    /// Read the concept pages the way the CLI does — once, before the lints run.
    fn scan(root: &Path) -> Vec<ConceptPage> {
        scan_concept_pages(root, "wiki").unwrap()
    }

    #[test]
    fn scan_extracts_slug_category_names_and_body() {
        let tmp = TempDir::new().unwrap();
        write_concept(
            tmp.path(),
            "rag",
            "id: rag\ncategory: ai-ml\ntitle: \"RAG\"\naliases: [\"RAG\", \"Retrieval-Augmented Generation\"]",
        );
        let pages = scan(tmp.path());
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].slug, "rag");
        assert_eq!(pages[0].category.as_deref(), Some("ai-ml"));
        assert_eq!(
            pages[0].names,
            vec!["rag", "RAG", "RAG", "Retrieval-Augmented Generation"],
            "the name set is slug + title + every alias, verbatim"
        );
        assert!(pages[0].body.contains("# rag"));
    }

    #[test]
    fn scan_is_resilient_to_malformed_frontmatter() {
        // An unclosed frontmatter block fails to parse; the page must still surface by
        // its file stem (so slug-only lints see it) with no category, slug-only names
        // and empty body.
        let tmp = TempDir::new().unwrap();
        let dir = tmp
            .path()
            .join("wiki")
            .join(lk_core::vault_path::CONCEPTS_SUBDIR);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("broken.md"),
            "---\nid: broken\nno closing delimiter\n",
        )
        .unwrap();
        let pages = scan(tmp.path());
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].slug, "broken");
        assert!(pages[0].category.is_none());
        assert_eq!(pages[0].names, vec!["broken"]);
        assert!(pages[0].body.is_empty());
    }

    #[test]
    fn empty_config_means_no_findings_regardless_of_pages() {
        let tmp = TempDir::new().unwrap();
        write_concept(tmp.path(), "x", "id: x\ncategory: anything");
        assert!(find_invalid_categories(&scan(tmp.path()), &[]).is_empty());
    }

    #[test]
    fn missing_concepts_dir_is_not_an_error() {
        let tmp = TempDir::new().unwrap();
        assert!(find_invalid_categories(&scan(tmp.path()), &cats(&["ai-ml"])).is_empty());
    }

    #[test]
    fn valid_category_is_silent() {
        let tmp = TempDir::new().unwrap();
        write_concept(tmp.path(), "x", "id: x\ncategory: ai-ml");
        assert!(find_invalid_categories(&scan(tmp.path()), &cats(&["ai-ml"])).is_empty());
    }

    #[test]
    fn unknown_category_is_flagged() {
        let tmp = TempDir::new().unwrap();
        write_concept(tmp.path(), "x", "id: x\ncategory: security");
        let result =
            find_invalid_categories(&scan(tmp.path()), &cats(&["ai-ml", "infrastructure"]));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].slug, "x");
        assert_eq!(result[0].category, "security");
    }

    #[test]
    fn missing_category_field_is_silent() {
        // Omitting the field is the documented way to mark a concept as uncategorised.
        let tmp = TempDir::new().unwrap();
        write_concept(tmp.path(), "x", "id: x");
        assert!(find_invalid_categories(&scan(tmp.path()), &cats(&["ai-ml"])).is_empty());
    }

    #[test]
    fn a_category_that_is_not_a_string_is_still_a_category() {
        // A value that can match no configured id is what this check is for, whatever its type.
        let tmp = TempDir::new().unwrap();
        write_concept(tmp.path(), "x", "id: x\ncategory: 123");
        let found = find_invalid_categories(&scan(tmp.path()), &cats(&["ai-ml"]));
        assert_eq!(found.len(), 1, "a non-string category is not uncategorised");
        assert_eq!(
            found[0].category, "123",
            "the finding shows what is on disk"
        );
    }

    #[test]
    fn an_empty_category_is_uncategorised_not_invalid() {
        // The reading the rest of the vault takes: `templates/concept.md.jinja` renders the field
        // under `{% if category %}`, and `wiki concepts` filters it out of the registry.
        let tmp = TempDir::new().unwrap();
        write_concept(tmp.path(), "x", "id: x\ncategory: \"\"");
        assert!(find_invalid_categories(&scan(tmp.path()), &cats(&["ai-ml"])).is_empty());
    }

    #[test]
    fn falls_back_to_filename_when_id_missing() {
        let tmp = TempDir::new().unwrap();
        write_concept(tmp.path(), "fallback-slug", "category: nope");
        let result = find_invalid_categories(&scan(tmp.path()), &cats(&["ai-ml"]));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].slug, "fallback-slug");
    }

    #[test]
    fn findings_are_sorted_by_slug_for_deterministic_output() {
        let tmp = TempDir::new().unwrap();
        write_concept(tmp.path(), "zeta", "id: zeta\ncategory: bogus");
        write_concept(tmp.path(), "alpha", "id: alpha\ncategory: bogus");
        write_concept(tmp.path(), "mu", "id: mu\ncategory: bogus");
        let result = find_invalid_categories(&scan(tmp.path()), &cats(&["ai-ml"]));
        let slugs: Vec<&str> = result.iter().map(|f| f.slug.as_str()).collect();
        assert_eq!(slugs, vec!["alpha", "mu", "zeta"]);
    }

    #[test]
    fn one_name_spelled_two_ways_is_flagged() {
        let tmp = TempDir::new().unwrap();
        write_concept(tmp.path(), "vector-db", "id: vector-db");
        write_concept(tmp.path(), "vectordb", "id: vectordb");
        write_concept(tmp.path(), "kubernetes", "id: kubernetes");
        let result = find_duplicate_concepts(&scan(tmp.path()));
        let pairs: Vec<(&str, &str)> = result.iter().map(|d| (&*d.a, &*d.b)).collect();
        assert_eq!(
            pairs,
            vec![("vector-db", "vectordb")],
            "only a break between numerals survives; nothing else may be flagged: {result:?}"
        );
    }

    #[test]
    fn a_break_beside_a_single_digit_is_still_one_name() {
        // The companion of `claude-3-5` ~ `claude-35` staying distinct: only a break
        // BETWEEN numerals is identity, so a one-sided one folds and the pair IS a finding.
        // The pipeline relies on the same rule to route `Claude 35` at one page.
        let tmp = TempDir::new().unwrap();
        write_concept(tmp.path(), "claude-35", "id: claude-35");
        write_concept(tmp.path(), "claude35", "id: claude35");
        let result = find_duplicate_concepts(&scan(tmp.path()));
        let pairs: Vec<(&str, &str)> = result.iter().map(|d| (&*d.a, &*d.b)).collect();
        assert_eq!(pairs, vec![("claude-35", "claude35")], "{result:?}");
    }

    #[test]
    fn an_alias_claiming_another_pages_name_is_flagged() {
        // The defect this lint exists for: a page registers an alias that is another
        // page's own name, so the pipeline's alias index routes that name at whichever
        // page it indexed first and the other's citations fragment away from it. The
        // finding names both sides of the claim so the reason needs no page opened.
        let tmp = TempDir::new().unwrap();
        write_concept(tmp.path(), "htmx", "id: htmx\ntitle: \"HTMX\"");
        write_concept(
            tmp.path(),
            "hypermedia-driven-frontend",
            "id: hypermedia-driven-frontend\naliases: [\"HTMX\"]",
        );
        let result = find_duplicate_concepts(&scan(tmp.path()));
        assert_eq!(result.len(), 1, "{result:?}");
        assert_eq!(result[0].a, "htmx");
        assert_eq!(result[0].b, "hypermedia-driven-frontend");
        // Each side reports the first of its own names that claimed the key — for `htmx`
        // that is its slug, for the other page the alias that reaches across.
        assert_eq!(result[0].a_name, "htmx");
        assert_eq!(result[0].b_name, "HTMX");
    }

    #[test]
    fn a_page_pair_is_reported_once_however_many_names_collide() {
        // Two pages can reach each other through several names each. The finding is about
        // the PAIR, so it is emitted once.
        let tmp = TempDir::new().unwrap();
        write_concept(
            tmp.path(),
            "vector-db",
            "id: vector-db\ntitle: \"Vector DB\"\naliases: [\"vectordb\"]",
        );
        write_concept(
            tmp.path(),
            "vectordb",
            "id: vectordb\ntitle: \"VectorDB\"\naliases: [\"vector db\"]",
        );
        let result = find_duplicate_concepts(&scan(tmp.path()));
        assert_eq!(result.len(), 1, "{result:?}");
    }

    #[test]
    fn a_pages_own_names_never_collide_with_itself() {
        // slug, title and alias are normally three spellings of one name; that is the
        // healthy state, not a finding.
        let tmp = TempDir::new().unwrap();
        write_concept(
            tmp.path(),
            "vector-db",
            "id: vector-db\ntitle: \"Vector DB\"\naliases: [\"Vector DB\", \"vectordb\"]",
        );
        assert!(find_duplicate_concepts(&scan(tmp.path())).is_empty());
    }

    #[test]
    fn distinct_names_are_not_flagged() {
        // Every class the softer rules false-fired on. The first four sank the similarity
        // scorer (measured on a real 1,599-concept vault); the last two are why neither an
        // order-insensitive multiset nor plural stripping survived review.
        let tmp = TempDir::new().unwrap();
        for slug in [
            // shared namespace prefix
            "amazon-sagemaker-ai",
            "amazon-sagemaker-hyperpod",
            // shared head noun
            "robot-foundation-model",
            "tabular-foundation-model",
            // character coincidence
            "agentops",
            "gentoo",
            // version families, medial and trailing
            "gpt-4",
            "gpt-4o",
            "gpt-5",
            "claude-3",
            "claude-3-5",
            "gemini-3-1-flash-lite",
            "gemini-3-5-flash-lite",
            // a qualifier that narrows the concept
            "s3",
            "s3-bucket",
            // short partial overlap
            "rag",
            "raga",
            // word order carries meaning
            "agent-harness",
            "harness-agent",
            // a trailing `s` that belongs to the word, not to a plural
            "http",
            "https",
            // a break between digits is the name: `3-5` is two numerals, `35` is one
            "claude-3-5",
            "claude-35",
            "web-2-0",
            "web20",
        ] {
            write_concept(tmp.path(), slug, &format!("id: {slug}"));
        }
        let result = find_duplicate_concepts(&scan(tmp.path()));
        assert!(result.is_empty(), "false positives: {result:?}");
    }

    #[test]
    fn only_what_carries_no_identity_is_folded() {
        // Names are compared through `slugify`, the same normalization that mints page ids,
        // so display styling never lets one name address two pages unnoticed…
        assert_eq!(
            identity_key("Chain of Thought"),
            identity_key("chain-of-thought")
        );
        assert_eq!(identity_key("A/I"), identity_key("a i"));
        assert_eq!(identity_key("ＲＡＧ"), identity_key("rag")); // NFKC full-width
        assert_eq!(identity_key("Vite+"), identity_key("vite"));
        assert_eq!(identity_key("vector-db"), identity_key("vectordb"));
        // …and nothing beyond that is folded: order, every character, and a break
        // between digits are all identity.
        assert_ne!(identity_key("agent harness"), identity_key("harness agent"));
        assert_ne!(identity_key("http"), identity_key("https"));
        assert_ne!(identity_key("doc-hub"), identity_key("docs-hub"));
        assert_ne!(identity_key("Claude 3.5"), identity_key("Claude 35"));
        assert_eq!(identity_key("!!!"), None);
    }

    /// The report is the only place an open disagreement is machine-visible, and Obsidian's
    /// callout title is optional — so a callout that stated its case in the body and left the
    /// title empty reported nothing at all. Two of the reference vault's 24 open conflicts
    /// were invisible that way.
    /// Three shapes where the reader had thrown away the structure it needed. A callout
    /// NESTED in the conflict had its own body reported as the outer callout's statement,
    /// while the outer callout's real statement sat on a later line and was never reached.
    /// An indented code line inside the body read as a fence opener once trimmed, which
    /// swallowed every statement after it. And a `[!conflict]` quoted inside a BLOCKQUOTED
    /// fence was reported as a disagreement the page does not record — `parse_fence` refuses
    /// a line beginning with `>`, so the raw-line fence state could not see that fence at all.
    #[test]
    fn a_callouts_statement_is_its_own_at_its_own_depth_and_outside_every_code_block() {
        let tmp = TempDir::new().unwrap();
        write_concept(
            tmp.path(),
            "nested",
            "id: nested\n---\n\n## 핵심\n\n> [!conflict]\n> > [!note] 참고\n> > 중첩 콜아웃의 본문이다.\n> 진짜 진술이다.\n\nbody",
        );
        write_concept(
            tmp.path(),
            "indented",
            "id: indented\n---\n\n## 핵심\n\n> [!conflict]\n>     ```\n> 진술이다.\n\nbody",
        );
        write_concept(
            tmp.path(),
            "quoted-fence",
            "id: quoted-fence\n---\n\n## 핵심\n\n> ```\n> [!conflict] 코드블록 안의 가짜 갈등\n> ```\n\nbody",
        );
        let by_slug: std::collections::BTreeMap<String, String> =
            find_unresolved_conflicts(&scan(tmp.path()))
                .into_iter()
                .map(|c| (c.slug, c.note))
                .collect();
        assert_eq!(
            by_slug.get("nested").map(String::as_str),
            Some("진짜 진술이다."),
            "a nested callout's body is its own; the outer callout's statement is on a later line"
        );
        assert_eq!(
            by_slug.get("indented").map(String::as_str),
            Some("진술이다."),
            "an indented code line is data, and trimming it made it read as a fence opener"
        );
        assert!(
            !by_slug.contains_key("quoted-fence"),
            "a callout quoted inside a blockquoted fence is content, which is what the scan's \
             own doc comment promises"
        );
    }

    #[test]
    fn a_callout_is_reported_by_its_statement_not_by_the_markup_around_it() {
        // The fallback reports a line the page carries, and a fence opener or a nested
        // callout's marker is a line the page carries — so without a statement test the
        // report showed "```" where the disagreement should be.
        let tmp = TempDir::new().unwrap();
        write_concept(
            tmp.path(),
            "fenced",
            "id: fenced\n---\n\n## 핵심\n\n> [!conflict]\n> ```\n> 284B\n> ```\n> 총 파라미터 수가 갈린다.\n\nbody",
        );
        write_concept(
            tmp.path(),
            "nested",
            "id: nested\n---\n\n## 핵심\n\n> [!conflict]\n> [!note] 참고\n> 두 출처가 엇갈린다.\n\nbody",
        );
        write_concept(
            tmp.path(),
            "markup-only",
            "id: markup-only\n---\n\n## 핵심\n\n> [!conflict]\n> ```\n\nbody",
        );
        let by_slug: std::collections::BTreeMap<String, String> =
            find_unresolved_conflicts(&scan(tmp.path()))
                .into_iter()
                .map(|c| (c.slug, c.note))
                .collect();
        assert_eq!(
            by_slug.get("fenced").map(String::as_str),
            Some("총 파라미터 수가 갈린다."),
            "the fenced block is data; the statement after it is what the callout says"
        );
        assert_eq!(
            by_slug.get("nested").map(String::as_str),
            Some("두 출처가 엇갈린다."),
            "a nested callout's own marker is not the disagreement"
        );
        assert_eq!(
            by_slug.get("markup-only").map(String::as_str),
            Some(""),
            "a callout whose body is an unclosed fence states nothing this can read"
        );
    }

    #[test]
    fn a_callout_without_a_title_is_reported_by_what_it_says() {
        let tmp = TempDir::new().unwrap();
        write_concept(
            tmp.path(),
            "spec",
            "id: spec\n---\n\n## 핵심\n\n> [!conflict]\n> 총 파라미터 수가 출처마다 다르다.\n> 753B 과 744B 로 갈린다.\n\nbody",
        );
        write_concept(
            tmp.path(),
            "silent",
            "id: silent\n---\n\n## 핵심\n\n> [!conflict]\n\nbody",
        );
        let by_slug: std::collections::BTreeMap<String, String> =
            find_unresolved_conflicts(&scan(tmp.path()))
                .into_iter()
                .map(|c| (c.slug, c.note))
                .collect();
        assert_eq!(
            by_slug.get("spec").map(String::as_str),
            Some("총 파라미터 수가 출처마다 다르다."),
            "the callout's first statement stands in for the missing title"
        );
        assert_eq!(
            by_slug.get("silent").map(String::as_str),
            Some(""),
            "a callout that says nothing anywhere still reports nothing"
        );
    }

    #[test]
    fn conflict_callout_is_flagged_with_its_title() {
        let tmp = TempDir::new().unwrap();
        write_concept(
            tmp.path(),
            "rag",
            "id: rag\n---\n\n## 핵심\n\n> [!conflict] sources disagree on chunk size\n\nbody",
        );
        let result = find_unresolved_conflicts(&scan(tmp.path()));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].slug, "rag");
        assert_eq!(result[0].note, "sources disagree on chunk size");
    }

    #[test]
    fn other_callout_types_and_fenced_callouts_are_not_flagged() {
        let tmp = TempDir::new().unwrap();
        // An ordinary callout must not fire.
        write_concept(tmp.path(), "a", "id: a\n---\n\n> [!note] just a note\n");
        // A conflict callout quoted inside a fenced block is documentation, not a marker.
        write_concept(
            tmp.path(),
            "b",
            "id: b\n---\n\n```\n> [!conflict] this is an example\n```\n",
        );
        let result = find_unresolved_conflicts(&scan(tmp.path()));
        assert!(result.is_empty(), "false positives: {result:?}");
    }

    #[test]
    fn conflict_callout_parser_handles_fold_flag_and_nested_quote() {
        assert_eq!(
            parse_conflict_callout("> [!conflict]- folded title"),
            Some((1, "folded title"))
        );
        // The depth travels with the title: it is what tells the callout's own continuation
        // lines from those of a callout nested inside it.
        assert_eq!(parse_conflict_callout("> > [!CONFLICT]"), Some((2, "")));
        assert_eq!(
            parse_conflict_callout("  > [!conflict] ok"),
            Some((1, "ok"))
        ); // 0-3 indent ok
        assert_eq!(parse_conflict_callout("> [!note] x"), None);
        assert_eq!(parse_conflict_callout("plain text"), None);
        // A blockquote marker is REQUIRED — a bare callout-shaped text line is not a marker.
        assert_eq!(parse_conflict_callout("[!conflict] no blockquote"), None);
        // 4+ spaces is an indented code block, not a blockquote.
        assert_eq!(parse_conflict_callout("    > [!conflict] indented"), None);
    }

    #[test]
    fn missing_concepts_dir_yields_no_duplicates() {
        let empty = TempDir::new().unwrap();
        assert!(find_duplicate_concepts(&scan(empty.path())).is_empty());
    }
}
