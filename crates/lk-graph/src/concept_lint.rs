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
                let category = page
                    .frontmatter
                    .get("category")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned);
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
            // An EMPTY value is the uncategorised state, not an invalid category. That is what
            // the rest of the vault already means by it: `templates/concept.md.jinja` renders
            // the field under `{% if category %}`, and `wiki concepts` filters the empty string
            // out of the registry. Flagging it here made this the one reader that disagreed,
            // and the finding it produced read `category=` with nothing after it.
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
/// when `lk_core::concept::identity_key` reduces them to one identity, and that fold covers
/// typography alone — case, punctuation, and every break except one between two numerals.
/// So a finding is never a similarity guess; it is a name the vault spells two ways.
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

/// If `line` is an Obsidian conflict callout (`> [!conflict] <title>`, allowing
/// nested blockquote markers and an optional `-`/`+` fold flag), return its title
/// (possibly empty). The callout type is matched case-insensitively against
/// `conflict` exactly — a callout of any other type is `None`, so an ordinary
/// `> [!note]` never trips the lint. A real blockquote marker (`>`) is required
/// and may carry only CommonMark's 0–3 spaces of leading indent (4+ is an indented
/// code block, so a callout copied into one is content, not a live marker) — this
/// keeps a bare `[!conflict]` text line or an indented example from false-firing.
fn parse_conflict_callout(line: &str) -> Option<&str> {
    let indent = line.len() - line.trim_start().len();
    if indent > 3 {
        return None;
    }
    let mut s = line.trim_start();
    // Require at least one blockquote marker, then peel any nested ones.
    s = s.strip_prefix('>')?.trim_start();
    while let Some(rest) = s.strip_prefix('>') {
        s = rest.trim_start();
    }
    let inner = s.strip_prefix("[!")?;
    let close = inner.find(']')?;
    if !inner[..close].trim().eq_ignore_ascii_case(CONFLICT_CALLOUT) {
        return None;
    }
    let after = inner[close + 1..].trim_start();
    Some(after.strip_prefix(['-', '+']).unwrap_or(after).trim())
}

/// Concept pages whose body carries an unresolved `> [!conflict]` callout. The scan
/// is fence-aware (a callout quoted inside a code block is content, not a live marker)
/// and reports each page once, keyed on the first marker's title. Read-only; the lint
/// reports, a human resolves the contradiction and deletes the callout to clear it.
pub fn find_unresolved_conflicts(pages: &[ConceptPage]) -> Vec<UnresolvedConflict> {
    pages
        .iter()
        .filter_map(|page| {
            let mut fence = FenceState::new();
            for line in page.body.lines() {
                if fence.apply(line) || !fence.is_closed() {
                    continue;
                }
                if let Some(title) = parse_conflict_callout(line) {
                    return Some(UnresolvedConflict {
                        path: page.path.clone(),
                        slug: page.slug.clone(),
                        note: title.to_owned(),
                    });
                }
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
    fn an_empty_category_is_uncategorised_not_invalid() {
        // The rest of the vault already means "uncategorised" by an empty value:
        // `templates/concept.md.jinja` renders the field under `{% if category %}`, and
        // `wiki concepts` filters the empty string out of the registry. This reader used to
        // disagree, reporting a violation whose message read `category=` with nothing after it.
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
            Some("folded title")
        );
        assert_eq!(parse_conflict_callout("> > [!CONFLICT]"), Some(""));
        assert_eq!(parse_conflict_callout("  > [!conflict] ok"), Some("ok")); // 0-3 indent ok
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
