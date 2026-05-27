//! Detect vault pages whose `updated` frontmatter date is older than a threshold.
//!
//! Purely deterministic — no heuristics, no LLM. A page is "stale" iff its
//! frontmatter `updated` (or `created`, if `updated` is absent) is strictly older
//! than `today - threshold_days`. Pages with neither field are skipped, never
//! reported, because a missing date is a separate condition from "old".

use std::path::{Path, PathBuf};

use lk_core::frontmatter::{self, Frontmatter};
use serde::Serialize;

use crate::GraphError;
use crate::scan::Page;

/// A vault page that exceeds the staleness threshold.
#[derive(Debug, Clone, Serialize)]
pub struct StalePage {
    /// Vault-relative path (e.g. `wiki/concepts/old-topic.md`).
    pub path: PathBuf,
    /// Frontmatter date that triggered the report (`updated`, or `created` as fallback).
    pub updated: jiff::civil::Date,
    /// Days between `updated` and `today` (always positive — see filter in
    /// [`find_stale`]).
    pub days_old: i64,
    /// Coarse vault category derived from the path prefix.
    pub category: Category,
}

/// Coarse vault category used to group stale pages in the report. The order of
/// variants is also the canonical display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Category {
    WikiConcepts,
    WikiDocuments,
    WikiExplorations,
    Daily,
    MeWorkLog,
    Weekly,
    Monthly,
    Quarterly,
    Annually,
    Other,
}

impl Category {
    /// Resolve the category from a vault-relative path. Path prefixes are matched
    /// in declaration order — `Other` is the catch-all.
    pub fn from_path(path: &Path) -> Self {
        let s = path.to_string_lossy().replace('\\', "/");
        // Order matters: `me/work-log/` is more specific than `me/`.
        for (prefix, cat) in [
            ("wiki/concepts/", Category::WikiConcepts),
            ("wiki/documents/", Category::WikiDocuments),
            ("wiki/explorations/", Category::WikiExplorations),
            ("daily/", Category::Daily),
            ("me/work-log/", Category::MeWorkLog),
            ("weekly/", Category::Weekly),
            ("monthly/", Category::Monthly),
            ("quarterly/", Category::Quarterly),
            ("annually/", Category::Annually),
        ] {
            if s.starts_with(prefix) {
                return cat;
            }
        }
        Category::Other
    }

    /// Display label for the category — same form the report prints under.
    pub fn label(self) -> &'static str {
        match self {
            Category::WikiConcepts => "wiki/concepts",
            Category::WikiDocuments => "wiki/documents",
            Category::WikiExplorations => "wiki/explorations",
            Category::Daily => "daily",
            Category::MeWorkLog => "me/work-log",
            Category::Weekly => "weekly",
            Category::Monthly => "monthly",
            Category::Quarterly => "quarterly",
            Category::Annually => "annually",
            Category::Other => "other",
        }
    }
}

/// Walk `pages` and return every page whose `updated` (or `created`) is more than
/// `threshold_days` before `today`. Pages with neither field are skipped.
///
/// Result ordering: by descending `days_old`, then by path for determinism. The
/// caller is responsible for grouping by category (the [`StalePage::category`]
/// field is precomputed for that).
pub fn find_stale(
    pages: &[Page],
    vault_root: &Path,
    today: jiff::civil::Date,
    threshold_days: u32,
) -> Result<Vec<StalePage>, GraphError> {
    let mut stale = Vec::new();

    for page in pages {
        let full = vault_root.join(&page.path);
        let raw = std::fs::read_to_string(&full)
            .map_err(|e| GraphError::Io(format!("failed to read {}: {e}", full.display())))?;
        let parsed = frontmatter::parse_page(&raw)
            .map_err(|e| GraphError::Io(format!("frontmatter in {}: {e}", full.display())))?;

        let Some(date) = extract_date(&parsed.frontmatter) else {
            continue;
        };

        let days_old = today.duration_since(date).as_secs() / 86_400;
        if days_old <= i64::from(threshold_days) {
            continue;
        }

        stale.push(StalePage {
            path: page.path.clone(),
            updated: date,
            days_old,
            category: Category::from_path(&page.path),
        });
    }

    // Stable, deterministic ordering: oldest first, then by path.
    stale.sort_by(|a, b| {
        b.days_old
            .cmp(&a.days_old)
            .then_with(|| a.path.cmp(&b.path))
    });

    Ok(stale)
}

/// Parse `updated` from the frontmatter, falling back to `created`. Returns `None`
/// if both are missing or unparseable.
fn extract_date(fm: &Frontmatter) -> Option<jiff::civil::Date> {
    for key in ["updated", "created"] {
        if let Some(s) = fm.get(key).and_then(|v| v.as_str())
            && let Ok(d) = s.parse::<jiff::civil::Date>()
        {
            return Some(d);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(dir: &TempDir, rel: &str, body: &str) {
        let path = dir.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();
    }

    fn page(id: &str, rel: &str) -> Page {
        Page {
            id: id.to_owned(),
            path: PathBuf::from(rel),
            title: id.to_owned(),
            outgoing: vec![],
        }
    }

    fn today() -> jiff::civil::Date {
        jiff::civil::date(2026, 5, 24)
    }

    #[test]
    fn finds_no_pages_under_threshold() {
        let dir = TempDir::new().unwrap();
        // updated 10 days before today — must NOT trigger at threshold=90.
        write(
            &dir,
            "wiki/concepts/fresh.md",
            "---\nupdated: 2026-05-14\n---\n\nbody\n",
        );
        let pages = vec![page("wiki/concepts/fresh", "wiki/concepts/fresh.md")];
        let stale = find_stale(&pages, dir.path(), today(), 90).unwrap();
        assert!(stale.is_empty());
    }

    #[test]
    fn finds_pages_older_than_threshold() {
        let dir = TempDir::new().unwrap();
        // 180 days old → must trigger at threshold=90.
        write(
            &dir,
            "wiki/concepts/old.md",
            "---\nupdated: 2025-11-25\n---\n\nbody\n",
        );
        // Exactly at threshold (90 days) → must NOT trigger (strict `>`).
        write(
            &dir,
            "wiki/concepts/edge.md",
            "---\nupdated: 2026-02-23\n---\n\nbody\n",
        );
        let pages = vec![
            page("wiki/concepts/old", "wiki/concepts/old.md"),
            page("wiki/concepts/edge", "wiki/concepts/edge.md"),
        ];
        let stale = find_stale(&pages, dir.path(), today(), 90).unwrap();
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].path, PathBuf::from("wiki/concepts/old.md"));
        assert_eq!(stale[0].days_old, 180);
        assert_eq!(stale[0].category, Category::WikiConcepts);
    }

    #[test]
    fn falls_back_to_created_when_updated_missing() {
        let dir = TempDir::new().unwrap();
        write(
            &dir,
            "wiki/concepts/c.md",
            "---\ncreated: 2025-11-25\n---\n\nbody\n",
        );
        let pages = vec![page("wiki/concepts/c", "wiki/concepts/c.md")];
        let stale = find_stale(&pages, dir.path(), today(), 90).unwrap();
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].updated, jiff::civil::date(2025, 11, 25));
    }

    #[test]
    fn skips_pages_without_either_field() {
        let dir = TempDir::new().unwrap();
        write(&dir, "wiki/concepts/n.md", "no frontmatter at all\n");
        write(
            &dir,
            "wiki/concepts/o.md",
            "---\ntitle: only title\n---\n\nbody\n",
        );
        let pages = vec![
            page("wiki/concepts/n", "wiki/concepts/n.md"),
            page("wiki/concepts/o", "wiki/concepts/o.md"),
        ];
        let stale = find_stale(&pages, dir.path(), today(), 90).unwrap();
        assert!(stale.is_empty());
    }

    #[test]
    fn groups_correctly_by_path_prefix() {
        assert_eq!(
            Category::from_path(Path::new("wiki/concepts/x.md")),
            Category::WikiConcepts
        );
        assert_eq!(
            Category::from_path(Path::new("wiki/documents/x.md")),
            Category::WikiDocuments
        );
        assert_eq!(
            Category::from_path(Path::new("wiki/explorations/x.md")),
            Category::WikiExplorations
        );
        assert_eq!(
            Category::from_path(Path::new("daily/ai-news/2026-05-23.md")),
            Category::Daily
        );
        assert_eq!(
            Category::from_path(Path::new("me/work-log/2026-05-23.md")),
            Category::MeWorkLog
        );
        assert_eq!(
            Category::from_path(Path::new("weekly/synthesis/2026-W21.md")),
            Category::Weekly
        );
        assert_eq!(
            Category::from_path(Path::new("monthly/me/2026-05.md")),
            Category::Monthly
        );
        assert_eq!(
            Category::from_path(Path::new("quarterly/me/2026-Q2.md")),
            Category::Quarterly
        );
        assert_eq!(
            Category::from_path(Path::new("annually/me/2026.md")),
            Category::Annually
        );
        // Anything outside the known prefixes → Other.
        assert_eq!(
            Category::from_path(Path::new("notes/random.md")),
            Category::Other
        );
        // `me/` without `work-log/` is not a recognised category.
        assert_eq!(
            Category::from_path(Path::new("me/other.md")),
            Category::Other
        );
    }
}
