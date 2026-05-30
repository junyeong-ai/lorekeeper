//! Slug normalization: detect filenames whose stem is not already a canonical slug,
//! rename them, and rewrite wikilinks that point at the old slug.
//!
//! The normalization rule itself is `lk_core::slugify` (NFKC-correct) — there is no
//! local slug function here. This module only owns the rename plan + wikilink rewrite.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use lk_core::concept::slugify;
use lk_core::vault_path::RESERVED_WIKI_FILES;
use lk_core::wikilink::{self, WIKILINK_RE};

use crate::GraphError;
use crate::scan::ScannedPage;

#[derive(Debug, Clone)]
pub struct Rename {
    pub old_path: PathBuf,
    pub new_path: PathBuf,
    pub old_slug: String,
    pub new_slug: String,
}

pub fn scan(pages: &[ScannedPage]) -> Vec<Rename> {
    let mut renames = Vec::new();
    let mut claimed_slugs: HashSet<String> = HashSet::new();

    for page in pages {
        if let Some(filename) = page.path.file_name().and_then(|f| f.to_str())
            && RESERVED_WIKI_FILES
                .iter()
                .any(|r| r.eq_ignore_ascii_case(filename))
        {
            continue;
        }

        let Some(stem) = page.path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };

        let Some(normalized) = slugify(stem) else {
            continue;
        };

        if stem != normalized {
            if !claimed_slugs.insert(normalized.clone()) {
                eprintln!(
                    "warning: skipping {}: rename to '{}' would collide with another file",
                    page.path.display(),
                    normalized
                );
                continue;
            }

            let new_path = page.path.with_file_name(format!("{normalized}.md"));
            renames.push(Rename {
                old_path: page.path.clone(),
                new_path,
                old_slug: stem.to_owned(),
                new_slug: normalized,
            });
        }
    }

    renames.sort_by(|a, b| a.old_slug.cmp(&b.old_slug));
    renames
}

pub fn apply(renames: &[Rename], pages: &[ScannedPage], root: &Path) -> Result<usize, GraphError> {
    if renames.is_empty() {
        return Ok(0);
    }

    for rename in renames {
        let target = root.join(&rename.new_path);
        if target.exists() {
            return Err(GraphError::Io(format!(
                "rename target already exists: {}",
                target.display()
            )));
        }
    }

    let renamed_normalized: HashSet<String> = renames
        .iter()
        .filter_map(|r| slugify(&r.old_slug))
        .collect();

    let path_map: HashMap<&Path, &Path> = renames
        .iter()
        .map(|r| (r.old_path.as_path(), r.new_path.as_path()))
        .collect();

    for rename in renames {
        let old = root.join(&rename.old_path);
        let new = root.join(&rename.new_path);
        std::fs::rename(&old, &new).map_err(|e| {
            GraphError::Io(format!(
                "failed to rename {} -> {}: {e}",
                old.display(),
                new.display()
            ))
        })?;
    }

    for page in pages {
        let rel_path = path_map
            .get(page.path.as_path())
            .copied()
            .unwrap_or(&page.path);
        let abs_path = root.join(rel_path);

        let content = std::fs::read_to_string(&abs_path)
            .map_err(|e| GraphError::Io(format!("failed to read {}: {e}", abs_path.display())))?;

        let updated = normalize_wikilinks(&content, &renamed_normalized);

        if updated != content {
            std::fs::write(&abs_path, &updated).map_err(|e| {
                GraphError::Io(format!("failed to write {}: {e}", abs_path.display()))
            })?;
        }
    }

    Ok(renames.len())
}

fn normalize_wikilinks(content: &str, renamed_slugs: &HashSet<String>) -> String {
    WIKILINK_RE
        .replace_all(content, |caps: &regex::Captures| {
            let full = &caps[0];
            // One decomposition path for the whole workspace: page / anchor / alias,
            // each preserved exactly once on rewrite. (`[[`/`]]` are ASCII — byte-safe.)
            let (page_raw, anchor, alias) =
                wikilink::split_wikilink_parts(&full[2..full.len() - 2]);
            let Some(normalized) = slugify(page_raw) else {
                return full.to_owned();
            };

            if renamed_slugs.contains(&normalized) && page_raw.trim() != normalized {
                format!("[[{normalized}{anchor}{alias}]]")
            } else {
                full.to_owned()
            }
        })
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan;

    fn make_page(path: &str, outgoing: &[&str]) -> ScannedPage {
        let rel = PathBuf::from(path);
        ScannedPage {
            id: scan::path_slug(&rel),
            path: rel,
            title: "test".to_owned(),
            outgoing: outgoing.iter().map(|s| (*s).to_string()).collect(),
            aliases: Vec::new(),
        }
    }

    #[test]
    fn detect_denormalized() {
        let pages = vec![
            make_page("wiki/good-name.md", &[]),
            make_page("wiki/Bad_Name.md", &[]),
            make_page("wiki/UPPER.md", &[]),
        ];
        let renames = scan(&pages);
        assert_eq!(renames.len(), 2);
        assert!(renames.iter().any(|r| r.old_slug == "Bad_Name"));
        assert!(renames.iter().any(|r| r.old_slug == "UPPER"));
    }

    #[test]
    fn normalized_slug_not_flagged() {
        let pages = vec![make_page("wiki/already-normalized.md", &[])];
        let renames = scan(&pages);
        assert!(renames.is_empty());
    }

    #[test]
    fn wikilink_normalization() {
        let slugs: HashSet<String> = ["concept-a".to_owned()].into();
        let content = "See [[Concept_A]] and [[Concept_A|Display Text]] here.";
        let updated = normalize_wikilinks(content, &slugs);
        assert_eq!(
            updated,
            "See [[concept-a]] and [[concept-a|Display Text]] here."
        );
    }

    #[test]
    fn wikilink_already_normalized_unchanged() {
        let slugs: HashSet<String> = ["concept-a".to_owned()].into();
        let content = "See [[concept-a]] here.";
        let updated = normalize_wikilinks(content, &slugs);
        assert_eq!(updated, content);
    }

    #[test]
    fn wikilink_anchor_preserved() {
        let slugs: HashSet<String> = ["concept-a".to_owned()].into();
        let content = "See [[Concept_A#heading]] and [[Concept_A^block|Label]] here.";
        let updated = normalize_wikilinks(content, &slugs);
        assert_eq!(
            updated,
            "See [[concept-a#heading]] and [[concept-a^block|Label]] here."
        );
    }

    #[test]
    fn scan_detects_collision() {
        let pages = vec![
            make_page("wiki/Foo_Bar.md", &[]),
            make_page("wiki/FOO-BAR.md", &[]),
        ];
        let renames = scan(&pages);
        assert_eq!(renames.len(), 1);
    }

    #[test]
    fn non_renamed_wikilinks_unchanged() {
        let slugs: HashSet<String> = ["concept-a".to_owned()].into();
        let content = "See [[Other Node]] here.";
        let updated = normalize_wikilinks(content, &slugs);
        assert_eq!(updated, content);
    }

    #[test]
    fn reserved_files_skipped() {
        let pages = vec![
            make_page("wiki/AGENTS.md", &[]),
            make_page("wiki/index.md", &[]),
            make_page("wiki/Bad_Name.md", &[]),
        ];
        let renames = scan(&pages);
        assert_eq!(renames.len(), 1);
        assert_eq!(renames[0].old_slug, "Bad_Name");
    }
}
