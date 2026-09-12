//! What the vault learned on one day.
//!
//! Seventy items a day arrive and nobody reads them; the concept pages are where they were
//! already reduced to what is worth keeping, so a day's reading is that day's concepts rather
//! than its sources. Roughly ten enter and thirty are touched again.
//!
//! Those two are different things and the split is the whole reduction. A concept the vault
//! did not hold yesterday is something learned, and it is read. One it already held, cited
//! once more, is the vault confirming what it knows — worth naming, not worth restating. So
//! the first carries what each page says and the second carries names only.

use std::path::Path;

use lk_core::config::VaultDirs;
use lk_core::frontmatter::parse_page;
use lk_core::link;
use lk_core::vault_path::concepts_dir;

use crate::VaultError;

/// One concept the day touched.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BriefEntry {
    pub id: String,
    pub path: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    pub source_count: u64,
    /// The page's opening statement. Carried for a concept that entered today and omitted for
    /// one merely cited again, which the reader already knows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub statement: Option<String>,
}

/// A day's knowledge, split by whether the vault already held it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Brief {
    pub date: jiff::civil::Date,
    /// Concepts the vault did not hold before this day, grouped by category and most-cited
    /// first within each — the vault's own axis, which is what lets a reader skip a group
    /// whole rather than read every line to find the ones that concern them.
    pub learned: Vec<BriefEntry>,
    /// Concepts it already held and cited again.
    pub revisited: Vec<BriefEntry>,
}

/// Read the day off the concept pages.
///
/// `created` and `updated` are the pages' own fields, written by the ingest that observed
/// them, so the same day re-read answers the same way however long afterwards — this is a
/// view of what the vault records, not of when it was run.
pub fn build_brief(
    vault_root: &Path,
    dirs: &VaultDirs,
    date: jiff::civil::Date,
) -> Result<Brief, VaultError> {
    let rel_dir = concepts_dir(dirs);
    let mut learned = Vec::new();
    let mut revisited = Vec::new();

    for path in crate::search::markdown_files(&vault_root.join(&rel_dir)) {
        let Ok(content) = std::fs::read_to_string(&path) else {
            tracing::warn!(path = %path.display(), "skipping unreadable page");
            continue;
        };
        let Ok(page) = parse_page(&content) else {
            tracing::warn!(path = %path.display(), "skipping page whose frontmatter will not parse");
            continue;
        };
        let day = |key: &str| {
            page.frontmatter
                .get(key)
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<jiff::civil::Date>().ok())
        };
        if day("updated") != Some(date) {
            continue;
        }
        let id = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let is_new = day("created") == Some(date);
        let entry = BriefEntry {
            title: page
                .frontmatter
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or(&id)
                .to_string(),
            path: rel_dir
                .join(path.file_name().unwrap_or_default())
                .to_string_lossy()
                .into_owned(),
            category: page
                .frontmatter
                .get("category")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from),
            source_count: page.frontmatter.source_count().unwrap_or(0),
            statement: is_new.then(|| {
                crate::search::opening_statement(&page.body)
                    .map(|s| crate::index::truncate_summary(&link::strip_links(&s)))
                    .unwrap_or_default()
            }),
            id,
        };
        if is_new { &mut learned } else { &mut revisited }.push(entry);
    }

    // Most evidence first, then by address. A concept thirty pages cite is the one a reader
    // has most reason to have an opinion about; nothing here weighs that against anything.
    learned.sort_by(|a, b| {
        a.category
            .cmp(&b.category)
            .then(b.source_count.cmp(&a.source_count))
            .then(a.id.cmp(&b.id))
    });
    revisited.sort_by(|a, b| b.source_count.cmp(&a.source_count).then(a.id.cmp(&b.id)));
    Ok(Brief {
        date,
        learned,
        revisited,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, created: &str, updated: &str, count: u64) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(name),
            format!(
                "---\ntype: concept\ntitle: \"{name}\"\ncreated: {created}\nupdated: {updated}\nsource_count: {count}\n---\n\n## 핵심\n\n{name}에 대한 한 줄.\n"
            ),
        )
        .unwrap();
    }

    /// The split the reduction rests on: what the vault did not hold before is read, what it
    /// already held is named. A day that touched forty concepts is ten statements, not forty.
    #[test]
    fn a_concept_the_vault_already_held_is_named_and_not_restated() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let concepts = tmp.path().join(concepts_dir(&dirs));
        write(&concepts, "new-today.md", "2026-09-12", "2026-09-12", 1);
        write(&concepts, "cited-again.md", "2026-05-01", "2026-09-12", 9);
        write(&concepts, "untouched.md", "2026-05-01", "2026-09-11", 4);

        let brief = build_brief(tmp.path(), &dirs, jiff::civil::date(2026, 9, 12)).unwrap();
        assert_eq!(
            brief
                .learned
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>(),
            ["new-today"]
        );
        assert_eq!(
            brief
                .revisited
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>(),
            ["cited-again"],
            "a day the page did not move is not this day's reading"
        );
        assert!(brief.learned[0].statement.is_some());
        assert!(
            brief.revisited[0].statement.is_none(),
            "restating what the reader already knows is the flood this exists to reduce"
        );
    }
}
