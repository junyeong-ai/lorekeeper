use std::path::Path;

use lk_core::{concept::file_revision, link};
use serde::Serialize;

use crate::{ExistingPageResolver, VaultError, page_path, parse_page};

#[derive(Debug, Serialize)]
pub struct LinkDetail {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rss_evidence: Option<crate::RssEvidencePreview>,
    pub line: usize,
    pub label: String,
    pub destination: String,
    pub target: Option<String>,
    pub context: String,
    pub context_truncated: bool,
}

pub(super) fn context_preview(line: &str) -> (String, bool) {
    let mut characters = line.chars();
    let preview = characters.by_ref().take(400).collect();
    (preview, characters.next().is_some())
}

#[derive(Debug, Serialize)]
pub struct LinkDetails {
    pub path: String,
    pub file_revision: String,
    pub links: Vec<LinkDetail>,
    pub total: usize,
    pub offset: usize,
    pub next_offset: Option<usize>,
}

pub struct LinkOptions<'a> {
    pub target: Option<&'a Path>,
    pub limit: usize,
    pub offset: usize,
    pub revision: Option<&'a str>,
}

/// Inspect explicit references without inferring semantic meaning or reading target bodies.
/// Repeated links remain separate occurrences, ordered as written. A revision pins subsequent
/// pages to the same source bytes; a changed source requires restarting pagination.
pub fn inspect_links(
    root: &Path,
    path: &Path,
    options: &LinkOptions<'_>,
) -> Result<LinkDetails, VaultError> {
    if options.limit == 0 || path.extension().is_none_or(|ext| ext != "md") {
        return Err(VaultError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "links requires a Markdown page and a positive limit",
        )));
    }
    let mut resolver = if let Some(target) = options.target {
        let full = page_path(root, target)?;
        match std::fs::metadata(full) {
            Ok(_) => Some(ExistingPageResolver::new(root, [target.to_path_buf()])?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        }
    } else {
        None
    };
    let raw = std::fs::read_to_string(page_path(root, path)?)?;
    let revision = file_revision(&raw);
    if options
        .revision
        .is_some_and(|expected| expected != revision)
    {
        return Err(VaultError::Io(std::io::Error::other(
            "source revision changed; restart link pagination",
        )));
    }
    let page = parse_page(&raw).map_err(VaultError::Frontmatter)?;
    crate::rss_concept_citations(root, path, &raw)?;
    let evidence_previews = crate::rss_link_previews(root, path, &raw)?;
    let line_offset = lk_core::markdown::lines_with_endings(&raw).count()
        - lk_core::markdown::lines_with_endings(&page.body).count();
    let mut links = Vec::new();
    let mut total = 0;
    for occurrence in link::extract_page_link_occurrences(&page.body) {
        let target = link::resolve_dest(path, &occurrence.link.dest);
        if let Some(filter) = options.target {
            let Some(destination) = &target else { continue };
            if destination != filter {
                let resolved = match resolver.as_mut() {
                    Some(resolver) => resolver.resolve(destination)?,
                    None => None,
                };
                if resolved.as_deref() != Some(filter) {
                    continue;
                }
            }
        }
        total += 1;
        if total <= options.offset || links.len() >= options.limit {
            continue;
        }
        let (context, context_truncated) = context_preview(occurrence.context);
        links.push(LinkDetail {
            rss_evidence: evidence_previews
                .get(&(line_offset + occurrence.line))
                .cloned(),
            line: line_offset + occurrence.line,
            label: occurrence.link.text,
            destination: occurrence.link.dest,
            target: target.map(|path| path.to_string_lossy().into_owned()),
            context,
            context_truncated,
        });
    }
    let end = options.offset.saturating_add(links.len());
    Ok(LinkDetails {
        path: path.to_string_lossy().into_owned(),
        file_revision: revision,
        links,
        total,
        offset: options.offset,
        next_offset: (end < total).then_some(end),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_is_a_bounded_unicode_preview_not_an_address() {
        let full = format!("{}[Target](target.md)", "가".repeat(400));
        let (preview, truncated) = context_preview(&full);
        assert_eq!(preview, "가".repeat(400));
        assert!(truncated);
        assert_eq!(context_preview(&preview), (preview, false));
    }

    #[test]
    fn mixed_commonmark_lines_agree_between_live_and_prepared_links() {
        let root = tempfile::tempdir().unwrap();
        let path = Path::new("wiki/documents/source.md");
        std::fs::create_dir_all(root.path().join(path.parent().unwrap())).unwrap();
        let raw = "\u{feff}---\r\ntype: document\r\n---\r\n\r\n# Source\r## Related Concepts\r- [Topic](../concepts/topic.md)\r\n## Tail\nKeep.\n";
        std::fs::write(root.path().join(path), raw).unwrap();
        let live = inspect_links(
            root.path(),
            path,
            &LinkOptions {
                target: None,
                limit: 20,
                offset: 0,
                revision: None,
            },
        )
        .unwrap();
        assert_eq!(live.links[0].line, 7);
        assert_eq!(live.links[0].context, "- [Topic](../concepts/topic.md)");
        let session = crate::MutationSession::acquire(root.path()).unwrap();
        let capture =
            crate::prepare_search(&session, &lk_core::config::VaultDirs::default()).unwrap();
        let captured = crate::inspect_indexed_links(
            root.path(),
            &lk_core::config::VaultDirs::default(),
            path,
            &crate::IndexedLinkOptions {
                snapshot: &capture.snapshot,
                direction: crate::LinkDirection::Outgoing,
                limit: 20,
                offset: 0,
            },
        )
        .unwrap();
        assert_eq!(captured.links[0].line, live.links[0].line);
        assert_eq!(captured.links[0].context, live.links[0].context);
        assert_eq!(
            std::fs::read_to_string(root.path().join(path)).unwrap(),
            raw
        );
    }

    #[test]
    fn links_preserve_locations_duplicates_and_revision_guarded_pagination() {
        let temp = tempfile::tempdir().unwrap();
        let path = Path::new("page.md");
        let raw = "\u{feff}---\r\ntitle: Page\r\n---\r\n\r\n## Related\r\n- [A](a.md#claim) — alternative.\r\n`[code](a.md)` ![image](a.md) [web](https://example.org)\r\n```md\r\n[example](a.md)\r\n```\r\n- [Again](./a.md) and [B](b.md).\r\n";
        std::fs::write(temp.path().join(path), raw).unwrap();
        let mut options = LinkOptions {
            target: Some(Path::new("a.md")),
            limit: 1,
            offset: 0,
            revision: None,
        };
        let first = inspect_links(temp.path(), path, &options).unwrap();
        assert_eq!(first.total, 2);
        assert_eq!(first.next_offset, Some(1));
        assert_eq!(first.links[0].line, 6);
        assert_eq!(first.links[0].context, "- [A](a.md#claim) — alternative.");
        assert_eq!(first.links[0].label, "A");
        assert_eq!(first.links[0].target.as_deref(), Some("a.md"));
        options.offset = 1;
        options.revision = Some(&first.file_revision);
        let next = inspect_links(temp.path(), path, &options).unwrap();
        assert_eq!(next.links[0].line, 11);
        assert_eq!(next.links[0].label, "Again");
        assert_eq!(next.next_offset, None);
        options.offset = usize::MAX;
        assert!(
            inspect_links(temp.path(), path, &options)
                .unwrap()
                .links
                .is_empty()
        );
        std::fs::write(
            temp.path().join(path),
            raw.replace("alternative", "component"),
        )
        .unwrap();
        assert!(inspect_links(temp.path(), path, &options).is_err());
        options.revision = None;
        options.limit = 0;
        assert!(inspect_links(temp.path(), path, &options).is_err());
    }

    #[test]
    fn links_report_missing_and_escaping_destinations_without_reading_them() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("page.md"),
            "[Missing](nested/missing.md)\n[Outside](../outside.md)\n",
        )
        .unwrap();
        let options = LinkOptions {
            target: None,
            limit: 20,
            offset: 0,
            revision: None,
        };
        let report = inspect_links(temp.path(), Path::new("page.md"), &options).unwrap();
        assert_eq!(report.links[0].line, 1);
        assert_eq!(report.links[0].target.as_deref(), Some("nested/missing.md"));
        assert_eq!(report.links[1].destination, "../outside.md");
        assert_eq!(report.links[1].target, None);
        assert!(inspect_links(temp.path(), Path::new("../outside.md"), &options).is_err());
        #[cfg(unix)]
        {
            std::fs::write(temp.path().join("real.md"), "Definition.").unwrap();
            std::os::unix::fs::symlink("real.md", temp.path().join("internal.md")).unwrap();
            std::fs::write(temp.path().join("references.md"), "[Alias](internal.md)").unwrap();
            let filtered = LinkOptions {
                target: Some(Path::new("real.md")),
                ..options
            };
            let found = inspect_links(temp.path(), Path::new("references.md"), &filtered).unwrap();
            assert_eq!(found.total, 1);
            assert_eq!(found.links[0].target.as_deref(), Some("internal.md"));
            let outside = tempfile::tempdir().unwrap();
            std::fs::write(outside.path().join("secret.md"), "[secret](value.md)").unwrap();
            std::os::unix::fs::symlink(
                outside.path().join("secret.md"),
                temp.path().join("alias.md"),
            )
            .unwrap();
            assert!(inspect_links(temp.path(), Path::new("alias.md"), &options).is_err());
        }
    }
}
