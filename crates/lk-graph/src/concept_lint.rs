//! Structural lints for concept pages.
//!
//! One scan supplies declared names, categories and explicit conflict callouts.
//! These checks report recorded structure, not semantic equivalence or factual truth.

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
    /// Collection-relative address without `.md`, including nested directories.
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
    /// Collection-relative address without `.md`, independent of declared names.
    pub slug: String,
    /// Vault-relative path, for lint output.
    pub path: PathBuf,
    /// `category` frontmatter value, if present, retaining its declared type.
    pub category: Option<serde_json::Value>,
    /// Declared title and aliases, with the file stem as the title fallback.
    /// An address does not itself claim a name. Malformed frontmatter declares none.
    pub names: Vec<String>,
    /// Page body (frontmatter stripped), for conflict-callout scanning.
    pub body: String,
}

/// Read Markdown owners beneath `{wiki}/concepts/` into [`ConceptPage`]s, sorted by slug (so every
/// lint's output is deterministic without re-sorting). The single disk pass behind
/// the concept lints uses the same validated inventory as name routing. A page with
/// malformed frontmatter retains its address, with no category, names or body.
/// A missing concepts dir is not an error.
pub fn scan_concept_pages(
    vault_root: &Path,
    wiki_dir: &str,
) -> Result<Vec<ConceptPage>, GraphError> {
    let concepts_dir = Path::new(wiki_dir).join(lk_core::vault_path::CONCEPTS_SUBDIR);
    let inventory = lk_vault::load_markdown_inventory(vault_root, &concepts_dir)
        .map_err(|error| GraphError::Io(format!("walk concepts: {error}")))?;

    let mut pages = Vec::new();
    for rel_path in inventory.owners {
        let path = lk_vault::page_path(vault_root, &rel_path)
            .map_err(|error| GraphError::Io(format!("read concept: {error}")))?;
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => return Err(GraphError::Io(format!("read {}: {e}", path.display()))),
        };
        let file_stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_owned();
        let slug = rel_path
            .strip_prefix(&concepts_dir)
            .map_err(|error| GraphError::Io(error.to_string()))?
            .with_extension("")
            .to_str()
            .ok_or_else(|| GraphError::Io("concept address is not UTF-8".into()))?
            .replace(std::path::MAIN_SEPARATOR, "/");
        let mut names = Vec::new();
        let (category, body) = match frontmatter::parse_page(&raw) {
            Ok(page) => {
                if !page.frontmatter.is_concept_or_legacy() {
                    continue;
                }
                let category = page.frontmatter.get("category").cloned();
                names.extend(
                    page.frontmatter
                        .get("title")
                        .and_then(|v| v.as_str())
                        .or(Some(file_stem.as_str()))
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
            slug,
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
/// Missing, null or empty categories are uncategorised. When `configured` is empty, the
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
            let value = page.category.as_ref()?;
            let category = match value {
                serde_json::Value::Null => return None,
                serde_json::Value::String(category)
                    if category.is_empty() || valid_ids.contains(category.as_str()) =>
                {
                    return None;
                }
                serde_json::Value::String(category) => category.clone(),
                _ => value.to_string(),
            };
            Some(InvalidCategoryConcept {
                path: page.path.clone(),
                slug: page.slug.clone(),
                category,
            })
        })
        .collect()
}

/// Two concept pages that declare names with the same identity.
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

/// Concept page pairs whose NAME SETS intersect: a title or alias (with a file-stem title fallback)
/// belongs to both pages at once. That is a defect about the vault, not a guess about the
/// ideas: one name reaching two pages fragments its citations by spelling, and the
/// pipeline's name resolver reports ambiguity. Read-only; `lore graph merge` is the
/// remedy a human triggers — or, when the pages are genuinely different things that happen
/// to share a name, renaming one of them is.
///
/// Report shared declared names, without inferring equivalence from file addresses
/// or spelling similarity. A shared name requires review, not automatic merging.
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
    /// Collection-relative address without `.md`, including nested directories.
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
/// body was being reported as the outer one's statement. `text` keeps its own indentation,
/// because trimming made `>     ``` ` read as a fence opener where it is an indented code
/// line, which swallowed every statement after it.
struct Quoted<'a> {
    depth: usize,
    text: &'a str,
}

/// How far a line is indented, in COLUMNS, with the text that follows.
///
/// Counted in columns rather than characters because a tab advances to the next four-column
/// stop, which is the whole of CommonMark's 0–3 rule: `\t> x` is an indented code block where
/// `   > x` is a blockquote, and both are one or three CHARACTERS. Counting characters made
/// `>\t> x` read as depth 1 with the second marker left in the text — the same defect as
/// `>  > x`, in the spelling an editor produces rather than a person.
fn indent_of(text: &str) -> (usize, &str) {
    let mut columns = 0;
    let rest = text.trim_start_matches(|c| match c {
        ' ' => {
            columns += 1;
            true
        }
        '\t' => {
            columns = columns / 4 * 4 + 4;
            true
        }
        _ => false,
    });
    (columns, rest)
}

fn quoted(line: &str) -> Option<Quoted<'_>> {
    let (indent, start) = indent_of(line);
    if indent > 3 {
        return None;
    }
    let mut rest = start.strip_prefix('>')?;
    let mut depth = 1;
    loop {
        // The columns here belong to two things: ONE is the marker's own optional space, and
        // what remains is a NESTED marker's indent, which CommonMark allows up to three of.
        // Charging all of them to the nested marker is off by one column, so `>    > x` —
        // four spaces, three of them the nested marker's — was declined as nesting and its
        // marker line fell through as the outer callout's prose. Where no marker follows,
        // that one space is still the marker's and the rest is the content's indentation,
        // which is why it is not trimmed away.
        let (columns, probe) = indent_of(rest);
        match probe.strip_prefix('>') {
            Some(next) if columns.saturating_sub(1) <= 3 => {
                depth += 1;
                rest = next;
            }
            _ => {
                return Some(Quoted {
                    depth,
                    text: rest.strip_prefix(' ').unwrap_or(rest),
                });
            }
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
    let (depth, kind, title) = parse_callout(line)?;
    kind.eq_ignore_ascii_case(CONFLICT_CALLOUT)
        .then_some((depth, title))
}

/// Shared explicit callout syntax; ordinary blockquotes carry no annotation marker.
pub(crate) fn parse_callout(line: &str) -> Option<(usize, &str, &str)> {
    let quoted = quoted(line)?;
    let inner = quoted.text.trim().strip_prefix("[!")?;
    let close = inner.find(']')?;
    let kind = inner[..close].trim();
    if kind.is_empty() {
        return None;
    }
    let after = inner[close + 1..].trim_start();
    Some((
        quoted.depth,
        kind,
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
///
/// The two layers END differently, and conflating that suppressed real conflicts. A fenced
/// block closes with its CONTAINER, so one opened inside a blockquote ends where the
/// blockquote does whether or not a closing delimiter was written, and the next `> …` block
/// is a new quote holding no fence. A top-level fence has no container but the document, so
/// it does run to the end. Carried across the boundary, one sloppily quoted snippet — or a
/// decorative `> ~~~~~ … ~~~~~` line, which parses as an opener because a tilde fence accepts
/// any info string — hid every `[!conflict]` later on the page, silently and with no reading
/// in which the page had stopped asserting one.
pub fn find_unresolved_conflicts(pages: &[ConceptPage]) -> Vec<UnresolvedConflict> {
    pages
        .iter()
        .filter_map(|page| {
            let lines: Vec<&str> = lk_core::markdown::lines_with_endings(&page.body)
                .map(|line| line.trim_end_matches(['\r', '\n']))
                .collect();
            let mut fence = FenceState::new();
            let mut quoted_fence = FenceState::new();
            for (at, line) in lines.iter().enumerate() {
                if fence.apply(line) || !fence.is_closed() {
                    continue;
                }
                let Some(in_quote) = quoted(line) else {
                    // The blockquote container ended, and a fence it held ended with it.
                    quoted_fence = FenceState::new();
                    continue;
                };
                if quoted_fence.apply(in_quote.text) || !quoted_fence.is_closed() {
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
                // that is not a blockquote, and equally at the first that leaves the quote
                // this callout sits in, so it can never reach into the block that follows.
                let note = match title.is_empty() {
                    false => title.to_owned(),
                    true => {
                        // The walk answers from the callout's OWN lines, which is three
                        // conditions rather than one. It ends where the callout's container
                        // ends — a shallower line closes it, and the depth-2 lines after a
                        // depth-1 line are a different nested quote. A DEEPER line belongs to
                        // a callout nested inside this one, whose body was being reported as
                        // this callout's statement while this one's real statement sat on a
                        // later line, never reached — so it is skipped without ending the
                        // walk, and it is never fed to the fence state either, because a
                        // fence it opens closes with the nested quote and not with this one.
                        let mut inner = FenceState::new();
                        lines[at + 1..]
                            .iter()
                            .map_while(|line| quoted(line))
                            .take_while(|q| q.depth >= depth)
                            .filter(|q| q.depth == depth)
                            .filter(|q| !inner.apply(q.text) && inner.is_closed())
                            // A line the quote indents four spaces or more is an indented code
                            // block, the other way markdown writes data — so its content is no
                            // more the callout's statement than a fenced line's is. This is why
                            // `Quoted::text` keeps its indentation: trimmed, `>     ``` ` read
                            // as a fence opener, and the statement after it was never reached.
                            .filter(|q| indent_of(q.text).0 < 4)
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
        let path = dir.join(format!("{slug}.md"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let content = format!("---\n{frontmatter_body}\n---\n\n# {slug}\n");
        std::fs::write(path, content).unwrap();
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
        assert_eq!(pages[0].category, Some(serde_json::json!("ai-ml")));
        assert_eq!(
            pages[0].names,
            vec!["RAG", "RAG", "Retrieval-Augmented Generation"],
            "the name set is title + every alias, verbatim"
        );
        assert!(pages[0].body.contains("# rag"));
    }

    #[test]
    fn scan_is_resilient_to_malformed_frontmatter() {
        // An unclosed frontmatter block fails to parse; the page must still surface by
        // its address with no category, declared names or body.
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
        assert_eq!(pages[0].names, Vec::<String>::new());
        assert!(pages[0].body.is_empty());
    }

    #[test]
    fn nested_concepts_participate_in_each_lint_with_complete_addresses() {
        let tmp = TempDir::new().unwrap();
        write_concept(
            tmp.path(),
            "first/entry",
            "title: First\ncategory: unknown\naliases: [Shared]",
        );
        write_concept(
            tmp.path(),
            "second/entry",
            "title: Second\ncategory: ai-ml\naliases: [Shared]",
        );
        let first = PathBuf::from("wiki/concepts/first/entry.md");
        let mut content = std::fs::read_to_string(tmp.path().join(&first)).unwrap();
        content.push_str("\n> [!conflict] Sources disagree\n");
        std::fs::write(tmp.path().join(&first), content).unwrap();

        let pages = scan(tmp.path());
        assert_eq!(pages.len(), 2);
        let categories = find_invalid_categories(&pages, &cats(&["ai-ml"]));
        assert_eq!(categories.len(), 1);
        assert_eq!(categories[0].slug, "first/entry");
        assert_eq!(categories[0].path, first);
        let duplicates = find_duplicate_concepts(&pages);
        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].a, "first/entry");
        assert_eq!(duplicates[0].b, "second/entry");
        assert_eq!(duplicates[0].a_name, "Shared");
        assert_eq!(duplicates[0].b_name, "Shared");
        let conflicts = find_unresolved_conflicts(&pages);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].slug, "first/entry");
        assert_eq!(conflicts[0].path, first);
        assert_eq!(conflicts[0].note, "Sources disagree");
    }

    #[test]
    fn nested_addresses_are_not_names_and_missing_titles_use_the_basename() {
        let tmp = TempDir::new().unwrap();
        write_concept(tmp.path(), "first/entry", "title: First");
        write_concept(tmp.path(), "second/entry", "title: Second");
        write_concept(tmp.path(), "address-name", "title: first/entry");
        write_concept(tmp.path(), "nested/fallback", "category: ai-ml");
        let pages = scan(tmp.path());
        assert_eq!(pages.len(), 4);
        assert!(find_duplicate_concepts(&pages).is_empty());
        let fallback = pages
            .iter()
            .find(|page| page.slug == "nested/fallback")
            .unwrap();
        assert_eq!(fallback.names, vec!["fallback"]);

        write_concept(
            tmp.path(),
            "explicit-alias",
            "title: Explicit\naliases: [First]",
        );
        let duplicates = find_duplicate_concepts(&scan(tmp.path()));
        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].a, "explicit-alias");
        assert_eq!(duplicates[0].b, "first/entry");
    }

    #[cfg(unix)]
    #[test]
    fn concept_scan_uses_distinct_inventory_owners_and_preserves_hard_links() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        write_concept(tmp.path(), "nested/owner", "title: Shared");
        let directory = tmp.path().join("wiki/concepts");
        symlink("nested/owner.md", directory.join("alias.md")).unwrap();
        let pages = scan(tmp.path());
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].slug, "nested/owner");
        assert!(find_duplicate_concepts(&pages).is_empty());

        std::fs::hard_link(directory.join("nested/owner.md"), directory.join("hard.md")).unwrap();
        let pages = scan(tmp.path());
        assert_eq!(pages.len(), 2);
        let duplicates = find_duplicate_concepts(&pages);
        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].a, "hard");
        assert_eq!(duplicates[0].b, "nested/owner");
    }

    #[cfg(unix)]
    #[test]
    fn concept_scan_propagates_incomplete_nested_inventory_errors() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        write_concept(tmp.path(), "nested/owner", "title: Owner");
        symlink(
            "missing.md",
            tmp.path().join("wiki/concepts/nested/dangling.md"),
        )
        .unwrap();
        assert!(scan_concept_pages(tmp.path(), "wiki").is_err());
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
    fn null_category_is_uncategorised() {
        for value in ["null", "~", ""] {
            let tmp = TempDir::new().unwrap();
            write_concept(tmp.path(), "x", &format!("id: x\ncategory: {value}"));
            assert!(find_invalid_categories(&scan(tmp.path()), &cats(&["ai-ml"])).is_empty());
        }
    }

    #[test]
    fn quoted_null_category_requires_a_configured_id() {
        let tmp = TempDir::new().unwrap();
        write_concept(tmp.path(), "x", "id: x\ncategory: \"null\"");
        let pages = scan(tmp.path());
        let findings = find_invalid_categories(&pages, &cats(&["ai-ml"]));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].category, "null");
        assert!(find_invalid_categories(&pages, &cats(&["null"])).is_empty());
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
    fn non_string_categories_cannot_match_configured_ids() {
        for value in ["123", "true"] {
            let tmp = TempDir::new().unwrap();
            let configured = cats(&[value]);
            write_concept(tmp.path(), "x", &format!("category: {value}"));
            assert_eq!(
                find_invalid_categories(&scan(tmp.path()), &configured).len(),
                1
            );
            write_concept(tmp.path(), "x", &format!("category: \"{value}\""));
            assert!(find_invalid_categories(&scan(tmp.path()), &configured).is_empty());
        }
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
    fn an_explicit_shared_name_is_flagged() {
        let tmp = TempDir::new().unwrap();
        write_concept(tmp.path(), "vector-db", "id: vector-db");
        write_concept(tmp.path(), "vectordb", "id: vectordb\naliases: [vector-db]");
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
    fn word_boundaries_do_not_imply_identity() {
        // The companion of `claude-3-5` ~ `claude-35` staying distinct: only a break
        // BETWEEN numerals is identity, so a one-sided one folds and the pair IS a finding.
        // The pipeline relies on the same rule to route `Claude 35` at one page.
        let tmp = TempDir::new().unwrap();
        write_concept(tmp.path(), "claude-35", "id: claude-35");
        write_concept(tmp.path(), "claude35", "id: claude35");
        let result = find_duplicate_concepts(&scan(tmp.path()));
        let pairs: Vec<(&str, &str)> = result.iter().map(|d| (&*d.a, &*d.b)).collect();
        assert!(pairs.is_empty(), "{result:?}");
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
        assert_eq!(result[0].a_name, "HTMX");
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
            "id: vector-db\ntitle: \"Vector DB\"\naliases: [\"VectorDB\"]",
        );
        write_concept(
            tmp.path(),
            "vectordb",
            "id: vectordb\ntitle: \"VectorDB\"\naliases: [\"Vector DB\"]",
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
    fn punctuation_does_not_imply_equivalence() {
        for (a, b) in [
            ("A/I", "A I"),
            ("Vite+", "Vite"),
            ("C", "C++"),
            ("vector-db", "vectordb"),
        ] {
            assert_ne!(identity_key(a), identity_key(b));
        }
    }

    #[test]
    fn a_fence_inside_a_quote_ends_with_the_quote_and_suppresses_nothing_after_it() {
        let tmp = TempDir::new().unwrap();
        write_concept(
            tmp.path(),
            "unclosed",
            "id: unclosed\n---\n\n## 핵심\n\n> ```\n> 닫히지 않은 코드\n\n평문 단락.\n\n> [!conflict] 진짜 갈등이다.\n",
        );
        write_concept(
            tmp.path(),
            "separator",
            "id: separator\n---\n\n## 핵심\n\n> ~~~~~ 구분선 ~~~~~\n\n> [!conflict] 진짜 갈등이다.\n",
        );
        // A fence at the TOP level has no container but the document, so it does run to the
        // end — the two layers must not be reset by the same event.
        write_concept(
            tmp.path(),
            "toplevel",
            "id: toplevel\n---\n\n## 핵심\n\n```\n> [!conflict] 코드 안의 가짜 갈등\n",
        );
        let by_slug: std::collections::BTreeMap<String, String> =
            find_unresolved_conflicts(&scan(tmp.path()))
                .into_iter()
                .map(|c| (c.slug, c.note))
                .collect();
        assert_eq!(
            by_slug.get("unclosed").map(String::as_str),
            Some("진짜 갈등이다."),
            "the quote ended, so the fence it held ended with it"
        );
        assert_eq!(
            by_slug.get("separator").map(String::as_str),
            Some("진짜 갈등이다."),
            "a tilde separator opens a fence that the quote's end closes"
        );
        assert!(
            !by_slug.contains_key("toplevel"),
            "an unclosed fence at the top level still runs to the end of the page"
        );
    }

    /// A callout answers from its OWN lines: a shallower line ends its container, and the
    /// depth-2 lines after a depth-1 line are a different nested quote rather than more of
    /// this callout. A nested marker also carries an indent of its own, up to three spaces,
    /// which reading exactly one space made `>  > x` parse as depth 1.
    #[test]
    fn a_callout_answers_at_its_own_depth_and_stops_where_its_quote_does() {
        let tmp = TempDir::new().unwrap();
        write_concept(
            tmp.path(),
            "nested-fence",
            "id: nested-fence\n---\n\n## 핵심\n\n> [!conflict]\n> > ```\n> > 코드\n> 진술이다.\n",
        );
        write_concept(
            tmp.path(),
            "wide-marker",
            "id: wide-marker\n---\n\n## 핵심\n\n> [!conflict]\n>  > [!note] 참고\n>  > 중첩 본문이다.\n> 진술이다.\n",
        );
        // Four spaces: ONE is the outer marker's own, three are the nested marker's, which is
        // the most CommonMark allows. Charging all four to the nested marker declined it.
        write_concept(
            tmp.path(),
            "wide-marker-4",
            "id: wide-marker-4\n---\n\n## 핵심\n\n> [!conflict]\n>    > [!note] 참고\n>    > 중첩 본문이다.\n> 진술이다.\n",
        );
        write_concept(
            tmp.path(),
            "tab-marker",
            "id: tab-marker\n---\n\n## 핵심\n\n> [!conflict]\n>\t> [!note] 참고\n>\t> 중첩 본문이다.\n> 진술이다.\n",
        );
        // A tab advances to the next four-column stop, so this one is an indented code block
        // and the callout inside it is content — the same rule, answering the other way.
        write_concept(
            tmp.path(),
            "tab-indent",
            "id: tab-indent\n---\n\n## 핵심\n\n\t> [!conflict] 코드블록 안이다\n",
        );
        write_concept(
            tmp.path(),
            "other-block",
            "id: other-block\n---\n\n## 핵심\n\n> 바깥 인용.\n> > [!conflict]\n> 바깥 이어짐.\n> > 다른 중첩 인용의 본문이다.\n",
        );
        let by_slug: std::collections::BTreeMap<String, String> =
            find_unresolved_conflicts(&scan(tmp.path()))
                .into_iter()
                .map(|c| (c.slug, c.note))
                .collect();
        assert_eq!(
            by_slug.get("nested-fence").map(String::as_str),
            Some("진술이다."),
            "a fence the NESTED quote opened closes with it, not with the callout"
        );
        assert_eq!(
            by_slug.get("wide-marker").map(String::as_str),
            Some("진술이다."),
            "two spaces between markers is still a nested marker"
        );
        assert_eq!(
            by_slug.get("wide-marker-4").map(String::as_str),
            Some("진술이다."),
            "three of the four columns are the nested marker's own indent"
        );
        assert_eq!(
            by_slug.get("tab-marker").map(String::as_str),
            Some("진술이다."),
            "a tab between markers is a nested marker's indent, measured in columns"
        );
        assert!(
            !by_slug.contains_key("tab-indent"),
            "one tab is four columns, which is an indented code block and not a blockquote"
        );
        assert_eq!(
            by_slug.get("other-block").map(String::as_str),
            Some(""),
            "the callout's quote ended at the shallower line; what follows is another block"
        );
    }

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
    fn conflict_callouts_use_commonmark_lines_without_reporting_literal_examples() {
        for ending in ["\n", "\r\n", "\r"] {
            let page = ConceptPage {
                slug: "topic".into(),
                path: "wiki/concepts/topic.md".into(),
                category: None,
                names: vec!["Topic".into()],
                body: "# Topic\n\n```md\n> [!conflict] literal example\n```\n\n> [!note] annotation\n\n> [!CONFLICT]-\n> Sources disagree.\n"
                    .replace('\n', ending),
            };
            let result = find_unresolved_conflicts(&[page]);
            assert_eq!(result.len(), 1, "{ending:?}: {result:?}");
            assert_eq!(result[0].note, "Sources disagree.");
        }
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
