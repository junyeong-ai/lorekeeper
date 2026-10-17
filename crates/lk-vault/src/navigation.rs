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

    fn owns(self, content: &str) -> bool {
        crate::parse_page(content).is_ok_and(|page| page.body.starts_with(self.marker()))
    }

    fn mark(self, content: &str) -> String {
        let parts = lk_core::frontmatter::split_page(content);
        if parts.yaml.is_empty() {
            format!("{}{content}", self.marker())
        } else {
            format!(
                "---\n{}\n---\n\n{}{}",
                parts.yaml.trim_end(),
                self.marker(),
                parts.body
            )
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
        let header = kind.mark(header);
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
            self.pages
                .insert(self.wiki.join(self.kind.file()), self.kind.mark(&content));
        }
        self
    }

    /// Publish the permanent SQLite entry guide and retire only explicitly marked exports.
    /// Call only after the search snapshot has been prepared successfully.
    pub fn refresh_entrypoint(
        session: &MutationSession,
        locale: lk_core::i18n::Locale,
        dirs: &lk_core::config::VaultDirs,
    ) -> Result<(), VaultError> {
        let (plan, _) = Self::plan_entrypoint(session.root(), locale, dirs, &[])?;
        session.apply(plan)
    }

    /// Build one guarded plan. Only explicitly revision-pinned root addresses may
    /// replace unmarked content; exact originals and their receipt precede retirement.
    pub fn plan_entrypoint(
        root: &Path,
        locale: lk_core::i18n::Locale,
        dirs: &lk_core::config::VaultDirs,
        archive_replace: &[(PathBuf, String)],
    ) -> Result<(MutationPlan, Option<PathBuf>), VaultError> {
        let wiki = Path::new(&dirs.wiki);
        let index = wiki.join(INDEX_FILE);
        let mut plan = MutationPlan::default();
        let mut approvals = BTreeMap::new();
        for (path, revision) in archive_replace {
            if ![INDEX_FILE, TIMELINE_FILE, MAP_FILE]
                .iter()
                .any(|file| path == &wiki.join(file))
                || revision.len() != 64
                || !revision
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                || approvals.insert(path.clone(), revision.clone()).is_some()
            {
                return Err(std::io::Error::other("archive-replace requires unique configured navigation root paths and full lowercase file revisions").into());
            }
        }
        let mut changes = Vec::new();
        let mut originals = Vec::new();
        let wiki_name = &dirs.wiki;
        let guide = match locale {
            lk_core::i18n::Locale::Ko => {
                format!(
                    "# 지식 탐색\n\n`lore wiki refresh`는 SQLite 탐색 스냅샷을 갱신합니다.\n\n- 탐색 세션마다 `lore wiki prepare`로 snapshot ID를 한 번 확보합니다(refresh 출력 ID도 사용 가능).\n- `lore wiki browse --snapshot ID --json`과 `lore wiki search <query> --snapshot ID --json`으로 반복 탐색합니다.\n- `lore wiki links <path> --snapshot ID --direction both`로 명시 링크·역링크를 읽습니다.\n- 검색의 file_revision으로 `lore wiki read <path> --revision REV`를 호출해 현재 원문을 확인합니다.\n\n전체 Markdown 내보내기는 `lore wiki index`, `lore wiki log`, `lore wiki map`으로 요청합니다.\n자동 내보내기는 `vault.navigation.markdown_exports: true`로 설정합니다.\n\nObsidian에서는 검색창에서 {wiki_name}/concepts 및 {wiki_name}/documents 경로로 범위를 좁히고 페이지의 백링크를 사용합니다. 클릭형 전체 목록이 필요하면 Markdown 내보내기를 켭니다.\n"
                )
            }
            lk_core::i18n::Locale::En => {
                format!(
                    "# Knowledge navigation\n\n`lore wiki refresh` refreshes the SQLite navigation snapshot.\n\n- Start a retrieval session with `lore wiki prepare` and retain its snapshot ID (or the ID printed by refresh).\n- Repeat `lore wiki browse --snapshot ID --json` and `lore wiki search <query> --snapshot ID --json`.\n- Inspect links/backlinks with `lore wiki links <path> --snapshot ID --direction both`.\n- Verify current originals with `lore wiki read <path> --revision REV`, using the hit's file_revision.\n\nRequest full Markdown exports with `lore wiki index`, `lore wiki log`, or `lore wiki map`.\nEnable automatic exports with `vault.navigation.markdown_exports: true`.\n\nIn Obsidian, narrow search to {wiki_name}/concepts and {wiki_name}/documents and use page backlinks. Enable Markdown exports for a complete clickable catalog.\n"
                )
            }
        };
        let guide = NavigationKind::Index.mark(&guide);
        for kind in [
            NavigationKind::Index,
            NavigationKind::Timeline,
            NavigationKind::Map,
        ] {
            let previous = Self::read(root, wiki, kind)?;
            for (path, content) in previous.pages {
                let approved = approvals.remove(&path);
                if let Some(revision) = approved {
                    if kind.owns(&content) || lk_core::concept::file_revision(&content) != revision
                    {
                        return Err(std::io::Error::other(format!(
                            "archive-replace revision changed or entry is already marked: {}",
                            path.display()
                        ))
                        .into());
                    }
                    crate::writable_page_path(root, &path)?;
                    let archive = Path::new(".lorekeeper/navigation-archives").join(format!(
                        "{}-{revision}.md",
                        path.file_stem().unwrap().to_string_lossy()
                    ));
                    preserve_navigation_file(root, &mut plan, &archive, &content)?;
                    originals.push(serde_json::json!({"original_path": path, "file_revision": revision, "archive_path": archive, "action": if path == index { "replace" } else { "retire" }}));
                } else if !kind.owns(&content) {
                    return Err(std::io::Error::other(format!(
                        "navigation entry is not marked generated; preserve or move it explicitly: {}",
                        path.display()
                    )).into());
                }
                // A marker cannot authorize deleting a source/knowledge page carrying a format.
                let page = crate::parse_page(&content).map_err(VaultError::Frontmatter)?;
                if page.frontmatter.get("type").is_some_and(|value| {
                    kind.file() != MAP_FILE
                        || value.as_str() != Some(lk_core::vault_path::MAP_FORMAT)
                }) {
                    return Err(std::io::Error::other(format!(
                        "generated navigation overlaps an authored page: {}",
                        path.display()
                    ))
                    .into());
                }
                changes.push((path, content));
            }
        }
        if !approvals.is_empty() {
            return Err(std::io::Error::other(
                "archive-replace address is absent; no replacement was performed",
            )
            .into());
        }
        let receipt = if originals.is_empty() {
            None
        } else {
            let content = serde_json::to_string_pretty(&serde_json::json!({"scheme": "lore-navigation-archive-v1", "originals": originals}))
                .map_err(|error| VaultError::Serialization(error.to_string()))? + "\n";
            let path = Path::new(".lorekeeper/navigation-archives").join(format!(
                "{}.json",
                lk_core::concept::file_revision(&content)
            ));
            preserve_navigation_file(root, &mut plan, &path, &content)?;
            Some(path)
        };
        for (path, content) in changes {
            if path == index {
                plan.edit(path, content, guide.clone());
            } else {
                plan.remove(path, content);
            }
        }
        if !crate::page_path(root, &index)?.try_exists()? {
            plan.create(index, guide);
        }
        Ok((plan, receipt))
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
        if !self.kind.owns(&self.pages[&index]) {
            return Err(
                std::io::Error::other("navigation entry omitted its generation marker").into(),
            );
        }
        crate::writer::refuse_format_change(
            &crate::writable_page_path(root, &index)?,
            &self.pages[&index],
        )?;
        if let Some(content) = before.pages.get(&index)
            && !self.kind.owns(content)
        {
            return Err(std::io::Error::other(format!(
                "navigation entry is not marked generated; preserve or move it explicitly: {}",
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

/// Reuse only exact direct files; immutable archive collisions never authorize replacement.
fn preserve_navigation_file(
    root: &Path,
    plan: &mut MutationPlan,
    path: &Path,
    content: &str,
) -> Result<(), VaultError> {
    let full = crate::writable_page_path(root, path)?;
    match std::fs::read_to_string(full) {
        Ok(previous) if previous == content => {
            plan.require_unchanged(path.to_owned(), previous);
            Ok(())
        }
        Ok(_) => Err(std::io::Error::other(format!(
            "navigation archive collision: {}",
            path.display()
        ))
        .into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            plan.create(path.to_owned(), content.to_owned());
            Ok(())
        }
        Err(error) => Err(error.into()),
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
    fn archive_replacement_guards_whole_plan_and_refuses_typed_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = lk_core::config::VaultDirs::default();
        let root_path = Path::new(&dirs.wiki).join(INDEX_FILE);
        let full = tmp.path().join(&root_path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        let original = "# Legacy\nAuthored edits are preserved verbatim.\n";
        std::fs::write(&full, original).unwrap();
        let session = MutationSession::acquire(tmp.path()).unwrap();
        let approval = vec![(root_path.clone(), lk_core::concept::file_revision(original))];
        let (plan, _) = NavigationCatalog::plan_entrypoint(
            tmp.path(),
            lk_core::i18n::Locale::En,
            &dirs,
            &approval,
        )
        .unwrap();
        std::fs::write(&full, "Changed after planning\n").unwrap();
        assert!(session.apply(plan).is_err());
        assert!(!tmp.path().join(".lorekeeper/navigation-archives").exists());
        assert_eq!(
            std::fs::read_to_string(&full).unwrap(),
            "Changed after planning\n"
        );
        let typed = "---\ntype: document\n---\n# Source\n";
        std::fs::write(&full, typed).unwrap();
        assert!(
            NavigationCatalog::plan_entrypoint(
                tmp.path(),
                lk_core::i18n::Locale::En,
                &dirs,
                &[(root_path, lk_core::concept::file_revision(typed))]
            )
            .is_err()
        );
        assert!(!tmp.path().join(".lorekeeper/navigation-archives").exists());
    }

    #[test]
    fn archive_replacement_reuses_exact_backups_and_refuses_corruption() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = lk_core::config::VaultDirs::default();
        let path = Path::new(&dirs.wiki).join(INDEX_FILE);
        let full = tmp.path().join(&path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        let original = "# Legacy\nExact original.\n";
        std::fs::write(&full, original).unwrap();
        let revision = lk_core::concept::file_revision(original);
        let approvals = vec![(path, revision.clone())];
        let session = MutationSession::acquire(tmp.path()).unwrap();
        let (plan, receipt) = NavigationCatalog::plan_entrypoint(
            tmp.path(),
            lk_core::i18n::Locale::En,
            &dirs,
            &approvals,
        )
        .unwrap();
        session.apply(plan).unwrap();
        std::fs::write(&full, original).unwrap();
        let (plan, reused_receipt) = NavigationCatalog::plan_entrypoint(
            tmp.path(),
            lk_core::i18n::Locale::En,
            &dirs,
            &approvals,
        )
        .unwrap();
        assert_eq!(receipt, reused_receipt);
        session.apply(plan).unwrap();
        std::fs::write(&full, original).unwrap();
        let archive = tmp.path().join(format!(
            ".lorekeeper/navigation-archives/index-{revision}.md"
        ));
        std::fs::write(&archive, "Corrupted archive\n").unwrap();
        assert!(
            NavigationCatalog::plan_entrypoint(
                tmp.path(),
                lk_core::i18n::Locale::En,
                &dirs,
                &approvals
            )
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(&full).unwrap(), original);
        assert_eq!(
            std::fs::read_to_string(&archive).unwrap(),
            "Corrupted archive\n"
        );
    }

    #[test]
    fn reused_archive_and_receipt_are_pinned_until_publication() {
        for receipt_changed in [false, true] {
            for deleted in [false, true] {
                let tmp = tempfile::tempdir().unwrap();
                let dirs = lk_core::config::VaultDirs::default();
                let path = Path::new(&dirs.wiki).join(INDEX_FILE);
                let full = tmp.path().join(&path);
                std::fs::create_dir_all(full.parent().unwrap()).unwrap();
                let original = "# Legacy\nRetain these exact bytes.\n";
                std::fs::write(&full, original).unwrap();
                let revision = lk_core::concept::file_revision(original);
                let approvals = vec![(path, revision.clone())];
                let session = MutationSession::acquire(tmp.path()).unwrap();
                let (initial, receipt) = NavigationCatalog::plan_entrypoint(
                    tmp.path(),
                    lk_core::i18n::Locale::En,
                    &dirs,
                    &approvals,
                )
                .unwrap();
                session.apply(initial).unwrap();
                std::fs::write(&full, original).unwrap();
                let (plan, _) = NavigationCatalog::plan_entrypoint(
                    tmp.path(),
                    lk_core::i18n::Locale::En,
                    &dirs,
                    &approvals,
                )
                .unwrap();
                let dependency = if receipt_changed {
                    tmp.path().join(receipt.unwrap())
                } else {
                    tmp.path().join(format!(
                        ".lorekeeper/navigation-archives/index-{revision}.md"
                    ))
                };
                if deleted {
                    std::fs::remove_file(&dependency).unwrap();
                } else {
                    std::fs::write(&dependency, "Changed after plan capture\n").unwrap();
                }
                assert!(session.apply(plan).is_err());
                assert_eq!(std::fs::read_to_string(&full).unwrap(), original);
                assert_eq!(!dependency.exists(), deleted);
                if !deleted {
                    assert_eq!(
                        std::fs::read_to_string(dependency).unwrap(),
                        "Changed after plan capture\n"
                    );
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn archive_replacement_does_not_adopt_a_leaf_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = lk_core::config::VaultDirs::default();
        let path = Path::new(&dirs.wiki).join(INDEX_FILE);
        std::fs::create_dir_all(tmp.path().join(&dirs.wiki)).unwrap();
        let original = "# Authored target\n";
        std::fs::write(tmp.path().join("saved.md"), original).unwrap();
        std::os::unix::fs::symlink("../saved.md", tmp.path().join(&path)).unwrap();
        assert!(
            NavigationCatalog::plan_entrypoint(
                tmp.path(),
                lk_core::i18n::Locale::En,
                &dirs,
                &[(path, lk_core::concept::file_revision(original))]
            )
            .is_err()
        );
        assert!(!tmp.path().join(".lorekeeper/navigation-archives").exists());
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("saved.md")).unwrap(),
            original
        );
    }

    #[test]
    fn sqlite_entry_retires_only_marked_views_in_one_plan() {
        let tmp = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(tmp.path()).unwrap();
        let dirs = lk_core::config::VaultDirs {
            wiki: "custom".into(),
            ..Default::default()
        };
        for kind in [
            NavigationKind::Index,
            NavigationKind::Timeline,
            NavigationKind::Map,
        ] {
            build(kind, 101).publish(&session).unwrap();
        }
        let authored = tmp.path().join("custom/catalog/keep.md");
        std::fs::write(&authored, "# Authored\n").unwrap();
        NavigationCatalog::refresh_entrypoint(&session, lk_core::i18n::Locale::En, &dirs).unwrap();
        assert_eq!(std::fs::read_to_string(&authored).unwrap(), "# Authored\n");
        assert!(!tmp.path().join("custom/log.md").exists());
        assert!(!tmp.path().join("custom/map.md").exists());
        assert!(!tmp.path().join("custom/catalog/0-0.md").exists());
        let entry = std::fs::read_to_string(tmp.path().join("custom/index.md")).unwrap();
        assert!(entry.contains("lore wiki browse"));
        assert!(entry.len() < 2000);
        NavigationCatalog::refresh_entrypoint(&session, lk_core::i18n::Locale::En, &dirs).unwrap();
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("custom/index.md")).unwrap(),
            entry
        );
        // Explicit export remains valid after switching to the SQLite entry.
        build(NavigationKind::Index, 101).publish(&session).unwrap();
    }

    #[test]
    fn retirement_refuses_unmarked_or_typed_collisions_before_any_edit() {
        for collision in [
            "# Hand-written\n".to_owned(),
            format!(
                "# Authored source\n{}\n```markdown\n<!-- lorekeeper:timeline -->\n```\n",
                "source text ".repeat(1000)
            ),
            "---\ntype: document\n---\n<!-- lorekeeper:timeline -->\nOriginal source\n".to_owned(),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let session = MutationSession::acquire(tmp.path()).unwrap();
            let dirs = lk_core::config::VaultDirs {
                wiki: "custom".into(),
                ..Default::default()
            };
            build(NavigationKind::Index, 101).publish(&session).unwrap();
            let old = std::fs::read(tmp.path().join("custom/index.md")).unwrap();
            std::fs::write(tmp.path().join("custom/log.md"), &collision).unwrap();
            assert!(
                NavigationCatalog::refresh_entrypoint(&session, lk_core::i18n::Locale::En, &dirs)
                    .is_err()
            );
            assert_eq!(
                std::fs::read(tmp.path().join("custom/index.md")).unwrap(),
                old
            );
            assert_eq!(
                std::fs::read_to_string(tmp.path().join("custom/log.md")).unwrap(),
                collision
            );
            assert!(tmp.path().join("custom/catalog/0-0.md").exists());
        }
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
