//! Concept merge: fold a duplicate concept page into a canonical one.
//!
//! `find_near_duplicate_concepts` (in `concepts`) only *reports* variant-spelling
//! duplicates (`vector-db` ~ `vector-database`); this module is the execution
//! counterpart that resolves one. It rewrites every wikilink that targets the
//! `from` concept so it points at `into`, then deletes the now-orphaned `from`
//! page. The `## Sources` body and `source_count` are deliberately NOT touched
//! here — they are owned by `backlinks-sync`, which re-derives them exactly from
//! the post-merge link graph on its next run.
//!
//! The merge is link-rewiring only: it never fabricates or copies prose. If the
//! `from` page carries body content a human authored (a synthesis paragraph,
//! curated notes), the merge ABORTS before touching anything unless `--force` is
//! given — so authored prose is never silently destroyed. The human salvages it
//! into `into`, then re-runs with `--force`.

use std::path::{Path, PathBuf};

use lk_core::concept::slugify;
use lk_core::i18n::Locale;
use lk_core::wikilink::{self, WIKILINK_RE};
use serde::Serialize;

use crate::GraphError;
use crate::scan::{ScannedPage, path_slug};

/// One page whose wikilinks were (or would be) rewritten by a merge.
#[derive(Debug, Clone, Serialize)]
pub struct RewrittenPage {
    /// Vault-relative path of the page.
    pub path: PathBuf,
    /// Number of `[[from]]` links rewritten to `[[into]]` on it.
    pub links: usize,
}

/// Outcome of a concept merge.
#[derive(Debug, Clone, Serialize)]
pub struct MergeResult {
    pub from_slug: String,
    pub into_slug: String,
    /// Pages whose links were (or, under `dry_run`, would be) rewritten.
    pub rewritten: Vec<RewrittenPage>,
    /// Whether the `from` page was (or would be) deleted.
    pub deleted: bool,
    /// True when the deleted `from` page had human-authored body content beyond the
    /// template scaffold — a salvage warning, not an error.
    pub from_authored: bool,
    pub dry_run: bool,
}

/// Fold the `from` concept into `into`: rewrite every wikilink targeting `from` to
/// `into` across the whole vault, then delete `from`'s page.
///
/// Errors when `from` and `into` are the same slug, when either concept page does
/// not exist, or on any I/O failure. `dry_run` reports the plan without writing or
/// deleting anything.
///
/// **Authored-body guard**: a merge discards the `from` page's body (only links are
/// rewired — prose is never copied to `into`). If `from` carries human-authored body
/// content and `force` is false, the merge aborts BEFORE mutating anything, so the
/// human can salvage that prose into `into` first. Re-run with `force` once salvaged.
/// `dry_run` reports the plan (including the authored-body flag) without this gate
/// firing, so a preview always works.
pub fn merge_concepts(
    pages: &[ScannedPage],
    vault_root: &Path,
    wiki_dir: &str,
    from_slug: &str,
    into_slug: &str,
    dry_run: bool,
    force: bool,
) -> Result<MergeResult, GraphError> {
    let from = slugify(from_slug)
        .ok_or_else(|| GraphError::Io(format!("invalid from slug: '{from_slug}'")))?;
    let into = slugify(into_slug)
        .ok_or_else(|| GraphError::Io(format!("invalid into slug: '{into_slug}'")))?;
    if from == into {
        return Err(GraphError::Io(
            "from and into are the same concept; nothing to merge".into(),
        ));
    }

    let concepts_dir = Path::new(wiki_dir).join(lk_core::vault_path::CONCEPTS_SUBDIR);
    let from_rel = concepts_dir.join(format!("{from}.md"));
    let into_rel = concepts_dir.join(format!("{into}.md"));
    if !vault_root.join(&from_rel).is_file() {
        return Err(GraphError::Io(format!(
            "from concept page not found: {}",
            from_rel.display()
        )));
    }
    if !vault_root.join(&into_rel).is_file() {
        return Err(GraphError::Io(format!(
            "into concept page not found: {}",
            into_rel.display()
        )));
    }

    // Gate BEFORE any mutation: a merge would discard `from`'s authored prose (links
    // are rewired, body is not). Refuse unless forced, so nothing is silently lost.
    let from_authored = concept_has_authored_body(&vault_root.join(&from_rel))?;
    if from_authored && !force && !dry_run {
        return Err(GraphError::Io(format!(
            "'{from}' has authored body content that a merge would discard — salvage it \
             into '{into}', then re-run with --force (or --dry-run to preview)"
        )));
    }

    // The two link forms that can target the from concept: a bare `[[from]]`
    // (resolved by filename) and a path link to the page id `[[<wiki>/concepts/from]]`.
    let from_path_id = path_slug(&from_rel);
    let into_path_id = path_slug(&into_rel);

    let mut rewritten = Vec::new();
    for page in pages {
        // The from page itself is about to be deleted — don't rewrite it.
        if page.path == from_rel {
            continue;
        }
        let abs = vault_root.join(&page.path);
        let content = std::fs::read_to_string(&abs)
            .map_err(|e| GraphError::Io(format!("read {}: {e}", abs.display())))?;
        let (updated, count) = rewrite_links(&content, &from, &from_path_id, &into, &into_path_id);
        if count > 0 {
            if !dry_run && updated != content {
                std::fs::write(&abs, &updated)
                    .map_err(|e| GraphError::Io(format!("write {}: {e}", abs.display())))?;
            }
            rewritten.push(RewrittenPage {
                path: page.path.clone(),
                links: count,
            });
        }
    }
    rewritten.sort_by(|a, b| a.path.cmp(&b.path));

    if !dry_run {
        std::fs::remove_file(vault_root.join(&from_rel))
            .map_err(|e| GraphError::Io(format!("delete {}: {e}", from_rel.display())))?;
    }

    Ok(MergeResult {
        from_slug: from,
        into_slug: into,
        rewritten,
        deleted: true,
        from_authored,
        dry_run,
    })
}

/// Rewrite every wikilink targeting the from concept (bare slug or path id) to the
/// into concept, preserving anchors and aliases. Returns the new content and the
/// number of links rewritten.
fn rewrite_links(
    content: &str,
    from_slug: &str,
    from_path_id: &str,
    into_slug: &str,
    into_path_id: &str,
) -> (String, usize) {
    let mut count = 0;
    let out = WIKILINK_RE
        .replace_all(content, |caps: &regex::Captures| {
            let full = caps.get(0).unwrap().as_str();
            // Decompose the FULL inner text (between `[[` and `]]`) into
            // page / anchor / alias so each is preserved exactly once when the page is
            // rewritten. `caps[1]` excludes the alias by regex design, so the full match
            // is sliced instead — `[[` and `]]` are ASCII, so byte slicing is safe.
            let (page_raw, anchor, alias) =
                wikilink::split_wikilink_parts(&full[2..full.len() - 2]);
            let trimmed = page_raw.trim();

            // Does this link target the from concept?
            let is_bare = slugify(trimmed).as_deref() == Some(from_slug) && !trimmed.contains('/');
            let is_path = trimmed == from_path_id;
            if !is_bare && !is_path {
                return full.to_owned();
            }
            count += 1;
            // Bare links rewrite to the bare into slug; path links to the into path id,
            // so each citation keeps its original form.
            let new_target = if is_path { into_path_id } else { into_slug };
            format!("[[{new_target}{anchor}{alias}]]")
        })
        .into_owned();
    (out, count)
}

/// True if a concept page has body content a human likely authored — any non-blank
/// line outside the frontmatter that is neither a heading (`#…`) nor a machine-owned
/// `## Sources` list item. Decides whether deletion needs `--force`.
///
/// Section-aware: a `- [[…]]` bullet is machine-owned ONLY under the Sources heading,
/// which `backlinks-sync` re-derives. The identical bullet under any other section —
/// notably `## Related`, whose links are human-curated via `lore-wiki audit` — is
/// authored knowledge the merge must never silently drop.
fn concept_has_authored_body(abs: &Path) -> Result<bool, GraphError> {
    let raw = std::fs::read_to_string(abs)
        .map_err(|e| GraphError::Io(format!("read {}: {e}", abs.display())))?;
    let body = lk_core::frontmatter::parse_page(&raw)
        .map(|p| p.body)
        .unwrap_or(raw);

    // Match the Sources heading under every locale — a page authored before a
    // `vault.locale` switch keeps its old-language heading (mirrors backlinks-sync).
    let source_headings: Vec<&str> = Locale::ALL
        .iter()
        .map(|l| l.strings().concept_sources)
        .collect();

    let mut in_sources = false;
    for line in body.lines() {
        // Heading detection mirrors backlinks-sync's EXACT column-0 `## <heading>` match
        // (`parse_existing_sources`): an indented or trailing-space variant is NOT the
        // section backlinks-sync would rewrite, so we must not treat its bullets as
        // machine-owned. Matching strictly here keeps the two paths in lockstep and errs
        // toward "authored" on any deviation.
        if let Some(rest) = line.strip_prefix("## ") {
            in_sources = source_headings.contains(&rest);
            continue;
        }
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if in_sources && t.starts_with("- [[") && t.ends_with("]]") {
            continue; // a Sources list item — machine-owned, re-derived by backlinks-sync
        }
        return Ok(true);
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(root: &Path, rel: &str, content: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    fn page(id: &str, rel: &str, outgoing: &[&str]) -> ScannedPage {
        ScannedPage {
            id: id.to_owned(),
            path: PathBuf::from(rel),
            title: id.to_owned(),
            outgoing: outgoing.iter().map(|s| (*s).to_string()).collect(),
            aliases: Vec::new(),
        }
    }

    #[test]
    fn rewrite_preserves_anchor_and_alias_without_duplication() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            root,
            "wiki/concepts/old.md",
            "---\nid: old\n---\n# Old\n## Sources\n",
        );
        write(
            root,
            "wiki/concepts/new.md",
            "---\nid: new\n---\n# New\n## Sources\n",
        );
        write(
            root,
            "daily/x/d.md",
            "- [[old]]\n- [[old#sec]]\n- [[old|Alias]]\n- [[old#sec|Alias]]\n",
        );
        let pages = vec![
            page("wiki/concepts/old", "wiki/concepts/old.md", &[]),
            page("wiki/concepts/new", "wiki/concepts/new.md", &[]),
            page("daily/x/d", "daily/x/d.md", &["old"]),
        ];
        merge_concepts(&pages, root, "wiki", "old", "new", false, false).unwrap();
        let daily = std::fs::read_to_string(root.join("daily/x/d.md")).unwrap();
        // Every form rewritten to `new`, with anchor and alias each kept exactly once
        // (the regression the `split_wikilink_parts` decomposition fixes).
        assert_eq!(
            daily.as_str(),
            "- [[new]]\n- [[new#sec]]\n- [[new|Alias]]\n- [[new#sec|Alias]]\n"
        );
    }

    #[test]
    fn rewrites_bare_links_and_deletes_from() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            root,
            "wiki/concepts/vector-db.md",
            "---\nid: vector-db\n---\n\n# vector-db\n",
        );
        write(
            root,
            "wiki/concepts/vector-database.md",
            "---\nid: vector-database\n---\n\n# vector-database\n",
        );
        write(
            root,
            "daily/x/2026-05-20.md",
            "See [[vector-db]] and [[vector-db#use|the db]].\n",
        );

        let pages = vec![
            page("wiki/concepts/vector-db", "wiki/concepts/vector-db.md", &[]),
            page(
                "wiki/concepts/vector-database",
                "wiki/concepts/vector-database.md",
                &[],
            ),
            page(
                "daily/x/2026-05-20",
                "daily/x/2026-05-20.md",
                &["vector-db"],
            ),
        ];

        let r = merge_concepts(
            &pages,
            root,
            "wiki",
            "vector-db",
            "vector-database",
            false,
            false,
        )
        .unwrap();
        assert_eq!(r.rewritten.len(), 1);
        assert_eq!(r.rewritten[0].links, 2);
        assert!(r.deleted);
        assert!(!root.join("wiki/concepts/vector-db.md").exists());

        let daily = std::fs::read_to_string(root.join("daily/x/2026-05-20.md")).unwrap();
        assert_eq!(
            daily,
            "See [[vector-database]] and [[vector-database#use|the db]].\n"
        );
    }

    #[test]
    fn dry_run_changes_nothing() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(root, "wiki/concepts/a.md", "---\nid: a\n---\n\n# a\n");
        write(root, "wiki/concepts/b.md", "---\nid: b\n---\n\n# b\n");
        write(root, "daily/x/d.md", "[[a]]\n");
        let pages = vec![
            page("wiki/concepts/a", "wiki/concepts/a.md", &[]),
            page("wiki/concepts/b", "wiki/concepts/b.md", &[]),
            page("daily/x/d", "daily/x/d.md", &["a"]),
        ];
        let before = std::fs::read_to_string(root.join("daily/x/d.md")).unwrap();
        let r = merge_concepts(&pages, root, "wiki", "a", "b", true, false).unwrap();
        assert!(r.dry_run);
        assert_eq!(r.rewritten.len(), 1);
        assert!(
            root.join("wiki/concepts/a.md").exists(),
            "dry-run must not delete"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("daily/x/d.md")).unwrap(),
            before
        );
    }

    #[test]
    fn rejects_same_slug_and_missing_pages() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(root, "wiki/concepts/a.md", "---\nid: a\n---\n\n# a\n");
        let pages = vec![page("wiki/concepts/a", "wiki/concepts/a.md", &[])];
        assert!(merge_concepts(&pages, root, "wiki", "a", "a", true, false).is_err());
        // into missing
        assert!(merge_concepts(&pages, root, "wiki", "a", "ghost", true, false).is_err());
        // from missing
        assert!(merge_concepts(&pages, root, "wiki", "ghost", "a", true, false).is_err());
    }

    #[test]
    fn flags_authored_body_for_salvage() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            root,
            "wiki/concepts/a.md",
            "---\nid: a\n---\n\n# a\n\n## 핵심\n\nA hand-written synthesis paragraph.\n\n## 출처\n\n- [[daily/x/d]]\n",
        );
        write(root, "wiki/concepts/b.md", "---\nid: b\n---\n\n# b\n");
        let pages = vec![
            page("wiki/concepts/a", "wiki/concepts/a.md", &[]),
            page("wiki/concepts/b", "wiki/concepts/b.md", &[]),
        ];
        let r = merge_concepts(&pages, root, "wiki", "a", "b", true, false).unwrap();
        assert!(
            r.from_authored,
            "authored prose must be flagged for salvage"
        );

        // A page with only scaffold + a source list is not flagged.
        write(
            root,
            "wiki/concepts/c.md",
            "---\nid: c\n---\n\n# c\n\n## 출처\n\n- [[daily/x/d]]\n",
        );
        let pages2 = vec![
            page("wiki/concepts/c", "wiki/concepts/c.md", &[]),
            page("wiki/concepts/b", "wiki/concepts/b.md", &[]),
        ];
        let r2 = merge_concepts(&pages2, root, "wiki", "c", "b", true, false).unwrap();
        assert!(
            !r2.from_authored,
            "scaffold + sources only must NOT be flagged"
        );
    }

    #[test]
    fn authored_body_aborts_without_force_and_proceeds_with_force() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        // `from` carries a hand-written synthesis paragraph a merge would discard.
        write(
            root,
            "wiki/concepts/a.md",
            "---\nid: a\n---\n\n# a\n\n## Synthesis\n\nHand-written prose worth keeping.\n",
        );
        write(root, "wiki/concepts/b.md", "---\nid: b\n---\n\n# b\n");
        let pages = vec![
            page("wiki/concepts/a", "wiki/concepts/a.md", &[]),
            page("wiki/concepts/b", "wiki/concepts/b.md", &[]),
        ];

        // No force, real run → abort BEFORE any mutation; `from` page still present.
        assert!(merge_concepts(&pages, root, "wiki", "a", "b", false, false).is_err());
        assert!(
            root.join("wiki/concepts/a.md").exists(),
            "abort must leave the from page intact for salvage"
        );

        // dry-run previews without the gate firing.
        let preview = merge_concepts(&pages, root, "wiki", "a", "b", true, false).unwrap();
        assert!(preview.from_authored);
        assert!(root.join("wiki/concepts/a.md").exists());

        // With force, the merge proceeds and deletes `from`.
        let forced = merge_concepts(&pages, root, "wiki", "a", "b", false, true).unwrap();
        assert!(forced.deleted);
        assert!(!root.join("wiki/concepts/a.md").exists());
    }

    #[test]
    fn related_only_body_is_authored_and_needs_force() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        // `from`'s only body is human-curated `## 관련` (Related) wikilinks, no synthesis
        // prose. Those links are confirmed via `lore-wiki audit` — authored knowledge,
        // NOT machine-derived like `## 출처` (Sources). The merge must refuse to silently
        // drop them without --force.
        write(
            root,
            "wiki/concepts/a.md",
            "---\nid: a\n---\n\n# a\n\n## 관련\n\n- [[other]]\n",
        );
        write(root, "wiki/concepts/b.md", "---\nid: b\n---\n\n# b\n");
        let pages = vec![
            page("wiki/concepts/a", "wiki/concepts/a.md", &[]),
            page("wiki/concepts/b", "wiki/concepts/b.md", &[]),
        ];
        // Without force: curated Related links make it authored → abort, page intact.
        assert!(merge_concepts(&pages, root, "wiki", "a", "b", false, false).is_err());
        assert!(
            root.join("wiki/concepts/a.md").exists(),
            "Related-only page must not be deleted without --force"
        );
        // With force: proceeds, flags the body, deletes the page.
        let forced = merge_concepts(&pages, root, "wiki", "a", "b", false, true).unwrap();
        assert!(forced.from_authored);
        assert!(forced.deleted);
    }

    #[test]
    fn indented_sources_heading_is_treated_as_authored() {
        // An indented `  ## 출처` is NOT the column-0 section backlinks-sync rewrites, so
        // its bullets are not machine-owned. The guard must match backlinks-sync's exact
        // detection and treat such a page as authored (require --force) rather than
        // silently deleting bullets backlinks-sync would never touch.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            root,
            "wiki/concepts/a.md",
            "---\nid: a\n---\n\n# a\n\n  ## 출처\n\n- [[daily/x/d]]\n",
        );
        write(root, "wiki/concepts/b.md", "---\nid: b\n---\n\n# b\n");
        let pages = vec![
            page("wiki/concepts/a", "wiki/concepts/a.md", &[]),
            page("wiki/concepts/b", "wiki/concepts/b.md", &[]),
        ];
        assert!(
            merge_concepts(&pages, root, "wiki", "a", "b", true, false)
                .unwrap()
                .from_authored,
            "indented Sources heading must not be mistaken for the machine-owned section"
        );
    }
}
