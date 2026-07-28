//! Contradiction-audit worklist for concept pages.
//!
//! `concept_lint::find_unresolved_conflicts` *tracks* contradictions a human already
//! flagged with a `> [!conflict]` callout, but nothing *surfaces* the concepts that
//! warrant a fresh look. A concept cited by two or more independent sources can carry
//! conflicting claims; once that source set changes, it deserves another audit.
//!
//! Selection is deterministic. A concept is a candidate iff it has at least two
//! citations (`source_count >= 2`) AND its current source set differs from the set at
//! the last audit — identified by hashing the canonical `## Sources` body and comparing
//! to the `audited_sources_hash` frontmatter marker. The hash (not a count) is what
//! makes the signal robust: a source swap that keeps the count constant still changes
//! the hash and resurfaces the concept, while an unchanged set keeps it off the list —
//! low-noise by construction. `mark_audited` (the `audit-mark` command `/lore-wiki
//! audit` calls after reviewing) stamps the current hash. The contradiction *judgment*
//! always stays with the LLM/human; this module only routes attention.

use std::path::{Path, PathBuf};

use lk_core::concept::slugify;
use lk_core::frontmatter;
use lk_core::i18n::Locale;
use lk_vault::{VaultWriter, section_body, set_frontmatter_field};
use serde::Serialize;

use crate::GraphError;

/// Minimum citations before a concept can carry a source-vs-source contradiction.
const MIN_SOURCES_FOR_AUDIT: u64 = 2;

/// A concept page due for a (re-)audit: multiply-cited, with a source set that has
/// changed since it was last reviewed.
#[derive(Debug, Clone, Serialize)]
pub struct AuditCandidate {
    /// Concept slug (the file stem — the canonical page identity).
    pub slug: String,
    /// Vault-relative path of the concept page.
    pub path: PathBuf,
    /// Current citation count (`source_count`, owned by `backlinks-sync`).
    pub source_count: u64,
}

/// Walk `{wiki}/concepts/*.md` and return the audit worklist, sorted by citation count
/// (descending — the most-cited concepts first), then by slug. A page with malformed
/// frontmatter is skipped; a missing concepts dir is not an error.
pub fn find_audit_candidates(
    vault_root: &Path,
    wiki_dir: &str,
    locale: Locale,
) -> Result<Vec<AuditCandidate>, GraphError> {
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

    let mut candidates = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| GraphError::Io(format!("walk concepts: {e}")))?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| GraphError::Io(format!("read {}: {e}", path.display())))?;
        let Ok(page) = frontmatter::parse_page(&raw) else {
            continue;
        };
        let source_count = page.frontmatter.source_count().unwrap_or(0);
        if source_count < MIN_SOURCES_FOR_AUDIT {
            continue;
        }
        let stored = page
            .frontmatter
            .get(frontmatter::field::AUDITED_SOURCES_HASH)
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if sources_hash(&raw, locale) == stored {
            continue;
        }

        let slug = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_owned();
        let rel_path = path.strip_prefix(vault_root).unwrap_or(&path).to_path_buf();
        candidates.push(AuditCandidate {
            slug,
            path: rel_path,
            source_count,
        });
    }

    candidates.sort_by(|a, b| {
        b.source_count
            .cmp(&a.source_count)
            .then_with(|| a.slug.cmp(&b.slug))
    });
    Ok(candidates)
}

/// Stamp `audited_sources_hash` on one concept page to the hash of its current
/// `## Sources` body — recording "this source set has been reviewed" so the concept
/// leaves the worklist until its sources change again. Returns whether the page was
/// rewritten (false when the marker was already current). Errors if the page is absent.
pub fn mark_audited(
    vault_root: &Path,
    wiki_dir: &str,
    slug: &str,
    locale: Locale,
) -> Result<bool, GraphError> {
    let Some(slug) = slugify(slug) else {
        return Err(GraphError::Io(format!("invalid concept slug: {slug:?}")));
    };
    let rel_path = PathBuf::from(wiki_dir)
        .join(lk_core::vault_path::CONCEPTS_SUBDIR)
        .join(format!("{slug}.md"));
    let full_path = vault_root.join(&rel_path);
    let raw = std::fs::read_to_string(&full_path)
        .map_err(|e| GraphError::Io(format!("read {}: {e}", full_path.display())))?;

    let updated = set_frontmatter_field(
        &raw,
        frontmatter::field::AUDITED_SOURCES_HASH,
        &sources_hash(&raw, locale),
    )
    .ok_or_else(|| {
        GraphError::Io(format!(
            "{}: no frontmatter block to record the audit marker in",
            rel_path.display()
        ))
    })?;
    if updated == raw {
        return Ok(false);
    }
    VaultWriter::new(vault_root)
        .write_page_sync(&rel_path, &updated)
        .map_err(|e| GraphError::Io(format!("write {}: {e}", rel_path.display())))?;
    Ok(true)
}

/// Hash (BLAKE3-128 hex) of a concept's canonical `## Sources` body — the identity of
/// its current source set. The heading is resolved under ANY locale (a page authored
/// before a `vault.locale` switch keeps its old heading), mirroring `backlinks`. The
/// body is `backlinks-sync`-canonical (sorted `- [title](relative-path)` lines), so the
/// hash is stable.
fn sources_hash(raw: &str, locale: Locale) -> String {
    let heading = Locale::ALL
        .iter()
        .map(|l| l.strings().concept_sources)
        .find(|h| section_body(raw, h).is_some())
        .unwrap_or_else(|| locale.strings().concept_sources);
    let body = section_body(raw, heading).unwrap_or("").trim();
    blake3::hash(body.as_bytes()).to_hex()[..32].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Write a concept page with the given frontmatter and `## Sources` lines (Ko
    /// heading `출처`, the test locale).
    fn write_concept(root: &Path, slug: &str, frontmatter_body: &str, sources: &[&str]) {
        let dir = root.join("wiki").join(lk_core::vault_path::CONCEPTS_SUBDIR);
        std::fs::create_dir_all(&dir).unwrap();
        let body = sources
            .iter()
            .map(|s| format!("- [{s}](../../daily/x/{s}.md)"))
            .collect::<Vec<_>>()
            .join("\n");
        let content =
            format!("---\n{frontmatter_body}\n---\n\n# {slug}\n\n## 출처\n\n{body}\n\n## 메타\n");
        std::fs::write(dir.join(format!("{slug}.md")), content).unwrap();
    }

    fn run(root: &Path) -> Vec<AuditCandidate> {
        find_audit_candidates(root, "wiki", Locale::Ko).unwrap()
    }

    #[test]
    fn multiply_cited_unaudited_concept_is_a_candidate() {
        let tmp = TempDir::new().unwrap();
        write_concept(
            tmp.path(),
            "rag",
            "id: rag\nsource_count: 3",
            &["a", "b", "c"],
        );
        let c = run(tmp.path());
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].slug, "rag");
        assert_eq!(c[0].source_count, 3);
    }

    #[test]
    fn single_source_concept_is_not_a_candidate() {
        let tmp = TempDir::new().unwrap();
        write_concept(tmp.path(), "rag", "id: rag\nsource_count: 1", &["a"]);
        assert!(run(tmp.path()).is_empty());
    }

    #[test]
    fn audited_concept_drops_off_and_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        write_concept(tmp.path(), "rag", "id: rag\nsource_count: 2", &["a", "b"]);
        assert_eq!(
            run(tmp.path()).len(),
            1,
            "unaudited multi-source concept surfaces"
        );
        // Mark audited → drops off; marking again is a no-op write.
        assert!(mark_audited(tmp.path(), "wiki", "rag", Locale::Ko).unwrap());
        assert!(
            run(tmp.path()).is_empty(),
            "an audited concept leaves the worklist"
        );
        assert!(!mark_audited(tmp.path(), "wiki", "rag", Locale::Ko).unwrap());
    }

    #[test]
    fn source_set_changes_are_detected_by_hash_not_count() {
        let tmp = TempDir::new().unwrap();
        // Audited at sources {a, b}.
        write_concept(tmp.path(), "rag", "id: rag\nsource_count: 2", &["a", "b"]);
        mark_audited(tmp.path(), "wiki", "rag", Locale::Ko).unwrap();
        assert!(run(tmp.path()).is_empty());

        // Swap one source so the set becomes {a, c} — SAME count (2), different set.
        // Edit in place so the `audited_sources_hash` marker is preserved. Count-only
        // tracking would miss this; the hash catches it.
        let path = tmp
            .path()
            .join("wiki")
            .join(lk_core::vault_path::CONCEPTS_SUBDIR)
            .join("rag.md");
        let swapped = std::fs::read_to_string(&path)
            .unwrap()
            .replace("- [b](../../daily/x/b.md)", "- [c](../../daily/x/c.md)");
        std::fs::write(&path, swapped).unwrap();

        let c = run(tmp.path());
        assert_eq!(
            c.len(),
            1,
            "same count, different source set must resurface"
        );
        assert_eq!(c[0].source_count, 2);
    }

    #[test]
    fn missing_concepts_dir_is_not_an_error() {
        let tmp = TempDir::new().unwrap();
        assert!(run(tmp.path()).is_empty());
    }

    #[test]
    fn mark_audited_errors_on_missing_page() {
        let tmp = TempDir::new().unwrap();
        assert!(mark_audited(tmp.path(), "wiki", "nope", Locale::Ko).is_err());
    }
}
