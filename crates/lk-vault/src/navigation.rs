//! The small generated entry guide for SQLite navigation.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use lk_core::config::VaultDirs;
use lk_core::i18n::Locale;
use lk_core::vault_path::INDEX_FILE;

use crate::{MutationPlan, VaultError};

/// Ownership token for the generated entry guide, not a page-content format.
pub const NAVIGATION_ENTRY_MARKER: &str = "<!-- lorekeeper:catalog -->\n";

/// Plan only the configured entry guide. Publish after preparing SQLite successfully.
/// Unmarked files and authored frontmatter are never adopted or replaced.
pub fn plan_navigation_entry(
    root: &Path,
    locale: Locale,
    dirs: &VaultDirs,
) -> Result<MutationPlan, VaultError> {
    let path = Path::new(&dirs.wiki).join(INDEX_FILE);
    let full = crate::writable_page_path(root, &path)?;
    let before = match std::fs::read_to_string(&full) {
        Ok(content) => {
            let page = crate::parse_page(&content).map_err(VaultError::Frontmatter)?;
            if !page.frontmatter.fields.is_empty()
                || !page.body.starts_with(NAVIGATION_ENTRY_MARKER)
            {
                return Err(std::io::Error::other(format!(
                    "navigation entry is not an owned generated guide; preserve or move it explicitly: {}",
                    path.display()
                ))
                .into());
            }
            Some(content)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let guide = match locale {
        Locale::Ko => format!(
            "# 지식 탐색\n\n`lore wiki refresh`는 SQLite 탐색 스냅샷을 갱신합니다.\n\n- 탐색 세션마다 `lore wiki prepare`로 snapshot ID를 한 번 확보합니다(refresh 출력 ID도 사용 가능).\n- `lore wiki browse --snapshot ID --json`과 `lore wiki search <query> --snapshot ID --json`으로 반복 탐색합니다.\n- `lore wiki links <path> --snapshot ID --direction both`로 명시 링크·역링크를 읽습니다.\n- 검색의 file_revision으로 `lore wiki read <path> --revision REV`를 호출해 현재 원문을 확인합니다.\n\nObsidian에서는 검색창에서 {}/concepts 및 {}/documents 경로로 범위를 좁히고 페이지의 백링크를 사용합니다.\n",
            dirs.wiki, dirs.wiki,
        ),
        Locale::En => format!(
            "# Knowledge navigation\n\n`lore wiki refresh` refreshes the SQLite navigation snapshot.\n\n- Start a retrieval session with `lore wiki prepare` and retain its snapshot ID (or the ID printed by refresh).\n- Repeat `lore wiki browse --snapshot ID --json` and `lore wiki search <query> --snapshot ID --json`.\n- Inspect links/backlinks with `lore wiki links <path> --snapshot ID --direction both`.\n- Verify current originals with `lore wiki read <path> --revision REV`, using the hit's file_revision.\n\nIn Obsidian, narrow search to {}/concepts and {}/documents and use page backlinks.\n",
            dirs.wiki, dirs.wiki,
        ),
    };
    let after = format!("{NAVIGATION_ENTRY_MARKER}{guide}");
    let mut plan = MutationPlan::default();
    match before {
        Some(before) => plan.edit(path, before, after),
        None => plan.create(path, after),
    }
    Ok(plan)
}

/// Exact configured runtime entry locations are not source evidence, including
/// when a source root overlaps the wiki through a filesystem alias.
pub(crate) struct ReservedNavigationPaths {
    files: BTreeSet<PathBuf>,
}

impl ReservedNavigationPaths {
    pub(crate) fn load(root: &Path, dirs: &VaultDirs) -> Result<Self, VaultError> {
        let wiki = crate::page_path(root, Path::new(&dirs.wiki))?;
        let mut files = BTreeSet::new();
        for name in lk_core::vault_path::RESERVED_WIKI_FILES {
            match wiki.join(name).canonicalize() {
                Ok(path) => {
                    files.insert(path);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(Self { files })
    }

    pub(crate) fn contains(&self, path: &Path) -> Result<bool, VaultError> {
        Ok(self.files.contains(&path.canonicalize()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MutationSession;

    #[test]
    fn localized_guide_only_writes_its_owned_entry() {
        for locale in [Locale::En, Locale::Ko] {
            let root = tempfile::tempdir().unwrap();
            let dirs = VaultDirs {
                wiki: "knowledge".into(),
                ..Default::default()
            };
            std::fs::create_dir_all(root.path().join("knowledge/catalog")).unwrap();
            let untouched = [
                "knowledge/log.md",
                "knowledge/map.md",
                "knowledge/catalog/0.md",
            ];
            for path in untouched {
                std::fs::write(root.path().join(path), "Authored content.\n").unwrap();
            }
            let plan = plan_navigation_entry(root.path(), locale, &dirs).unwrap();
            let entry = root.path().join("knowledge/index.md");
            assert!(!entry.exists());
            let session = MutationSession::acquire(root.path()).unwrap();
            session.apply(plan).unwrap();
            let first = std::fs::read_to_string(&entry).unwrap();
            assert!(first.starts_with(NAVIGATION_ENTRY_MARKER));
            assert!(first.contains("lore wiki browse --snapshot ID --json"));
            assert!(first.contains("knowledge/concepts"));
            assert!(first.len() < 2000);
            assert!(!first.contains("markdown_exports"));
            assert!(!first.contains("lore wiki map"));
            session
                .apply(plan_navigation_entry(root.path(), locale, &dirs).unwrap())
                .unwrap();
            assert_eq!(std::fs::read_to_string(&entry).unwrap(), first);
            for path in untouched {
                assert_eq!(
                    std::fs::read_to_string(root.path().join(path)).unwrap(),
                    "Authored content.\n"
                );
            }
        }
    }

    #[test]
    fn authored_or_malformed_entries_are_never_adopted() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        std::fs::create_dir(root.path().join("wiki")).unwrap();
        let path = root.path().join("wiki/index.md");
        for body in [
            "# Authored index\n",
            "# Authored\n<!-- lorekeeper:catalog -->\n",
            "---\ntype: document\n---\n<!-- lorekeeper:catalog -->\n# Authored\n",
            "---\ncustom: preserved\n---\n<!-- lorekeeper:catalog -->\n# Authored\n",
            "---\nbroken: [\n---\n<!-- lorekeeper:catalog -->\n",
        ] {
            std::fs::write(&path, body).unwrap();
            assert!(plan_navigation_entry(root.path(), Locale::En, &dirs).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
        }
    }

    #[test]
    fn exact_existing_and_absent_entry_guards_preserve_external_changes() {
        for existing in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let dirs = VaultDirs::default();
            std::fs::create_dir(root.path().join("wiki")).unwrap();
            let path = root.path().join("wiki/index.md");
            if existing {
                std::fs::write(&path, format!("{NAVIGATION_ENTRY_MARKER}Old guide.\n")).unwrap();
            }
            let plan = plan_navigation_entry(root.path(), Locale::En, &dirs).unwrap();
            std::fs::write(&path, "External edit.\n").unwrap();
            let session = MutationSession::acquire(root.path()).unwrap();
            assert!(session.apply(plan).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "External edit.\n");
        }
    }

    #[test]
    fn directory_entry_is_a_read_error_not_an_empty_guide() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("wiki/index.md")).unwrap();
        assert!(plan_navigation_entry(root.path(), Locale::En, &VaultDirs::default()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn leaf_and_escaping_ancestor_aliases_refuse_publication() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("wiki")).unwrap();
        std::fs::write(root.path().join("source.md"), "Authored source.\n").unwrap();
        std::os::unix::fs::symlink("../source.md", root.path().join("wiki/index.md")).unwrap();
        assert!(plan_navigation_entry(root.path(), Locale::En, &VaultDirs::default()).is_err());
        assert_eq!(
            std::fs::read_to_string(root.path().join("source.md")).unwrap(),
            "Authored source.\n"
        );
        std::os::unix::fs::symlink(outside.path(), root.path().join("escaped")).unwrap();
        let dirs = VaultDirs {
            wiki: "escaped".into(),
            ..Default::default()
        };
        assert!(plan_navigation_entry(root.path(), Locale::En, &dirs).is_err());
    }
}
