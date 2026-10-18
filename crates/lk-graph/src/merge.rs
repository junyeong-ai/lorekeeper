//! Concept merge: fold a duplicate concept page into a canonical one.
//!
//! `concept_lint::find_duplicate_concepts` only *reports* pages that answer to one
//! name (`doc-hub` ~ `docs-hub`); this module is the execution counterpart a human
//! triggers to resolve one. It repoints every link that targets the `from`
//! concept at `into`, folds `from`'s names (title + aliases) into `into`'s `aliases`
//! so the synonym stays in the concept registry the LLM dedups against, then deletes
//! the now-orphaned `from` page. The `## Sources` body and `source_count` are
//! deliberately NOT touched here — they are owned by `backlinks-sync`, which
//! re-derives them exactly from the post-merge link graph on its next run.
//!
//! The merge is link-rewiring only: it never fabricates or copies prose. If the
//! `from` page carries body content a human authored (a synthesis paragraph,
//! curated notes), the merge ABORTS before touching anything unless `--force` is
//! given — so authored prose is never silently destroyed. The human salvages it
//! into `into`, then re-runs with `--force`.

use std::path::{Path, PathBuf};

use lk_core::concept::slugify;
use lk_core::link;
use serde::Serialize;

use crate::GraphError;
use crate::scan::ScannedPage;

/// One page whose links were (or would be) rewritten by a merge.
#[derive(Debug, Clone, Serialize)]
pub struct RewrittenPage {
    /// Vault-relative path of the page.
    pub path: PathBuf,
    /// Number of links repointed from `from` to `into` on it.
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

/// Fold the `from` concept into `into`: repoint every link targeting `from` at
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
    if dry_run {
        return plan_merge(
            pages, vault_root, wiki_dir, from_slug, into_slug, true, force,
        )
        .map(|(result, _)| result);
    }
    let session = lk_vault::MutationSession::acquire(vault_root)
        .map_err(|e| GraphError::Io(e.to_string()))?;
    merge_in_session(pages, &session, wiki_dir, from_slug, into_slug, force)
}

pub fn merge_in_session(
    pages: &[ScannedPage],
    session: &lk_vault::MutationSession,
    wiki_dir: &str,
    from_slug: &str,
    into_slug: &str,
    force: bool,
) -> Result<MergeResult, GraphError> {
    let (result, plan) = plan_merge(
        pages,
        session.root(),
        wiki_dir,
        from_slug,
        into_slug,
        false,
        force,
    )?;
    session
        .apply(plan)
        .map_err(|e| GraphError::Io(e.to_string()))?;
    Ok(result)
}

fn plan_merge(
    pages: &[ScannedPage],
    vault_root: &Path,
    wiki_dir: &str,
    from_slug: &str,
    into_slug: &str,
    dry_run: bool,
    force: bool,
) -> Result<(MergeResult, lk_vault::MutationPlan), GraphError> {
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

    // Compute the merged alias list BEFORE mutating any page, so a malformed `from`/`into`
    // aborts the merge before a single link is repointed (validate-before-mutate). The list
    // is derived from names only, so it stays valid even though the rewrite loop below may
    // rewrite `into`'s own body links.
    //
    // Computing it is not enough: the WRITE can fail on its own, and `into` having nowhere to
    // record aliases is exactly the case — a page hand-created in Obsidian with no frontmatter
    // block. Failing at the write leaves every citation repointed to `into`, `from` still on
    // disk and now cited by nothing, and no alias recorded; every re-run fails identically, so
    // the half-merge is permanent. Refusing here costs nothing and names the repair.
    let absorbed_aliases = if dry_run {
        None
    } else {
        let aliases = compute_absorbed_aliases(vault_root, &from_rel, &into_rel)?;
        let into_raw = std::fs::read_to_string(vault_root.join(&into_rel))
            .map_err(|e| GraphError::Io(format!("read {}: {e}", into_rel.display())))?;
        if lk_vault::set_frontmatter_field(&into_raw, "aliases", "[]").is_none() {
            return Err(GraphError::Io(format!(
                "'{into}' has no frontmatter block to record the merged aliases in — give it \
                 one, then re-run"
            )));
        }
        Some(aliases)
    };

    if !dry_run {
        lk_vault::KnowledgeReview::ensure_evidence_paths_preserved(
            vault_root,
            &pages
                .iter()
                .map(|page| page.path.clone())
                .collect::<Vec<_>>(),
            std::slice::from_ref(&from_rel),
        )
        .map_err(|e| GraphError::Io(e.to_string()))?;
    }

    let mut plan = lk_vault::MutationPlan::default();
    let mut into_updated = None;
    let mut rewritten = Vec::new();
    for page in pages {
        if page.path == from_rel {
            continue;
        }
        let mut cites_from = false;
        for link in &page.outgoing {
            cites_from |=
                lk_vault::same_existing_file(vault_root, Path::new(&link.dest), &from_rel)
                    .map_err(|error| GraphError::Io(error.to_string()))?;
        }
        if !cites_from {
            continue;
        }
        let abs = vault_root.join(&page.path);
        let content = std::fs::read_to_string(&abs)
            .map_err(|e| GraphError::Io(format!("read {}: {e}", abs.display())))?;
        let (mut updated, count) =
            rewrite_links(&content, &page.path, &from_rel, &into_rel, vault_root)?;
        if count > 0 && page.rss_evidence {
            lk_vault::rss_concept_citations(vault_root, &page.path, &content)
                .map_err(|e| GraphError::Io(e.to_string()))?;
            updated = lk_vault::repoint_rss_evidence(
                vault_root,
                &updated,
                &[(from_rel.clone(), into_rel.clone())],
            )
            .map_err(|e| GraphError::Io(e.to_string()))?;
            lk_vault::guard_rss_evidence(&mut plan, vault_root, &page.path, &updated)
                .map_err(|e| GraphError::Io(e.to_string()))?;
        }
        if count > 0 {
            if !dry_run && updated != content {
                plan.edit(&page.path, content.clone(), updated.clone());
                if page.path == into_rel {
                    into_updated = Some(updated);
                }
            }
            rewritten.push(RewrittenPage {
                path: page.path.clone(),
                links: count,
            });
        }
    }
    rewritten.sort_by(|a, b| a.path.cmp(&b.path));

    if !dry_run {
        if let Some(aliases) = &absorbed_aliases {
            let before = match into_updated {
                Some(content) => content,
                None => std::fs::read_to_string(vault_root.join(&into_rel))
                    .map_err(|e| GraphError::Io(e.to_string()))?,
            };
            let value =
                serde_json::to_string(aliases).map_err(|e| GraphError::Io(e.to_string()))?;
            let updated = lk_vault::set_frontmatter_field(&before, "aliases", &value)
                .ok_or_else(|| GraphError::Io("merge target has no frontmatter".into()))?;
            plan.edit(&into_rel, before, updated);
        }
        let original = std::fs::read_to_string(vault_root.join(&from_rel))
            .map_err(|e| GraphError::Io(e.to_string()))?;
        plan.remove(&from_rel, original);
    }

    Ok((
        MergeResult {
            from_slug: from,
            into_slug: into,
            rewritten,
            deleted: true,
            from_authored,
            dry_run,
        },
        plan,
    ))
}

/// Compute the `into` page's alias list after absorbing `from`'s identity (its title and
/// any declared aliases), so the merged names stay in the concept registry
/// (`lore wiki concepts`) the LLM dedups against — a later extraction of the old name
/// converges on `into` instead of re-minting a page. The `into` title stays first
/// (Obsidian convention) and duplicates are dropped. This is the fallible half (it reads
/// and parses both pages), kept separate so the caller runs it BEFORE any mutation — a
/// malformed page aborts the merge before a single link is repointed.
fn compute_absorbed_aliases(
    vault_root: &Path,
    from_rel: &Path,
    into_rel: &Path,
) -> Result<Vec<String>, GraphError> {
    fn aliases_of(page: &lk_core::frontmatter::VaultPage) -> Vec<String> {
        page.frontmatter
            .get("aliases")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
    }
    fn title_of(page: &lk_core::frontmatter::VaultPage) -> Option<String> {
        page.frontmatter
            .get("title")
            .and_then(|v| v.as_str())
            .map(String::from)
    }

    let from_raw = std::fs::read_to_string(vault_root.join(from_rel))
        .map_err(|e| GraphError::Io(format!("read {}: {e}", from_rel.display())))?;
    let from = lk_core::frontmatter::parse_page(&from_raw)
        .map_err(|e| GraphError::Io(format!("parse {}: {e}", from_rel.display())))?;

    let into_raw = std::fs::read_to_string(vault_root.join(into_rel))
        .map_err(|e| GraphError::Io(format!("read {}: {e}", into_rel.display())))?;
    let into = lk_core::frontmatter::parse_page(&into_raw)
        .map_err(|e| GraphError::Io(format!("parse {}: {e}", into_rel.display())))?;

    // Names to absorb: `from`'s title (a surface form of the same concept — title and slug
    // slugify identically) plus every alias `from` itself carried.
    let mut absorb = Vec::new();
    absorb.extend(title_of(&from));
    absorb.extend(aliases_of(&from));

    // Rebuild the alias list deterministically: the `into` title first, then its existing
    // aliases, then the absorbed `from` names — all deduped, so a title that sat mid-list or
    // a pre-existing duplicate is normalized too.
    let mut aliases: Vec<String> = Vec::new();
    for name in title_of(&into)
        .into_iter()
        .chain(aliases_of(&into))
        .chain(absorb)
    {
        if !aliases.contains(&name) {
            aliases.push(name);
        }
    }
    Ok(aliases)
}

/// Repoint every link targeting the from concept at the into concept, preserving the
/// display text and any heading anchor. Returns the new content and the number of
/// links rewritten.
fn rewrite_links(
    content: &str,
    page_path: &Path,
    from_rel: &Path,
    into_rel: &Path,
    root: &Path,
) -> Result<(String, usize), GraphError> {
    let mut count = 0;
    let mut failure = None;
    // Rewrite OUTSIDE code only (shared helper): a from-link shown inside a code
    // fence/span is a code example, not a graph edge — `extract_dests`/`broken` ignore
    // it, so it never dangles after the from page is deleted, and rewriting it would
    // corrupt the doc.
    let out = link::rewrite_links_outside_code(content, |text, raw_dest| {
        // Does this link target the from concept? Resolve the destination the SAME way
        // the graph does — `.md` destinations only, resolved against this page's
        // location, then checked against the actual filesystem — so exactly the spellings scan
        // counts as citations (`../concepts/from.md`, `./from.md`, the OKF absolute
        // form, actual filesystem aliases) are rewritten, no more and no fewer.
        let (dest, anchor) = link::split_raw_dest(raw_dest);
        let decoded = link::decode_dest(dest);
        if !Path::new(&decoded).extension().is_some_and(|e| e == "md") {
            return None;
        }
        let resolved = link::resolve_dest(page_path, &decoded)?;
        match lk_vault::same_existing_file(root, &resolved, from_rel) {
            Ok(true) => {}
            Ok(false) => return None,
            Err(error) => {
                failure = Some(GraphError::Io(error.to_string()));
                return None;
            }
        }
        count += 1;
        // `text` arrives exactly as written (escapes included) — reassemble verbatim
        // rather than through `md_link`, which would escape it a second time.
        let new_dest = format!("{}{anchor}", link::relative_dest(page_path, into_rel));
        Some(format!("[{text}]({new_dest})"))
    });
    if let Some(error) = failure {
        return Err(error);
    }
    Ok((out, count))
}

/// True if a concept page has body content a human likely authored — any non-blank
/// line outside the frontmatter that is neither a heading (`#…`) nor a machine-owned
/// `## Sources` list item. Decides whether deletion needs `--force`.
///
/// Section-aware: a `- [title](dest)` bullet is machine-owned ONLY under the Sources
/// heading, which `backlinks-sync` re-derives. The identical bullet under any other
/// section — notably `## Related`, whose links are human-curated via `lore-wiki
/// audit` — is authored knowledge the merge must never silently drop.
fn concept_has_authored_body(abs: &Path) -> Result<bool, GraphError> {
    let raw = std::fs::read_to_string(abs)
        .map_err(|e| GraphError::Io(format!("read {}: {e}", abs.display())))?;
    let body = lk_core::frontmatter::parse_page(&raw)
        .map(|p| p.body)
        .unwrap_or(raw);

    let source_headings: Vec<&str> = lk_vault::section_headings(|s| s.concept_sources).collect();

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
        if in_sources && t.starts_with("- [") && t.ends_with(')') && t.contains("](") {
            continue; // a Sources list item — machine-owned, re-derived by backlinks-sync
        }
        return Ok(true);
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::Link;
    use tempfile::TempDir;

    fn write(root: &Path, rel: &str, content: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    fn page(id: &str, rel: &str, outgoing: &[&str]) -> ScannedPage {
        ScannedPage {
            rss_evidence: false,
            revision: Some("revision".into()),
            id: id.to_owned(),
            path: PathBuf::from(rel),
            title: id.to_owned(),
            format: None,
            outgoing: outgoing
                .iter()
                .map(|s| Link::to(&format!("{s}.md")))
                .collect(),
        }
    }

    #[test]
    fn merge_does_not_retarget_a_distinct_punctuation_address() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        for name in ["a-b", "a_b", "target"] {
            write(
                root,
                &format!("wiki/concepts/{name}.md"),
                &format!("---\ntitle: {name}\ntype: concept\n---\n# {name}\n"),
            );
        }
        let original = "[distinct](../../wiki/concepts/a_b.md)\n";
        write(root, "daily/test/day.md", original);
        let pages = crate::scan::scan_vault(root, false).unwrap();
        let result = merge_concepts(&pages, root, "wiki", "a-b", "target", false, false).unwrap();
        assert!(result.rewritten.is_empty());
        assert_eq!(
            std::fs::read_to_string(root.join("daily/test/day.md")).unwrap(),
            original
        );
        assert!(root.join("wiki/concepts/a_b.md").is_file());
    }

    #[test]
    fn historical_review_evidence_blocks_mutation_before_any_write() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("wiki/concepts")).unwrap();
        let old = "---\ntype: concept\ntitle: Old\n---\nOld prose.\n";
        let into = "---\ntype: concept\ntitle: Into\n---\n";
        std::fs::write(root.join("wiki/concepts/old.md"), old).unwrap();
        std::fs::write(root.join("wiki/concepts/into.md"), into).unwrap();
        let review = "---\ntype: exploration\nreview: {status: open, evidence: [{path: wiki/concepts/old.md, revision: historical}]}\n---\n[Old](wiki/concepts/old.md)\n";
        std::fs::write(root.join("review.md"), review).unwrap();
        let pages = crate::scan::scan_vault(root, false).unwrap();
        assert!(merge_concepts(&pages, root, "wiki", "old", "into", false, true).is_err());
        assert_eq!(
            std::fs::read_to_string(root.join("wiki/concepts/old.md")).unwrap(),
            old
        );
        assert_eq!(
            std::fs::read_to_string(root.join("wiki/concepts/into.md")).unwrap(),
            into
        );
        assert_eq!(
            std::fs::read_to_string(root.join("review.md")).unwrap(),
            review
        );
        assert!(!root.join("wiki/concepts/new.md").exists());
    }

    #[test]
    fn a_destination_that_cannot_record_aliases_is_refused_before_anything_moves() {
        // The alias write is the last step; failing there leaves every citation repointed to
        // `into`, `from` still on disk and cited by nothing, and no alias recorded — and every
        // re-run fails at the same place, so the half-merge is permanent.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("wiki/concepts")).unwrap();
        std::fs::create_dir_all(root.join("daily/x")).unwrap();
        std::fs::write(
            root.join("wiki/concepts/from.md"),
            "---\nid: from\ntitle: \"From\"\n---\n\n## Sources\n",
        )
        .unwrap();
        // No frontmatter block at all — a page hand-created in an editor.
        std::fs::write(root.join("wiki/concepts/into.md"), "# Into\n\n## Sources\n").unwrap();
        let daily = "---\nid: d\ntitle: \"D\"\n---\n\n- [From](../../wiki/concepts/from.md)\n";
        std::fs::write(root.join("daily/x/d.md"), daily).unwrap();

        let pages = vec![
            page("wiki/concepts/from", "wiki/concepts/from.md", &[]),
            page("wiki/concepts/into", "wiki/concepts/into.md", &[]),
            page("daily/x/d", "daily/x/d.md", &["wiki/concepts/from"]),
        ];
        let result = merge_concepts(&pages, root, "wiki", "from", "into", false, false);

        assert!(result.is_err(), "must refuse");
        assert_eq!(
            std::fs::read_to_string(root.join("daily/x/d.md")).unwrap(),
            daily,
            "no citation may be repointed by a merge that cannot finish"
        );
        assert!(root.join("wiki/concepts/from.md").exists());
    }

    #[test]
    fn rewrite_preserves_text_and_anchor_without_duplication() {
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
            "- [Old](../../wiki/concepts/old.md)\n- [Old](../../wiki/concepts/old.md#sec)\n- [Alias](../../wiki/concepts/old.md)\n",
        );
        let pages = vec![
            page("wiki/concepts/old", "wiki/concepts/old.md", &[]),
            page("wiki/concepts/new", "wiki/concepts/new.md", &[]),
            page("daily/x/d", "daily/x/d.md", &["wiki/concepts/old"]),
        ];
        merge_concepts(&pages, root, "wiki", "old", "new", false, false).unwrap();
        let daily = std::fs::read_to_string(root.join("daily/x/d.md")).unwrap();
        // Every citation repointed to `new`, display text and anchor kept verbatim.
        assert_eq!(
            daily.as_str(),
            "- [Old](../../wiki/concepts/new.md)\n- [Old](../../wiki/concepts/new.md#sec)\n- [Alias](../../wiki/concepts/new.md)\n"
        );
    }

    #[test]
    fn links_inside_code_are_not_rewritten() {
        // A from-link shown inside a code fence or inline span is a code example, not a
        // citation edge (`broken` ignores it too, so it never dangles after deletion). Merge
        // rewires only the real citation, leaving the code verbatim.
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
            "wiki/documents/doc.md",
            "Cites [Old](../concepts/old.md).\n```\nexample [Old](../concepts/old.md)\n```\nInline `[Old](../concepts/old.md)`.\n",
        );
        let pages = vec![
            page("wiki/concepts/old", "wiki/concepts/old.md", &[]),
            page("wiki/concepts/new", "wiki/concepts/new.md", &[]),
            page(
                "wiki/documents/doc",
                "wiki/documents/doc.md",
                &["wiki/concepts/old"],
            ),
        ];
        merge_concepts(&pages, root, "wiki", "old", "new", false, false).unwrap();
        let doc = std::fs::read_to_string(root.join("wiki/documents/doc.md")).unwrap();
        assert!(
            doc.contains("Cites [Old](../concepts/new.md)."),
            "real citation rewired:\n{doc}"
        );
        assert!(
            doc.contains("example [Old](../concepts/old.md)"),
            "fenced code untouched:\n{doc}"
        );
        assert!(
            doc.contains("Inline `[Old](../concepts/old.md)`."),
            "inline code untouched:\n{doc}"
        );
    }

    #[test]
    fn rewrites_any_spelling_that_resolves_to_from_and_deletes_from() {
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
        // Three spellings of the same address: canonical relative, dot-relative, and
        // the OKF absolute form — all resolve to the from page and must be rewritten.
        write(
            root,
            "daily/x/2026-05-20.md",
            "See [V](../../wiki/concepts/vector-db.md) and [V](../../wiki/./concepts/vector-db.md) and [V](/wiki/concepts/vector-db.md).\n",
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
                &["wiki/concepts/vector-db"],
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
        assert_eq!(r.rewritten[0].links, 3);
        assert!(r.deleted);
        assert!(!root.join("wiki/concepts/vector-db.md").exists());

        let daily = std::fs::read_to_string(root.join("daily/x/2026-05-20.md")).unwrap();
        assert_eq!(
            daily,
            "See [V](../../wiki/concepts/vector-database.md) and [V](../../wiki/concepts/vector-database.md) and [V](../../wiki/concepts/vector-database.md).\n"
        );
    }

    #[test]
    fn case_and_unicode_aliases_are_repointed_only_when_the_filesystem_resolves_them() {
        use unicode_normalization::UnicodeNormalization;
        let decomposed: String = "개념".nfd().collect();
        for (from, alias) in [("old", "OLD"), ("개념", decomposed.as_str())] {
            let tmp = TempDir::new().unwrap();
            let root = tmp.path();
            let from_path = format!("wiki/concepts/{from}.md");
            write(root, &from_path, "---\nid: old\n---\n\n# old\n");
            write(root, "wiki/concepts/new.md", "---\nid: new\n---\n\n# new\n");
            let alias_resolves = root.join(format!("wiki/concepts/{alias}.md")).is_file();
            // Always include the exact address: it must be repointed on every filesystem.
            // The other spelling is a citation only where it really reaches that file.
            let original = format!(
                "See [Exact](../../wiki/concepts/{from}.md) and [Alias](../../wiki/concepts/{alias}.md) and [pdf](../../wiki/concepts/{from}.pdf).\n"
            );
            write(root, "daily/x/d.md", &original);
            let pages = crate::scan::scan_vault(root, false).unwrap();
            let result = merge_concepts(&pages, root, "wiki", from, "new", false, false).unwrap();
            assert_eq!(result.rewritten.len(), 1);
            assert_eq!(result.rewritten[0].links, 1 + usize::from(alias_resolves));
            let expected_alias = if alias_resolves { "new" } else { alias };
            assert_eq!(
                std::fs::read_to_string(root.join("daily/x/d.md")).unwrap(),
                format!(
                    "See [Exact](../../wiki/concepts/new.md) and [Alias](../../wiki/concepts/{expected_alias}.md) and [pdf](../../wiki/concepts/{from}.pdf).\n"
                ),
                "a missing alias stays untouched; a real alias follows its owner"
            );
            assert!(result.deleted);
            assert!(!root.join(from_path).exists());
        }
    }

    #[test]
    fn a_distinct_case_variant_page_is_never_repointed_as_the_merged_page() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(root, "wiki/concepts/old.md", "---\nid: old\n---\n# old\n");
        // A second entry is representable only on a case-sensitive filesystem.
        let uppercase = root.join("wiki/concepts/OLD.md");
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&uppercase)
        {
            Ok(_) => std::fs::write(&uppercase, "# distinct page\n").unwrap(),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return,
            Err(error) => panic!("create distinct case variant: {error}"),
        }
        write(root, "wiki/concepts/new.md", "---\nid: new\n---\n# new\n");
        write(
            root,
            "daily/x/d.md",
            "[Lower](../../wiki/concepts/old.md) [Upper](../../wiki/concepts/OLD.md)\n",
        );
        let pages = crate::scan::scan_vault(root, false).unwrap();
        let result = merge_concepts(&pages, root, "wiki", "old", "new", false, false).unwrap();
        assert_eq!(result.rewritten[0].links, 1);
        assert_eq!(
            std::fs::read_to_string(root.join("daily/x/d.md")).unwrap(),
            "[Lower](../../wiki/concepts/new.md) [Upper](../../wiki/concepts/OLD.md)\n"
        );
        assert_eq!(
            std::fs::read_to_string(uppercase).unwrap(),
            "# distinct page\n"
        );
    }

    #[test]
    fn titled_destination_is_still_repointed() {
        // A CommonMark title after the destination (`[T](x.md "tip")`) must not hide
        // the link from the rewrite: extraction counts it as a citation of `from`
        // (`split_raw_dest` drops the title), so the merge must repoint it too —
        // otherwise it dangles once `from` is deleted.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(root, "wiki/concepts/a.md", "---\nid: a\n---\n\n# a\n");
        write(root, "wiki/concepts/b.md", "---\nid: b\n---\n\n# b\n");
        write(
            root,
            "daily/x/d.md",
            "See [A](../../wiki/concepts/a.md \"tooltip\").\n",
        );
        let pages = vec![
            page("wiki/concepts/a", "wiki/concepts/a.md", &[]),
            page("wiki/concepts/b", "wiki/concepts/b.md", &[]),
            page("daily/x/d", "daily/x/d.md", &["wiki/concepts/a"]),
        ];
        let r = merge_concepts(&pages, root, "wiki", "a", "b", false, false).unwrap();
        assert_eq!(r.rewritten.len(), 1, "titled citation must be repointed");
        let daily = std::fs::read_to_string(root.join("daily/x/d.md")).unwrap();
        assert_eq!(daily, "See [A](../../wiki/concepts/b.md).\n");
    }

    #[test]
    fn dry_run_changes_nothing() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(root, "wiki/concepts/a.md", "---\nid: a\n---\n\n# a\n");
        write(root, "wiki/concepts/b.md", "---\nid: b\n---\n\n# b\n");
        write(root, "daily/x/d.md", "[a](../../wiki/concepts/a.md)\n");
        let pages = vec![
            page("wiki/concepts/a", "wiki/concepts/a.md", &[]),
            page("wiki/concepts/b", "wiki/concepts/b.md", &[]),
            page("daily/x/d", "daily/x/d.md", &["wiki/concepts/a"]),
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
            "---\nid: a\n---\n\n# a\n\n## 핵심\n\nA hand-written synthesis paragraph.\n\n## 출처\n\n- [d](../../daily/x/d.md)\n",
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
            "---\nid: c\n---\n\n# c\n\n## 출처\n\n- [d](../../daily/x/d.md)\n",
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
        // `from`'s only body is human-curated `## 관련` (Related) links, no synthesis
        // prose. Those links are confirmed via `lore-wiki audit` — authored knowledge,
        // NOT machine-derived like `## 출처` (Sources). The merge must refuse to silently
        // drop them without --force.
        write(
            root,
            "wiki/concepts/a.md",
            "---\nid: a\n---\n\n# a\n\n## 관련\n\n- [other](other.md)\n",
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
            "---\nid: a\n---\n\n# a\n\n  ## 출처\n\n- [d](../../daily/x/d.md)\n",
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

    #[test]
    fn merge_absorbs_from_names_into_into_aliases() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            root,
            "wiki/concepts/vector-db.md",
            "---\nid: vector-db\ntitle: \"Vector DB\"\naliases: [\"Vector DB\",\"vecdb\"]\n---\n\n# Vector DB\n",
        );
        write(
            root,
            "wiki/concepts/vector-database.md",
            "---\nid: vector-database\ntitle: \"Vector Database\"\naliases: [\"Vector Database\"]\n---\n\n# Vector Database\n",
        );
        let pages = vec![
            page("wiki/concepts/vector-db", "wiki/concepts/vector-db.md", &[]),
            page(
                "wiki/concepts/vector-database",
                "wiki/concepts/vector-database.md",
                &[],
            ),
        ];
        merge_concepts(
            &pages,
            root,
            "wiki",
            "vector-db",
            "vector-database",
            false,
            false,
        )
        .unwrap();
        // `from`'s title and aliases are folded into `into`'s aliases (into-title stays
        // first, duplicates dropped), so the merged names stay in the concept registry
        // the LLM dedups against.
        let into = std::fs::read_to_string(root.join("wiki/concepts/vector-database.md")).unwrap();
        assert!(
            into.contains(r#"aliases: ["Vector Database","Vector DB","vecdb"]"#),
            "from's names must be absorbed into the canonical page's aliases:\n{into}"
        );
        assert!(!root.join("wiki/concepts/vector-db.md").exists());
    }

    // An unparseable page may hide structured historical evidence. A preview
    // remains available, but deletion requires repairing that uncertainty first.
    #[test]
    fn an_unparseable_page_blocks_deletion_but_allows_preview() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("wiki/concepts")).unwrap();
        std::fs::create_dir_all(root.join("daily/notes")).unwrap();
        write(
            root,
            "wiki/concepts/from.md",
            "---\nid: from\ntype: concept\n---\n\n# From\n\n## Sources\n",
        );
        write(
            root,
            "wiki/concepts/into.md",
            "---\nid: into\ntype: concept\n---\n\n# Into\n\n## Sources\n",
        );
        write(
            root,
            "daily/notes/2026-07-31.md",
            "---\nid: notes\ntitle: unclosed\n\n# Notes\n\n[F](../../wiki/concepts/from.md)\n",
        );
        let citing = root.join("daily/notes/2026-07-31.md");

        let pages = crate::scan::scan_vault(root, false).unwrap();
        let before = std::fs::read_to_string(&citing).unwrap();
        let preview = merge_concepts(&pages, root, "wiki", "from", "into", true, false).unwrap();
        assert_eq!(preview.rewritten.len(), 1);
        let error = merge_concepts(&pages, root, "wiki", "from", "into", false, false).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("cannot establish review evidence safety")
        );
        assert_eq!(std::fs::read_to_string(&citing).unwrap(), before);
        assert!(root.join("wiki/concepts/from.md").exists());
    }
}
