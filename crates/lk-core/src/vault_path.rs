use std::path::{Path, PathBuf};

use crate::config::VaultDirs;

/// Generated wiki entry guide and page-format contract. Discovery lives in SQLite;
/// neither generated page is knowledge or a citation source.
pub const INDEX_FILE: &str = "index.md";
pub const SCHEMA_FILE: &str = "AGENTS.md";
pub const RESERVED_WIKI_FILES: [&str; 2] = [INDEX_FILE, SCHEMA_FILE];

/// Generated files and the commands that refresh them after configuration changes.
pub const GENERATED_WIKI_PAGES: [(&str, &str); 2] =
    [(INDEX_FILE, "wiki refresh"), (SCHEMA_FILE, "schema")];

pub const SCHEMA_FORMAT: &str = "schema";

/// The task board's format.
///
/// Named here rather than only in `lk-task` because the graph has to recognise it without
/// depending on that crate: the board is the one managed page that states INTENT rather than
/// knowledge, so it is neither a citation source nor a page whose links are checked.
pub const TASK_BOARD_FORMAT: &str = "task-board";

/// Every value the `type` frontmatter key takes on a page this tool writes — the page-format
/// ids `lore schema` publishes, and OKF's one required key.
///
/// A page therefore states its own provenance, which is what lets a directory be recognized as
/// holding Lorekeeper's output rather than someone's own notes without guessing at content. The
/// personal-module formats are included: whether they are PRODUCED depends on config, but a
/// page carrying one was still written here. The generated schema guide is included;
/// the navigation entry guide is untyped and establishes no page-format ownership.
pub const PAGE_FORMATS: [&str; 12] = [
    "concept",
    "daily",
    "document",
    "exploration",
    "weekly-synthesis",
    "work-log",
    "weekly-review",
    "monthly-review",
    "quarterly-review",
    "annual-review",
    TASK_BOARD_FORMAT,
    SCHEMA_FORMAT,
];

/// Fixed subdirectory names within the configurable top-level vault dirs. The
/// top-level dirs (`<daily>`, `<personal>`, `<synthesis>`, `<wiki>`) are user-set
/// via `vault.dirs.*`; these leaf names are structural and single-sourced here so
/// every crate that builds or scans a vault path agrees on them — never an inline
/// literal.
pub const CONCEPTS_SUBDIR: &str = "concepts";
pub const DOCUMENTS_SUBDIR: &str = "documents";
pub const EXPLORATIONS_SUBDIR: &str = "explorations";
pub const WORK_LOG_SUBDIR: &str = "work-log";
pub const PROJECTION_CACHE_DIR: &str = ".lorekeeper/cache/projections";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultPath(PathBuf);

impl VaultPath {
    pub fn daily(dirs: &VaultDirs, source_id: &str, date: jiff::civil::Date) -> Self {
        Self(
            PathBuf::from(&dirs.daily)
                .join(source_id)
                .join(format!("{date}.md")),
        )
    }

    /// The task board, at the file `personal.tasks.board` names under `<personal>`.
    ///
    /// One page rather than one per day: it holds what is OPEN, which is not a record of a day
    /// — the record of a day is the daily page a completed task is archived onto.
    pub fn task_board(dirs: &VaultDirs, board: &str) -> Self {
        Self(PathBuf::from(&dirs.personal).join(board))
    }

    pub fn work_log(dirs: &VaultDirs, date: jiff::civil::Date) -> Self {
        Self(work_log_dir(dirs).join(format!("{date}.md")))
    }

    pub fn weekly_synthesis(dirs: &VaultDirs, year: i16, week: u8) -> Self {
        Self(
            PathBuf::from(&dirs.synthesis)
                .join(&dirs.weekly)
                .join(format!("{year}-W{week:02}.md")),
        )
    }

    pub fn weekly_review(dirs: &VaultDirs, year: i16, week: u8) -> Self {
        Self(
            PathBuf::from(&dirs.personal)
                .join(&dirs.weekly)
                .join(format!("{year}-W{week:02}.md")),
        )
    }

    pub fn monthly_review(dirs: &VaultDirs, year: i16, month: u8) -> Self {
        Self(
            PathBuf::from(&dirs.personal)
                .join(&dirs.monthly)
                .join(format!("{year}-{month:02}.md")),
        )
    }

    pub fn quarterly_review(dirs: &VaultDirs, year: i16, quarter: u8) -> Self {
        Self(
            PathBuf::from(&dirs.personal)
                .join(&dirs.quarterly)
                .join(format!("{year}-Q{quarter}.md")),
        )
    }

    pub fn annual_review(dirs: &VaultDirs, year: i16) -> Self {
        Self(
            PathBuf::from(&dirs.personal)
                .join(&dirs.annual)
                .join(format!("{year}.md")),
        )
    }

    pub fn concept(dirs: &VaultDirs, slug: &str) -> Self {
        Self(concepts_dir(dirs).join(format!("{slug}.md")))
    }

    pub fn document(dirs: &VaultDirs, slug: &str) -> Self {
        Self(documents_dir(dirs).join(format!("{slug}.md")))
    }

    pub fn exploration(dirs: &VaultDirs, slug: &str) -> Self {
        Self(explorations_dir(dirs).join(format!("{slug}.md")))
    }
}

/// Relative directory holding the work-log pages (`<personal>/work-log/`).
pub fn work_log_dir(dirs: &VaultDirs) -> PathBuf {
    PathBuf::from(&dirs.personal).join(WORK_LOG_SUBDIR)
}

/// Relative directory holding the concept pages (`<wiki>/concepts/`).
pub fn concepts_dir(dirs: &VaultDirs) -> PathBuf {
    PathBuf::from(&dirs.wiki).join(CONCEPTS_SUBDIR)
}

/// Relative directory holding the document pages (`<wiki>/documents/`).
pub fn documents_dir(dirs: &VaultDirs) -> PathBuf {
    PathBuf::from(&dirs.wiki).join(DOCUMENTS_SUBDIR)
}

/// Relative directory holding the exploration pages (`<wiki>/explorations/`).
pub fn explorations_dir(dirs: &VaultDirs) -> PathBuf {
    PathBuf::from(&dirs.wiki).join(EXPLORATIONS_SUBDIR)
}

impl AsRef<Path> for VaultPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl std::fmt::Display for VaultPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.display())
    }
}

pub fn quarter_of_month(month: u8) -> u8 {
    debug_assert!((1..=12).contains(&month), "month must be 1-12, got {month}");
    (month - 1) / 3 + 1
}

/// Node key for a vault-relative path: drop the extension, normalize separators to `/`, then
/// name each segment by the rule that segment is named under — directories by the filesystem
/// ([`crate::fs::fold_name`]), the file by the concept slug (so `wiki/Concept A.md` →
/// `wiki/concept-a`).
///
/// A concept page's FILE is named by its slug, which is why two files whose stems slugify alike
/// are one address and a violation. A DIRECTORY is not: the vault addresses it literally, so
/// `slugify` applied there answers a question nobody asked and folds directories the filesystem
/// keeps apart. `daily_/` beside `daily/` became one id: a broken link written on a page under
/// `daily_/` was reported against the page under `daily/`, where a reader looking for it finds
/// no such link, and the two real files at two real addresses were reported as one address with
/// two files.
///
/// Single-sourced here because the id is what a CITATION is counted as: `lore graph
/// backlinks-sync` derives a concept's evidence set from these ids, and the ingest pipeline
/// records the same set on a concept page it creates. Two derivations of the id would make the
/// two disagree about whether a synthesis answers the evidence it was written from.
pub fn path_slug(rel: &Path) -> String {
    let no_ext = rel.with_extension("");
    let s = no_ext.to_string_lossy().replace('\\', "/");
    let mut segments: Vec<&str> = s.split('/').collect();
    let stem = segments.pop();
    join_segments(
        segments
            .into_iter()
            .map(crate::fs::fold_name)
            .chain(stem.and_then(crate::concept::slugify)),
    )
}

/// The id prefix a vault-relative DIRECTORY contributes: every segment is a directory, so every
/// segment is named by the filesystem rule. Sharing [`path_slug`] here would slugify the last
/// segment as though it were a page's file, and a configured `daily_` would then match no page
/// at all — it would ask for the prefix `daily`, which is not what any page under it carries.
pub fn dir_slug(dir: &Path) -> String {
    let s = dir.to_string_lossy().replace('\\', "/");
    join_segments(s.split('/').map(crate::fs::fold_name))
}

fn join_segments(segments: impl Iterator<Item = String>) -> String {
    segments
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daily_path() {
        let dirs = VaultDirs::default();
        let date = jiff::civil::date(2026, 5, 23);
        let path = VaultPath::daily(&dirs, "ai-news", date);
        assert_eq!(path.to_string(), "daily/ai-news/2026-05-23.md");
    }

    #[test]
    fn work_log_path() {
        let dirs = VaultDirs::default();
        let date = jiff::civil::date(2026, 5, 23);
        let path = VaultPath::work_log(&dirs, date);
        assert_eq!(path.to_string(), "me/work-log/2026-05-23.md");
    }

    #[test]
    fn weekly_synthesis_path() {
        let dirs = VaultDirs::default();
        let path = VaultPath::weekly_synthesis(&dirs, 2026, 21);
        assert_eq!(path.to_string(), "synthesis/weekly/2026-W21.md");
    }

    #[test]
    fn weekly_review_path() {
        let dirs = VaultDirs::default();
        let path = VaultPath::weekly_review(&dirs, 2026, 21);
        assert_eq!(path.to_string(), "me/weekly/2026-W21.md");
    }

    #[test]
    fn monthly_review_path() {
        let dirs = VaultDirs::default();
        let path = VaultPath::monthly_review(&dirs, 2026, 5);
        assert_eq!(path.to_string(), "me/monthly/2026-05.md");
    }

    #[test]
    fn quarterly_review_path() {
        let dirs = VaultDirs::default();
        let path = VaultPath::quarterly_review(&dirs, 2026, 2);
        assert_eq!(path.to_string(), "me/quarterly/2026-Q2.md");
    }

    #[test]
    fn annual_review_path() {
        let dirs = VaultDirs::default();
        let path = VaultPath::annual_review(&dirs, 2026);
        assert_eq!(path.to_string(), "me/annual/2026.md");
    }

    #[test]
    fn concept_path() {
        let dirs = VaultDirs::default();
        let path = VaultPath::concept(&dirs, "claude-code");
        assert_eq!(path.to_string(), "wiki/concepts/claude-code.md");
    }

    #[test]
    fn document_path() {
        let dirs = VaultDirs::default();
        let path = VaultPath::document(&dirs, "cloud-sql-extensions-report");
        assert_eq!(
            path.to_string(),
            "wiki/documents/cloud-sql-extensions-report.md"
        );
    }

    #[test]
    fn exploration_path() {
        let dirs = VaultDirs::default();
        let path = VaultPath::exploration(&dirs, "api-replay-checklist");
        assert_eq!(
            path.to_string(),
            "wiki/explorations/api-replay-checklist.md"
        );
    }

    #[test]
    fn custom_dirs() {
        let dirs = VaultDirs {
            personal: "my-stuff".into(),
            ..Default::default()
        };
        let date = jiff::civil::date(2026, 5, 23);
        let path = VaultPath::work_log(&dirs, date);
        assert_eq!(path.to_string(), "my-stuff/work-log/2026-05-23.md");
    }

    #[test]
    fn quarter_calculation() {
        assert_eq!(quarter_of_month(1), 1);
        assert_eq!(quarter_of_month(3), 1);
        assert_eq!(quarter_of_month(4), 2);
        assert_eq!(quarter_of_month(6), 2);
        assert_eq!(quarter_of_month(7), 3);
        assert_eq!(quarter_of_month(9), 3);
        assert_eq!(quarter_of_month(10), 4);
        assert_eq!(quarter_of_month(12), 4);
    }

    #[test]
    fn path_slug_basic() {
        assert_eq!(path_slug(Path::new("wiki/Concept A.md")), "wiki/concept-a");
        assert_eq!(path_slug(Path::new("wiki/Bad_Name.md")), "wiki/bad-name");
    }

    #[test]
    fn path_slug_preserves_directory_structure() {
        assert_eq!(
            path_slug(Path::new("wiki/sub/Topic Name.md")),
            "wiki/sub/topic-name"
        );
    }

    /// A directory is addressed literally, so two directories the filesystem keeps apart stay
    /// two ids. Slugifying them folded `daily_/` onto `daily/`: a broken link written under one
    /// was reported against a page in the other, where nothing of the kind is written, and the
    /// two real files were reported as one address holding two.
    #[test]
    fn a_directory_is_named_by_the_filesystem_not_by_the_concept_rule() {
        assert_ne!(
            path_slug(Path::new("daily_/notes/2026-07-30.md")),
            path_slug(Path::new("daily/notes/2026-07-30.md"))
        );
        assert_eq!(
            path_slug(Path::new("daily_/notes/2026-07-30.md")),
            "daily_/notes/2026-07-30"
        );
        // The FILE is still named by the concept rule, which is what makes two stems that
        // slugify alike one address and a violation.
        assert_eq!(
            path_slug(Path::new("wiki/concepts/Bad_Name.md")),
            path_slug(Path::new("wiki/concepts/bad-name.md"))
        );
    }
}
