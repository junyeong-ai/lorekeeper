//! Slug normalization: detect filenames whose stem is not already a canonical slug,
//! rename them, and repoint links that address the old path.
//!
//! The normalization rule itself is `lk_core::slugify` (NFKC-correct) — there is no
//! local slug function here. This module only owns the rename plan + link rewrite.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use lk_core::concept::slugify;
use lk_core::link;
use lk_core::vault_path::RESERVED_WIKI_FILES;

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
                tracing::warn!(
                    path = %page.path.display(),
                    rename_to = %normalized,
                    "skipping rename: would collide with another file"
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
        // A renamed page keeps its directory (only the filename changes), so links
        // inside it still resolve from the same base — read it at its new path.
        let rel_path = path_map
            .get(page.path.as_path())
            .copied()
            .unwrap_or(&page.path);
        let abs_path = root.join(rel_path);

        let content = std::fs::read_to_string(&abs_path)
            .map_err(|e| GraphError::Io(format!("failed to read {}: {e}", abs_path.display())))?;

        let updated = repoint_renamed_links(&content, rel_path, &path_map);

        if updated != content {
            lk_core::fs::write_atomic(&abs_path, updated.as_bytes(), None).map_err(|e| {
                GraphError::Io(format!("failed to write {}: {e}", abs_path.display()))
            })?;
        }
    }

    Ok(renames.len())
}

/// Repoint every link whose destination resolves to a renamed page at that page's new
/// path. Rewrites OUTSIDE code only: a link shown inside a code fence/span is a code
/// example, not a graph edge (`extract_dests` ignores it), so rewriting it would
/// corrupt the document.
fn repoint_renamed_links(
    content: &str,
    page_path: &Path,
    path_map: &HashMap<&Path, &Path>,
) -> String {
    link::rewrite_links_outside_code(content, |text, raw_dest| {
        let (dest, anchor) = link::split_dest_anchor(raw_dest);
        let resolved = link::resolve_dest(page_path, &link::decode_dest(dest))?;
        let new_path = path_map.get(resolved.as_path())?;
        // `text` arrives exactly as written (escapes included) — reassemble verbatim
        // rather than through `md_link`, which would escape it a second time.
        let new_dest = format!("{}{anchor}", link::relative_dest(page_path, new_path));
        Some(format!("[{text}]({new_dest})"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan;

    fn build_page(path: &str, outgoing: &[&str]) -> ScannedPage {
        let rel = PathBuf::from(path);
        ScannedPage {
            id: scan::path_slug(&rel),
            path: rel,
            title: "test".to_owned(),
            outgoing: outgoing.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn detect_denormalized() {
        let pages = vec![
            build_page("wiki/good-name.md", &[]),
            build_page("wiki/Bad_Name.md", &[]),
            build_page("wiki/UPPER.md", &[]),
        ];
        let renames = scan(&pages);
        assert_eq!(renames.len(), 2);
        assert!(renames.iter().any(|r| r.old_slug == "Bad_Name"));
        assert!(renames.iter().any(|r| r.old_slug == "UPPER"));
    }

    #[test]
    fn normalized_slug_not_flagged() {
        let pages = vec![build_page("wiki/already-normalized.md", &[])];
        let renames = scan(&pages);
        assert!(renames.is_empty());
    }

    #[test]
    fn link_repointing_preserves_text_and_anchor() {
        let old = PathBuf::from("wiki/Concept_A.md");
        let new = PathBuf::from("wiki/concept-a.md");
        let path_map: HashMap<&Path, &Path> = [(old.as_path(), new.as_path())].into();
        let content = "See [A](Concept_A.md) and [A](Concept_A.md#part) and [B](other.md) here.";
        let updated = repoint_renamed_links(content, Path::new("wiki/linker.md"), &path_map);
        assert_eq!(
            updated,
            "See [A](concept-a.md) and [A](concept-a.md#part) and [B](other.md) here."
        );
    }

    #[test]
    fn scan_detects_collision() {
        let pages = vec![
            build_page("wiki/Foo_Bar.md", &[]),
            build_page("wiki/FOO-BAR.md", &[]),
        ];
        let renames = scan(&pages);
        assert_eq!(renames.len(), 1);
    }

    #[test]
    fn links_inside_code_are_not_repointed() {
        // A link shown as a code example is not a graph edge, so normalize must
        // leave it verbatim — only the real prose link is repointed.
        let old = PathBuf::from("wiki/Concept A.md");
        let new = PathBuf::from("wiki/concept-a.md");
        let path_map: HashMap<&Path, &Path> = [(old.as_path(), new.as_path())].into();
        let content = "Prose [A](Concept%20A.md).\n```\nexample [A](Concept%20A.md)\n```\nInline `[A](Concept%20A.md)`.\n";
        let updated = repoint_renamed_links(content, Path::new("wiki/linker.md"), &path_map);
        assert!(updated.contains("Prose [A](concept-a.md)."));
        assert!(updated.contains("example [A](Concept%20A.md)"));
        assert!(updated.contains("Inline `[A](Concept%20A.md)`."));
    }

    #[test]
    fn apply_renames_and_repoints_across_vault() {
        let tmp = tempfile::tempdir().unwrap();
        let wiki = tmp.path().join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::write(wiki.join("Bad_Name.md"), "# Bad\n").unwrap();
        std::fs::write(wiki.join("linker.md"), "See [Bad](Bad_Name.md).\n").unwrap();

        let pages = vec![
            build_page("wiki/Bad_Name.md", &[]),
            build_page("wiki/linker.md", &["wiki/bad-name"]),
        ];
        let renames = scan(&pages);
        let applied = apply(&renames, &pages, tmp.path()).unwrap();
        assert_eq!(applied, 1);

        assert!(!wiki.join("Bad_Name.md").exists());
        assert!(wiki.join("bad-name.md").exists());
        let linker = std::fs::read_to_string(wiki.join("linker.md")).unwrap();
        assert_eq!(linker, "See [Bad](bad-name.md).\n");
    }

    #[test]
    fn reserved_files_skipped() {
        let pages = vec![
            build_page("wiki/AGENTS.md", &[]),
            build_page("wiki/index.md", &[]),
            build_page("wiki/Bad_Name.md", &[]),
        ];
        let renames = scan(&pages);
        assert_eq!(renames.len(), 1);
        assert_eq!(renames[0].old_slug, "Bad_Name");
    }
}
