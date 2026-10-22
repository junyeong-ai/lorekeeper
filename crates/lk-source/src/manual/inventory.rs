//! Acquisition-only incoming-reference inventory. A complete scan never proves publication.
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::SourceError;
use crate::manual_bundle::{
    MAX_DEPTH, direct_member, html_references, local_path, markdown_references, text_kind,
};

fn error(message: impl std::fmt::Display) -> SourceError {
    SourceError::Parse(format!("manual dependency inventory: {message}"))
}

#[derive(Debug, PartialEq, Eq)]
enum EntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Debug)]
struct TextRevision {
    bytes: usize,
    digest: blake3::Hash,
}

/// Every direct markup entry is a potential owner, including entries omitted from admission.
/// Diagnostics mean incoming-reference coverage is incomplete and block retirement.
#[derive(Debug)]
pub struct ManualDependencyInventory {
    root_address: PathBuf,
    root: PathBuf,
    membership: BTreeMap<OsString, EntryKind>,
    texts: BTreeMap<PathBuf, TextRevision>,
    members: BTreeSet<PathBuf>,
    referenced: BTreeSet<String>,
    diagnostics: Vec<String>,
}

impl ManualDependencyInventory {
    pub fn capture(root: &Path, text_limit: usize) -> Result<Self, SourceError> {
        if text_limit == 0 {
            return Err(error("text limit must be positive"));
        }
        let root_address = root.to_path_buf();
        let root = root.canonicalize().map_err(error)?;
        let membership = list(&root)?;
        let mut texts = BTreeMap::new();
        let mut members = BTreeSet::new();
        let mut referenced = BTreeSet::new();
        let mut diagnostics = BTreeSet::new();
        let mut adjacency = BTreeMap::new();
        for (name, kind) in &membership {
            let entry = PathBuf::from(name);
            if *kind == EntryKind::Symlink {
                diagnostics.insert(format!(
                    "{}: symlink entry has unknown dependency coverage",
                    entry.display()
                ));
                continue;
            }
            if text_kind(&entry).is_none() && !opaque_dependencies(&entry) {
                continue;
            }
            if *kind != EntryKind::File {
                if *kind != EntryKind::Directory {
                    diagnostics.insert(format!("{}: not a regular markup entry", entry.display()));
                }
                continue;
            }
            let mut visited = BTreeSet::from([entry.clone()]);
            let mut pending = VecDeque::from([(entry.clone(), 0usize)]);
            while let Some((member, depth)) = pending.pop_front() {
                if depth > MAX_DEPTH {
                    diagnostics.insert(format!(
                        "{}: dependency depth exceeds {MAX_DEPTH}",
                        entry.display()
                    ));
                    continue;
                }
                if !adjacency.contains_key(&member) {
                    let dependencies = dependencies(
                        &root,
                        &member,
                        text_limit,
                        &mut texts,
                        &mut members,
                        &mut diagnostics,
                    );
                    adjacency.insert(member.clone(), dependencies);
                }
                for next in &adjacency[&member] {
                    if next.components().count() == 1 && !membership.contains_key(next.as_os_str())
                    {
                        diagnostics.insert(format!(
                            "{}: reference is not an exact top-level entry: {}",
                            member.display(),
                            next.display()
                        ));
                    }
                    if next != &entry
                        && next.components().count() == 1
                        && let Some(name) = next.to_str()
                    {
                        referenced.insert(name.to_owned());
                    }
                    if visited.insert(next.clone()) {
                        if visited.len() > lk_core::manual_original::MAX_ORIGINAL_MEMBERS {
                            diagnostics.insert(format!(
                                "{}: dependency member limit exceeded",
                                entry.display()
                            ));
                            pending.clear();
                            break;
                        }
                        pending.push_back((next.clone(), depth + 1));
                    }
                }
            }
        }
        Ok(Self {
            root_address,
            root,
            membership,
            texts,
            members,
            referenced,
            diagnostics: diagnostics.into_iter().collect(),
        })
    }

    pub fn is_referenced(&self, entry: &str) -> bool {
        self.referenced.contains(entry)
    }

    pub fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }

    pub(super) fn is_for_root(&self, root: &Path) -> bool {
        self.root == root
    }

    pub(super) fn has_entry(&self, entry: &str) -> bool {
        self.membership.get(std::ffi::OsStr::new(entry)) == Some(&EntryKind::File)
    }

    /// Recheck exact membership, contained regular paths, and bytes before retirement.
    /// External editors remain outside this check; the caller owns publication ordering.
    pub fn is_current(&self) -> Result<bool, SourceError> {
        if !self.diagnostics.is_empty() {
            return Ok(false);
        }
        match self.root_address.canonicalize() {
            Ok(root) if root == self.root => {}
            Ok(_) => return Ok(false),
            Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(failure) => return Err(error(failure)),
        }
        if list(&self.root)? != self.membership {
            return Ok(false);
        }
        for member in &self.members {
            if direct_member(&self.root, member).is_err() {
                return Ok(false);
            }
        }
        for (member, expected) in &self.texts {
            let path = match direct_member(&self.root, member) {
                Ok(path) => path,
                Err(_) => return Ok(false),
            };
            let file = std::fs::File::open(path).map_err(error)?;
            let metadata = file.metadata().map_err(error)?;
            if !metadata.is_file() || metadata.len() != expected.bytes as u64 {
                return Ok(false);
            }
            let (bytes, digest) = lk_core::captured_file::hash_reader(
                file.take((expected.bytes as u64).saturating_add(1)),
            )
            .map_err(error)?;
            if bytes != expected.bytes || digest.value != expected.digest.to_hex().as_str() {
                return Ok(false);
            }
            if direct_member(&self.root, member).is_err() {
                return Ok(false);
            }
        }
        Ok(list(&self.root)? == self.membership)
    }
}

fn list(root: &Path) -> Result<BTreeMap<OsString, EntryKind>, SourceError> {
    std::fs::read_dir(root)
        .map_err(error)?
        .map(|entry| {
            let entry = entry.map_err(error)?;
            let kind = entry.file_type().map_err(error)?;
            Ok((
                entry.file_name(),
                if kind.is_symlink() {
                    EntryKind::Symlink
                } else if kind.is_file() {
                    EntryKind::File
                } else if kind.is_dir() {
                    EntryKind::Directory
                } else {
                    EntryKind::Other
                },
            ))
        })
        .collect()
}

fn opaque_dependencies(path: &Path) -> bool {
    path.extension().is_some_and(|extension| {
        extension.eq_ignore_ascii_case("css") || extension.eq_ignore_ascii_case("svg")
    })
}

fn read_text(path: &Path, limit: usize) -> Result<(String, TextRevision), SourceError> {
    let mut file = std::fs::File::open(path).map_err(error)?;
    if !file.metadata().map_err(error)?.is_file() {
        return Err(error("not a regular markup member"));
    }
    let mut bytes = Vec::new();
    let mut digest = blake3::Hasher::new();
    let mut buffer = [0; 65536];
    loop {
        let remaining = limit.saturating_sub(bytes.len());
        let count = (&mut file)
            .take(remaining.saturating_add(1) as u64)
            .read(&mut buffer)
            .map_err(error)?;
        if count == 0 {
            break;
        }
        if count > remaining {
            return Err(error(format!("markup exceeds {limit}-byte text limit")));
        }
        bytes.extend_from_slice(&buffer[..count]);
        digest.update(&buffer[..count]);
    }
    let revision = TextRevision {
        bytes: bytes.len(),
        digest: digest.finalize(),
    };
    Ok((String::from_utf8(bytes).map_err(error)?, revision))
}

fn dependencies(
    root: &Path,
    member: &Path,
    text_limit: usize,
    texts: &mut BTreeMap<PathBuf, TextRevision>,
    members: &mut BTreeSet<PathBuf>,
    diagnostics: &mut BTreeSet<String>,
) -> Vec<PathBuf> {
    let mut scan = || -> Result<_, SourceError> {
        let path = direct_member(root, member)?;
        members.insert(member.to_path_buf());
        let Some(html) = text_kind(&path) else {
            if opaque_dependencies(&path) {
                diagnostics.insert(format!(
                    "{}: opaque CSS/SVG dependencies are unsupported",
                    member.display()
                ));
            }
            return Ok(Vec::new());
        };
        let (text, revision) = read_text(&path, text_limit)?;
        direct_member(root, member)?;
        texts.insert(member.to_path_buf(), revision);
        let (references, unsupported) = if html {
            html_references(&text)
        } else {
            let (_, body) = super::split_frontmatter(&text, &path);
            markdown_references(&body)?
        };
        for construct in unsupported {
            diagnostics.insert(format!("{}: unsupported {construct}", member.display()));
        }
        let mut next = BTreeSet::new();
        for reference in references {
            match local_path(root, member, &reference, false) {
                Ok(Some(path)) => {
                    next.insert(path);
                }
                Ok(None) => {}
                Err(failure) => {
                    diagnostics.insert(format!("{}: {failure}", member.display()));
                }
            }
        }
        Ok(next.into_iter().collect())
    };
    match scan() {
        Ok(dependencies) => dependencies,
        Err(failure) => {
            diagnostics.insert(format!("{}: {failure}", member.display()));
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, path: &str, text: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn omitted_markup_roots_and_frontmatter_have_the_same_reference_grammar() {
        let root = tempfile::tempdir().unwrap();
        write(root.path(), "selected.txt", "literal text");
        write(
            root.path(),
            "omitted.MARKDOWN",
            "---\nexample: '[not a dependency](absent.md)'\n---\n[selected](selected.txt)\n",
        );
        write(
            root.path(),
            "omitted.HTML",
            "<a href='selected.txt'>Selected</a>",
        );
        let inventory = ManualDependencyInventory::capture(root.path(), 4096).unwrap();
        assert!(inventory.is_referenced("selected.txt"));
        assert!(!inventory.is_referenced("absent.md"));
        assert!(inventory.diagnostics().is_empty());
        assert!(inventory.is_current().unwrap());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 3);
    }

    #[test]
    fn nested_shared_sources_propagate_every_roots_provenance_and_terminate_cycles() {
        let root = tempfile::tempdir().unwrap();
        write(
            root.path(),
            "a.md",
            "[shared](nested/shared.md)\n[self](a.md)",
        );
        write(
            root.path(),
            "b.html",
            "<a href='nested/shared.md'>Shared</a>",
        );
        write(
            root.path(),
            "nested/shared.md",
            "[cycle](loop.html)\n[A](../a.md)\n[target](../target.pdf)",
        );
        write(
            root.path(),
            "nested/loop.html",
            "<a href='shared.md'>Cycle</a>",
        );
        write(root.path(), "target.pdf", "binary attachment stays unread");
        let inventory = ManualDependencyInventory::capture(root.path(), 4096).unwrap();
        assert!(
            inventory.diagnostics().is_empty(),
            "{:?}",
            inventory.diagnostics()
        );
        assert!(inventory.is_referenced("a.md"), "the b root also reaches a");
        assert!(inventory.is_referenced("target.pdf"));
        assert!(!inventory.is_referenced("b.html"));
        assert_eq!(inventory.texts.len(), 4, "shared markup is read once");
        assert!(inventory.is_current().unwrap());
    }

    #[test]
    fn self_reference_does_not_protect_its_only_root() {
        let root = tempfile::tempdir().unwrap();
        write(root.path(), "a.md", "[nested](nested/body.md)");
        write(root.path(), "nested/body.md", "[self](../a.md)");
        let inventory = ManualDependencyInventory::capture(root.path(), 4096).unwrap();
        assert!(!inventory.is_referenced("a.md"));
        assert!(inventory.is_current().unwrap());
    }

    #[test]
    fn binary_leaves_have_path_guards_without_copies_or_content_hashes() {
        let root = tempfile::tempdir().unwrap();
        write(root.path(), "a.md", "[binary](nested/file.pdf)");
        write(root.path(), "nested/file.pdf", "original binary");
        let inventory = ManualDependencyInventory::capture(root.path(), 4096).unwrap();
        assert!(inventory.diagnostics().is_empty());
        assert_eq!(inventory.texts.len(), 1);
        assert_eq!(inventory.members.len(), 2);
        write(
            root.path(),
            "nested/file.pdf",
            "changed bytes remain the bundle capture's guard",
        );
        assert!(inventory.is_current().unwrap());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 2);
        std::fs::remove_file(root.path().join("nested/file.pdf")).unwrap();
        assert!(!inventory.is_current().unwrap());
    }

    #[test]
    fn dependency_depth_keeps_the_original_bundle_bound() {
        let root = tempfile::tempdir().unwrap();
        write(root.path(), "a.md", "[nested](nested/0.md)");
        for index in 0..=MAX_DEPTH {
            write(
                root.path(),
                &format!("nested/{index}.md"),
                &format!("[next]({}.md)", index + 1),
            );
        }
        let inventory = ManualDependencyInventory::capture(root.path(), 4096).unwrap();
        assert!(
            inventory
                .diagnostics()
                .iter()
                .any(|message| message.contains("depth exceeds"))
        );
        assert!(!inventory.is_current().unwrap());
    }

    #[test]
    fn changed_text_and_added_or_removed_roots_invalidate_exact_capture() {
        let root = tempfile::tempdir().unwrap();
        write(root.path(), "a.md", "Original body");
        let inventory = ManualDependencyInventory::capture(root.path(), 4096).unwrap();
        write(root.path(), "a.md", "Changed body!");
        assert!(
            !inventory.is_current().unwrap(),
            "equal lengths still require a digest"
        );
        let inventory = ManualDependencyInventory::capture(root.path(), 4096).unwrap();
        write(root.path(), "later.html", "<a href='a.md'>New owner</a>");
        assert!(!inventory.is_current().unwrap());
        let inventory = ManualDependencyInventory::capture(root.path(), 4096).unwrap();
        std::fs::remove_file(root.path().join("a.md")).unwrap();
        assert!(!inventory.is_current().unwrap());
    }

    #[test]
    fn reference_spelling_never_uses_filesystem_folding_as_retirement_proof() {
        let root = tempfile::tempdir().unwrap();
        write(root.path(), "owner.md", "[attachment](TARGET.pdf)");
        write(root.path(), "target.pdf", "original attachment");
        let inventory = ManualDependencyInventory::capture(root.path(), 4096).unwrap();
        assert!(inventory.diagnostics().iter().any(|message| {
            message.contains("not an exact top-level entry") && message.contains("TARGET.pdf")
        }));
        assert!(!inventory.is_current().unwrap());
        assert_eq!(
            std::fs::read(root.path().join("target.pdf")).unwrap(),
            b"original attachment"
        );
    }

    #[test]
    fn exact_text_limit_is_allowed_and_overflow_refuses_without_truncation() {
        let root = tempfile::tempdir().unwrap();
        write(root.path(), "entry.md", "1234");
        let exact = ManualDependencyInventory::capture(root.path(), 4).unwrap();
        assert!(exact.diagnostics().is_empty());
        assert!(exact.is_current().unwrap());
        let oversized = ManualDependencyInventory::capture(root.path(), 3).unwrap();
        assert!(!oversized.diagnostics().is_empty());
        assert!(!oversized.is_current().unwrap());
        assert_eq!(
            std::fs::read(root.path().join("entry.md")).unwrap(),
            b"1234"
        );
    }

    #[cfg(unix)]
    #[test]
    fn inbox_root_alias_is_pinned_even_when_replacement_contents_match() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        for name in ["first", "second"] {
            std::fs::create_dir(directory.path().join(name)).unwrap();
            write(&directory.path().join(name), "entry.md", "same source");
        }
        let alias = directory.path().join("inbox");
        symlink(directory.path().join("first"), &alias).unwrap();
        let inventory = ManualDependencyInventory::capture(&alias, 4096).unwrap();
        assert!(inventory.is_current().unwrap());
        std::fs::remove_file(&alias).unwrap();
        symlink(directory.path().join("second"), &alias).unwrap();
        assert!(!inventory.is_current().unwrap());
    }

    #[test]
    fn unsupported_unreadable_and_over_limit_markup_blocks_only_retirement() {
        for (name, bytes, limit) in [
            ("style.css", b"@import 'a.md';".as_slice(), 4096),
            ("shape.svg", b"<svg/>".as_slice(), 4096),
            ("source.html", b"<base href='elsewhere/'>".as_slice(), 4096),
            ("source.md", b"[[implicit]]".as_slice(), 4096),
            ("source.md", b"\xff".as_slice(), 4096),
            ("source.md", b"too long".as_slice(), 3),
            ("source.md", b"[missing](absent.md)".as_slice(), 4096),
        ] {
            let root = tempfile::tempdir().unwrap();
            std::fs::write(root.path().join(name), bytes).unwrap();
            let inventory = ManualDependencyInventory::capture(root.path(), limit).unwrap();
            assert!(!inventory.diagnostics().is_empty(), "{name}");
            assert!(!inventory.is_current().unwrap());
            assert_eq!(std::fs::read(root.path().join(name)).unwrap(), bytes);
            assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
        }
        let root = tempfile::tempdir().unwrap();
        assert!(ManualDependencyInventory::capture(&root.path().join("missing"), 4096).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_roots_and_changed_nested_paths_never_prove_complete_coverage() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        write(root.path(), "owner.md", "[nested](nested/body.md)");
        write(root.path(), "nested/body.md", "[owner](../owner.md)");
        let inventory = ManualDependencyInventory::capture(root.path(), 4096).unwrap();
        std::fs::rename(root.path().join("nested"), root.path().join("moved")).unwrap();
        symlink(root.path().join("moved"), root.path().join("nested")).unwrap();
        assert!(!inventory.is_current().unwrap());
        let inventory = ManualDependencyInventory::capture(root.path(), 4096).unwrap();
        assert!(!inventory.diagnostics().is_empty());
        assert!(!inventory.is_current().unwrap());

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write(outside.path(), "owner.md", "[unknown](other.md)");
        symlink(
            outside.path().join("owner.md"),
            root.path().join("owner.md"),
        )
        .unwrap();
        let inventory = ManualDependencyInventory::capture(root.path(), 4096).unwrap();
        assert!(!inventory.diagnostics().is_empty());
        assert!(
            inventory.texts.is_empty(),
            "never follow the markup symlink"
        );
        assert!(!inventory.is_current().unwrap());
    }
}
