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
use lk_core::frontmatter::parse_page;
use lk_core::link;
use lk_core::vault_path::{concepts_dir, documents_dir};

use crate::VaultError;

/// One concept the day named.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BriefEntry {
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
    let concepts_rel = concepts_dir(dirs);
    let mut learned = Vec::new();
    let mut revisited = Vec::new();

    for id in named_on(vault_root, dirs, date) {
        let rel = concepts_rel.join(format!("{id}.md"));
        let Ok(content) = std::fs::read_to_string(vault_root.join(&rel)) else {
            // A citation to a page that is not there is `lore graph lint`'s finding, and
            // answering it again here would put a second verdict beside it.
            continue;
        };
        let Ok(page) = parse_page(&content) else {
            tracing::warn!(concept = %id, "skipping page whose frontmatter will not parse");
            continue;
        };
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
                .unwrap_or(&id)
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
                    .map(|s| crate::index::truncate_summary(&s))
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
    // Most evidence first, then by address. A concept thirty pages cite is the one a reader
    // has most reason to have an opinion about; nothing here weighs that against anything.
    revisited.sort_by(|a, b| b.source_count.cmp(&a.source_count).then(a.id.cmp(&b.id)));
    Ok(Brief {
        date,
        learned,
        revisited,
    })
}

/// Every concept the day's own pages cite, by page id.
///
/// A day writes two kinds of page: one per source under `<daily>/{source}/{date}.md`, and a
/// document page for each file a person handed the vault that day. Both carry their concept
/// links in the body, so both are read — a link answers for the page that wrote it, which a
/// date on the concept cannot.
fn named_on(vault_root: &Path, dirs: &VaultDirs, date: jiff::civil::Date) -> BTreeSet<String> {
    let concepts_rel = concepts_dir(dirs);
    let mut named = BTreeSet::new();
    for rel in pages_of(vault_root, dirs, date) {
        let Ok(content) = std::fs::read_to_string(vault_root.join(&rel)) else {
            continue;
        };
        let body = match parse_page(&content) {
            Ok(page) => page.body,
            Err(_) => content,
        };
        for dest in link::extract_dests(&body) {
            let Some(resolved) = link::resolve_dest(&rel, &dest) else {
                continue;
            };
            // Gated exactly as the graph gates an edge: a destination landing in the concepts
            // directory with a `.md` name is a citation, and nothing else is.
            if resolved.parent() == Some(concepts_rel.as_path())
                && resolved.extension().is_some_and(|e| e == "md")
                && let Some(stem) = resolved.file_stem()
            {
                named.insert(stem.to_string_lossy().into_owned());
            }
        }
    }
    named
}

/// The vault-relative pages this date wrote.
fn pages_of(vault_root: &Path, dirs: &VaultDirs, date: jiff::civil::Date) -> Vec<PathBuf> {
    let mut pages = Vec::new();

    let daily_root = vault_root.join(&dirs.daily);
    if let Ok(sources) = std::fs::read_dir(&daily_root) {
        for source in sources.flatten() {
            if !source.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let rel = Path::new(&dirs.daily)
                .join(source.file_name())
                .join(format!("{date}.md"));
            if vault_root.join(&rel).is_file() {
                pages.push(rel);
            }
        }
    }

    // A document page carries no date in its address, so its own `created` is what places it.
    let documents_rel = documents_dir(dirs);
    if let Ok(entries) = std::fs::read_dir(vault_root.join(&documents_rel)) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "md") {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            let created = parse_page(&content).ok().and_then(|page| {
                page.frontmatter
                    .get("created")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<jiff::civil::Date>().ok())
            });
            if created == Some(date) {
                pages.push(documents_rel.join(entry.file_name()));
            }
        }
    }
    pages
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// A document page carries no date in its address, so a file handed to the vault on a day
    /// would go unread if only the daily directories were consulted.
    #[test]
    fn a_document_written_that_day_contributes_its_concepts() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        concept(
            &tmp.path().join(concepts_dir(&dirs)),
            "datasette",
            "2026-09-10",
            1,
        );
        write(
            &tmp.path().join(documents_dir(&dirs)).join("an-article.md"),
            "---\ntype: document\ncreated: 2026-09-10\n---\n\n## 관련 개념\n\n- [Datasette](../concepts/datasette.md)\n",
        );

        let brief = build_brief(tmp.path(), &dirs, jiff::civil::date(2026, 9, 10)).unwrap();
        assert_eq!(
            brief
                .learned
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>(),
            ["datasette"]
        );
    }
}
