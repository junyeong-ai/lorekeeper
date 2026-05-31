//! Detect vault pages whose `updated` frontmatter date is older than a threshold.
//!
//! Purely deterministic — no heuristics, no LLM. A page is "stale" iff its
//! frontmatter `updated` (or `created`, if `updated` is absent) is strictly older
//! than `today - threshold_days`. Pages with neither field are skipped, never
//! reported, because a missing date is a separate condition from "old".

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use lk_core::config::VaultDirs;
use lk_core::frontmatter::{self, Frontmatter};
use serde::Serialize;

use crate::GraphError;
use crate::scan::{ScannedPage, VaultExistence, is_valid_source};

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
    Annual,
    Other,
}

impl Category {
    /// Resolve the category from a vault-relative path. Path prefixes are matched
    /// in declaration order — `Other` is the catch-all.
    pub fn from_path(path: &Path, dirs: &VaultDirs) -> Self {
        let s = path.to_string_lossy().replace('\\', "/");
        // Order matters: more specific prefixes first.
        let prefixes: [(String, Category); 10] = [
            (format!("{}/concepts/", dirs.wiki), Category::WikiConcepts),
            (format!("{}/documents/", dirs.wiki), Category::WikiDocuments),
            (
                format!("{}/explorations/", dirs.wiki),
                Category::WikiExplorations,
            ),
            (format!("{}/", dirs.daily), Category::Daily),
            (format!("{}/work-log/", dirs.personal), Category::MeWorkLog),
            (
                format!("{}/{}/", dirs.personal, dirs.weekly),
                Category::Weekly,
            ),
            (
                format!("{}/{}/", dirs.synthesis, dirs.weekly),
                Category::Weekly,
            ),
            (
                format!("{}/{}/", dirs.personal, dirs.monthly),
                Category::Monthly,
            ),
            (
                format!("{}/{}/", dirs.personal, dirs.quarterly),
                Category::Quarterly,
            ),
            (
                format!("{}/{}/", dirs.personal, dirs.annual),
                Category::Annual,
            ),
        ];
        for (prefix, cat) in &prefixes {
            if s.starts_with(prefix) {
                return *cat;
            }
        }
        Category::Other
    }

    /// Display label for the category, built from the configured directory names.
    pub fn label(self, dirs: &VaultDirs) -> String {
        match self {
            Category::WikiConcepts => format!("{}/concepts", dirs.wiki),
            Category::WikiDocuments => format!("{}/documents", dirs.wiki),
            Category::WikiExplorations => format!("{}/explorations", dirs.wiki),
            Category::Daily => dirs.daily.clone(),
            Category::MeWorkLog => format!("{}/work-log", dirs.personal),
            Category::Weekly => dirs.weekly.clone(),
            Category::Monthly => format!("{}/{}", dirs.personal, dirs.monthly),
            Category::Quarterly => format!("{}/{}", dirs.personal, dirs.quarterly),
            Category::Annual => format!("{}/{}", dirs.personal, dirs.annual),
            Category::Other => "other".to_string(),
        }
    }
}

/// Return every candidate in `pages` that is both **old** and **dormant**: its
/// `updated` (or `created`) is more than `threshold_days` before `today`, AND it has
/// no incoming citation from a page that is itself recent. Pages with neither date
/// field are skipped.
///
/// `all_pages` is the full-vault universe used only to derive incoming-citation
/// recency — a concept still cited by this week's daily notes is *live*, not stale,
/// even when its own `updated` is old. This is the deterministic, graph-derived line
/// between "old" and "actually dormant": no heuristic, no content inspection. Callers
/// pass the report scope as `pages` and the whole vault as `all_pages` (a superset).
///
/// Result ordering: by descending `days_old`, then by path for determinism. The
/// caller is responsible for grouping by category (the [`StalePage::category`]
/// field is precomputed for that).
pub fn find_stale(
    pages: &[ScannedPage],
    all_pages: &[ScannedPage],
    vault_root: &Path,
    today: jiff::civil::Date,
    threshold_days: u32,
    dirs: &VaultDirs,
) -> Result<Vec<StalePage>, GraphError> {
    // Read every page's date once, keyed by page id. A page dated by neither
    // `updated` nor `created` is absent from the map and contributes no recency.
    let mut date_by_id: HashMap<&str, jiff::civil::Date> = HashMap::with_capacity(all_pages.len());
    for page in all_pages {
        let full = vault_root.join(&page.path);
        let raw = std::fs::read_to_string(&full)
            .map_err(|e| GraphError::Io(format!("failed to read {}: {e}", full.display())))?;
        let parsed = frontmatter::parse_page(&raw)
            .map_err(|e| GraphError::Io(format!("frontmatter in {}: {e}", full.display())))?;
        if let Some(date) = extract_date(&parsed.frontmatter) {
            date_by_id.insert(page.id.as_str(), date);
        }
    }

    // For each page, the freshest date among the pages that cite it. Built from the
    // resolved wikilink graph over the full vault so a bare `[[concept]]` from a daily
    // note counts toward that concept's liveness.
    let existence = VaultExistence::from_pages(all_pages, dirs);
    let mut inbound_fresh: HashMap<&str, jiff::civil::Date> = HashMap::new();
    for page in all_pages {
        // Only a real citation source (event/work-log/synthesis/document/exploration)
        // reinforces liveness. A concept-to-concept `## Related` link is curated
        // structure, not recent activity — it must not keep an otherwise-dormant concept
        // off the stale report. Same `is_valid_source` definition backlinks uses.
        if !is_valid_source(&page.path, dirs) {
            continue;
        }
        let Some(&src_date) = date_by_id.get(page.id.as_str()) else {
            continue;
        };
        // A machine-written `updated:`/`created:` can be mis-stamped in the future
        // (LLM error, clock skew). A future-dated source must never become a permanent
        // liveness anchor that suppresses staleness for every concept it cites.
        if src_date > today {
            continue;
        }
        for target in &page.outgoing {
            if let Some(target_id) = existence.resolve(target)
                && target_id != page.id.as_str()
            {
                inbound_fresh
                    .entry(target_id)
                    .and_modify(|d| {
                        if src_date > *d {
                            *d = src_date;
                        }
                    })
                    .or_insert(src_date);
            }
        }
    }

    let mut stale = Vec::new();
    for page in pages {
        let Some(&date) = date_by_id.get(page.id.as_str()) else {
            continue;
        };
        let days_old = today.duration_since(date).as_secs() / 86_400;
        if days_old <= i64::from(threshold_days) {
            continue;
        }
        // A recent incoming citation keeps the page alive — exempt it.
        if let Some(&inbound) = inbound_fresh.get(page.id.as_str()) {
            let inbound_days_old = today.duration_since(inbound).as_secs() / 86_400;
            if inbound_days_old <= i64::from(threshold_days) {
                continue;
            }
        }

        stale.push(StalePage {
            path: page.path.clone(),
            updated: date,
            days_old,
            category: Category::from_path(&page.path, dirs),
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

    fn page(id: &str, rel: &str) -> ScannedPage {
        ScannedPage {
            id: id.to_owned(),
            path: PathBuf::from(rel),
            title: id.to_owned(),
            outgoing: vec![],
            aliases: Vec::new(),
        }
    }

    fn today() -> jiff::civil::Date {
        jiff::civil::date(2026, 5, 24)
    }

    #[test]
    fn finds_no_pages_under_threshold() {
        let dir = TempDir::new().unwrap();
        let dirs = VaultDirs::default();
        // updated 10 days before today — must NOT trigger at threshold=90.
        write(
            &dir,
            "wiki/concepts/fresh.md",
            "---\nupdated: 2026-05-14\n---\n\nbody\n",
        );
        let pages = vec![page("wiki/concepts/fresh", "wiki/concepts/fresh.md")];
        let stale = find_stale(&pages, &pages, dir.path(), today(), 90, &dirs).unwrap();
        assert!(stale.is_empty());
    }

    #[test]
    fn finds_pages_older_than_threshold() {
        let dir = TempDir::new().unwrap();
        let dirs = VaultDirs::default();
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
        let stale = find_stale(&pages, &pages, dir.path(), today(), 90, &dirs).unwrap();
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].path, PathBuf::from("wiki/concepts/old.md"));
        assert_eq!(stale[0].days_old, 180);
        assert_eq!(stale[0].category, Category::WikiConcepts);
    }

    #[test]
    fn falls_back_to_created_when_updated_missing() {
        let dir = TempDir::new().unwrap();
        let dirs = VaultDirs::default();
        write(
            &dir,
            "wiki/concepts/c.md",
            "---\ncreated: 2025-11-25\n---\n\nbody\n",
        );
        let pages = vec![page("wiki/concepts/c", "wiki/concepts/c.md")];
        let stale = find_stale(&pages, &pages, dir.path(), today(), 90, &dirs).unwrap();
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].updated, jiff::civil::date(2025, 11, 25));
    }

    #[test]
    fn skips_pages_without_either_field() {
        let dir = TempDir::new().unwrap();
        let dirs = VaultDirs::default();
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
        let stale = find_stale(&pages, &pages, dir.path(), today(), 90, &dirs).unwrap();
        assert!(stale.is_empty());
    }

    #[test]
    fn old_page_cited_by_recent_note_is_not_stale() {
        // An old concept reinforced by a recent incoming citation is live, not stale.
        let dir = TempDir::new().unwrap();
        let dirs = VaultDirs::default();
        write(
            &dir,
            "wiki/concepts/rag.md",
            "---\nupdated: 2025-11-25\n---\n\nbody\n",
        );
        write(
            &dir,
            "daily/ai-news/2026-05-19.md",
            "---\nupdated: 2026-05-19\n---\n\nSee [[rag]].\n",
        );
        let concept = page("wiki/concepts/rag", "wiki/concepts/rag.md");
        let daily = ScannedPage {
            id: "daily/ai-news/2026-05-19".to_owned(),
            path: PathBuf::from("daily/ai-news/2026-05-19.md"),
            title: "d".to_owned(),
            outgoing: vec!["rag".to_owned()],
            aliases: Vec::new(),
        };
        let all = vec![concept.clone(), daily];
        let stale = find_stale(&[concept], &all, dir.path(), today(), 90, &dirs).unwrap();
        assert!(
            stale.is_empty(),
            "a concept cited by a recent page is live, not stale: {stale:?}"
        );
    }

    #[test]
    fn future_dated_source_does_not_keep_old_concept_alive() {
        // A source page whose `updated:` is mis-stamped in the future must NOT become a
        // permanent liveness anchor. The old concept it cites stays stale.
        let dir = TempDir::new().unwrap();
        let dirs = VaultDirs::default();
        write(
            &dir,
            "wiki/concepts/rag.md",
            "---\nupdated: 2025-11-25\n---\n\nbody\n",
        );
        write(
            &dir,
            "daily/ai-news/2099-01-01.md",
            "---\nupdated: 2099-01-01\n---\n\nSee [[rag]].\n",
        );
        let concept = page("wiki/concepts/rag", "wiki/concepts/rag.md");
        let future = ScannedPage {
            id: "daily/ai-news/2099-01-01".to_owned(),
            path: PathBuf::from("daily/ai-news/2099-01-01.md"),
            title: "d".to_owned(),
            outgoing: vec!["rag".to_owned()],
            aliases: Vec::new(),
        };
        let all = vec![concept.clone(), future];
        let stale = find_stale(&[concept], &all, dir.path(), today(), 90, &dirs).unwrap();
        assert_eq!(
            stale.len(),
            1,
            "a future-dated citation must not suppress staleness: {stale:?}"
        );
    }

    #[test]
    fn concept_kept_alive_only_by_another_concept_is_still_stale() {
        // A `## Related` link from a recent CONCEPT is curated structure, not activity —
        // it must not reinforce liveness (only valid sources do). The old concept stays stale.
        let dir = TempDir::new().unwrap();
        let dirs = VaultDirs::default();
        write(
            &dir,
            "wiki/concepts/rag.md",
            "---\nupdated: 2025-11-25\n---\n\nbody\n",
        );
        write(
            &dir,
            "wiki/concepts/vector-search.md",
            "---\nupdated: 2026-05-20\n---\n\nSee [[rag]].\n",
        );
        let rag = page("wiki/concepts/rag", "wiki/concepts/rag.md");
        let related = ScannedPage {
            id: "wiki/concepts/vector-search".to_owned(),
            path: PathBuf::from("wiki/concepts/vector-search.md"),
            title: "vs".to_owned(),
            outgoing: vec!["rag".to_owned()],
            aliases: vec![],
        };
        let all = vec![rag.clone(), related];
        let stale = find_stale(&[rag], &all, dir.path(), today(), 90, &dirs).unwrap();
        assert_eq!(
            stale.len(),
            1,
            "a concept reinforced only by another concept is still stale: {stale:?}"
        );
    }

    #[test]
    fn old_page_cited_only_by_old_notes_is_stale() {
        // Citations that are themselves old don't keep a page alive — it stays dormant.
        let dir = TempDir::new().unwrap();
        let dirs = VaultDirs::default();
        write(
            &dir,
            "wiki/concepts/rag.md",
            "---\nupdated: 2025-11-25\n---\n\nbody\n",
        );
        write(
            &dir,
            "daily/ai-news/2025-11-20.md",
            "---\nupdated: 2025-11-20\n---\n\nSee [[rag]].\n",
        );
        let concept = page("wiki/concepts/rag", "wiki/concepts/rag.md");
        let daily = ScannedPage {
            id: "daily/ai-news/2025-11-20".to_owned(),
            path: PathBuf::from("daily/ai-news/2025-11-20.md"),
            title: "d".to_owned(),
            outgoing: vec!["rag".to_owned()],
            aliases: Vec::new(),
        };
        let all = vec![concept.clone(), daily];
        let stale = find_stale(&[concept], &all, dir.path(), today(), 90, &dirs).unwrap();
        assert_eq!(
            stale.len(),
            1,
            "a concept cited only by old notes stays stale: {stale:?}"
        );
        assert_eq!(stale[0].path, PathBuf::from("wiki/concepts/rag.md"));
    }

    #[test]
    fn groups_correctly_by_path_prefix() {
        let dirs = VaultDirs::default();
        assert_eq!(
            Category::from_path(Path::new("wiki/concepts/x.md"), &dirs),
            Category::WikiConcepts
        );
        assert_eq!(
            Category::from_path(Path::new("wiki/documents/x.md"), &dirs),
            Category::WikiDocuments
        );
        assert_eq!(
            Category::from_path(Path::new("wiki/explorations/x.md"), &dirs),
            Category::WikiExplorations
        );
        assert_eq!(
            Category::from_path(Path::new("daily/ai-news/2026-05-23.md"), &dirs),
            Category::Daily
        );
        assert_eq!(
            Category::from_path(Path::new("me/work-log/2026-05-23.md"), &dirs),
            Category::MeWorkLog
        );
        assert_eq!(
            Category::from_path(Path::new("synthesis/weekly/2026-W21.md"), &dirs),
            Category::Weekly
        );
        assert_eq!(
            Category::from_path(Path::new("me/weekly/2026-W21.md"), &dirs),
            Category::Weekly
        );
        assert_eq!(
            Category::from_path(Path::new("me/monthly/2026-05.md"), &dirs),
            Category::Monthly
        );
        assert_eq!(
            Category::from_path(Path::new("me/quarterly/2026-Q2.md"), &dirs),
            Category::Quarterly
        );
        assert_eq!(
            Category::from_path(Path::new("me/annual/2026.md"), &dirs),
            Category::Annual
        );
        // Anything outside the known prefixes → Other.
        assert_eq!(
            Category::from_path(Path::new("notes/random.md"), &dirs),
            Category::Other
        );
        // `me/` without a recognized subdirectory is not a recognised category.
        assert_eq!(
            Category::from_path(Path::new("me/other.md"), &dirs),
            Category::Other
        );
    }

    #[test]
    fn custom_dirs_categorize_correctly() {
        let dirs = VaultDirs {
            personal: "my-logs".into(),
            synthesis: "team-synth".into(),
            ..Default::default()
        };
        assert_eq!(
            Category::from_path(Path::new("my-logs/work-log/2026-05-23.md"), &dirs),
            Category::MeWorkLog
        );
        assert_eq!(
            Category::from_path(Path::new("my-logs/weekly/2026-W21.md"), &dirs),
            Category::Weekly
        );
        assert_eq!(
            Category::from_path(Path::new("team-synth/weekly/2026-W21.md"), &dirs),
            Category::Weekly
        );
        assert_eq!(
            Category::from_path(Path::new("my-logs/annual/2026.md"), &dirs),
            Category::Annual
        );
        // Old default paths should NOT match with custom dirs.
        assert_eq!(
            Category::from_path(Path::new("me/weekly/2026-W21.md"), &dirs),
            Category::Other
        );
    }
}
