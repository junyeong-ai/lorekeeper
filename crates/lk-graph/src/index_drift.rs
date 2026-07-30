use std::collections::HashSet;
use std::path::Path;

use lk_core::config::VaultDirs;
use lk_core::i18n::Locale;
use lk_core::link;

use crate::GraphError;
use crate::scan;

/// A page the catalog does not carry, an entry no page justifies, and whether the catalog
/// differs from a re-derivation at all.
#[derive(Debug)]
pub struct IndexDrift {
    pub missing_from_index: Vec<String>,
    pub missing_from_disk: Vec<String>,
    /// Whether the file differs from what re-deriving it produces, in ANY way.
    ///
    /// The two lists above name which PAGES differ, which is what a reader needs — but they are
    /// not the verdict. A catalog carries each entry's title and one-line summary as well as its
    /// link, so renaming a page leaves the catalog stating a title no page has while the page
    /// set is unchanged and both lists are empty. Drift is the file disagreeing with its own
    /// derivation; the lists explain it.
    stale: bool,
    /// The catalog a re-derivation produces right now — what [`fix`] writes.
    rebuilt: String,
}

impl IndexDrift {
    pub fn is_in_sync(&self) -> bool {
        !self.stale
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
        stale: rebuilt != on_disk,
        rebuilt,
    })
}

/// Write the re-derived catalog, repairing drift in both directions at once. Returns the
/// number of drifted entries the write resolves.
pub fn fix(drift: &IndexDrift, root: &Path, dirs: &VaultDirs) -> Result<usize, GraphError> {
    if drift.is_in_sync() {
        return Ok(0);
    }
    // The write is the whole catalog either way; the number reported is how many PAGES the
    // repair settles. A catalog stale only in an entry's title settles none of them by that
    // count and is still rewritten — reporting 0 while writing would be the lie.
    let repaired = (drift.missing_from_index.len() + drift.missing_from_disk.len()).max(1);

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
        // The catalog as the builder writes it, so the only question left is the extra page.
        let built =
            lk_vault::build_index(tmp.path(), Locale::default(), &VaultDirs::default()).unwrap();
        write(tmp.path(), "wiki/index.md", &built);
        write(
            tmp.path(),
            "wiki/scratch/note.md",
            "---\nid: note\ntype: document\ntitle: \"Note\"\n---\n\n# Note\n",
        );

        let drift = drift_of(tmp.path());
        assert!(drift.missing_from_index.is_empty(), "{drift:?}");
        assert!(drift.is_in_sync(), "{drift:?}");
    }

    #[test]
    fn a_catalog_stating_a_title_no_page_has_is_drift() {
        // The page set is unchanged, so neither list names anything — and the catalog still
        // says something no page does. Drift is the file disagreeing with its own derivation.
        let tmp = TempDir::new().unwrap();
        concept(tmp.path(), "alpha");
        let built =
            lk_vault::build_index(tmp.path(), Locale::default(), &VaultDirs::default()).unwrap();
        // Only the display text — the link, and so the page set, is untouched.
        write(
            tmp.path(),
            "wiki/index.md",
            &built.replace("[alpha]", "[Renamed]"),
        );

        let drift = drift_of(tmp.path());
        assert!(drift.missing_from_index.is_empty());
        assert!(!drift.is_in_sync(), "a stale title is drift");
        assert_eq!(fix(&drift, tmp.path(), &VaultDirs::default()).unwrap(), 1);
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
