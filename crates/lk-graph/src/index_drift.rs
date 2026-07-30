use std::collections::HashSet;
use std::path::Path;

use lk_core::config::VaultDirs;
use lk_core::i18n::Locale;
use lk_core::link;

use crate::GraphError;
use crate::scan;

/// A page the catalog does not carry, and an entry no page justifies.
#[derive(Debug)]
pub struct IndexDrift {
    pub missing_from_index: Vec<String>,
    pub missing_from_disk: Vec<String>,
    /// The catalog a re-derivation produces right now — what [`fix`] writes.
    rebuilt: String,
}

impl IndexDrift {
    pub fn is_in_sync(&self) -> bool {
        self.missing_from_index.is_empty() && self.missing_from_disk.is_empty()
    }
}

/// Compare `index.md` against the catalog a re-derivation would produce.
///
/// `index.md` is a materialized view: `lore wiki index` re-derives it WHOLESALE. Drift is
/// therefore exactly the difference between the catalog on disk and the one the BUILDER
/// produces from the current vault — asked of the builder itself, so which pages belong in
/// the catalog is defined in one place. A second definition here disagrees with it on every
/// page the builder does not catalog: this check reports the page missing, `--fix` appends it,
/// and the next `wiki index` drops it again — two repairs undoing each other while the vault
/// stays permanently contradicted.
pub fn diff(root: &Path, locale: Locale, dirs: &VaultDirs) -> Result<IndexDrift, GraphError> {
    let index_rel = Path::new(&dirs.wiki).join(lk_core::vault_path::INDEX_FILE);
    let index_path = root.join(&index_rel);

    let rebuilt = lk_vault::build_index(root, locale, dirs)
        .map_err(|e| GraphError::Io(format!("rebuild {}: {e}", index_rel.display())))?;

    // A MISSING index is a legitimate "not built yet" state → every page reads as missing from
    // it, prompting `lore wiki index`. A real read error (permissions, corruption) must NOT
    // masquerade as "in sync" — propagate it.
    let on_disk = match std::fs::read_to_string(&index_path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(GraphError::Io(format!(
                "read {}: {e}",
                index_path.display()
            )));
        }
    };

    // Both sides read through the same resolution, so a spelling difference between two
    // renderings of one entry cannot register as drift. Summaries are link-stripped by the
    // builder, so every link in a catalog is an entry.
    let catalogued = |content: &str| -> HashSet<String> {
        link::extract_dests(content)
            .into_iter()
            .filter_map(|dest| link::resolve_dest(&index_rel, &dest))
            .map(|resolved| scan::path_slug(&resolved))
            .filter(|id| !id.is_empty())
            .collect()
    };
    let expected = catalogued(&rebuilt);
    let present = catalogued(&on_disk);

    let mut missing_from_index: Vec<String> = expected.difference(&present).cloned().collect();
    missing_from_index.sort();
    let mut missing_from_disk: Vec<String> = present.difference(&expected).cloned().collect();
    missing_from_disk.sort();

    Ok(IndexDrift {
        missing_from_index,
        missing_from_disk,
        rebuilt,
    })
}

/// Write the re-derived catalog, repairing drift in both directions at once. Returns the
/// number of drifted entries the write resolves.
pub fn fix(drift: &IndexDrift, root: &Path, dirs: &VaultDirs) -> Result<usize, GraphError> {
    let repaired = drift.missing_from_index.len() + drift.missing_from_disk.len();
    if repaired == 0 {
        return Ok(0);
    }

    let index_path = root.join(&dirs.wiki).join(lk_core::vault_path::INDEX_FILE);
    if let Some(parent) = index_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| GraphError::Io(format!("create {}: {e}", parent.display())))?;
    }
    lk_core::fs::write_atomic(&index_path, drift.rebuilt.as_bytes(), None)
        .map_err(|e| GraphError::Io(format!("failed to write {}: {e}", index_path.display())))?;

    Ok(repaired)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn concept(root: &Path, slug: &str) {
        write(
            root,
            &format!("wiki/concepts/{slug}.md"),
            &format!(
                "---\nid: {slug}\ntype: concept\ntitle: \"{slug}\"\n---\n\n# {slug}\n\nbody\n"
            ),
        );
    }

    fn index(root: &Path, entries: &str) {
        write(root, "wiki/index.md", &format!("# Index\n\n{entries}"));
    }

    fn drift_of(root: &Path) -> IndexDrift {
        diff(root, Locale::default(), &VaultDirs::default()).unwrap()
    }

    #[test]
    fn a_page_the_catalog_omits_is_drift() {
        let tmp = TempDir::new().unwrap();
        for slug in ["alpha", "beta", "gamma"] {
            concept(tmp.path(), slug);
        }
        index(
            tmp.path(),
            "- [alpha](concepts/alpha.md)\n- [beta](concepts/beta.md)\n",
        );

        let drift = drift_of(tmp.path());
        assert_eq!(drift.missing_from_index, vec!["wiki/concepts/gamma"]);
        assert!(drift.missing_from_disk.is_empty());
    }

    #[test]
    fn an_entry_no_page_justifies_is_drift() {
        let tmp = TempDir::new().unwrap();
        concept(tmp.path(), "alpha");
        index(
            tmp.path(),
            "- [alpha](concepts/alpha.md)\n- [gone](concepts/gone.md)\n",
        );

        let drift = drift_of(tmp.path());
        assert_eq!(drift.missing_from_disk, vec!["wiki/concepts/gone"]);
        assert!(drift.missing_from_index.is_empty());
    }

    #[test]
    fn a_page_the_builder_does_not_catalog_is_not_drift() {
        // The builder catalogs concepts, documents and explorations — not every file that
        // happens to sit under the wiki dir. A second opinion here would report this page
        // missing, `--fix` would append it, and the next `wiki index` would drop it: drift
        // that no repair can clear, from two definitions of the same rule.
        let tmp = TempDir::new().unwrap();
        concept(tmp.path(), "alpha");
        write(
            tmp.path(),
            "wiki/scratch/note.md",
            "---\nid: note\ntype: document\ntitle: \"Note\"\n---\n\n# Note\n",
        );
        index(tmp.path(), "- [alpha](concepts/alpha.md)\n");

        assert!(drift_of(tmp.path()).is_in_sync());
    }

    #[test]
    fn a_missing_index_reports_every_page_missing() {
        // Absence is "not built yet", not "in sync" — that would hide a catalog nobody built.
        let tmp = TempDir::new().unwrap();
        concept(tmp.path(), "alpha");
        concept(tmp.path(), "beta");
        std::fs::create_dir_all(tmp.path().join("wiki")).unwrap();

        let drift = drift_of(tmp.path());
        assert!(!drift.is_in_sync());
        assert_eq!(
            drift.missing_from_index,
            vec!["wiki/concepts/alpha", "wiki/concepts/beta"]
        );
    }

    #[test]
    fn fix_resolves_drift_in_both_directions() {
        let tmp = TempDir::new().unwrap();
        concept(tmp.path(), "alpha");
        concept(tmp.path(), "gamma");
        index(
            tmp.path(),
            "- [alpha](concepts/alpha.md)\n- [gone](concepts/gone.md)\n",
        );

        let drift = drift_of(tmp.path());
        assert_eq!(
            fix(&drift, tmp.path(), &VaultDirs::default()).unwrap(),
            2,
            "one page missing from the catalog, one entry no page justifies"
        );

        let after = drift_of(tmp.path());
        assert!(after.is_in_sync(), "{after:?}");
        let content = std::fs::read_to_string(tmp.path().join("wiki/index.md")).unwrap();
        assert!(content.contains("(concepts/gamma.md)"));
        assert!(!content.contains("(concepts/gone.md)"));
    }

    #[test]
    fn fix_is_a_no_op_without_drift() {
        let tmp = TempDir::new().unwrap();
        concept(tmp.path(), "alpha");
        let rebuilt =
            lk_vault::build_index(tmp.path(), Locale::default(), &VaultDirs::default()).unwrap();
        write(tmp.path(), "wiki/index.md", &rebuilt);

        let drift = drift_of(tmp.path());
        assert!(drift.is_in_sync());
        assert_eq!(fix(&drift, tmp.path(), &VaultDirs::default()).unwrap(), 0);
    }
}
