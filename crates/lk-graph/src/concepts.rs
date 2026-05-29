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
use serde::Serialize;
use strsim::sorensen_dice;

use crate::GraphError;

/// One concept page whose `category` frontmatter does not appear in the
/// configured `concepts.categories[].id` set.
#[derive(Debug, Clone, Serialize)]
pub struct InvalidCategoryConcept {
    /// Vault-relative path of the offending concept page.
    pub path: PathBuf,
    /// The slug that identifies the concept (from frontmatter `id`, falling
    /// back to the file stem so the lint can still locate broken pages).
    pub slug: String,
    /// The `category` value as found on disk — the thing that fails to match
    /// any configured id.
    pub category: String,
}

/// Walk `{wiki}/concepts/*.md` and return every page whose `category` frontmatter
/// is set to a value not in `configured`. Pages without a `category` field are not
/// flagged — leaving the field unset is the documented way to mark a concept as
/// uncategorised. When `configured` is empty, the categorisation feature is off and
/// nothing is flagged regardless of what's on disk.
pub fn invalid_categories(
    vault_root: &Path,
    wiki_dir: &str,
    configured: &[ConceptCategory],
) -> Result<Vec<InvalidCategoryConcept>, GraphError> {
    if configured.is_empty() {
        return Ok(Vec::new());
    }
    let valid_ids: std::collections::HashSet<&str> =
        configured.iter().map(|c| c.id.as_str()).collect();

    let concepts_dir = vault_root.join(wiki_dir).join("concepts");
    let entries = match std::fs::read_dir(&concepts_dir) {
        Ok(e) => e,
        // No concepts directory yet → nothing to lint, not an error.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(GraphError::Io(format!(
                "read {}: {e}",
                concepts_dir.display()
            )));
        }
    };

    let mut findings = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| GraphError::Io(format!("walk concepts: {e}")))?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                return Err(GraphError::Io(format!("read {}: {e}", path.display())));
            }
        };
        let Ok(page) = frontmatter::parse_page(&raw) else {
            // Malformed frontmatter is the responsibility of structural lints
            // elsewhere; skip rather than double-report.
            continue;
        };
        let Some(category) = page.frontmatter.get("category").and_then(|v| v.as_str()) else {
            continue;
        };
        if valid_ids.contains(category) {
            continue;
        }
        let slug = page
            .frontmatter
            .get("id")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| {
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .to_owned()
            });
        let rel = path.strip_prefix(vault_root).unwrap_or(&path).to_path_buf();
        findings.push(InvalidCategoryConcept {
            path: rel,
            slug,
            category: category.to_owned(),
        });
    }
    findings.sort_by(|a, b| a.slug.cmp(&b.slug));
    Ok(findings)
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
pub fn near_duplicate_concepts(
    vault_root: &Path,
    wiki_dir: &str,
    threshold: f64,
) -> Result<Vec<NearDuplicateConcept>, GraphError> {
    if !(0.0..=1.0).contains(&threshold) || threshold == 0.0 {
        return Ok(Vec::new());
    }

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

    let mut slugs: Vec<String> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| GraphError::Io(format!("walk concepts: {e}")))?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        // The filename stem IS the canonical slug (concepts are written as
        // `{slug}.md`); no need to parse frontmatter for identity.
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            slugs.push(stem.to_owned());
        }
    }
    slugs.sort();

    let mut findings = Vec::new();
    for i in 0..slugs.len() {
        for j in (i + 1)..slugs.len() {
            let similarity = sorensen_dice(&slugs[i], &slugs[j]);
            if similarity >= threshold {
                findings.push(NearDuplicateConcept {
                    a: slugs[i].clone(),
                    b: slugs[j].clone(),
                    similarity,
                });
            }
        }
    }
    Ok(findings)
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

    #[test]
    fn empty_config_means_no_findings_regardless_of_pages() {
        let tmp = TempDir::new().unwrap();
        write_concept(tmp.path(), "x", "id: x\ncategory: anything");
        let result = invalid_categories(tmp.path(), "wiki", &[]).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn missing_concepts_dir_is_not_an_error() {
        let tmp = TempDir::new().unwrap();
        let result = invalid_categories(tmp.path(), "wiki", &cats(&["ai-ml"])).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn valid_category_is_silent() {
        let tmp = TempDir::new().unwrap();
        write_concept(tmp.path(), "x", "id: x\ncategory: ai-ml");
        let result = invalid_categories(tmp.path(), "wiki", &cats(&["ai-ml"])).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn unknown_category_is_flagged() {
        let tmp = TempDir::new().unwrap();
        write_concept(tmp.path(), "x", "id: x\ncategory: security");
        let result =
            invalid_categories(tmp.path(), "wiki", &cats(&["ai-ml", "infrastructure"])).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].slug, "x");
        assert_eq!(result[0].category, "security");
    }

    #[test]
    fn missing_category_field_is_silent() {
        // Omitting the field is the documented way to mark a concept as uncategorised.
        let tmp = TempDir::new().unwrap();
        write_concept(tmp.path(), "x", "id: x");
        let result = invalid_categories(tmp.path(), "wiki", &cats(&["ai-ml"])).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn falls_back_to_filename_when_id_missing() {
        let tmp = TempDir::new().unwrap();
        write_concept(tmp.path(), "fallback-slug", "category: nope");
        let result = invalid_categories(tmp.path(), "wiki", &cats(&["ai-ml"])).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].slug, "fallback-slug");
    }

    #[test]
    fn findings_are_sorted_by_slug_for_deterministic_output() {
        let tmp = TempDir::new().unwrap();
        write_concept(tmp.path(), "zeta", "id: zeta\ncategory: bogus");
        write_concept(tmp.path(), "alpha", "id: alpha\ncategory: bogus");
        write_concept(tmp.path(), "mu", "id: mu\ncategory: bogus");
        let result = invalid_categories(tmp.path(), "wiki", &cats(&["ai-ml"])).unwrap();
        let slugs: Vec<&str> = result.iter().map(|f| f.slug.as_str()).collect();
        assert_eq!(slugs, vec!["alpha", "mu", "zeta"]);
    }

    #[test]
    fn near_duplicate_slugs_are_flagged_distinct_ones_are_not() {
        let tmp = TempDir::new().unwrap();
        write_concept(tmp.path(), "vector-database", "id: vector-database");
        write_concept(tmp.path(), "vector-db", "id: vector-db");
        write_concept(tmp.path(), "kubernetes", "id: kubernetes");
        let result = near_duplicate_concepts(tmp.path(), "wiki", 0.6).unwrap();
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
    fn near_duplicate_threshold_zero_or_missing_dir_is_empty() {
        let tmp = TempDir::new().unwrap();
        write_concept(tmp.path(), "vector-database", "id: vector-database");
        write_concept(tmp.path(), "vector-db", "id: vector-db");
        assert!(
            near_duplicate_concepts(tmp.path(), "wiki", 0.0)
                .unwrap()
                .is_empty()
        );
        let empty = TempDir::new().unwrap();
        assert!(
            near_duplicate_concepts(empty.path(), "wiki", 0.85)
                .unwrap()
                .is_empty()
        );
    }
}
