//! Revision-guarded concept merging after the caller establishes equivalence.
//!
//! Exact collection-relative addresses select endpoints. A merge repoints links,
//! absorbs declared names and removes the old page; it never copies prose.
//! Authored content requires explicit salvage and force. Backlinks reconciliation
//! owns the resulting Sources, source count and synthesis input.

use std::path::{Path, PathBuf};

use lk_core::link;
use serde::Serialize;

use crate::GraphError;
use crate::scan::ScannedPage;

pub struct MergeRevisions<'a> {
    pub from: &'a str,
    pub into: &'a str,
}

pub struct MergeRequest<'a> {
    pub from: &'a str,
    pub into: &'a str,
    pub revisions: Option<MergeRevisions<'a>>,
    pub force: bool,
}

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
    pub from_revision: String,
    pub into_revision: String,
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
/// Both endpoints must be distinct existing concepts. Publication requires both
/// full file revisions from the caller's reads. `dry_run` reports current revisions
/// and the plan without writing or deleting anything.
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
    request: &MergeRequest<'_>,
    dry_run: bool,
) -> Result<MergeResult, GraphError> {
    if dry_run {
        return plan_merge(pages, vault_root, wiki_dir, request, true).map(|(result, _)| result);
    }
    require_revisions(request)?;
    let session = lk_vault::MutationSession::acquire(vault_root)
        .map_err(|error| GraphError::Io(error.to_string()))?;
    merge_in_session(pages, &session, wiki_dir, request, None)
}

pub fn merge_in_session(
    pages: &[ScannedPage],
    session: &lk_vault::MutationSession,
    wiki_dir: &str,
    request: &MergeRequest<'_>,
    workflow_id: Option<&lk_core::execution::WorkflowId>,
) -> Result<MergeResult, GraphError> {
    require_revisions(request)?;
    let (result, mut plan) = plan_merge(pages, session.root(), wiki_dir, request, false)?;
    if let Some(workflow_id) = workflow_id {
        let concepts = Path::new(wiki_dir).join(lk_core::vault_path::CONCEPTS_SUBDIR);
        let from = concepts.join(format!("{}.md", result.from_slug));
        let into = concepts.join(format!("{}.md", result.into_slug));
        let original = std::fs::read_to_string(session.root().join(&from))
            .map_err(|error| GraphError::Io(error.to_string()))?;
        crate::audit::record_merge(
            &mut plan,
            session.root(),
            workflow_id,
            &from,
            &original,
            &into,
        )?;
    }
    session
        .apply(plan)
        .map_err(|error| GraphError::Io(error.to_string()))?;
    Ok(result)
}

fn require_revisions(request: &MergeRequest<'_>) -> Result<(), GraphError> {
    if request.revisions.is_none() {
        return Err(GraphError::Io("merge requires both read revisions; preview with --dry-run, read both concepts and their evidence, then supply --from-revision and --into-revision".into()));
    }
    Ok(())
}

fn resolve_concept(
    root: &Path,
    concepts: &Path,
    slug: &str,
    resolver: &mut lk_vault::ExistingPageResolver,
) -> Result<(String, PathBuf), GraphError> {
    let path = Path::new(slug);
    if slug.is_empty()
        || slug.contains('\\')
        || path
            .components()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
        || path
            .components()
            .collect::<PathBuf>()
            .to_str()
            .map(|value| value.replace(std::path::MAIN_SEPARATOR, "/"))
            .as_deref()
            != Some(slug)
    {
        return Err(GraphError::Io(format!(
            "expected an exact collection-relative concept address: {slug}"
        )));
    }
    let relative = concepts.join(format!("{slug}.md"));
    lk_vault::writable_page_path(root, &relative)
        .map_err(|error| GraphError::Io(error.to_string()))?;
    let actual = resolver
        .resolve(&relative)
        .map_err(|error| GraphError::Io(error.to_string()))?
        .filter(|actual| actual.starts_with(concepts))
        .ok_or_else(|| GraphError::Io(format!("concept page not found: {}", relative.display())))?;
    let slug = actual
        .strip_prefix(concepts)
        .expect("concept collection")
        .with_extension("")
        .to_str()
        .ok_or_else(|| GraphError::Io("concept address must be UTF-8".into()))?
        .replace(std::path::MAIN_SEPARATOR, "/");
    Ok((slug, actual))
}

fn plan_merge(
    pages: &[ScannedPage],
    vault_root: &Path,
    wiki_dir: &str,
    request: &MergeRequest<'_>,
    dry_run: bool,
) -> Result<(MergeResult, lk_vault::MutationPlan), GraphError> {
    let concepts_dir = Path::new(wiki_dir).join(lk_core::vault_path::CONCEPTS_SUBDIR);
    let mut resolver =
        lk_vault::ExistingPageResolver::new(vault_root, pages.iter().map(|page| page.path.clone()))
            .map_err(|error| GraphError::Io(error.to_string()))?;
    let (from, from_rel) = resolve_concept(vault_root, &concepts_dir, request.from, &mut resolver)?;
    let (into, into_rel) = resolve_concept(vault_root, &concepts_dir, request.into, &mut resolver)?;
    let canonical = |path: &Path| {
        std::fs::canonicalize(vault_root.join(path))
            .map_err(|error| GraphError::Io(error.to_string()))
    };
    if from_rel == into_rel || canonical(&from_rel)? == canonical(&into_rel)? {
        return Err(GraphError::Io(
            "from and into are the same concept; nothing to merge".into(),
        ));
    }
    for path in [&from_rel, &into_rel] {
        if pages
            .iter()
            .find(|page| &page.path == path)
            .and_then(|page| page.format.as_deref())
            .is_some_and(|format| format != "concept")
        {
            return Err(GraphError::Io(format!(
                "merge target is not a concept: {}",
                path.display()
            )));
        }
    }
    let from_raw = std::fs::read_to_string(
        lk_vault::page_path(vault_root, &from_rel)
            .map_err(|error| GraphError::Io(error.to_string()))?,
    )
    .map_err(|error| GraphError::Io(error.to_string()))?;
    let into_raw = std::fs::read_to_string(
        lk_vault::page_path(vault_root, &into_rel)
            .map_err(|error| GraphError::Io(error.to_string()))?,
    )
    .map_err(|error| GraphError::Io(error.to_string()))?;
    let from_revision = lk_core::concept::file_revision(&from_raw);
    let into_revision = lk_core::concept::file_revision(&into_raw);
    if let Some(expected) = &request.revisions
        && (expected.from != from_revision || expected.into != into_revision)
    {
        return Err(GraphError::Io(
            "merge target revision changed; reread both concepts and reassess before merging"
                .into(),
        ));
    }
    let force = request.force;

    // Gate BEFORE any mutation: a merge would discard `from`'s authored prose (links
    // are rewired, body is not). Refuse unless forced, so nothing is silently lost.
    let from_authored = concept_has_authored_body(vault_root, &from_rel)?;
    if from_authored && !force && !dry_run {
        return Err(GraphError::Io(format!(
            "'{from}' has authored body content that a merge would discard — salvage it \
             into '{into}', then re-run with --force (or --dry-run to preview)"
        )));
    }

    // Validate alias publication before deriving any rewrites.
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
    let mut review_updates = if dry_run {
        std::collections::BTreeMap::new()
    } else {
        lk_vault::plan_review_repointing(
            vault_root,
            &pages
                .iter()
                .map(|page| page.path.clone())
                .collect::<Vec<_>>(),
            &[(from_rel.clone(), into_rel.clone())],
            &mut plan,
        )
        .map_err(|error| GraphError::Io(error.to_string()))?
    };
    let mut into_updated = None;
    let mut rewritten = Vec::new();
    for page in pages {
        if page.path == from_rel {
            continue;
        }
        let mut cites_from = false;
        for link in &page.outgoing {
            cites_from |=
                lk_vault::same_existing_entry(vault_root, Path::new(&link.dest), &from_rel)
                    .map_err(|error| GraphError::Io(error.to_string()))?;
        }
        if !cites_from && !review_updates.contains_key(&page.path) {
            continue;
        }
        let abs = vault_root.join(&page.path);
        let (content, typed) = match review_updates.remove(&page.path) {
            Some(update) => update,
            None => {
                let content = std::fs::read_to_string(&abs)
                    .map_err(|e| GraphError::Io(format!("read {}: {e}", abs.display())))?;
                (content.clone(), content)
            }
        };
        let (mut updated, count) = if dry_run && lk_core::frontmatter::body_offset(&typed).is_none()
        {
            tracing::warn!(path = %page.path.display(), "unclosed frontmatter: preview counts diagnostic body links; publication requires repair");
            let diagnostic = lk_core::frontmatter::split_page(&typed).body;
            let (_, count) =
                rewrite_links(&diagnostic, &page.path, &from_rel, &into_rel, vault_root)?;
            (typed.clone(), count)
        } else {
            rewrite_links(&typed, &page.path, &from_rel, &into_rel, vault_root)?
        };
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
        if !dry_run {
            if page.path == into_rel {
                into_updated = Some(updated.clone());
            }
            if page.path != into_rel && updated != content {
                plan.edit(&page.path, content.clone(), updated.clone());
            } else if page.path != into_rel {
                plan.require_unchanged(&page.path, content.clone());
            }
        }
        if count > 0 || updated != content {
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
                None => into_raw.clone(),
            };
            let value =
                serde_json::to_string(aliases).map_err(|e| GraphError::Io(e.to_string()))?;
            let updated = lk_vault::set_frontmatter_field(&before, "aliases", &value)
                .ok_or_else(|| GraphError::Io("merge target has no frontmatter".into()))?;
            plan.edit(&into_rel, into_raw, updated);
        }
        plan.remove(&from_rel, from_raw);
    }

    Ok((
        MergeResult {
            from_slug: from,
            into_slug: into,
            from_revision,
            into_revision,
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
    fn title_of(page: &lk_core::frontmatter::VaultPage, path: &Path) -> Option<String> {
        page.frontmatter
            .get("title")
            .and_then(|v| v.as_str())
            .or_else(|| path.file_stem().and_then(|stem| stem.to_str()))
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

    // The caller has established equivalence; retain both declared name sets.
    let mut absorb = Vec::new();
    absorb.extend(title_of(&from, from_rel));
    absorb.extend(aliases_of(&from));

    // Rebuild the alias list deterministically: the `into` title first, then its existing
    // aliases, then the absorbed `from` names — all deduped, so a title that sat mid-list or
    // a pre-existing duplicate is normalized too.
    let mut aliases: Vec<String> = Vec::new();
    for name in title_of(&into, into_rel)
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
    let body_offset = lk_core::frontmatter::body_offset(content)
        .ok_or_else(|| GraphError::Io("cannot rewrite links in unclosed frontmatter".into()))?;
    // Frontmatter and code/HTML examples do not contribute graph edges.
    let out = link::rewrite_links_outside_code(&content[body_offset..], |inline| {
        // Does this link target the from concept? Resolve the destination the SAME way
        // the graph does — `.md` destinations only, resolved against this page's
        // location, then checked against the actual filesystem — so exactly the spellings scan
        // counts as citations (`../concepts/from.md`, `./from.md`, the OKF absolute
        // form, actual filesystem aliases) are rewritten, no more and no fewer.
        let (dest, anchor) = link::split_dest_anchor(inline.destination);
        if link::is_external(dest) {
            return None;
        }
        let decoded = link::decode_dest(dest);
        if !Path::new(&decoded).extension().is_some_and(|e| e == "md") {
            return None;
        }
        let resolved = link::resolve_dest(page_path, &decoded)?;
        match lk_vault::same_existing_entry(root, &resolved, from_rel) {
            Ok(true) => {}
            Ok(false) => return None,
            Err(error) => {
                failure = Some(GraphError::Io(error.to_string()));
                return None;
            }
        }
        count += 1;
        let new_dest = format!("{}{anchor}", link::relative_dest(page_path, into_rel));
        Some(inline.render_with_destination(&new_dest))
    });
    if let Some(error) = failure {
        return Err(error);
    }
    Ok((format!("{}{out}", &content[..body_offset]), count))
}

/// Recognize only the template title, owned section headings and generated Sources.
fn concept_has_authored_body(root: &Path, path: &Path) -> Result<bool, GraphError> {
    use pulldown_cmark::{Event, HeadingLevel, Parser, Tag};

    let raw = std::fs::read_to_string(
        lk_vault::page_path(root, path).map_err(|error| GraphError::Io(error.to_string()))?,
    )
    .map_err(|error| GraphError::Io(format!("read {}: {error}", path.display())))?;
    let parsed = lk_core::frontmatter::parse_page(&raw)
        .map_err(|error| GraphError::Io(format!("parse {}: {error}", path.display())))?;
    let title = parsed
        .frontmatter
        .get("title")
        .and_then(|value| value.as_str())
        .map(|title| format!("# {title}"));
    let headings = lk_vault::section_headings(|strings| strings.concept_synthesis)
        .chain(lk_vault::section_headings(|strings| {
            strings.concept_sources
        }))
        .chain(lk_vault::section_headings(|strings| strings.related))
        .map(|heading| format!("## {heading}"))
        .collect::<std::collections::BTreeSet<_>>();
    let mut body = parsed.body;
    for heading in lk_vault::section_headings(|strings| strings.concept_sources) {
        if let Some(sources) = lk_vault::section_body(&body, heading)
            && crate::sources::classify(sources, path) != crate::sources::SourcesBody::Authored
        {
            body = lk_vault::replace_section(&body, heading, "");
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut heading_end = 0;
    let parser = Parser::new(&body);
    if parser.reference_definitions().iter().next().is_some() {
        return Ok(true);
    }
    for (event, range) in parser.into_offset_iter() {
        if range.start < heading_end {
            continue;
        }
        let heading = body[range.clone()].trim_end();
        match event {
            Event::Start(Tag::Heading { level, .. })
                if ((level == HeadingLevel::H1
                    && heading.starts_with("# ")
                    && title.as_deref().is_none_or(|title| heading == title)
                    && seen.is_empty())
                    || (level == HeadingLevel::H2 && headings.contains(heading)))
                    && seen.insert(heading.to_owned()) =>
            {
                heading_end = range.end;
            }
            _ => return Ok(true),
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::Link;
    use tempfile::TempDir;

    #[test]
    fn exact_nested_owners_and_read_revisions_guard_publication() {
        for wiki in ["wiki", "knowledge"] {
            let tmp = TempDir::new().unwrap();
            let root = tmp.path();
            let from = "group/A_B";
            let into = "canonical/Keep_V1";
            let from_path = format!("{wiki}/concepts/{from}.md");
            let into_path = format!("{wiki}/concepts/{into}.md");
            let flat = format!("{wiki}/concepts/group-a-b.md");
            for (path, title) in [
                (&from_path, "Duplicate"),
                (&into_path, "Canonical"),
                (&flat, "Unrelated"),
            ] {
                write(
                    root,
                    path,
                    &format!("---\ntype: concept\ntitle: {title}\n---\n# {title}\n"),
                );
            }
            let source = "daily/test/day.md";
            write(
                root,
                source,
                &format!("[Nested](../../{from_path}) [Flat](../../{flat})\n"),
            );
            let pages = vec![
                page("from", &from_path, &[]),
                page("into", &into_path, &[]),
                page("flat", &flat, &[]),
                page(
                    "source",
                    source,
                    &[
                        &format!("{wiki}/concepts/{from}"),
                        &format!("{wiki}/concepts/group-a-b"),
                    ],
                ),
            ];
            let mut request = MergeRequest {
                from,
                into,
                revisions: None,
                force: false,
            };
            let preview = super::merge_concepts(&pages, root, wiki, &request, true).unwrap();
            assert_eq!(preview.from_slug, from);
            assert_eq!(preview.into_slug, into);
            assert!(super::merge_concepts(&pages, root, wiki, &request, false).is_err());
            assert!(!root.join(".lorekeeper").exists());
            request.revisions = Some(MergeRevisions {
                from: &preview.from_revision,
                into: &preview.into_revision,
            });
            let before = std::fs::read_to_string(root.join(&into_path)).unwrap();
            write(root, &into_path, &format!("{before}\nChanged by editor.\n"));
            assert!(
                super::merge_concepts(&pages, root, wiki, &request, false)
                    .unwrap_err()
                    .to_string()
                    .contains("revision changed")
            );
            assert!(root.join(&from_path).is_file());
            write(root, &into_path, &before);
            let original_from = std::fs::read_to_string(root.join(&from_path)).unwrap();
            write(
                root,
                &from_path,
                &format!("{original_from}\nNew qualification.\n"),
            );
            assert!(
                super::merge_concepts(&pages, root, wiki, &request, false)
                    .unwrap_err()
                    .to_string()
                    .contains("revision changed")
            );
            write(root, &from_path, &original_from);
            super::merge_concepts(&pages, root, wiki, &request, false).unwrap();
            assert!(!root.join(&from_path).exists());
            assert!(root.join(&flat).is_file());
            let rewritten = std::fs::read_to_string(root.join(source)).unwrap();
            assert!(rewritten.contains(&format!("../../{into_path}")));
            assert!(rewritten.contains(&format!("../../{flat}")));
        }
    }

    #[test]
    fn merge_rejects_nonexact_collection_addresses() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "wiki/concepts/target.md",
            "---\ntitle: Target\n---\n# Target\n",
        );
        let pages = vec![page("target", "wiki/concepts/target.md", &[])];
        for from in [
            "",
            "../target",
            "/target",
            "a//target",
            "./target",
            "a/../target",
            "a\\target",
        ] {
            let request = MergeRequest {
                from,
                into: "target",
                revisions: None,
                force: false,
            };
            assert!(
                super::merge_concepts(&pages, tmp.path(), "wiki", &request, true).is_err(),
                "{from}"
            );
        }
    }

    #[test]
    fn merge_preserves_legacy_fallback_names_without_adding_filename_aliases() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            root,
            "wiki/concepts/group/Old_Name.md",
            "---\ntype: concept\n---\n# Old_Name\n",
        );
        write(
            root,
            "wiki/concepts/keep.md",
            "---\ntype: concept\ntitle: Canonical\n---\n# Canonical\n",
        );
        let pages = crate::scan::scan_vault(root, false).unwrap();
        merge_concepts(&pages, root, "wiki", "group/Old_Name", "keep", false, false).unwrap();
        let raw = std::fs::read_to_string(root.join("wiki/concepts/keep.md")).unwrap();
        let parsed = lk_core::frontmatter::parse_page(&raw).unwrap();
        assert_eq!(
            parsed.frontmatter.get("aliases"),
            Some(&serde_json::json!(["Canonical", "Old_Name"]))
        );
    }

    #[cfg(unix)]
    #[test]
    fn merge_with_sibling_alias_writes_refuses_without_partial_publication() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let old = "---\ntype: concept\ntitle: Old\n---\n# Old\n";
        let keep = "---\ntype: concept\ntitle: Keep\naliases: [Keep]\n---\n# Keep\n\n[Old](/wiki/concepts/old.md)\n";
        write(root, "wiki/concepts/old.md", old);
        write(root, "wiki/concepts/group/keep.md", keep);
        std::os::unix::fs::symlink("group", root.join("wiki/concepts/alias")).unwrap();
        let pages = crate::scan::scan_vault(root, true).unwrap();
        let error =
            merge_concepts(&pages, root, "wiki", "old", "group/keep", false, false).unwrap_err();
        assert!(error.to_string().contains("same file entry"), "{error}");
        assert_eq!(
            std::fs::read_to_string(root.join("wiki/concepts/old.md")).unwrap(),
            old
        );
        assert_eq!(
            std::fs::read_to_string(root.join("wiki/concepts/group/keep.md")).unwrap(),
            keep
        );
        assert!(!root.join(".lorekeeper/mutation/pending.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn merge_rejects_two_addresses_of_one_entry_but_keeps_distinct_hard_links() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let raw = "---\ntype: concept\ntitle: Topic\naliases: [Topic]\n---\n# Topic\n";
        write(root, "wiki/concepts/group/a.md", raw);
        std::os::unix::fs::symlink("group", root.join("wiki/concepts/alias")).unwrap();
        let pages = crate::scan::scan_vault(root, true).unwrap();
        let revision = lk_core::concept::file_revision(raw);
        let request = MergeRequest {
            from: "group/a",
            into: "alias/a",
            revisions: Some(MergeRevisions {
                from: &revision,
                into: &revision,
            }),
            force: false,
        };
        for dry_run in [true, false] {
            assert!(super::merge_concepts(&pages, root, "wiki", &request, dry_run).is_err());
            assert_eq!(
                std::fs::read_to_string(root.join("wiki/concepts/group/a.md")).unwrap(),
                raw
            );
            assert_eq!(
                std::fs::read_to_string(root.join("wiki/concepts/alias/a.md")).unwrap(),
                raw
            );
        }
        std::fs::hard_link(
            root.join("wiki/concepts/group/a.md"),
            root.join("wiki/concepts/hard.md"),
        )
        .unwrap();
        let pages = crate::scan::scan_vault(root, false).unwrap();
        let request = MergeRequest {
            into: "hard",
            ..request
        };
        super::merge_concepts(&pages, root, "wiki", &request, false).unwrap();
        assert!(!root.join("wiki/concepts/group/a.md").exists());
        assert!(root.join("wiki/concepts/hard.md").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn merge_preserves_links_to_an_unselected_hard_link_entry() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let source = "wiki/concepts/a.md";
        let distinct = "wiki/concepts/c.md";
        write(root, source, "---\ntype: concept\n---\n");
        std::fs::hard_link(root.join(source), root.join(distinct)).unwrap();
        std::os::unix::fs::symlink("a.md", root.join("wiki/concepts/alias.md")).unwrap();
        write(
            root,
            "wiki/concepts/b.md",
            "---\ntype: concept\ntitle: B\n---\n# B\n",
        );
        write(
            root,
            "wiki/documents/source.md",
            "---\ntype: document\n---\n[A](../concepts/a.md#detail)\n[Alias](../concepts/alias.md#detail)\n[C](../concepts/c.md#detail)\n",
        );
        let pages = crate::scan::scan_vault(root, false).unwrap();
        let result = merge_concepts(&pages, root, "wiki", "a", "b", false, false).unwrap();
        assert_eq!(
            result
                .rewritten
                .iter()
                .map(|page| page.links)
                .sum::<usize>(),
            2
        );
        assert_eq!(
            std::fs::read_to_string(root.join("wiki/documents/source.md")).unwrap(),
            "---\ntype: document\n---\n[A](../concepts/b.md#detail)\n[Alias](../concepts/b.md#detail)\n[C](../concepts/c.md#detail)\n"
        );
        assert!(!root.join(source).exists());
        assert_eq!(
            std::fs::read_to_string(root.join(distinct)).unwrap(),
            "---\ntype: concept\n---\n"
        );
    }

    #[test]
    fn authored_blocks_are_not_template_headings() {
        let tmp = TempDir::new().unwrap();
        let path = Path::new("wiki/concepts/topic.md");
        for body in [
            "#define RETRY_LIMIT 3\n",
            "#standalone-tag\n",
            "### A necessary qualification\n",
            "# Another subject\n",
            "```text\n## Sources\n```\n",
            "<!-- retained explanation -->\n",
            "Title\n=====\n",
            "[source]: https://example.test/evidence \"Original\"\n",
        ] {
            write(
                tmp.path(),
                path.to_str().unwrap(),
                &format!("---\ntitle: Topic\n---\n# Topic\n\n## Synthesis\n\n{body}"),
            );
            assert!(
                concept_has_authored_body(tmp.path(), path).unwrap(),
                "{body}"
            );
        }
        for locale in lk_core::i18n::Locale::ALL {
            let strings = locale.strings();
            write(
                tmp.path(),
                path.to_str().unwrap(),
                &format!(
                    "---\ntitle: Topic\n---\n# Topic\n\n## {}\n\n## {}\n\n{}\n\n## {}\n",
                    strings.concept_synthesis,
                    strings.concept_sources,
                    crate::sources::render(*locale),
                    strings.related
                ),
            );
            assert!(!concept_has_authored_body(tmp.path(), path).unwrap());
        }
    }

    fn merge_concepts(
        pages: &[ScannedPage],
        root: &Path,
        wiki: &str,
        from: &str,
        into: &str,
        dry_run: bool,
        force: bool,
    ) -> Result<MergeResult, GraphError> {
        let concepts = Path::new(wiki).join(lk_core::vault_path::CONCEPTS_SUBDIR);
        let read_revision = |slug: &str| {
            lk_core::concept::file_revision(
                &std::fs::read_to_string(root.join(&concepts).join(format!("{slug}.md")))
                    .unwrap_or_default(),
            )
        };
        let from_revision = read_revision(from);
        let into_revision = read_revision(into);
        super::merge_concepts(
            pages,
            root,
            wiki,
            &MergeRequest {
                from,
                into,
                revisions: Some(MergeRevisions {
                    from: &from_revision,
                    into: &into_revision,
                }),
                force,
            },
            dry_run,
        )
    }

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
    fn merge_preserves_declared_reviews_through_convergence() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let old = "wiki/concepts/old.md";
        let into = "wiki/concepts/into.md";
        let final_path = "wiki/concepts/final.md";
        let pointer_review = "wiki/explorations/pointer.md";
        let explicit_review = "wiki/explorations/explicit.md";
        let source = "wiki/documents/source.md";
        write(
            root,
            source,
            "---\ntype: document\n---\nA scoped observation.\n",
        );
        let evidence = lk_vault::inspect_evidence(root, &[source.into()]).unwrap();
        for (path, title) in [(old, "Old"), (into, "Into"), (final_path, "Final")] {
            let pointer = if path == old {
                format!("knowledge_review: {pointer_review}\n")
            } else {
                String::new()
            };
            write(
                root,
                path,
                &format!(
                    "---\ntype: concept\ntitle: {title}\n{pointer}---\n## Synthesis\n\n## Related\n\n## Sources\n"
                ),
            );
        }
        let explicit = serde_json::json!({"status":"open","subjects":[old,into],"evidence":evidence.citations,"resolution":null});
        let pointer = serde_json::json!({"status":"resolved","subjects":[old],"evidence":evidence.citations,"resolution":"The scope distinction is established."});
        let explicit_before = format!(
            "---\ntype: exploration\nreview: {explicit}\n---\n[Old](../concepts/old.md) needs further evidence.\n"
        );
        let pointer_before =
            format!("---\ntype: exploration\nreview: {pointer}\n---\nA scoped judgment.\n");
        write(root, explicit_review, &explicit_before);
        write(root, pointer_review, &pointer_before);
        let prior =
            lk_vault::inspect_evidence(root, &[explicit_review.into(), pointer_review.into()])
                .unwrap();
        let pages = crate::scan::scan_vault(root, false).unwrap();
        merge_concepts(&pages, root, "wiki", "old", "into", true, false).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join(explicit_review)).unwrap(),
            explicit_before
        );
        assert!(!root.join(".lorekeeper/evidence").exists());
        merge_concepts(&pages, root, "wiki", "old", "into", false, false).unwrap();
        for report in lk_vault::list_reviews(root, &lk_core::config::VaultDirs::default()).unwrap()
        {
            assert!(report.errors.is_empty());
            let review = report.review.unwrap();
            assert_eq!(review.subjects, [into]);
            assert_eq!(review.evidence, evidence.citations);
            if report.path == pointer_review {
                assert_eq!(review.status, lk_vault::ReviewStatus::Resolved);
            }
        }
        for citation in prior.citations {
            let archived = lk_vault::load_archived_evidence(root, &citation.revision)
                .unwrap()
                .unwrap();
            let before = if citation.path == explicit_review {
                &explicit_before
            } else {
                &pointer_before
            };
            assert_eq!(archived.original.as_deref(), Some(before.as_str()));
        }
        let after_first =
            lk_vault::inspect_evidence(root, &[explicit_review.into(), pointer_review.into()])
                .unwrap();
        let pages = crate::scan::scan_vault(root, false).unwrap();
        let plan = crate::audit::plan_selected(
            root,
            &lk_core::config::VaultDirs::default(),
            &pages,
            1,
            &[into.into()],
            u64::MAX,
        )
        .unwrap();
        assert_eq!(plan.candidates[0].reviews.len(), 2);
        merge_concepts(&pages, root, "wiki", "into", "final", false, false).unwrap();
        for report in lk_vault::list_reviews(root, &lk_core::config::VaultDirs::default()).unwrap()
        {
            assert_eq!(report.review.unwrap().subjects, [final_path]);
        }
        for citation in after_first.citations {
            assert!(
                lk_vault::load_archived_evidence(root, &citation.revision)
                    .unwrap()
                    .is_some()
            );
        }
    }

    #[test]
    fn merge_refuses_losing_an_unavailable_pointer_only_review() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let old = "---\ntype: concept\ntitle: Old\nknowledge_review: wiki/explorations/missing.md\n---\n## Sources\n";
        let into = "---\ntype: concept\ntitle: Into\n---\n## Sources\n";
        write(root, "wiki/concepts/old.md", old);
        write(root, "wiki/concepts/into.md", into);
        let pages = crate::scan::scan_vault(root, false).unwrap();
        let error = merge_concepts(&pages, root, "wiki", "old", "into", false, false).unwrap_err();
        assert!(error.to_string().contains("knowledge_review"));
        assert_eq!(
            std::fs::read_to_string(root.join("wiki/concepts/old.md")).unwrap(),
            old
        );
        assert_eq!(
            std::fs::read_to_string(root.join("wiki/concepts/into.md")).unwrap(),
            into
        );
        assert!(!root.join(".lorekeeper/evidence").exists());
    }

    #[test]
    fn merge_refuses_promoting_undeclared_pointer_scope_even_when_forced() {
        for force in [false, true] {
            let tmp = TempDir::new().unwrap();
            let root = tmp.path();
            let old = "---\ntype: concept\ntitle: Old\nknowledge_review: wiki/explorations/review.md\n---\n## Sources\n";
            let into = "---\ntype: concept\ntitle: Into\n---\n## Sources\n";
            write(root, "wiki/concepts/old.md", old);
            write(root, "wiki/concepts/into.md", into);
            write(root, "source.md", "---\ntype: document\n---\nEvidence.\n");
            let evidence = lk_vault::inspect_evidence(root, &["source.md".into()]).unwrap();
            let metadata = serde_json::json!({"status":"open","subjects":[],"evidence":evidence.citations,"resolution":null});
            let review =
                format!("---\ntype: exploration\nreview: {metadata}\n---\nAn unscoped judgment.\n");
            write(root, "wiki/explorations/review.md", &review);
            let pages = crate::scan::scan_vault(root, false).unwrap();
            let error =
                merge_concepts(&pages, root, "wiki", "old", "into", false, force).unwrap_err();
            assert!(error.to_string().contains("explicitly reassess"));
            assert_eq!(
                std::fs::read_to_string(root.join("wiki/concepts/old.md")).unwrap(),
                old
            );
            assert_eq!(
                std::fs::read_to_string(root.join("wiki/concepts/into.md")).unwrap(),
                into
            );
            assert_eq!(
                std::fs::read_to_string(root.join("wiki/explorations/review.md")).unwrap(),
                review
            );
            assert!(!root.join(".lorekeeper/evidence").exists());
        }
    }

    #[test]
    fn merge_keeps_unrelated_invalid_review_scope_unchanged() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            root,
            "wiki/concepts/old.md",
            "---\ntype: concept\ntitle: Old\n---\n## Sources\n",
        );
        write(
            root,
            "wiki/concepts/into.md",
            "---\ntype: concept\ntitle: Into\n---\n## Sources\n",
        );
        write(root, "source.md", "---\ntype: document\n---\nEvidence.\n");
        let evidence = lk_vault::inspect_evidence(root, &["source.md".into()]).unwrap();
        let metadata = serde_json::json!({"status":"open","subjects":["../invalid.md"],"evidence":evidence.citations,"resolution":null});
        let review = format!(
            "---\ntype: exploration\nreview: {metadata}\n---\nAn unrelated invalid scope.\n"
        );
        write(root, "wiki/explorations/review.md", &review);
        let pages = crate::scan::scan_vault(root, false).unwrap();
        merge_concepts(&pages, root, "wiki", "old", "into", false, false).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("wiki/explorations/review.md")).unwrap(),
            review
        );
        assert_eq!(
            lk_vault::list_reviews(root, &lk_core::config::VaultDirs::default()).unwrap()[0]
                .effective_state,
            lk_vault::ReviewEffectiveState::Invalid
        );
        assert!(!root.join(".lorekeeper/evidence").exists());
    }

    #[test]
    fn merge_review_migration_recovery_preserves_guards_and_archives() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let old = "wiki/concepts/old.md";
        let into = "wiki/concepts/into.md";
        let review_path = "wiki/explorations/review.md";
        write(
            root,
            old,
            "---\ntype: concept\ntitle: Old\n---\n## Sources\n",
        );
        write(
            root,
            into,
            "---\ntype: concept\ntitle: Into\n---\n## Sources\n",
        );
        write(root, "source.md", "---\ntype: document\n---\nEvidence.\n");
        let evidence = lk_vault::inspect_evidence(root, &["source.md".into()]).unwrap();
        let metadata = serde_json::json!({"status":"open","subjects":[old],"evidence":evidence.citations,"resolution":null});
        let before =
            format!("---\ntype: exploration\nreview: {metadata}\n---\nA disputed claim.\n");
        write(root, review_path, &before);
        let pages = crate::scan::scan_vault(root, false).unwrap();
        let (_, plan) = plan_merge(
            &pages,
            root,
            "wiki",
            &MergeRequest {
                from: "old",
                into: "into",
                revisions: None,
                force: false,
            },
            false,
        )
        .unwrap();
        let archive = lk_vault::archive_evidence(root, &[review_path.into()]).unwrap();
        let journal = serde_json::json!({"version":1,"plan":plan});
        write(
            root,
            ".lorekeeper/mutation/pending.json",
            &journal.to_string(),
        );
        write(
            root,
            review_path,
            &before.replace("disputed", "externally changed"),
        );
        assert!(lk_vault::MutationSession::acquire(root).is_err());
        assert!(root.join(old).exists());
        assert!(root.join(".lorekeeper/mutation/pending.json").exists());
        write(root, review_path, &before);
        let _session = lk_vault::MutationSession::acquire(root).unwrap();
        assert!(!root.join(old).exists());
        let reports = lk_vault::list_reviews(root, &lk_core::config::VaultDirs::default()).unwrap();
        assert_eq!(reports[0].review.as_ref().unwrap().subjects, [into]);
        let archived = lk_vault::load_archived_evidence(root, &archive.citations[0].revision)
            .unwrap()
            .unwrap();
        assert_eq!(archived.original.as_deref(), Some(before.as_str()));
        assert!(!root.join(".lorekeeper/mutation/pending.json").exists());
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
    fn merge_rewrites_body_links_without_editing_frontmatter_or_html() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        for name in ["old", "new"] {
            write(
                root,
                &format!("wiki/concepts/{name}.md"),
                &format!("---\ntype: concept\ntitle: {name}\n---\n# {name}\n"),
            );
        }
        let prefix = "\u{feff}---\r\ntype: document\r\nliteral: |\r\n  ```\r\n  [Metadata](../concepts/old.md)\r\n---\r\n\r\n";
        let html =
            "<div>\r\n[HTML](../concepts/old.md)\r\n</div>\r\n\r\n<!--\r\n```\r\n-->\r\n\r\n";
        let source = format!("{prefix}{html}[Body](../concepts/old.md#claim)\r\n");
        write(root, "wiki/documents/source.md", &source);
        let pages = crate::scan::scan_vault(root, false).unwrap();
        let merged = merge_concepts(&pages, root, "wiki", "old", "new", false, false).unwrap();
        assert_eq!(merged.rewritten.len(), 1);
        assert_eq!(merged.rewritten[0].links, 1);
        assert_eq!(
            std::fs::read_to_string(root.join("wiki/documents/source.md")).unwrap(),
            format!("{prefix}{html}[Body](../concepts/new.md#claim)\r\n")
        );
        assert!(!root.join("wiki/concepts/old.md").exists());
        assert!(
            rewrite_links(
                "---\ntitle: unclosed\n",
                Path::new("wiki/documents/source.md"),
                Path::new("wiki/concepts/old.md"),
                Path::new("wiki/concepts/new.md"),
                root,
            )
            .is_err()
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
        // the link from the rewrite: extraction counts it as a citation of `from`,
        // so the merge must repoint it too —
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
        assert_eq!(daily, "See [A](../../wiki/concepts/b.md \"tooltip\").\n");
    }

    #[test]
    fn parsed_commonmark_destinations_are_scanned_and_rewritten_consistently() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let page = Path::new("wiki/concepts/source.md");
        let into = Path::new("wiki/concepts/new&amp;.md");
        write(root, into.to_str().unwrap(), "# New\n");
        for (raw, name, anchor) in [
            ("<target.md#part>", "target.md", "#part"),
            ("<a b.md#part>", "a b.md", "#part"),
            (r"foo\(bar\).md#part", "foo(bar).md", "#part"),
            ("a&amp;b.md#part", "a&b.md", "#part"),
            (
                "<target.md#anchor with spaces>",
                "target.md",
                "#anchor&#32;with&#32;spaces",
            ),
            ("target.md#anchor&#10;line", "target.md", "#anchor&#10;line"),
        ] {
            let from = Path::new("wiki/concepts").join(name);
            write(root, from.to_str().unwrap(), "# Original\n");
            let content = format!("[Raw \\[label\\]]({raw} \"A (condition)\")\n");
            write(root, page.to_str().unwrap(), &content);
            let pages = crate::scan::scan_vault(root, false).unwrap();
            let source = pages.iter().find(|scanned| scanned.path == page).unwrap();
            assert_eq!(
                source.outgoing,
                [crate::scan::Link::to(from.to_str().unwrap())]
            );
            let (updated, count) = rewrite_links(&content, page, &from, into, root).unwrap();
            assert_eq!(count, 1, "{content}");
            assert_eq!(
                updated,
                format!("[Raw \\[label\\]](new%26amp;.md{anchor} \"A (condition)\")\n")
            );
            assert_eq!(link::extract_dests(&updated), ["new&amp;.md"]);
        }
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
    fn compact_sources_are_managed_but_appended_authored_prose_still_blocks_merge() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(root, "wiki/concepts/b.md", "---\nid: b\n---\n# b\n");
        let pages = vec![
            page("wiki/concepts/a", "wiki/concepts/a.md", &[]),
            page("wiki/concepts/b", "wiki/concepts/b.md", &[]),
        ];
        for locale in lk_core::i18n::Locale::ALL {
            let generated = format!(
                "---\nid: a\n---\n# a\n\n## {}\n\n{}\n",
                locale.strings().concept_sources,
                crate::sources::render(*locale)
            );
            write(root, "wiki/concepts/a.md", &generated);
            assert!(
                !merge_concepts(&pages, root, "wiki", "a", "b", true, false)
                    .unwrap()
                    .from_authored
            );
            write(
                root,
                "wiki/concepts/a.md",
                &format!("{generated}\nAuthored explanation.\n"),
            );
            assert!(
                merge_concepts(&pages, root, "wiki", "a", "b", true, false)
                    .unwrap()
                    .from_authored
            );
            assert!(merge_concepts(&pages, root, "wiki", "a", "b", false, false).is_err());
            assert!(root.join("wiki/concepts/a.md").is_file());
        }
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
        assert_eq!(preview.rewritten[0].links, 1);
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
