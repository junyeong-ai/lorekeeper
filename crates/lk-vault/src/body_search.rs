//! Literal evidence in canonical Markdown bodies, rather than ranked visible prose.
//! A result is a matching physical line, not a semantic claim or an independent source.

use std::path::Path;

use lk_core::config::VaultDirs;
use regex::RegexBuilder;
use serde::Serialize;

use crate::VaultError;
use crate::search::{Page, SearchOptions, visit_pages};

const MAX_LIMIT: usize = 100;
const MAX_CONTEXT: usize = 5;
const CONTEXT_CHARACTERS: usize = 400;
const MAX_QUERY_BYTES: usize = 4096;

/// Scope, pagination and metadata filters have the same meaning as discovery.
/// A query is required. An explicit snapshot pins the captured Markdown bodies.
pub struct BodySearchOptions<'a> {
    pub search: SearchOptions<'a>,
    pub regex: bool,
    pub ignore_case: bool,
    pub context: usize,
}

impl Default for BodySearchOptions<'_> {
    fn default() -> Self {
        Self {
            search: SearchOptions::default(),
            regex: false,
            ignore_case: false,
            context: 1,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct BodyContextLine {
    /// One-based physical file line, including frontmatter.
    pub line: usize,
    /// One-based Unicode scalar column where this literal substring begins.
    pub column: usize,
    pub text: String,
    pub truncated_before: bool,
    pub truncated_after: bool,
}

#[derive(Debug, Serialize)]
pub struct BodySearchHit {
    pub path: String,
    pub title: String,
    pub file_revision: String,
    pub line: usize,
    /// First match on this line; columns and lengths count Unicode scalars, not bytes.
    pub column: usize,
    pub match_characters: usize,
    pub context: Vec<BodyContextLine>,
}

#[derive(Debug, Serialize)]
pub struct BodySearchResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    pub query: String,
    pub regex: bool,
    pub ignore_case: bool,
    pub hits: Vec<BodySearchHit>,
    /// Matching lines after scope and metadata filters, not substring occurrences.
    pub total: usize,
    pub matching_pages: usize,
    pub scanned_pages: usize,
    pub offset: usize,
    pub limit: usize,
    pub next_offset: Option<usize>,
    /// Some matching lines are outside this page of results; context has separate flags.
    pub truncated: bool,
    pub context_lines: usize,
    pub context_characters_per_line: usize,
}

/// Scan managed bodies with bounded output and exact totals. Scope/file/line order
/// is deterministic; without a snapshot, external edits can change later result pages.
/// Every hit carries the full file revision accepted by `inspect_document`.
///
/// Matching is line-oriented, case-sensitive literal substring by default. Regex and
/// Unicode-aware case folding are explicit. Markdown syntax, code and link destinations
/// remain searchable; frontmatter is excluded. BOM/CRLF follow the shared page parser.
/// Exact totals require a complete scan, regardless of the returned limit.
pub fn search_body(
    root: &Path,
    dirs: &VaultDirs,
    options: &BodySearchOptions<'_>,
) -> Result<BodySearchResult, VaultError> {
    let invalid = |message: &str| {
        VaultError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            message,
        ))
    };
    let search = &options.search;
    let query = search.query.unwrap_or_default();
    if query.trim().is_empty() || query.len() > MAX_QUERY_BYTES || query.contains(['\r', '\n']) {
        return Err(invalid(
            "body search requires a nonempty single-line query of at most 4096 bytes",
        ));
    }
    if !(1..=MAX_LIMIT).contains(&search.limit) || options.context > MAX_CONTEXT {
        return Err(invalid(
            "body search requires --limit 1..100 and --context 0..5",
        ));
    }
    let pattern = if options.regex {
        query.to_owned()
    } else {
        regex::escape(query)
    };
    let matcher = RegexBuilder::new(&pattern)
        .case_insensitive(options.ignore_case)
        .size_limit(1 << 20)
        .build()
        .map_err(|error| invalid(&format!("invalid body search pattern: {error}")))?;
    let mut hits = Vec::new();
    let mut total = 0;
    let mut matching_pages = 0;
    let mut scanned_pages = 0;
    let visit = |page: Page, line_offset| {
        if !page.matches_filters(search) {
            return Ok(());
        }
        scanned_pages += 1;
        let lines: Vec<_> = page.page.body.lines().collect();
        let mut page_matches = false;
        for (index, line) in lines.iter().enumerate() {
            let Some(found) = matcher.find(line) else {
                continue;
            };
            page_matches = true;
            total += 1;
            if total <= search.offset || hits.len() >= search.limit {
                continue;
            }
            let column = line[..found.start()].chars().count();
            let start = index.saturating_sub(options.context);
            let end = index
                .saturating_add(options.context)
                .saturating_add(1)
                .min(lines.len());
            let context = (start..end)
                .map(|at| {
                    let text = lines[at];
                    let characters = text.chars().count();
                    let from = if at == index {
                        column.saturating_sub(CONTEXT_CHARACTERS / 2)
                    } else {
                        0
                    };
                    let excerpt: String =
                        text.chars().skip(from).take(CONTEXT_CHARACTERS).collect();
                    let to = from + excerpt.chars().count();
                    BodyContextLine {
                        line: line_offset + at + 1,
                        column: from + 1,
                        text: excerpt,
                        truncated_before: from > 0,
                        truncated_after: to < characters,
                    }
                })
                .collect();
            hits.push(BodySearchHit {
                path: page.rel_path.to_string_lossy().into_owned(),
                title: page.title.clone(),
                file_revision: page.file_revision.clone(),
                line: line_offset + index + 1,
                column: column + 1,
                match_characters: line[found.range()].chars().count(),
                context,
            });
        }
        matching_pages += usize::from(page_matches);
        Ok(())
    };
    if let Some(snapshot) = search.snapshot {
        super::search_index::visit_body_pages(root, dirs, search.include_sources, snapshot, visit)?;
    } else {
        visit_pages(root, dirs, search.include_sources, visit)?;
    }
    let end = search.offset.saturating_add(hits.len());
    Ok(BodySearchResult {
        snapshot: search.snapshot.map(str::to_owned),
        query: query.into(),
        regex: options.regex,
        ignore_case: options.ignore_case,
        truncated: hits.len() < total,
        hits,
        total,
        matching_pages,
        scanned_pages,
        offset: search.offset,
        limit: search.limit,
        next_offset: (end < total).then_some(end),
        context_lines: options.context,
        context_characters_per_line: CONTEXT_CHARACTERS,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DocumentOptions, inspect_document};

    fn write(root: &Path, path: &str, text: &str) {
        let full = root.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, text).unwrap();
    }

    #[test]
    fn literal_locations_pagination_and_revision_checked_read() {
        let root = tempfile::tempdir().unwrap();
        let path = "wiki/documents/nested/a.md";
        let raw = "\u{feff}---\r\ntitle: Evidence\r\ntype: document\r\n---\r\n\r\n## Claim\r\n한글 [a+b](target.md) a+b\r\n```\r\na+b\r\n```\r\nA+B\r\n";
        write(root.path(), path, raw);
        let mut options = BodySearchOptions {
            search: SearchOptions {
                query: Some("a+b"),
                limit: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        let first = search_body(root.path(), &VaultDirs::default(), &options).unwrap();
        assert_eq!(first.total, 2); // lines, not three substring occurrences
        assert_eq!(first.matching_pages, 1);
        assert_eq!(first.next_offset, Some(1));
        assert!(first.truncated);
        let hit = &first.hits[0];
        assert_eq!(hit.path, path);
        assert_eq!(hit.line, 7);
        assert_eq!(hit.column, 5);
        assert_eq!(hit.context[1].text, "한글 [a+b](target.md) a+b");
        let read = DocumentOptions {
            section: Some(1),
            offset: 0,
            limit: 100,
            revision: Some(&hit.file_revision),
        };
        assert!(
            inspect_document(root.path(), Path::new(path), &read)
                .unwrap()
                .content
                .unwrap()
                .text
                .contains("a+b")
        );
        options.search.offset = 1;
        let next = search_body(root.path(), &VaultDirs::default(), &options).unwrap();
        assert_eq!(next.hits[0].line, 9);
        assert_eq!(next.next_offset, None);
        options.search.offset = usize::MAX;
        assert!(
            search_body(root.path(), &VaultDirs::default(), &options)
                .unwrap()
                .hits
                .is_empty()
        );
        write(root.path(), path, &raw.replace("Claim", "Changed"));
        assert!(inspect_document(root.path(), Path::new(path), &read).is_err());
    }

    #[test]
    fn scope_filters_regex_and_literal_markdown_boundaries() {
        let root = tempfile::tempdir().unwrap();
        write(
            root.path(),
            "wiki/concepts/a.md",
            "---\ntitle: MetadataOnly\ntype: concept\ncategory: ops\n---\n## Claim\n[link](secret-path.md)\nPROBE_3\n",
        );
        write(
            root.path(),
            "daily/mail/a.md",
            "---\ntype: daily\ncategory: ops\nsource_record: {status: active}\n---\nprobe-4\n",
        );
        write(
            root.path(),
            "me/tasks.md",
            "---\ntype: task-board\n---\nprobe-5\n",
        );
        let dirs = VaultDirs::default();
        let mut options = BodySearchOptions {
            search: SearchOptions {
                query: Some("MetadataOnly"),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(search_body(root.path(), &dirs, &options).unwrap().total, 0);
        options.search.query = Some("secret-path.md");
        assert_eq!(search_body(root.path(), &dirs, &options).unwrap().total, 1);
        options.search.query = Some("probe[-_]\\d");
        options.regex = true;
        options.ignore_case = true;
        assert_eq!(search_body(root.path(), &dirs, &options).unwrap().total, 1);
        options.search.include_sources = true;
        assert_eq!(search_body(root.path(), &dirs, &options).unwrap().total, 2);
        options.search.source_status = Some("active");
        let report = search_body(root.path(), &dirs, &options).unwrap();
        assert_eq!(report.total, 1);
        assert_eq!(report.scanned_pages, 1);
        options.search.category = Some("other");
        assert_eq!(search_body(root.path(), &dirs, &options).unwrap().total, 0);
        options.search.category = Some("ops");
        options.search.format = Some("concept");
        assert_eq!(search_body(root.path(), &dirs, &options).unwrap().total, 0);
        options.search.query = Some("[");
        assert!(search_body(root.path(), &dirs, &options).is_err());
        options.regex = false;
        options.search.snapshot = Some("snapshot");
        assert!(search_body(root.path(), &dirs, &options).is_err());
    }

    #[test]
    fn bounded_context_and_invalid_inputs_do_not_hide_partial_scans() {
        let root = tempfile::tempdir().unwrap();
        let long = format!("{}needle{}", "가".repeat(800), "나".repeat(800));
        write(
            root.path(),
            "wiki/documents/a.md",
            &format!("---\ntype: document\n---\n{long}\n"),
        );
        let dirs = VaultDirs::default();
        let mut options = BodySearchOptions {
            search: SearchOptions {
                query: Some("needle"),
                ..Default::default()
            },
            context: 0,
            ..Default::default()
        };
        let report = search_body(root.path(), &dirs, &options).unwrap();
        let hit = &report.hits[0];
        assert_eq!(hit.line, 4);
        assert_eq!(hit.column, 801);
        assert_eq!(hit.context[0].text.chars().count(), 400);
        assert!(hit.context[0].text.contains("needle"));
        assert!(hit.context[0].truncated_before && hit.context[0].truncated_after);
        options.search.limit = 101;
        assert!(search_body(root.path(), &dirs, &options).is_err());
        options.search.limit = 20;
        options.context = 6;
        assert!(search_body(root.path(), &dirs, &options).is_err());
        options.context = 0;
        write(
            root.path(),
            "wiki/documents/b.md",
            "---\ninvalid: [\n---\nneedle\n",
        );
        assert!(search_body(root.path(), &dirs, &options).is_err());
    }

    #[test]
    fn snapshot_preserves_body_locations_scope_and_pagination_until_reprepared() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let path = "wiki/documents/a.md";
        write(
            root.path(),
            path,
            "\u{feff}---\r\ntitle: a+b metadata\r\ntype: document\r\ncategory: ops\r\nsource_record: {status: archived}\r\n---\r\n## Claim\r\n한글 [a+b](hidden-target.md)\r\n```\r\nA+B\r\n```\r\na+b",
        );
        write(
            root.path(),
            "wiki/concepts/b.md",
            "---\ntype: concept\n---\na+b\n",
        );
        write(
            root.path(),
            "daily/feed/a.md",
            "---\ntype: daily\n---\na+b\n",
        );
        let session = crate::MutationSession::acquire(root.path()).unwrap();
        let snapshot = crate::prepare_search(&session, &dirs).unwrap().snapshot;
        drop(session);
        for (query, regex, ignore_case) in [
            ("a+b", false, false),
            ("a\\+b", true, true),
            ("hidden-target.md", false, false),
            ("한글", false, false),
            ("metadata", false, false),
        ] {
            for include_sources in [false, true] {
                for filtered in [false, true] {
                    for offset in [0, 1, 10] {
                        let mut options = BodySearchOptions {
                            search: SearchOptions {
                                query: Some(query),
                                include_sources,
                                format: filtered.then_some("document"),
                                category: filtered.then_some("ops"),
                                source_status: filtered.then_some("archived"),
                                limit: 1,
                                offset,
                                ..Default::default()
                            },
                            regex,
                            ignore_case,
                            context: 1,
                        };
                        let live = search_body(root.path(), &dirs, &options).unwrap();
                        options.search.snapshot = Some(&snapshot);
                        let mut captured = search_body(root.path(), &dirs, &options).unwrap();
                        assert_eq!(captured.snapshot.take().as_deref(), Some(snapshot.as_str()));
                        assert_eq!(
                            serde_json::to_value(captured).unwrap(),
                            serde_json::to_value(live).unwrap()
                        );
                    }
                }
            }
        }
        let mut options = BodySearchOptions {
            search: SearchOptions {
                query: Some("hidden-target.md"),
                snapshot: Some(&snapshot),
                ..Default::default()
            },
            ..Default::default()
        };
        let captured = search_body(root.path(), &dirs, &options).unwrap();
        assert_eq!(captured.hits[0].line, 8);
        write(root.path(), path, "---\ntype: document\n---\nchanged\n");
        assert_eq!(search_body(root.path(), &dirs, &options).unwrap().total, 1);
        let read = DocumentOptions {
            revision: Some(&captured.hits[0].file_revision),
            section: None,
            offset: 0,
            limit: 20,
        };
        assert!(inspect_document(root.path(), Path::new(path), &read).is_err());
        options.search.snapshot = None;
        assert_eq!(search_body(root.path(), &dirs, &options).unwrap().total, 0);
        let session = crate::MutationSession::acquire(root.path()).unwrap();
        crate::prepare_search(&session, &dirs).unwrap();
        drop(session);
        options.search.snapshot = Some(&snapshot);
        assert!(search_body(root.path(), &dirs, &options).is_err());
    }
}
