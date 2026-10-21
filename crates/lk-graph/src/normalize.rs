//! Slug normalization: detect filenames whose stem is not already a canonical slug,
//! rename them, and repoint links that address the old path.
//!
//! The normalization rule itself is `lk_core::slugify` (NFKC-correct) — there is no
//! local slug function here. This module only owns the rename plan + link rewrite.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use lk_core::concept::slugify;
use lk_core::link;
use lk_core::vault_path::RESERVED_WIKI_FILES;

use crate::GraphError;
use crate::scan::ScannedPage;
#[cfg(test)]
use crate::scan::page_id as path_slug;

#[derive(Debug, Clone)]
pub struct Rename {
    pub old_path: PathBuf,
    pub new_path: PathBuf,
    pub old_slug: String,
    pub new_slug: String,
}

pub fn scan(pages: &[ScannedPage]) -> Vec<Rename> {
    let mut renames = Vec::new();

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
    let session =
        lk_vault::MutationSession::acquire(root).map_err(|e| GraphError::Io(e.to_string()))?;
    apply_in_session(renames, pages, &session)
}

pub fn apply_in_session(
    renames: &[Rename],
    pages: &[ScannedPage],
    session: &lk_vault::MutationSession,
) -> Result<usize, GraphError> {
    let root = session.root();
    if renames.is_empty() {
        return Ok(0);
    }

    lk_vault::KnowledgeReview::ensure_evidence_paths_preserved(
        root,
        &pages
            .iter()
            .map(|page| page.path.clone())
            .collect::<Vec<_>>(),
        &renames
            .iter()
            .map(|rename| rename.old_path.clone())
            .collect::<Vec<_>>(),
    )
    .map_err(|e| GraphError::Io(e.to_string()))?;

    for rename in renames {
        let target = root.join(&rename.new_path);
        if target.exists()
            && !lk_vault::same_existing_entry(root, &rename.old_path, &rename.new_path)
                .map_err(|e| GraphError::Io(e.to_string()))?
        {
            return Err(GraphError::Io(format!(
                "rename target already exists: {}",
                target.display()
            )));
        }
    }

    let renamed_paths: HashMap<&Path, &Path> = renames
        .iter()
        .map(|r| (r.old_path.as_path(), r.new_path.as_path()))
        .collect();
    let mut destinations: HashMap<PathBuf, &Path> = HashMap::new();
    let mut plan = lk_vault::MutationPlan::default();
    let mut review_updates = lk_vault::plan_review_repointing(
        root,
        &pages
            .iter()
            .map(|page| page.path.clone())
            .collect::<Vec<_>>(),
        &renames
            .iter()
            .map(|rename| (rename.old_path.clone(), rename.new_path.clone()))
            .collect::<Vec<_>>(),
        &mut plan,
    )
    .map_err(|error| GraphError::Io(error.to_string()))?;
    for rename in renames {
        destinations.insert(rename.old_path.clone(), &rename.new_path);
        let absolute = root
            .join(&rename.old_path)
            .canonicalize()
            .map_err(|e| GraphError::Io(e.to_string()))?;
        destinations.insert(absolute, &rename.new_path);
        let before = match review_updates.get(&rename.old_path) {
            Some((before, _)) => before.clone(),
            None => std::fs::read_to_string(root.join(&rename.old_path))
                .map_err(|e| GraphError::Io(e.to_string()))?,
        };
        plan.rename(&rename.old_path, &rename.new_path, before);
    }

    for page in pages {
        let rel_path = renamed_paths
            .get(page.path.as_path())
            .copied()
            .unwrap_or(&page.path);
        let typed = review_updates.remove(&page.path);
        let has_association = typed.is_some();
        let (content, typed) = match typed {
            Some(update) => update,
            None => {
                let content = std::fs::read_to_string(root.join(&page.path))
                    .map_err(|e| GraphError::Io(format!("read {}: {e}", page.path.display())))?;
                (content.clone(), content)
            }
        };
        let mut updated = repoint_renamed_links(&typed, &page.path, &destinations, root)?;
        if page.rss_evidence {
            lk_vault::rss_concept_citations(root, &page.path, &content)
                .map_err(|e| GraphError::Io(e.to_string()))?;
            updated = lk_vault::repoint_rss_evidence(
                root,
                &updated,
                &renames
                    .iter()
                    .map(|r| (r.old_path.clone(), r.new_path.clone()))
                    .collect::<Vec<_>>(),
            )
            .map_err(|e| GraphError::Io(e.to_string()))?;
            lk_vault::guard_rss_evidence(&mut plan, root, rel_path, &updated)
                .map_err(|e| GraphError::Io(e.to_string()))?;
        }

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

        if has_association && content == updated && !renamed_paths.contains_key(page.path.as_path())
        {
            plan.require_unchanged(rel_path, content);
        } else {
            plan.edit(rel_path, content, updated);
        }
    }
    session
        .apply(plan)
        .map_err(|e| GraphError::Io(e.to_string()))?;

    Ok(renames.len())
}

fn repoint_renamed_links(
    content: &str,
    page_path: &Path,
    path_map: &HashMap<PathBuf, &Path>,
    root: &Path,
) -> Result<String, GraphError> {
    let mut error = None;
    let body_offset = lk_core::frontmatter::body_offset(content)
        .ok_or_else(|| GraphError::Io("cannot rewrite links in unclosed frontmatter".into()))?;
    let updated = link::rewrite_links_outside_code(&content[body_offset..], |text, raw_dest| {
        let (dest, anchor) = link::split_raw_dest(raw_dest);
        let decoded = link::decode_dest(dest);
        if !Path::new(&decoded).extension().is_some_and(|e| e == "md") {
            return None;
        }
        let resolved = link::resolve_dest(page_path, &decoded)?;
        let target = if let Some(target) = path_map.get(&resolved) {
            Some(target)
        } else {
            match root.join(&resolved).canonicalize() {
                Ok(path) => path_map.get(&path),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => {
                    error = Some(e);
                    None
                }
            }
        }?;
        let new_dest = format!("{}{anchor}", link::relative_dest(page_path, target));
        Some(format!("[{text}]({new_dest})"))
    });
    match error {
        Some(e) => Err(GraphError::Io(e.to_string())),
        None => Ok(format!("{}{updated}", &content[..body_offset])),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::Link;

    fn build_page(path: &str, outgoing: &[&str]) -> ScannedPage {
        let rel = PathBuf::from(path);
        ScannedPage {
            rss_evidence: false,
            revision: Some("revision".into()),
            id: path_slug(&rel),
            path: rel,
            title: "test".to_owned(),
            format: None,
            outgoing: outgoing
                .iter()
                .map(|s| Link::to(&format!("{s}.md")))
                .collect(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_rewrite_resumes_after_the_original_filename_disappears() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join("notes")).unwrap();
        std::fs::write(root.join("Bad_Name.md"), "# Original\n").unwrap();
        std::fs::write(root.join("notes/link.md"), "[Original](../Bad_Name.md)\n").unwrap();
        let pages = crate::scan::scan_vault(root, false).unwrap();
        let renames = scan(&pages);
        let readonly = root.join("notes");
        std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o500)).unwrap();
        let result = apply(&renames, &pages, root);
        std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(!root.join("Bad_Name.md").exists());
        assert!(root.join("bad-name.md").exists());
        let pages = crate::scan::scan_vault(root, false).unwrap();
        assert!(scan(&pages).is_empty());
        if result.is_err() {
            apply(&[], &pages, root).unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(root.join("notes/link.md")).unwrap(),
            "[Original](../bad-name.md)\n"
        );
        assert!(!root.join(".lorekeeper/mutation/pending.json").exists());
    }

    #[test]
    fn normalization_moves_review_subjects_and_pointers_but_preserves_historical_evidence() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let old = "wiki/concepts/Old_Name.md";
        let new = "wiki/concepts/old-name.md";
        let old_review = "wiki/explorations/Review_Name.md";
        let new_review = "wiki/explorations/review-name.md";
        for directory in ["wiki/concepts", "wiki/explorations"] {
            std::fs::create_dir_all(root.join(directory)).unwrap();
        }
        let concept = format!(
            "---\ntype: concept\ntitle: Scoped subject\nknowledge_review: {old_review}\n---\n## Sources\n"
        );
        std::fs::write(root.join(old), &concept).unwrap();
        let evidence = lk_vault::archive_evidence(root, &[old.into()]).unwrap();
        let metadata = serde_json::json!({"status":"open","subjects":[old],"evidence":evidence.citations,"resolution":null});
        let review = format!(
            "---\ntype: exploration\nreview: {metadata}\n---\n[Subject](../concepts/Old_Name.md) needs evidence.\n"
        );
        std::fs::write(root.join(old_review), &review).unwrap();
        let prior = lk_vault::inspect_evidence(root, &[old_review.into()]).unwrap();
        let pages = crate::scan::scan_vault(root, false).unwrap();
        let renames = scan(&pages);
        assert_eq!(apply(&renames, &pages, root).unwrap(), 2);
        assert!(!root.join(old).exists());
        assert!(!root.join(old_review).exists());
        let concept =
            lk_vault::parse_page(&std::fs::read_to_string(root.join(new)).unwrap()).unwrap();
        assert_eq!(
            concept
                .frontmatter
                .get("knowledge_review")
                .and_then(serde_json::Value::as_str),
            Some(new_review)
        );
        let reports = lk_vault::list_reviews(root, &lk_core::config::VaultDirs::default()).unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].path, new_review);
        assert_eq!(
            reports[0].effective_state,
            lk_vault::ReviewEffectiveState::Open
        );
        let current = reports[0].review.as_ref().unwrap();
        assert_eq!(current.subjects, [new]);
        assert_eq!(current.evidence, evidence.citations);
        assert_eq!(reports[0].missing_evidence, [old]);
        assert_eq!(reports[0].archived_evidence, [old]);
        let archived = lk_vault::load_archived_evidence(root, &prior.citations[0].revision)
            .unwrap()
            .unwrap();
        assert_eq!(archived.original.as_deref(), Some(review.as_str()));
        let pages = crate::scan::scan_vault(root, false).unwrap();
        let audit = crate::audit::plan_selected(
            root,
            &lk_core::config::VaultDirs::default(),
            &pages,
            1,
            &[new.into()],
            u64::MAX,
        )
        .unwrap();
        assert_eq!(audit.candidates[0].reviews[0].path, new_review);
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
        assert!(
            apply(
                &[Rename {
                    old_path: "wiki/concepts/old.md".into(),
                    new_path: "wiki/concepts/new.md".into(),
                    old_slug: "old".into(),
                    new_slug: "new".into()
                }],
                &pages,
                root
            )
            .is_err()
        );
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
        let path_map: HashMap<PathBuf, &Path> = [(old, new.as_path())].into();
        let content = "See [A](Concept_A.md) and [A](Concept_A.md#part) and [B](other.md) here.";
        let updated = repoint_renamed_links(
            content,
            Path::new("wiki/linker.md"),
            &path_map,
            Path::new("/"),
        )
        .unwrap();
        assert_eq!(
            updated,
            "See [A](concept-a.md) and [A](concept-a.md#part) and [B](other.md) here."
        );
    }

    #[test]
    fn normalization_rewrites_body_links_without_editing_frontmatter_or_html() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join("wiki")).unwrap();
        std::fs::write(root.join("wiki/Old_Name.md"), "# Old\n").unwrap();
        let prefix = "\u{feff}---\r\ntype: document\r\nliteral: |\r\n  ```\r\n  [Metadata](Old_Name.md)\r\n---\r\n\r\n";
        let html = "<div>\r\n[HTML](Old_Name.md)\r\n</div>\r\n\r\n<!--\r\n```\r\n-->\r\n\r\n";
        let source = format!("{prefix}{html}[Body](Old_Name.md#claim)\r\n");
        std::fs::write(root.join("wiki/source.md"), source).unwrap();
        let pages = crate::scan::scan_vault(root, false).unwrap();
        assert_eq!(apply(&scan(&pages), &pages, root).unwrap(), 1);
        assert_eq!(
            std::fs::read_to_string(root.join("wiki/source.md")).unwrap(),
            format!("{prefix}{html}[Body](old-name.md#claim)\r\n")
        );
        assert!(!root.join("wiki/Old_Name.md").exists());
        assert!(root.join("wiki/old-name.md").exists());
        assert!(
            repoint_renamed_links(
                "---\ntitle: unclosed\n",
                Path::new("wiki/source.md"),
                &HashMap::new(),
                root,
            )
            .is_err()
        );
    }

    #[test]
    fn titled_destination_is_still_repointed() {
        // Same rule as merge: a CommonMark title must not hide a link from the
        // rename repoint — extraction resolves it to the renamed page, so the
        // rewrite must follow.
        let old_p = PathBuf::from("wiki/Concept_A.md");
        let new_p = PathBuf::from("wiki/concept-a.md");
        let path_map: HashMap<PathBuf, &Path> = [(old_p, new_p.as_path())].into();
        let content = "See [A](Concept_A.md \"tip\") here.";
        let updated = repoint_renamed_links(
            content,
            Path::new("wiki/linker.md"),
            &path_map,
            Path::new("/"),
        )
        .unwrap();
        assert_eq!(updated, "See [A](concept-a.md) here.");
    }

    #[test]
    fn distinct_unresolved_spelling_is_not_repointed() {
        let old = PathBuf::from("wiki/Concept_A.md");
        let new = PathBuf::from("wiki/concept-a.md");
        let path_map: HashMap<PathBuf, &Path> = [(old, new.as_path())].into();
        let content = "See [A](concept_a.md) and [A](CONCEPT-A.md) here.";
        let updated = repoint_renamed_links(
            content,
            Path::new("wiki/linker.md"),
            &path_map,
            Path::new("/"),
        )
        .unwrap();
        assert_eq!(updated, content);
    }

    #[test]
    fn scan_detects_collision() {
        let pages = vec![
            build_page("wiki/Foo_Bar.md", &[]),
            build_page("wiki/FOO-BAR.md", &[]),
        ];
        let renames = scan(&pages);
        assert_eq!(renames.len(), 2);
    }

    #[test]
    fn links_inside_code_are_not_repointed() {
        // A link shown as a code example is not a graph edge, so normalize must
        // leave it verbatim — only the real prose link is repointed.
        let old = PathBuf::from("wiki/Concept A.md");
        let new = PathBuf::from("wiki/concept-a.md");
        let path_map: HashMap<PathBuf, &Path> = [(old, new.as_path())].into();
        let content = "Prose [A](Concept%20A.md).\n```\nexample [A](Concept%20A.md)\n```\nInline `[A](Concept%20A.md)`.\n";
        let updated = repoint_renamed_links(
            content,
            Path::new("wiki/linker.md"),
            &path_map,
            Path::new("/"),
        )
        .unwrap();
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
