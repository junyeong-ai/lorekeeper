use std::path::Path;

use lk_core::concept::{file_revision, source_revision};
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use serde::Serialize;

use crate::{VaultError, page_path, parse_page};

#[derive(Debug, Serialize)]
pub struct DocumentSection {
    pub id: usize,
    pub level: u8,
    pub title: String,
    pub characters: usize,
    #[serde(skip)]
    start: usize,
    #[serde(skip)]
    end: usize,
}

#[derive(Debug, Serialize)]
pub struct DocumentContent {
    pub section: usize,
    pub offset: usize,
    pub next_offset: Option<usize>,
    pub text: String,
}

#[derive(Debug, Serialize)]
pub struct DocumentView {
    pub path: String,
    pub title: String,
    pub file_revision: String,
    pub source_revision: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_record: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_identity: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_file: Option<lk_core::frontmatter::SourceFile>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub metadata_issues: Vec<String>,
    pub total_sections: usize,
    pub offset: usize,
    pub next_offset: Option<usize>,
    pub sections: Vec<DocumentSection>,
    pub content: Option<DocumentContent>,
}

pub struct DocumentOptions<'a> {
    pub section: Option<usize>,
    pub offset: usize,
    pub limit: usize,
    pub revision: Option<&'a str>,
}

/// Read an outline or a bounded section from one exact page snapshot. Offsets count
/// Unicode scalar values in the parsed Markdown body, not bytes or rendered glyphs.
pub fn inspect_document(
    root: &Path,
    path: &Path,
    options: &DocumentOptions<'_>,
) -> Result<DocumentView, VaultError> {
    let invalid = |message: &str| {
        VaultError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            message,
        ))
    };
    if options.limit == 0 || path.extension().is_none_or(|extension| extension != "md") {
        return Err(invalid(
            "read requires a Markdown page and a positive limit",
        ));
    }
    let raw = std::fs::read_to_string(page_path(root, path)?)?;
    let revision = file_revision(&raw);
    if options
        .revision
        .is_some_and(|expected| expected != revision)
    {
        return Err(invalid(
            "source revision changed; restart document navigation",
        ));
    }
    let page = parse_page(&raw).map_err(VaultError::Frontmatter)?;
    let mut sections = outline(&page.body);
    let total_sections = sections.len();
    if options.section.is_none() && options.offset > total_sections {
        return Err(invalid("offset exceeds outline length"));
    }
    let content = options
        .section
        .map(|id| {
            let section = sections.get(id).ok_or_else(|| invalid("unknown section"))?;
            if options.offset > section.characters {
                return Err(invalid("offset exceeds section length"));
            }
            let text: String = page.body[section.start..section.end]
                .chars()
                .skip(options.offset)
                .take(options.limit)
                .collect();
            let end = options.offset.saturating_add(text.chars().count());
            Ok(DocumentContent {
                section: id,
                offset: options.offset,
                next_offset: (end < section.characters).then_some(end),
                text,
            })
        })
        .transpose()?;
    let next_offset = if let Some(id) = options.section {
        sections.retain(|section| section.id == id);
        None
    } else {
        sections = sections
            .into_iter()
            .skip(options.offset)
            .take(options.limit)
            .collect();
        let end = options.offset.saturating_add(sections.len());
        (end < total_sections).then_some(end)
    };
    let mut metadata_issues = Vec::new();
    let source_file = page.frontmatter.source_file().unwrap_or_else(|issue| {
        metadata_issues.push(issue);
        None
    });
    Ok(DocumentView {
        path: path.to_string_lossy().into_owned(),
        title: page
            .frontmatter
            .get("title")
            .and_then(|value| value.as_str())
            .unwrap_or_else(|| {
                path.file_stem()
                    .and_then(|value| value.to_str())
                    .unwrap_or("")
            })
            .into(),
        file_revision: revision,
        source_revision: source_revision(&page),
        source_record: page.frontmatter.get("source_record").cloned(),
        source_identity: page
            .frontmatter
            .get("source_identity")
            .filter(|value| !value.is_null())
            .cloned(),
        source_url: page
            .frontmatter
            .get("source_url")
            .and_then(|value| value.as_str())
            .map(String::from),
        source_file,
        metadata_issues,
        total_sections,
        offset: options.offset,
        next_offset,
        sections,
        content,
    })
}

fn outline(body: &str) -> Vec<DocumentSection> {
    let mut sections = vec![DocumentSection {
        id: 0,
        level: 0,
        title: String::new(),
        characters: body.chars().count(),
        start: 0,
        end: body.len(),
    }];
    let mut heading = None;
    let mut open = vec![0];
    for (event, range) in Parser::new(body).into_offset_iter() {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                let level = level as u8;
                while let Some(&id) = open.last() {
                    if id != 0 && sections[id].level < level {
                        break;
                    }
                    sections[id].end = range.start;
                    open.pop();
                }
                open.push(sections.len());
                heading = Some(sections.len());
                sections.push(DocumentSection {
                    id: sections.len(),
                    level,
                    title: String::new(),
                    characters: 0,
                    start: range.start,
                    end: body.len(),
                });
            }
            Event::Text(text) | Event::Code(text) if heading.is_some() => {
                sections[heading.unwrap()].title.push_str(&text);
            }
            Event::SoftBreak | Event::HardBreak if heading.is_some() => {
                sections[heading.unwrap()].title.push(' ');
            }
            Event::End(TagEnd::Heading(_)) => heading = None,
            _ => {}
        }
    }
    for section in &mut sections {
        section.characters = body[section.start..section.end].chars().count();
    }
    sections
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_reads_preserve_single_and_composite_source_references() {
        let root = tempfile::tempdir().unwrap();
        let dirs = lk_core::config::VaultDirs::default();
        let path = lk_core::vault_path::documents_dir(&dirs).join("page.md");
        std::fs::create_dir_all(root.path().join(path.parent().unwrap())).unwrap();
        let options = DocumentOptions {
            section: None,
            offset: 0,
            limit: 10,
            revision: None,
        };
        for source in [
            serde_json::json!("docs/decision.md"),
            serde_json::json!(["docs/decision.md", "git:abc123"]),
            serde_json::Value::Null,
        ] {
            let raw = format!("---\ntype: document\nsource_file: {source}\n---\nEvidence\n");
            std::fs::write(root.path().join(&path), &raw).unwrap();
            let view = inspect_document(root.path(), &path, &options).unwrap();
            let value = serde_json::to_value(view).unwrap();
            assert_eq!(value.get("source_file"), Some(&source));
            let search =
                crate::search(root.path(), &dirs, &crate::SearchOptions::default()).unwrap();
            assert_eq!(search.hits[0].source_file.as_ref(), Some(&source));
            let provenance = crate::provenance::document_page(
                path.to_string_lossy().into_owned(),
                &crate::parse_page(&raw).unwrap(),
                file_revision(&raw),
                lk_core::concept::source_revision(&crate::parse_page(&raw).unwrap()),
            )
            .unwrap();
            assert_eq!(provenance.metadata["source_file"], source);
            assert!(provenance.issues.is_empty());
        }
        let raw = "---\ntype: document\nsource_file: [valid.md, 7]\n---\nEvidence\n";
        std::fs::write(root.path().join(&path), raw).unwrap();
        let read = inspect_document(root.path(), &path, &options).unwrap();
        let search = crate::search(root.path(), &dirs, &crate::SearchOptions::default()).unwrap();
        let provenance = crate::provenance::document_page(
            path.to_string_lossy().into_owned(),
            &crate::parse_page(raw).unwrap(),
            file_revision(raw),
            lk_core::concept::source_revision(&crate::parse_page(raw).unwrap()),
        )
        .unwrap();
        assert!(read.source_file.is_none());
        assert_eq!(read.metadata_issues.len(), 1);
        assert_eq!(read.metadata_issues, search.hits[0].metadata_issues);
        assert_eq!(read.metadata_issues, provenance.issues);
    }

    #[test]
    fn outline_and_unicode_pagination_preserve_context_and_reject_revision_drift() {
        let root = tempfile::tempdir().unwrap();
        let path = Path::new("page.md");
        std::fs::write(root.path().join(path), "---\r\ntitle: Example\r\n---\r\n\r\n# 주제\n\n## 같은 이름\n가나다🙂\n\n### 세부\n근거\n\n```md\n## 코드\n```\n\n## 같은 이름\n다른 근거\n").unwrap();
        let mut options = DocumentOptions {
            section: None,
            offset: 0,
            limit: 5,
            revision: None,
        };
        let view = inspect_document(root.path(), path, &options).unwrap();
        assert!(view.content.is_none());
        assert_eq!(
            view.sections
                .iter()
                .map(|s| s.title.as_str())
                .collect::<Vec<_>>(),
            ["", "주제", "같은 이름", "세부", "같은 이름"]
        );
        options.section = Some(2);
        options.limit = 4;
        options.revision = Some(&view.file_revision);
        let mut collected = String::new();
        loop {
            let content = inspect_document(root.path(), path, &options)
                .unwrap()
                .content
                .unwrap();
            collected.push_str(&content.text);
            let Some(next) = content.next_offset else {
                break;
            };
            options.offset = next;
        }
        assert!(collected.contains("가나다🙂"));
        assert!(collected.contains("### 세부"));
        assert!(!collected.contains("다른 근거"));
        std::fs::write(root.path().join(path), "changed").unwrap();
        assert!(inspect_document(root.path(), path, &options).is_err());
    }

    #[test]
    fn outline_pagination_is_bounded_and_section_reads_do_not_repeat_the_outline() {
        let root = tempfile::tempdir().unwrap();
        let path = Path::new("page.md");
        std::fs::write(root.path().join(path), "# One\n## Two\n## Three\n").unwrap();
        let mut options = DocumentOptions {
            section: None,
            offset: 0,
            limit: 2,
            revision: None,
        };
        let first = inspect_document(root.path(), path, &options).unwrap();
        assert_eq!(first.total_sections, 4);
        assert_eq!(first.sections.len(), 2);
        assert_eq!(first.next_offset, Some(2));
        options.offset = 2;
        options.revision = Some(&first.file_revision);
        let second = inspect_document(root.path(), path, &options).unwrap();
        assert_eq!(
            second.sections.iter().map(|s| s.id).collect::<Vec<_>>(),
            [2, 3]
        );
        assert_eq!(second.next_offset, None);
        options.offset = 0;
        options.section = Some(3);
        let content = inspect_document(root.path(), path, &options).unwrap();
        assert_eq!(content.sections.len(), 1);
        assert_eq!(content.sections[0].id, 3);
        assert_eq!(content.content.unwrap().text, "##");
    }

    #[test]
    fn unheaded_pages_and_invalid_ranges_are_explicit() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("page.md"), "근거").unwrap();
        let mut options = DocumentOptions {
            section: Some(0),
            offset: 0,
            limit: 10,
            revision: None,
        };
        assert_eq!(
            inspect_document(root.path(), Path::new("page.md"), &options)
                .unwrap()
                .content
                .unwrap()
                .text,
            "근거"
        );
        options.offset = 3;
        assert!(inspect_document(root.path(), Path::new("page.md"), &options).is_err());
        options.offset = 0;
        options.section = Some(1);
        assert!(inspect_document(root.path(), Path::new("page.md"), &options).is_err());
        assert!(inspect_document(root.path(), Path::new("../page.md"), &options).is_err());
    }
}
