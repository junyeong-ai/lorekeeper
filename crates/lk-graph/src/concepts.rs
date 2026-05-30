//! Structural lints for `{wiki}/concepts/*.md` pages.
//!
//! At ingest time the pipeline silently strips any `category` value the LLM invented
//! outside the configured slate (see `Pipeline::plan` → `filter_valid_concepts`). But
//! the queue-mode path defers concept-page creation to `/lore-process`, which writes
//! the page directly. If the skill emits a category not in `config.concepts.categories`
//! — a category id renamed in config, an LLM-side hallucination — the page lands with
//! a category the rest of the system doesn't recognize: the wiki index can't bucket it,
//! and downstream tooling that filters by category silently drops it.
//!
//! This module scans concept pages and surfaces those mismatches as a `graph lint`
//! finding. Pure read; the lint reports, it does not repair.

use std::path::{Path, PathBuf};

use lk_core::config::ConceptCategory;
use lk_core::frontmatter;
use lk_core::markdown::FenceState;
use serde::Serialize;
use strsim::sorensen_dice;

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
    /// Canonical identity = the file stem, matching how the wikilink graph identifies
    /// every page (`scan::ScannedPage.id` is path-derived). Concepts are written as
    /// `{slug}.md`, so the stem IS the slug — independent of frontmatter, so it holds
    /// even when frontmatter is absent or malformed.
    pub slug: String,
    /// Vault-relative path, for lint output.
    pub rel_path: PathBuf,
    /// `category` frontmatter value, if present.
    pub category: Option<String>,
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
    let concepts_dir = vault_root.join(wiki_dir).join("concepts");
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
        // category and body come from parsing, and a malformed page degrades to none/empty.
        let (category, body) = match frontmatter::parse_page(&raw) {
            Ok(page) => {
                let category = page
                    .frontmatter
                    .get("category")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned);
                (category, page.body)
            }
            Err(_) => (None, String::new()),
        };
        pages.push(ConceptPage {
            slug: file_stem,
            rel_path,
            category,
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
pub fn invalid_categories(
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
            let category = page.category.as_deref()?;
            if valid_ids.contains(category) {
                return None;
            }
            Some(InvalidCategoryConcept {
                path: page.rel_path.clone(),
                slug: page.slug.clone(),
                category: category.to_owned(),
            })
        })
        .collect()
}

/// A pair of concept pages whose slugs are near-identical — likely variant-spelling
/// duplicates the LLM dedup hint missed (`vector-db` vs `vector-database`).
#[derive(Debug, Clone, Serialize)]
pub struct NearDuplicateConcept {
    /// The two concept slugs, ordered lexicographically for deterministic output.
    pub a: String,
    pub b: String,
    /// Sørensen-Dice similarity of the two slugs, in `[threshold, 1.0)`.
    pub similarity: f64,
}

/// Concept slug pairs whose Sørensen-Dice similarity is at or above `threshold` (and
/// below 1.0 — exact duplicates can't co-exist as separate files). These are
/// candidate merges: a variant spelling that fragments the concept graph. Read-only;
/// the lint reports, a human decides. `threshold` outside `(0, 1]` yields nothing.
pub fn near_duplicate_concepts(pages: &[ConceptPage], threshold: f64) -> Vec<NearDuplicateConcept> {
    if !(0.0..=1.0).contains(&threshold) || threshold == 0.0 {
        return Vec::new();
    }

    // `pages` is slug-sorted, so the (a, b) pairs come out lexicographically ordered.
    let mut findings = Vec::new();
    for i in 0..pages.len() {
        for j in (i + 1)..pages.len() {
            // Version variants (`gpt-4`/`gpt-4o`, `claude-3`/`claude-3-5`) are
            // intentionally distinct concepts, not spelling duplicates — skip them.
            if is_version_variant(&pages[i].slug, &pages[j].slug) {
                continue;
            }
            // Score on separator-stripped slugs so the kebab `-` doesn't inflate
            // bigram overlap between otherwise-different slugs.
            let similarity = sorensen_dice(&deslug(&pages[i].slug), &deslug(&pages[j].slug));
            if similarity >= threshold {
                findings.push(NearDuplicateConcept {
                    a: pages[i].slug.clone(),
                    b: pages[j].slug.clone(),
                    similarity,
                });
            }
        }
    }
    findings
}

/// Slug with separators removed, for similarity scoring (`vector-db` → `vectordb`).
fn deslug(slug: &str) -> String {
    slug.chars().filter(|c| *c != '-').collect()
}

/// True when two slugs are the same model/version family that should stay split,
/// not a spelling duplicate. Two independent signatures qualify:
///
/// 1. **Prefix extension** — `long` extends a digit-ending `short` by a pure
///    version suffix: directly attached (`gpt-4` ⊂ `gpt-4o`) or a `-<digits>`
///    segment (`claude-3` ⊂ `claude-3-5`). A `-<word>` suffix names a DIFFERENT
///    concept and is NOT skipped (`s3` ⊄ `s3-bucket`, `gpt-4` ⊄ `gpt-4-api`).
/// 2. **Sibling version** — identical base, differing trailing version token
///    (`gpt-4`/`gpt-5`, `claude-3`/`claude-4`, `llama-2`/`llama-3`). These are
///    distinct releases, not variant spellings — but they are NOT prefixes of
///    each other, so signature (1) alone misses them and the near-duplicate
///    lint would false-fire on every adjacent model generation.
fn is_version_variant(a: &str, b: &str) -> bool {
    prefix_version_variant(a, b) || sibling_version_variant(a, b)
}

/// Signature (1): one slug is the other extended by a pure version suffix.
fn prefix_version_variant(a: &str, b: &str) -> bool {
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    if short == long || !long.starts_with(short) {
        return false;
    }
    if !short
        .chars()
        .next_back()
        .is_some_and(|c| c.is_ascii_digit())
    {
        return false;
    }
    // The shared prefix must name a model family, not a bare short id — `s3`⊂`s30`
    // is two distinct identifiers, not a version extension (same gate as `version_split`).
    if short.chars().filter(|c| c.is_ascii_alphabetic()).count() < MIN_VERSION_BASE_ALPHA {
        return false;
    }
    let suffix = &long[short.len()..];
    match suffix.strip_prefix('-') {
        // `-<segment>`: a version variant only when the segment is all digits.
        Some(rest) => !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()),
        // Directly-attached suffix (no separator): a model-letter variant like `4o`.
        None => true,
    }
}

/// Signature (2): both slugs end in a version token and share an identical base.
fn sibling_version_variant(a: &str, b: &str) -> bool {
    match (version_split(a), version_split(b)) {
        (Some((base_a, ver_a)), Some((base_b, ver_b))) => base_a == base_b && ver_a != ver_b,
        _ => false,
    }
}

/// Minimum alphabetic characters a base must carry to count as a model-family name.
/// A real version family (`gpt`-4, `claude`-3, `llama`-2) names itself with a word;
/// a bare letter+digit slug (`s3`/`s4`, `h2`/`h3`, `q1`/`q2`) is two distinct short
/// identifiers, not a versioned family. Requiring ≥2 alphabetic chars in the base
/// keeps genuine families split while letting unrelated short slugs fall through to
/// the normal near-duplicate scorer (so a real spelling-variant pair still surfaces).
const MIN_VERSION_BASE_ALPHA: usize = 2;

/// Split a trailing version token off a slug, returning `(base, version)` when the
/// slug ends in a token of the form `<digits>[<letter>]` (optionally preceded by a
/// single `-`) over a base that names a model family (≥2 alphabetic chars).
/// `gpt-4`→`("gpt","4")`, `gpt-4o`→`("gpt","4o")`, `claude-3-5`→`("claude-3","5")`.
/// Returns `None` when there is no trailing digit run (`vector-db`, `s3-bucket`) or
/// the base is too short to be a family name (`s3`, `h2`) — so neither non-versioned
/// slugs nor bare letter+digit identifiers are ever treated as version siblings.
fn version_split(slug: &str) -> Option<(&str, &str)> {
    let bytes = slug.as_bytes();
    let len = bytes.len();
    let mut i = len;
    // An optional single trailing lowercase letter (the model letter in `4o`).
    if i > 0 && bytes[i - 1].is_ascii_lowercase() {
        i -= 1;
    }
    let digits_end = i;
    while i > 0 && bytes[i - 1].is_ascii_digit() {
        i -= 1;
    }
    if i == digits_end {
        return None; // no digit run → not a version token
    }
    let version = &slug[i..len];
    // Consume one separating `-` so the base excludes it.
    let base_end = if i > 0 && bytes[i - 1] == b'-' {
        i - 1
    } else {
        i
    };
    let base = &slug[..base_end];
    // A version family is named by a word, not a single letter — `s3`/`s4` and
    // `h2`/`h3` are distinct short ids, not a versioned family. Gate on alphabetic
    // content so they fall through to the normal duplicate scorer.
    if base.chars().filter(|c| c.is_ascii_alphabetic()).count() < MIN_VERSION_BASE_ALPHA {
        return None;
    }
    Some((base, version))
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
pub fn unresolved_conflicts(pages: &[ConceptPage]) -> Vec<UnresolvedConflict> {
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
                        path: page.rel_path.clone(),
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
        let dir = root.join("wiki").join("concepts");
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
    fn scan_extracts_slug_category_and_body() {
        let tmp = TempDir::new().unwrap();
        write_concept(tmp.path(), "rag", "id: rag\ncategory: ai-ml");
        let pages = scan(tmp.path());
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].slug, "rag");
        assert_eq!(pages[0].category.as_deref(), Some("ai-ml"));
        assert!(pages[0].body.contains("# rag"));
    }

    #[test]
    fn scan_is_resilient_to_malformed_frontmatter() {
        // An unclosed frontmatter block fails to parse; the page must still surface by
        // its file stem (so slug-only lints see it) with no category and empty body.
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("wiki").join("concepts");
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
        assert!(pages[0].body.is_empty());
    }

    #[test]
    fn empty_config_means_no_findings_regardless_of_pages() {
        let tmp = TempDir::new().unwrap();
        write_concept(tmp.path(), "x", "id: x\ncategory: anything");
        assert!(invalid_categories(&scan(tmp.path()), &[]).is_empty());
    }

    #[test]
    fn missing_concepts_dir_is_not_an_error() {
        let tmp = TempDir::new().unwrap();
        assert!(invalid_categories(&scan(tmp.path()), &cats(&["ai-ml"])).is_empty());
    }

    #[test]
    fn valid_category_is_silent() {
        let tmp = TempDir::new().unwrap();
        write_concept(tmp.path(), "x", "id: x\ncategory: ai-ml");
        assert!(invalid_categories(&scan(tmp.path()), &cats(&["ai-ml"])).is_empty());
    }

    #[test]
    fn unknown_category_is_flagged() {
        let tmp = TempDir::new().unwrap();
        write_concept(tmp.path(), "x", "id: x\ncategory: security");
        let result = invalid_categories(&scan(tmp.path()), &cats(&["ai-ml", "infrastructure"]));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].slug, "x");
        assert_eq!(result[0].category, "security");
    }

    #[test]
    fn missing_category_field_is_silent() {
        // Omitting the field is the documented way to mark a concept as uncategorised.
        let tmp = TempDir::new().unwrap();
        write_concept(tmp.path(), "x", "id: x");
        assert!(invalid_categories(&scan(tmp.path()), &cats(&["ai-ml"])).is_empty());
    }

    #[test]
    fn falls_back_to_filename_when_id_missing() {
        let tmp = TempDir::new().unwrap();
        write_concept(tmp.path(), "fallback-slug", "category: nope");
        let result = invalid_categories(&scan(tmp.path()), &cats(&["ai-ml"]));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].slug, "fallback-slug");
    }

    #[test]
    fn findings_are_sorted_by_slug_for_deterministic_output() {
        let tmp = TempDir::new().unwrap();
        write_concept(tmp.path(), "zeta", "id: zeta\ncategory: bogus");
        write_concept(tmp.path(), "alpha", "id: alpha\ncategory: bogus");
        write_concept(tmp.path(), "mu", "id: mu\ncategory: bogus");
        let result = invalid_categories(&scan(tmp.path()), &cats(&["ai-ml"]));
        let slugs: Vec<&str> = result.iter().map(|f| f.slug.as_str()).collect();
        assert_eq!(slugs, vec!["alpha", "mu", "zeta"]);
    }

    #[test]
    fn near_duplicate_slugs_are_flagged_distinct_ones_are_not() {
        let tmp = TempDir::new().unwrap();
        write_concept(tmp.path(), "vector-database", "id: vector-database");
        write_concept(tmp.path(), "vector-db", "id: vector-db");
        write_concept(tmp.path(), "kubernetes", "id: kubernetes");
        let result = near_duplicate_concepts(&scan(tmp.path()), 0.6);
        assert!(
            result
                .iter()
                .any(|d| d.a == "vector-database" && d.b == "vector-db"),
            "variant spellings must be flagged: {result:?}"
        );
        assert!(
            !result
                .iter()
                .any(|d| d.a == "kubernetes" || d.b == "kubernetes"),
            "an unrelated slug must not be flagged"
        );
    }

    #[test]
    fn version_variants_are_not_flagged_as_duplicates() {
        let tmp = TempDir::new().unwrap();
        write_concept(tmp.path(), "gpt-4", "id: gpt-4");
        write_concept(tmp.path(), "gpt-4o", "id: gpt-4o");
        write_concept(tmp.path(), "claude-3", "id: claude-3");
        write_concept(tmp.path(), "claude-3-5", "id: claude-3-5");
        let result = near_duplicate_concepts(&scan(tmp.path()), 0.6);
        assert!(
            result.is_empty(),
            "model version variants are distinct concepts, not duplicates: {result:?}"
        );
    }

    #[test]
    fn sibling_model_generations_are_not_flagged() {
        // Adjacent model generations share a base and differ only in the version
        // token; they are NOT prefixes of each other, so the prefix rule misses them.
        // The near-duplicate lint must not flag every new model release.
        let tmp = TempDir::new().unwrap();
        write_concept(tmp.path(), "gpt-4", "id: gpt-4");
        write_concept(tmp.path(), "gpt-5", "id: gpt-5");
        write_concept(tmp.path(), "claude-3", "id: claude-3");
        write_concept(tmp.path(), "claude-4", "id: claude-4");
        write_concept(tmp.path(), "llama-2", "id: llama-2");
        write_concept(tmp.path(), "llama-3", "id: llama-3");
        let result = near_duplicate_concepts(&scan(tmp.path()), 0.6);
        assert!(
            result.is_empty(),
            "sibling model generations must not be flagged as duplicates: {result:?}"
        );
    }

    #[test]
    fn sibling_version_detection_is_precise() {
        // Same base, differing version token → sibling (distinct).
        assert!(is_version_variant("gpt-4", "gpt-5"));
        assert!(is_version_variant("claude-3", "claude-4"));
        assert!(is_version_variant("llama-2", "llama-3"));
        assert!(is_version_variant("gpt-4", "gpt-4o")); // prefix rule still holds
        // Different base, or no version token → NOT a version variant: a genuine
        // variant-spelling pair must still surface for review.
        assert!(!is_version_variant("vector-db", "vector-database"));
        assert!(!is_version_variant("gpt-4", "bert-4")); // different base
        assert!(!is_version_variant("s3", "s3-bucket")); // word suffix, not a version
    }

    #[test]
    fn bare_letter_digit_slugs_are_not_version_families() {
        // A single-letter base + digit is two distinct short identifiers, NOT a
        // version family — they must still be eligible for near-duplicate review
        // (the version-variant skip must not swallow them via either signature).
        assert!(!is_version_variant("s3", "s4")); // AWS S3 vs an unrelated `s4`
        assert!(!is_version_variant("h2", "h3")); // HTTP/2 vs HTTP/3 — distinct, but not "merge candidates" swallowed silently
        assert!(!is_version_variant("q1", "q2"));
        assert!(!is_version_variant("s3", "s30")); // prefix-extension of a bare id, not a family
        assert!(!is_version_variant("v2", "v3")); // single-letter `v` base: not a family
        // The genuine families (≥2 alphabetic base chars) are still recognised.
        assert!(is_version_variant("gpt-4", "gpt-5"));
    }

    #[test]
    fn word_suffix_after_digit_is_not_treated_as_version_variant() {
        // `s3` ⊂ `s3-bucket` and `gpt-4` ⊂ `gpt-4-api` share a digit-ending prefix but
        // the `-<word>` suffix names a DIFFERENT concept — the version-variant skip must
        // NOT suppress them (they're simply distinct, scored normally).
        assert!(!is_version_variant("s3", "s3-bucket"));
        assert!(!is_version_variant("gpt-4", "gpt-4-api"));
        // Genuine version suffixes are still recognised.
        assert!(is_version_variant("gpt-4", "gpt-4o"));
        assert!(is_version_variant("claude-3", "claude-3-5"));
    }

    #[test]
    fn conflict_callout_is_flagged_with_its_title() {
        let tmp = TempDir::new().unwrap();
        write_concept(
            tmp.path(),
            "rag",
            "id: rag\n---\n\n## 핵심\n\n> [!conflict] sources disagree on chunk size\n\nbody",
        );
        let result = unresolved_conflicts(&scan(tmp.path()));
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
        let result = unresolved_conflicts(&scan(tmp.path()));
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
    fn near_duplicate_threshold_zero_or_missing_dir_is_empty() {
        let tmp = TempDir::new().unwrap();
        write_concept(tmp.path(), "vector-database", "id: vector-database");
        write_concept(tmp.path(), "vector-db", "id: vector-db");
        assert!(near_duplicate_concepts(&scan(tmp.path()), 0.0).is_empty());
        let empty = TempDir::new().unwrap();
        assert!(near_duplicate_concepts(&scan(empty.path()), 0.85).is_empty());
    }
}
