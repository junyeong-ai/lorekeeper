//! Bounded generated navigation, shared by the topic catalog, timeline and citation map.
//! Rows describe addresses, not inferred relationships. Child addresses are reconstructible
//! navigation positions; durable citations continue to address the original pages.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use lk_core::link;
use lk_core::vault_path::{CATALOG_SUBDIR, INDEX_FILE, MAP_FILE, TIMELINE_FILE};

use crate::{MutationPlan, MutationSession, VaultError};

pub const NAVIGATION_PAGE_ENTRIES: usize = 100;

#[derive(Debug, Clone, Copy)]
pub enum NavigationKind {
    Index,
    Map,
    Timeline,
}

impl NavigationKind {
    fn file(self) -> &'static str {
        match self {
            Self::Index => INDEX_FILE,
            Self::Map => MAP_FILE,
            Self::Timeline => TIMELINE_FILE,
        }
    }

    fn marker(self) -> &'static str {
        match self {
            Self::Index => "<!-- lorekeeper:catalog -->\n",
            Self::Map => "<!-- lorekeeper:map -->\n",
            Self::Timeline => "<!-- lorekeeper:timeline -->\n",
        }
    }

    fn directory(self, wiki: &Path) -> PathBuf {
        let dir = wiki.join(CATALOG_SUBDIR);
        match self {
            Self::Index => dir,
            Self::Map | Self::Timeline => dir.join(Path::new(self.file()).file_stem().unwrap()),
        }
    }
}

/// Semantic heading identity is separate from its optional display count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NavigationHeading {
    pub label: String,
    pub count: Option<usize>,
}

impl NavigationHeading {
    pub fn counted(label: impl Into<String>, count: usize) -> Self {
        Self {
            label: label.into(),
            count: Some(count),
        }
    }
}

impl From<String> for NavigationHeading {
    fn from(label: String) -> Self {
        Self { label, count: None }
    }
}

impl From<&str> for NavigationHeading {
    fn from(label: &str) -> Self {
        label.to_owned().into()
    }
}

pub struct NavigationRow {
    pub headings: Vec<NavigationHeading>,
    pub title: String,
    pub path: PathBuf,
    pub summary: Option<String>,
}

#[derive(Debug)]
pub struct NavigationCatalog {
    pub pages: BTreeMap<PathBuf, String>,
    kind: NavigationKind,
    wiki: PathBuf,
}

struct Branch {
    path: PathBuf,
    label: String,
    first: String,
    last: String,
    count: usize,
}

impl NavigationCatalog {
    pub(crate) fn from_pages(
        kind: NavigationKind,
        wiki: &Path,
        pages: BTreeMap<PathBuf, String>,
    ) -> Self {
        Self {
            pages,
            kind,
            wiki: wiki.to_owned(),
        }
    }

    pub fn build(
        kind: NavigationKind,
        wiki: &Path,
        title: &str,
        header: &str,
        rows: Vec<NavigationRow>,
    ) -> Self {
        let index = wiki.join(kind.file());
        let mut pages = BTreeMap::new();
        if rows.len() <= NAVIGATION_PAGE_ENTRIES {
            pages.insert(
                index,
                format!("{header}{}", render_rows(&wiki.join(kind.file()), &rows)),
            );
            return Self {
                pages,
                kind,
                wiki: wiki.to_owned(),
            };
        }
        let dir = kind.directory(wiki);
        let mut branches = Vec::new();
        // Pack small groups whole; only an oversized group may cross a leaf boundary.
        let mut leaves = Vec::new();
        let mut pending_start = 0;
        let mut pending_end = 0;
        let mut start = 0;
        while start < rows.len() {
            let end = start
                + rows[start..]
                    .iter()
                    .take_while(|row| row.headings == rows[start].headings)
                    .count();
            let size = end - start;
            if pending_end > pending_start
                && (size > NAVIGATION_PAGE_ENTRIES || end - pending_start > NAVIGATION_PAGE_ENTRIES)
            {
                leaves.push((pending_start, pending_end, None));
                pending_start = start;
            }
            if size > NAVIGATION_PAGE_ENTRIES {
                let parts = size.div_ceil(NAVIGATION_PAGE_ENTRIES);
                for part in 0..parts {
                    let chunk_start = start + part * NAVIGATION_PAGE_ENTRIES;
                    let chunk_end = (chunk_start + NAVIGATION_PAGE_ENTRIES).min(end);
                    let label = format!("{} · {}/{}", group_label(&rows[start]), part + 1, parts);
                    leaves.push((chunk_start, chunk_end, Some(label)));
                }
                pending_start = end;
            }
            pending_end = end;
            start = end;
        }
        if pending_end > pending_start {
            leaves.push((pending_start, pending_end, None));
        }
        for (number, (start, end, continuation)) in leaves.into_iter().enumerate() {
            let chunk = &rows[start..end];
            let first = continuation
                .clone()
                .unwrap_or_else(|| group_label(&chunk[0]));
            let last = continuation.unwrap_or_else(|| group_label(chunk.last().unwrap()));
            let path = dir.join(format!("0-{number}.md"));
            let body = format!(
                "{}# {title}\n\n{}\n{}",
                kind.marker(),
                link::md_link("↑", &link::relative_dest(&path, &index)),
                render_rows(&path, chunk),
            );
            pages.insert(path.clone(), body);
            branches.push(Branch {
                path,
                label: group_range(&first, &last),
                first,
                last,
                count: chunk.len(),
            });
        }
        let mut level = 1;
        while branches.len() > NAVIGATION_PAGE_ENTRIES {
            let mut parents = Vec::new();
            for (number, chunk) in branches.chunks(NAVIGATION_PAGE_ENTRIES).enumerate() {
                let path = dir.join(format!("{level}-{number}.md"));
                pages.insert(
                    path.clone(),
                    format!(
                        "{}# {title}\n\n{}\n\n{}",
                        kind.marker(),
                        link::md_link("↑", &link::relative_dest(&path, &index)),
                        render_branches(&path, chunk),
                    ),
                );
                parents.push(Branch {
                    path,
                    label: group_range(&chunk[0].first, &chunk.last().unwrap().last),
                    first: chunk[0].first.clone(),
                    last: chunk.last().unwrap().last.clone(),
                    count: chunk.iter().map(|branch| branch.count).sum(),
                });
            }
            branches = parents;
            level += 1;
        }
        pages.insert(
            index,
            format!(
                "{header}\n{}",
                render_branches(&wiki.join(kind.file()), &branches)
            ),
        );
        Self {
            pages,
            kind,
            wiki: wiki.to_owned(),
        }
    }

    /// Preserve a small view's existing inline layout without changing the shared bound.
    pub fn with_inline(mut self, content: String) -> Self {
        if self.pages.len() == 1 {
            self.pages.insert(self.wiki.join(self.kind.file()), content);
        }
        self
    }

    pub fn read(root: &Path, wiki: &Path, kind: NavigationKind) -> Result<Self, VaultError> {
        let index = wiki.join(kind.file());
        let mut pages = BTreeMap::new();
        match std::fs::read_to_string(crate::page_path(root, &index)?) {
            Ok(content) => {
                pages.insert(index, content);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let dir = kind.directory(wiki);
        let absolute = crate::page_path(root, &dir)?;
        match std::fs::read_dir(&absolute) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry?;
                    // Each view owns only direct marked children of its own namespace.
                    // Index publication never descends into the map or timeline namespaces.
                    if entry.file_type()?.is_dir()
                        || entry.path().extension().is_none_or(|ext| ext != "md")
                    {
                        continue;
                    }
                    let path = dir.join(entry.file_name());
                    let content = std::fs::read_to_string(crate::page_path(root, &path)?)?;
                    if content.starts_with(kind.marker()) {
                        pages.insert(path, content);
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(Self {
            pages,
            kind,
            wiki: wiki.to_owned(),
        })
    }

    /// Preflight the entire view, then journal children, entry page and stale removals.
    /// Unknown files are never adopted or deleted, even when their names match a shard.
    pub fn publish(&self, session: &MutationSession) -> Result<(), VaultError> {
        let root = session.root();
        let before = Self::read(root, &self.wiki, self.kind)?;
        let index = self.wiki.join(self.kind.file());
        let directory = self.kind.directory(&self.wiki);
        if !self.pages.contains_key(&index) {
            return Err(std::io::Error::other(format!(
                "navigation output omitted its entry page: {}",
                index.display()
            ))
            .into());
        }
        let mut plan = MutationPlan::default();
        for (path, content) in self
            .pages
            .iter()
            .filter(|(path, _)| **path != index)
            .chain(self.pages.get_key_value(&index))
        {
            if path != &index
                && (path.parent() != Some(directory.as_path())
                    || !content.starts_with(self.kind.marker()))
            {
                return Err(std::io::Error::other(format!(
                    "navigation output outside its generated namespace: {}",
                    path.display()
                ))
                .into());
            }
            crate::writer::refuse_format_change(&crate::writable_page_path(root, path)?, content)?;
            if path != &index
                && !before.pages.contains_key(path)
                && crate::page_path(root, path)?.try_exists()?
            {
                return Err(std::io::Error::other(format!(
                    "navigation output is not generated: {}",
                    path.display()
                ))
                .into());
            }
            match before.pages.get(path) {
                Some(previous) => plan.edit(path, previous.clone(), content.clone()),
                None => plan.create(path, content.clone()),
            }
        }
        for (path, content) in before.pages {
            if !self.pages.contains_key(&path) {
                plan.remove(path, content);
            }
        }
        session.apply(plan)
    }
}

fn group_label(row: &NavigationRow) -> String {
    if row.headings.is_empty() {
        row.title.clone()
    } else {
        row.headings
            .iter()
            .map(|heading| heading.label.as_str())
            .collect::<Vec<_>>()
            .join(" / ")
    }
}

fn group_range(first: &str, last: &str) -> String {
    if first == last {
        first.to_owned()
    } else {
        format!("{first} … {last}")
    }
}

fn render_rows(path: &Path, rows: &[NavigationRow]) -> String {
    let mut out = String::new();
    let mut previous: &[NavigationHeading] = &[];
    for row in rows {
        let shared = previous
            .iter()
            .zip(&row.headings)
            .take_while(|(a, b)| a == b)
            .count();
        for (level, heading) in row.headings.iter().enumerate().skip(shared) {
            let label = match heading.count {
                Some(count) => format!("{} ({count})", heading.label),
                None => heading.label.clone(),
            };
            writeln!(out, "\n{} {label}\n", "#".repeat(level + 2)).unwrap();
        }
        let entry = link::md_link(&row.title, &link::relative_dest(path, &row.path));
        match &row.summary {
            Some(summary) => writeln!(out, "- {entry} — {summary}").unwrap(),
            None => writeln!(out, "- {entry}").unwrap(),
        }
        previous = &row.headings;
    }
    out
}

fn render_branches(path: &Path, branches: &[Branch]) -> String {
    let mut out = String::new();
    for branch in branches {
        let label = format!("{} ({})", branch.label, branch.count);
        writeln!(
            out,
            "- {}",
            link::md_link(&label, &link::relative_dest(path, &branch.path))
        )
        .unwrap();
    }
    out
}

/// Exact configured navigation locations, resolved through the holding filesystem.
/// A source page sharing a basename elsewhere remains knowledge.
pub(crate) struct ReservedNavigationPaths {
    files: std::collections::BTreeSet<PathBuf>,
    catalog: Option<PathBuf>,
}

impl ReservedNavigationPaths {
    pub(crate) fn load(root: &Path, dirs: &lk_core::config::VaultDirs) -> Result<Self, VaultError> {
        let wiki = crate::page_path(root, Path::new(&dirs.wiki))?;
        let canonical_if_present = |path: PathBuf| match path.canonicalize() {
            Ok(path) => Ok(Some(path)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(VaultError::Io(error)),
        };
        let mut files = std::collections::BTreeSet::new();
        for name in lk_core::vault_path::RESERVED_WIKI_FILES {
            if let Some(path) = canonical_if_present(wiki.join(name))? {
                files.insert(path);
            }
        }
        Ok(Self {
            files,
            catalog: canonical_if_present(wiki.join(lk_core::vault_path::CATALOG_SUBDIR))?,
        })
    }

    pub(crate) fn contains(&self, path: &Path) -> Result<bool, VaultError> {
        let canonical = path.canonicalize()?;
        Ok(self.files.contains(&canonical)
            || self
                .catalog
                .as_ref()
                .is_some_and(|catalog| canonical.starts_with(catalog)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn build(kind: NavigationKind, count: usize) -> NavigationCatalog {
        NavigationCatalog::build(
            kind,
            Path::new("custom"),
            "Navigation",
            "# Navigation\n",
            (0..count)
                .map(|number| NavigationRow {
                    headings: vec![format!("Group {}", number / 300).into()],
                    title: format!("항목 [{number}]"),
                    path: PathBuf::from(format!("custom/concepts/Item {number}.md")),
                    summary: None,
                })
                .collect(),
        )
    }

    #[test]
    fn heading_counts_do_not_leak_into_branch_group_labels() {
        let rows = (0..101)
            .map(|number| NavigationRow {
                headings: vec![
                    NavigationHeading::counted("Concepts", 101),
                    NavigationHeading::counted("auth-identity (literal)", 101),
                ],
                title: format!("Document {number}"),
                path: PathBuf::from(format!("wiki/documents/{number}.md")),
                summary: None,
            })
            .collect();
        let catalog = NavigationCatalog::build(
            NavigationKind::Index,
            Path::new("wiki"),
            "Navigation",
            "# Navigation\n",
            rows,
        );
        let root = &catalog.pages[&PathBuf::from("wiki/index.md")];
        assert!(root.contains("Concepts / auth-identity (literal) · 1/2 (100)"));
        assert!(!root.contains("Concepts (101)"));
        let detail = catalog
            .pages
            .values()
            .find(|page| page.contains("## Concepts (101)"))
            .unwrap();
        assert!(detail.contains("### auth-identity (literal) (101)"));
    }

    #[test]
    fn every_view_bounds_each_page_and_reaches_every_exact_destination() {
        for kind in [
            NavigationKind::Index,
            NavigationKind::Map,
            NavigationKind::Timeline,
        ] {
            for count in [0, 1, 100, 101, 10_201] {
                let catalog = build(kind, count);
                assert_eq!(catalog.pages, build(kind, count).pages);
                if count == 10_201 {
                    let root = &catalog.pages[&Path::new("custom").join(kind.file())];
                    // The parent's boundary continues a group; its two links must
                    // identify different continuation ranges, not repeat its name.
                    assert!(root.contains("Group 0 · 1/3 … Group 33 · 1/3"), "{root}");
                    assert!(root.contains("Group 33 · 2/3 … Group 34"), "{root}");
                }
                let mut visited = HashSet::new();
                let mut reached = HashSet::new();
                let mut pending = vec![Path::new("custom").join(kind.file())];
                while let Some(path) = pending.pop() {
                    if !visited.insert(path.clone()) {
                        continue;
                    }
                    let content = &catalog.pages[&path];
                    assert!(
                        content
                            .lines()
                            .filter(|line| line.starts_with("- "))
                            .count()
                            <= NAVIGATION_PAGE_ENTRIES
                    );
                    for dest in link::extract_dests(content) {
                        let target = link::resolve_dest(&path, &dest).unwrap();
                        if catalog.pages.contains_key(&target) {
                            pending.push(target);
                        } else {
                            assert!(reached.insert(target));
                        }
                    }
                }
                let expected: HashSet<_> = (0..count)
                    .map(|number| PathBuf::from(format!("custom/concepts/Item {number}.md")))
                    .collect();
                assert_eq!(reached, expected);
                assert_eq!(visited.len(), catalog.pages.len());
            }
        }
    }

    #[test]
    fn detail_pages_keep_groups_whole_and_label_only_large_group_continuations() {
        for kind in [
            NavigationKind::Index,
            NavigationKind::Map,
            NavigationKind::Timeline,
        ] {
            let mut rows = Vec::new();
            for (group, count) in [("First", 60), ("Second", 60), ("Large", 205)] {
                for number in 0..count {
                    rows.push(NavigationRow {
                        headings: vec![group.into()],
                        title: format!("Long document title {group} {number}"),
                        path: PathBuf::from(format!("custom/documents/{group} {number}.md")),
                        summary: None,
                    });
                }
            }
            let catalog = NavigationCatalog::build(
                kind,
                Path::new("custom"),
                "Navigation",
                "# Navigation\n",
                rows,
            );
            let root = &catalog.pages[&Path::new("custom").join(kind.file())];
            assert!(!root.contains("Long document title"), "{root}");
            assert!(root.contains("[First (60)]"), "{root}");
            assert!(root.contains("[Second (60)]"), "{root}");
            for part in 1..=3 {
                assert!(root.contains(&format!("Large · {part}/3")), "{root}");
            }
            assert_eq!(catalog.pages.len(), 6);
            for (path, content) in &catalog.pages {
                if path == &Path::new("custom").join(kind.file()) {
                    continue;
                }
                assert_eq!(
                    content
                        .lines()
                        .filter(|line| line.starts_with("## "))
                        .count(),
                    1
                );
            }
        }
    }

    #[test]
    fn many_small_groups_keep_the_entry_bounded_and_parent_labels_unambiguous() {
        let rows = (0..101)
            .map(|number| NavigationRow {
                headings: vec![format!("Group {number:03}").into()],
                title: format!("Document {number}"),
                path: PathBuf::from(format!("custom/documents/{number}.md")),
                summary: None,
            })
            .collect();
        let catalog = NavigationCatalog::build(
            NavigationKind::Index,
            Path::new("custom"),
            "Navigation",
            "# Navigation\n",
            rows,
        );
        let root = &catalog.pages[Path::new("custom/index.md")];
        assert!(root.contains("Group 000 … Group 099 (100)"), "{root}");
        assert!(root.contains("[Group 100 (1)]"), "{root}");
        assert!(!root.contains("Group 100 … Group 100"));
        assert_eq!(
            root.lines().filter(|line| line.starts_with("- ")).count(),
            2
        );
        assert_eq!(catalog.pages.len(), 3);
    }

    #[test]
    fn publication_refuses_unknown_collisions_before_writing_and_shrinks_only_its_view() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let session = MutationSession::acquire(root).unwrap();
        let child = root.join("custom/catalog/map/0-0.md");
        std::fs::create_dir_all(child.parent().unwrap()).unwrap();
        std::fs::write(&child, "authored note").unwrap();
        assert!(build(NavigationKind::Map, 101).publish(&session).is_err());
        assert!(!root.join("custom/map.md").exists());
        std::fs::rename(&child, child.with_file_name("note.md")).unwrap();
        build(NavigationKind::Index, 101).publish(&session).unwrap();
        build(NavigationKind::Timeline, 101)
            .publish(&session)
            .unwrap();
        let large = build(NavigationKind::Map, 101);
        large.publish(&session).unwrap();
        large.publish(&session).unwrap();
        assert_eq!(
            NavigationCatalog::read(root, Path::new("custom"), NavigationKind::Map)
                .unwrap()
                .pages,
            large.pages
        );
        build(NavigationKind::Map, 1).publish(&session).unwrap();
        assert!(!child.exists());
        assert_eq!(
            std::fs::read_to_string(child.with_file_name("note.md")).unwrap(),
            "authored note"
        );
        assert!(root.join("custom/catalog/0-0.md").exists());
        assert!(root.join("custom/catalog/log/0-0.md").exists());
        // One publisher cannot adopt output belonging to another view even at its own address.
        std::fs::write(
            &child,
            format!("{}foreign output", NavigationKind::Timeline.marker()),
        )
        .unwrap();
        let previous = std::fs::read_to_string(root.join("custom/map.md")).unwrap();
        assert!(large.publish(&session).is_err());
        assert_eq!(
            std::fs::read_to_string(root.join("custom/map.md")).unwrap(),
            previous
        );
    }

    #[test]
    fn an_unreadable_navigation_directory_preserves_the_existing_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(tmp.path()).unwrap();
        build(NavigationKind::Map, 1).publish(&session).unwrap();
        let index = tmp.path().join("custom/map.md");
        let previous = std::fs::read_to_string(&index).unwrap();
        std::fs::create_dir_all(tmp.path().join("custom/catalog")).unwrap();
        std::fs::write(tmp.path().join("custom/catalog/map"), "not a directory").unwrap();
        assert!(build(NavigationKind::Map, 101).publish(&session).is_err());
        assert_eq!(std::fs::read_to_string(index).unwrap(), previous);
    }
}
