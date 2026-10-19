//! What the vault learned on one day.
//!
//! Seventy items a day arrive and nobody reads them; the concept pages are where they were
//! already reduced to what is worth keeping, so a day's reading is that day's concepts rather
//! than its sources. Roughly ten enter and thirty are named again.
//!
//! Those two are different things and the split is the whole reduction. A concept the vault
//! did not hold before is something learned, and it is read. One it already held, named again,
//! is the vault confirming what it knows — worth naming, not worth restating. So the first
//! carries what each page says and the second carries names only.
//!
//! Which concepts a day named is read off THAT DAY'S PAGES, never off a field on the concept
//! itself. A concept page records when it was last cited and not every day it was, so a
//! concept introduced on Tuesday and named again on Wednesday would be missing from Tuesday —
//! and, no longer new on Wednesday, missing from every day. The pages a day wrote are the
//! evidence of what that day named, and they do not move.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use lk_core::config::VaultDirs;
use lk_core::link;
use lk_core::vault_path::{concepts_dir, documents_dir, explorations_dir};

use crate::VaultError;

/// One concept the day named.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BriefEntry {
    /// Extensionless address relative to the concepts collection, including folders.
    pub id: String,
    pub path: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    pub source_count: u64,
    /// The page's opening statement. Carried for a concept the day introduced and omitted for
    /// one named again, which the reader already knows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub statement: Option<String>,
}

/// A day's knowledge, split by whether the vault already held it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Brief {
    pub date: jiff::civil::Date,
    /// Concepts the day introduced, grouped by category and most-cited first within each —
    /// the vault's own axis, which is what lets a reader skip a group whole rather than read
    /// every line to find the ones that concern them.
    pub learned: Vec<BriefEntry>,
    /// Concepts the vault already held that the day named again.
    pub revisited: Vec<BriefEntry>,
}

/// Read the day off the pages it wrote.
pub fn build_brief(
    vault_root: &Path,
    dirs: &VaultDirs,
    date: jiff::civil::Date,
) -> Result<Brief, VaultError> {
    let mut learned = Vec::new();
    let mut revisited = Vec::new();

    for rel in named_on(vault_root, dirs, date)? {
        let stem = rel.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
        let address = rel
            .strip_prefix(concepts_dir(dirs))
            .map_err(std::io::Error::other)?
            .with_extension("");
        let id = address
            .to_str()
            .unwrap_or_default()
            .replace(std::path::MAIN_SEPARATOR, "/");
        let page = match crate::store::read_page(vault_root, &vault_root.join(&rel)) {
            Ok(page) => page,
            Err(VaultError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if page
            .frontmatter
            .knowledge_status()
            .map_err(VaultError::Frontmatter)?
            == lk_core::frontmatter::KnowledgeStatus::Retired
        {
            continue;
        }
        let introduced = page
            .frontmatter
            .get("created")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<jiff::civil::Date>().ok())
            == Some(date);
        let entry = BriefEntry {
            title: page
                .frontmatter
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or(stem)
                .to_string(),
            path: rel.to_string_lossy().into_owned(),
            category: page
                .frontmatter
                .get("category")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from),
            source_count: page.frontmatter.source_count().unwrap_or(0),
            statement: introduced.then(|| {
                crate::search::opening_statement(&link::strip_links(&page.body))
                    .map(|s| crate::summary::truncate_summary(&s))
                    .unwrap_or_default()
            }),
            id,
        };
        if introduced {
            &mut learned
        } else {
            &mut revisited
        }
        .push(entry);
    }

    learned.sort_by(|a, b| {
        a.category
            .cmp(&b.category)
            .then(b.source_count.cmp(&a.source_count))
            .then(a.id.cmp(&b.id))
    });
    // Citing-page count orders navigation; it does not establish independent evidence.
    revisited.sort_by(|a, b| b.source_count.cmp(&a.source_count).then(a.id.cmp(&b.id)));
    Ok(Brief {
        date,
        learned,
        revisited,
    })
}

/// Every concept the day's own pages cite, by page id.
///
/// A day writes three kinds of page: one per source under `<daily>/{source}/{date}.md`, a
/// document page for each file handed to the vault, and an exploration for each question
/// answered from it. Each carries its concept links in the body, so each is read — a link
/// answers for the page that wrote it, which a date on the concept cannot.
///
/// A concept page NOTHING cites is absent from every day by construction, which is the rule
/// working rather than a gap: the day reports what its own material named, and a page with no
/// citation was named by nothing. Such a page is `lore graph lint`'s orphan finding.
fn named_on(
    vault_root: &Path,
    dirs: &VaultDirs,
    date: jiff::civil::Date,
) -> Result<BTreeSet<PathBuf>, VaultError> {
    let concepts_rel = concepts_dir(dirs);
    let paths = crate::search::markdown_files(vault_root, &vault_root.join(&concepts_rel))?
        .into_iter()
        .filter_map(|path| path.strip_prefix(vault_root).ok().map(Path::to_path_buf));
    let mut resolver = crate::ExistingPageResolver::new(vault_root, paths)?;
    let mut named = BTreeSet::new();
    for rel in pages_of(vault_root, dirs, date)? {
        let body = crate::store::read_page(vault_root, &vault_root.join(&rel))?.body;
        for dest in link::extract_dests(&body) {
            let Some(resolved) = link::resolve_dest(&rel, &dest) else {
                continue;
            };
            if resolved.extension().is_some_and(|e| e == "md")
                && let Some(actual) = resolver.resolve(&resolved)?
                && actual.starts_with(&concepts_rel)
            {
                named.insert(actual);
            }
        }
    }
    Ok(named)
}

/// The vault-relative pages this date wrote.
fn pages_of(
    vault_root: &Path,
    dirs: &VaultDirs,
    date: jiff::civil::Date,
) -> Result<Vec<PathBuf>, VaultError> {
    let mut pages = Vec::new();

    let daily_root = vault_root.join(&dirs.daily);
    let sources = match std::fs::read_dir(&daily_root) {
        Ok(sources) => Some(sources),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if let Some(sources) = sources {
        for source in sources {
            let source = source?;
            if !source.file_type()?.is_dir() {
                continue;
            }
            let rel = Path::new(&dirs.daily)
                .join(source.file_name())
                .join(format!("{date}.md"));
            match std::fs::metadata(vault_root.join(&rel)) {
                Ok(metadata) if metadata.is_file() => pages.push(rel),
                Ok(_) => {
                    return Err(std::io::Error::other(format!(
                        "expected source page: {}",
                        rel.display()
                    ))
                    .into());
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    // Neither a document nor an exploration carries a date in its address, so its own
    // `created` is what places it. Walked rather than listed, and by the same walker the
    // search uses: a page filed into a subdirectory is one the reader can find, so a day that
    // could not see it would report a different vault than the one they search.
    for rel_dir in [documents_dir(dirs), explorations_dir(dirs)] {
        for path in crate::search::markdown_files(vault_root, &vault_root.join(&rel_dir))? {
            let page = crate::store::read_page(vault_root, &path)?;
            let created = page
                .frontmatter
                .get("created")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<jiff::civil::Date>().ok());
            if created == Some(date)
                && let Ok(rel) = path.strip_prefix(vault_root)
            {
                pages.push(rel.to_path_buf());
            }
        }
    }
    Ok(pages)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brief_keeps_nested_active_knowledge_and_excludes_both_retired_groups() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let concepts = root.path().join(concepts_dir(&dirs));
        concept(&concepts, "group/active", "2026-09-10", 1);
        write(
            &concepts.join("other/active.md"),
            "---\ntype: concept\ncreated: 2026-01-01\n---\nAnother active concept.\n",
        );
        for (name, created) in [("retired-new", "2026-09-10"), ("retired-old", "2026-01-01")] {
            write(
                &concepts.join(format!("{name}.md")),
                &format!(
                    "---\ntype: concept\ntitle: {name}\ncreated: {created}\nknowledge_status: retired\n---\n## Synthesis\nHistorical unsupported claim.\n"
                ),
            );
        }
        write(
            &root.path().join("daily/news/2026-09-10.md"),
            "---\ntype: daily\n---\n[Active](../../wiki/concepts/group/active.md)\n[Other](../../wiki/concepts/other/active.md)\n[New](../../wiki/concepts/retired-new.md)\n[Old](../../wiki/concepts/retired-old.md)\n",
        );
        let brief = build_brief(root.path(), &dirs, jiff::civil::date(2026, 9, 10)).unwrap();
        assert_eq!(brief.learned.len(), 1);
        assert_eq!(brief.learned[0].id, "group/active");
        assert_eq!(brief.learned[0].path, "wiki/concepts/group/active.md");
        assert_eq!(brief.revisited.len(), 1);
        assert_eq!(brief.revisited[0].id, "other/active");
        assert_eq!(brief.revisited[0].path, "wiki/concepts/other/active.md");
        assert_eq!(brief.revisited[0].title, "active");
        write(
            &concepts.join("retired-new.md"),
            "---\ntype: concept\nknowledge_status: [retired]\n---\nBody.",
        );
        assert!(build_brief(root.path(), &dirs, jiff::civil::date(2026, 9, 10)).is_err());
    }

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn concept(dir: &Path, id: &str, created: &str, count: u64) {
        write(
            &dir.join(format!("{id}.md")),
            &format!(
                "---\ntype: concept\ntitle: \"{id}\"\ncreated: {created}\nupdated: 2026-09-20\nsource_count: {count}\n---\n\n## 핵심\n\n{id}에 대한 한 줄.\n"
            ),
        );
    }

    /// The defect that reading the day's own pages avoids: a concept page records when it was
    /// LAST cited, not every day it was, so a concept introduced on one day and named again on
    /// the next went missing from the first — and, no longer new on the second, from every day.
    #[test]
    fn brief_uses_the_actual_owner_of_a_filesystem_case_alias() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let concepts = root.path().join(concepts_dir(&dirs));
        concept(&concepts, "topic", "2026-09-10", 1);
        write(
            &root.path().join("daily/news/2026-09-10.md"),
            "---\ntype: daily\n---\n[Topic](../../wiki/concepts/TOPIC.md)\n",
        );
        let exists = concepts.join("TOPIC.md").is_file();
        let brief = build_brief(root.path(), &dirs, jiff::civil::date(2026, 9, 10)).unwrap();
        assert_eq!(brief.learned.len(), usize::from(exists));
        if exists {
            assert_eq!(brief.learned[0].id, "topic");
            assert_eq!(brief.learned[0].path, "wiki/concepts/topic.md");
        }
    }

    #[test]
    fn a_concept_named_again_later_still_belongs_to_the_day_it_was_introduced() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let concepts = tmp.path().join(concepts_dir(&dirs));
        concept(&concepts, "react-native", "2026-09-10", 2);
        concept(&concepts, "vllm", "2026-05-01", 9);
        concept(&concepts, "untouched", "2026-05-01", 4);

        write(
            &tmp.path()
                .join(&dirs.daily)
                .join("news")
                .join("2026-09-10.md"),
            "---\ntype: daily\n---\n\n## 관련 개념\n\n- [React Native](../../wiki/concepts/react-native.md)\n- [vLLM](../../wiki/concepts/vllm.md)\n",
        );
        // The next day names it again — the move that used to erase it from both days.
        write(
            &tmp.path()
                .join(&dirs.daily)
                .join("news")
                .join("2026-09-11.md"),
            "---\ntype: daily\n---\n\n## 관련 개념\n\n- [React Native](../../wiki/concepts/react-native.md)\n",
        );

        let first = build_brief(tmp.path(), &dirs, jiff::civil::date(2026, 9, 10)).unwrap();
        assert_eq!(
            first
                .learned
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>(),
            ["react-native"],
            "the day that introduced it keeps it however often it is named later"
        );
        assert_eq!(
            first
                .revisited
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>(),
            ["vllm"]
        );
        assert!(first.learned[0].statement.is_some());
        assert!(
            first.revisited[0].statement.is_none(),
            "restating what the reader already knows is the flood this exists to reduce"
        );

        let second = build_brief(tmp.path(), &dirs, jiff::civil::date(2026, 9, 11)).unwrap();
        assert!(second.learned.is_empty());
        assert_eq!(
            second
                .revisited
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>(),
            ["react-native"],
            "a day that named an established concept reports it as one"
        );
    }

    /// Neither a document nor an exploration carries a date in its address, so a file handed
    /// to the vault and a question answered from it would both go unread if only the daily
    /// directories were consulted — and a skill that writes one directly is exactly the case
    /// with no daily page behind it.
    #[test]
    fn a_document_or_exploration_written_that_day_contributes_its_concepts() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let concepts = tmp.path().join(concepts_dir(&dirs));
        concept(&concepts, "datasette", "2026-09-10", 1);
        concept(&concepts, "nl2sql", "2026-09-10", 1);
        write(
            &tmp.path().join(documents_dir(&dirs)).join("an-article.md"),
            "---\ntype: document\ncreated: 2026-09-10\n---\n\n## 관련 개념\n\n- [Datasette](../concepts/datasette.md)\n",
        );
        write(
            &tmp.path()
                .join(explorations_dir(&dirs))
                .join("a-question.md"),
            "---\ntype: exploration\ncreated: 2026-09-10\n---\n\n## 근거\n\n- [NL2SQL](../concepts/nl2sql.md)\n",
        );

        let brief = build_brief(tmp.path(), &dirs, jiff::civil::date(2026, 9, 10)).unwrap();
        assert_eq!(
            brief
                .learned
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>(),
            ["datasette", "nl2sql"]
        );
    }
}
