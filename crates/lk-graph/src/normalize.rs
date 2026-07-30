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
use crate::scan::{self, ScannedPage};

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

/// Whether two paths name the SAME file on disk, which is not the same question as whether
/// they are the same string.
///
/// The rename this module performs is usually case-only (`Bad_Name` → `bad-name`,
/// `allUsers-…` → `allusers-…`), and macOS and Windows default to case-INSENSITIVE
/// filesystems. There the new path already "exists" — as the file being renamed — so an
/// existence check alone refuses the very repair `--fix` exists to make, on the platform
/// `lore schedule --format launchd` is written for. `canonicalize` resolves both to the
/// name the filesystem actually holds, so a case-only rename compares equal while a genuine
/// collision (two distinct files, as on a case-sensitive volume) still does not.
fn is_same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        // Unresolvable means we cannot claim they are the same; the caller then refuses,
        // which is the safe direction for a destructive rename.
        _ => false,
    }
}

pub fn apply(renames: &[Rename], pages: &[ScannedPage], root: &Path) -> Result<usize, GraphError> {
    if renames.is_empty() {
        return Ok(0);
    }

    for rename in renames {
        let target = root.join(&rename.new_path);
        if target.exists() && !is_same_file(&root.join(&rename.old_path), &target) {
            return Err(GraphError::Io(format!(
                "rename target already exists: {}",
                target.display()
            )));
        }
    }

    // Two maps, two matching semantics. A page's OWN location is exact — the rename
    // map is keyed by literal path so a just-renamed page is read at its new file.
    // Link destinations are matched the way the graph matches them — by page id
    // (`path_slug`) — so a case/punctuation variant spelling of a renamed file's path
    // is repointed too: exactly the citations scan resolves to it.
    let renamed_paths: HashMap<&Path, &Path> = renames
        .iter()
        .map(|r| (r.old_path.as_path(), r.new_path.as_path()))
        .collect();
    let renamed_ids: HashMap<String, &Path> = renames
        .iter()
        .map(|r| (scan::path_slug(&r.old_path), r.new_path.as_path()))
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
        let rel_path = renamed_paths
            .get(page.path.as_path())
            .copied()
            .unwrap_or(&page.path);
        let abs_path = root.join(rel_path);

        let content = std::fs::read_to_string(&abs_path)
            .map_err(|e| GraphError::Io(format!("failed to read {}: {e}", abs_path.display())))?;

        let mut updated = repoint_renamed_links(&content, rel_path, &renamed_ids);

        // A renamed page records its own address in `id`, so leaving that behind would
        // publish a page whose frontmatter names a file that no longer exists. The stem is
        // the identity every consumer resolves by, which is exactly why the stale copy is
        // worth removing rather than tolerating.
        //
        // Both `None`s here are the absence of anything to rewrite, not a dropped failure. A
        // page's graph id comes from its PATH (`scan::path_slug`), so a page carrying no
        // frontmatter at all is scanned and renamed like any other — and it has no `id` key to
        // go stale, which is the only case `set_frontmatter_field` declines. Making either an
        // error would fail `--fix` on a page it has nothing to do to.
        if renamed_paths.contains_key(page.path.as_path())
            && let Some(new_slug) = rel_path.file_stem().and_then(|s| s.to_str())
            && let Some(with_id) = lk_vault::set_frontmatter_field(
                &updated,
                "id",
                &serde_json::to_string(new_slug).expect("a str always serializes"),
            )
        {
            updated = with_id;
        }

        if updated != content {
            lk_core::fs::write_atomic(&abs_path, updated.as_bytes(), None).map_err(|e| {
                GraphError::Io(format!("failed to write {}: {e}", abs_path.display()))
            })?;
        }
    }

    Ok(renames.len())
}

/// Repoint every link whose destination resolves to a renamed page at that page's new
/// path. Destinations are matched by page id (the graph's resolution), gated to `.md`
/// like extraction. Rewrites OUTSIDE code only: a link shown inside a code fence/span
/// is a code example, not a graph edge (`extract_dests` ignores it), so rewriting it
/// would corrupt the document.
fn repoint_renamed_links(
    content: &str,
    page_path: &Path,
    path_map: &HashMap<String, &Path>,
) -> String {
    link::rewrite_links_outside_code(content, |text, raw_dest| {
        let (dest, anchor) = link::split_raw_dest(raw_dest);
        let decoded = link::decode_dest(dest);
        if !Path::new(&decoded).extension().is_some_and(|e| e == "md") {
            return None;
        }
        let resolved = link::resolve_dest(page_path, &decoded)?;
        let new_path = path_map.get(&scan::path_slug(&resolved))?;
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
    use crate::scan::Link;

    fn build_page(path: &str, outgoing: &[&str]) -> ScannedPage {
        let rel = PathBuf::from(path);
        ScannedPage {
            id: scan::path_slug(&rel),
            path: rel,
            title: "test".to_owned(),
            outgoing: outgoing
                .iter()
                .map(|s| Link::to(&format!("{s}.md")))
                .collect(),
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
        let path_map: HashMap<String, &Path> = [(scan::path_slug(&old), new.as_path())].into();
        let content = "See [A](Concept_A.md) and [A](Concept_A.md#part) and [B](other.md) here.";
        let updated = repoint_renamed_links(content, Path::new("wiki/linker.md"), &path_map);
        assert_eq!(
            updated,
            "See [A](concept-a.md) and [A](concept-a.md#part) and [B](other.md) here."
        );
    }

    #[test]
    fn titled_destination_is_still_repointed() {
        // Same rule as merge: a CommonMark title must not hide a link from the
        // rename repoint — extraction resolves it to the renamed page, so the
        // rewrite must follow.
        let old_p = PathBuf::from("wiki/Concept_A.md");
        let new_p = PathBuf::from("wiki/concept-a.md");
        let path_map: HashMap<String, &Path> = [(scan::path_slug(&old_p), new_p.as_path())].into();
        let content = "See [A](Concept_A.md \"tip\") here.";
        let updated = repoint_renamed_links(content, Path::new("wiki/linker.md"), &path_map);
        assert_eq!(updated, "See [A](concept-a.md) here.");
    }

    #[test]
    fn case_variant_destination_is_repointed_like_the_graph_resolves_it() {
        // scan matches links to pages by id (path_slug), so `[A](concept_a.md)` is a
        // citation of `wiki/Concept_A.md`. The rename repoint must match at the same
        // level — a literal-path comparison would miss it and leave the link stale.
        let old = PathBuf::from("wiki/Concept_A.md");
        let new = PathBuf::from("wiki/concept-a.md");
        let path_map: HashMap<String, &Path> = [(scan::path_slug(&old), new.as_path())].into();
        let content = "See [A](concept_a.md) and [A](CONCEPT-A.md) here.";
        let updated = repoint_renamed_links(content, Path::new("wiki/linker.md"), &path_map);
        assert_eq!(updated, "See [A](concept-a.md) and [A](concept-a.md) here.");
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
        let path_map: HashMap<String, &Path> = [(scan::path_slug(&old), new.as_path())].into();
        let content = "Prose [A](Concept%20A.md).\n```\nexample [A](Concept%20A.md)\n```\nInline `[A](Concept%20A.md)`.\n";
        let updated = repoint_renamed_links(content, Path::new("wiki/linker.md"), &path_map);
        assert!(updated.contains("Prose [A](concept-a.md)."));
        assert!(updated.contains("example [A](Concept%20A.md)"));
        assert!(updated.contains("Inline `[A](Concept%20A.md)`."));
    }

    #[test]
    fn a_case_only_rename_is_not_mistaken_for_a_collision() {
        // The common real rename differs ONLY in case (`allUsers-…` → `allusers-…`), and on
        // a case-insensitive filesystem — macOS and Windows by default — the target then
        // already "exists" as the file being renamed. An existence check alone refuses the
        // repair `--fix` exists to make. Every other test here renames a name that differs
        // by punctuation too, so none of them ever reached this path.
        let tmp = tempfile::tempdir().unwrap();
        let wiki = tmp.path().join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::write(wiki.join("allUsers.md"), "# allUsers\n").unwrap();
        std::fs::write(wiki.join("linker.md"), "See [x](allUsers.md).\n").unwrap();

        let pages = vec![
            build_page("wiki/allUsers.md", &[]),
            build_page("wiki/linker.md", &["wiki/allusers"]),
        ];
        let renames = scan(&pages);
        assert_eq!(renames.len(), 1, "{renames:?}");
        assert_eq!(apply(&renames, &pages, tmp.path()).unwrap(), 1);

        let linker = std::fs::read_to_string(wiki.join("linker.md")).unwrap();
        assert!(linker.contains("[x](allusers.md)"), "{linker}");
    }

    /// The other side of the case-only rename above. `is_same_file` exists to tell a
    /// case-only rename (same file, must proceed) from a genuine collision (two distinct
    /// pages, must refuse) — and only the first half was covered, so an `is_same_file` that
    /// answered "same" to everything read as correct while `--fix` silently overwrote a real
    /// page with another. A rename is destructive and there is no undo.
    #[test]
    fn a_rename_onto_a_different_page_refuses_rather_than_overwriting_it() {
        let tmp = tempfile::tempdir().unwrap();
        let wiki = tmp.path().join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::write(wiki.join("Bad_Name.md"), "# the one being renamed\n").unwrap();
        std::fs::write(wiki.join("bad-name.md"), "# a DIFFERENT page\n").unwrap();

        let pages = vec![
            build_page("wiki/Bad_Name.md", &[]),
            build_page("wiki/bad-name.md", &[]),
        ];
        let renames = scan(&pages);
        assert_eq!(renames.len(), 1, "{renames:?}");

        let err = apply(&renames, &pages, tmp.path()).unwrap_err();
        assert!(
            format!("{err}").contains("already exists"),
            "collision must be refused, got: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(wiki.join("bad-name.md")).unwrap(),
            "# a DIFFERENT page\n",
            "the occupant must survive a refused rename"
        );
    }

    #[test]
    fn a_renamed_page_stops_recording_its_old_address() {
        // `id` is the page's own record of where it lives. A rename that leaves it behind
        // publishes frontmatter naming a file that no longer exists.
        let tmp = tempfile::tempdir().unwrap();
        let wiki = tmp.path().join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::write(
            wiki.join("Bad_Name.md"),
            "---\nid: Bad_Name\ntype: concept\n---\n\n# Bad\n",
        )
        .unwrap();

        let pages = vec![build_page("wiki/Bad_Name.md", &[])];
        let renames = scan(&pages);
        assert_eq!(apply(&renames, &pages, tmp.path()).unwrap(), 1);

        let moved = std::fs::read_to_string(wiki.join("bad-name.md")).unwrap();
        assert!(moved.contains("id: \"bad-name\""), "{moved}");
        assert!(!moved.contains("Bad_Name"), "{moved}");
    }

    #[test]
    fn a_real_collision_is_still_refused() {
        // Two DISTINCT files whose ids differ only in case can coexist on a case-sensitive
        // volume; renaming one onto the other would destroy it, so the guard must still
        // fire. Skipped where the filesystem cannot represent the situation.
        let tmp = tempfile::tempdir().unwrap();
        let wiki = tmp.path().join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::write(wiki.join("Dup.md"), "# upper\n").unwrap();
        if std::fs::write(wiki.join("dup.md"), "# lower\n").is_err()
            || std::fs::read_to_string(wiki.join("Dup.md")).unwrap() != "# upper\n"
        {
            return; // case-insensitive volume: the two names are one file
        }
        let pages = vec![build_page("wiki/Dup.md", &[])];
        let renames = scan(&pages);
        assert!(apply(&renames, &pages, tmp.path()).is_err());
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
