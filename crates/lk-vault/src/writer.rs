use std::path::{Path, PathBuf};

use crate::VaultError;

/// What a page's bytes say its format is. `Untyped` is a page that parses and carries no
/// `type`; `Illegible` is one whose bytes do not yield an answer at all — unclosed or invalid
/// frontmatter, a `type` that is not a string. The three are kept apart because they license
/// different writes: an untyped page states no format to preserve, an illegible one states
/// nothing we can rely on.
enum Format {
    Untyped,
    Declared(String),
    Illegible,
}

impl Format {
    fn of(content: &str) -> Self {
        let Ok(page) = lk_core::frontmatter::parse_page(content) else {
            return Self::Illegible;
        };
        match page.frontmatter.get("type") {
            None => Self::Untyped,
            Some(value) => value
                .as_str()
                .map_or(Self::Illegible, |name| Self::Declared(name.to_owned())),
        }
    }

    fn describe(&self) -> String {
        match self {
            Self::Untyped => "a page with no `type`".to_string(),
            Self::Declared(name) => format!("a '{name}' page"),
            Self::Illegible => "a page whose `type` cannot be read".to_string(),
        }
    }
}

/// Where a page's content comes from, which decides how much of it there is to protect.
///
/// [`Provenance::Authored`] pages carry material no renderer can reproduce — an LLM section, a
/// hand-written synthesis, a curated category — so a write onto one that cannot be shown to
/// preserve its format is refused. [`Provenance::Generated`] pages are re-derived wholesale from
/// config and the vault on every run, so the only thing worth refusing is another format's page
/// sitting at the same path.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    Authored,
    Generated,
}

/// Refuse a write that would replace a page of one format with a page of another.
///
/// A page's format is decided by where it sits, so two page formats sharing a path is a
/// contradiction the vault cannot represent — and the write is a whole-file replacement, so
/// resolving it by proceeding destroys the page that was there. It happens: two `vault.dirs`
/// roots the filesystem treats as one directory (case, Unicode normalization, a symlink) put
/// `<daily>/{source-id}/{date}.md` and `<wiki>/concepts/{slug}.md` on the same file, and a
/// source id of `concepts` with an event dated like a concept's slug then has a daily digest
/// overwrite a curated concept page — hand-written synthesis, established category and
/// citation count gone, `exit 0`, nothing in `lint` or `doctor` hinting at a loss.
///
/// A write proceeds only when the format it leaves behind is provably the format already there.
/// The three licensed cases are: nothing is there yet; the page there declares no format, so
/// there is none to contradict (which is what lets a page whose frontmatter was deleted be
/// re-rendered, and what lets the untyped wiki catalog and timeline rewrite themselves); or both
/// sides name the same format. Everything else is refused — a typed page about to be replaced by
/// an untyped one is replaced just as wholesale, and a target whose bytes cannot be read or
/// parsed states no format to check, so overwriting on the strength of not knowing is the loss
/// itself.
///
/// Judged from what the bytes declare, so it holds for any route to a collision rather than the
/// one that exposed it — and the other reachable route is a hand-edited `type`, which locks the
/// format that owns a page out of writing it. That is the safe direction and it is loud, so the
/// refusal names both causes and how to get back from either; a failure a user cannot clear is
/// not an improvement on a silent loss.
///
/// [`Provenance::Generated`] narrows it to the type-against-type case. A page rendered wholesale
/// from config holds nothing a user wrote, so "cannot read what is there" is not a reason to
/// refuse — and refusing meant `lore schema` could not repair its own output once that output was
/// truncated by hand or left write-only, which is a defect the guard introduced rather than
/// prevented. A different DECLARED format still refuses, because that is a contradiction rather
/// than damage.
///
/// Coverage bound: this guards writes routed through `VaultWriter`. `graph merge`,
/// `graph normalize` and `index_drift` call `lk_core::fs::write_atomic` directly — each edits a
/// page in place without changing its format, so none of them can be the write this refuses.
/// The check and the publish are separate syscalls, so two concurrent processes writing
/// different formats to one path can both pass it; the vault has no cross-process lock, and
/// every writer in it is check-then-write.
fn refuse_format_change(
    full: &Path,
    content: &str,
    provenance: Provenance,
) -> Result<(), VaultError> {
    let existing = match std::fs::read_to_string(full) {
        Ok(existing) => existing,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) if provenance == Provenance::Generated => return Ok(()),
        Err(error) => {
            return Err(VaultError::Io(std::io::Error::new(
                error.kind(),
                format!(
                    "refusing to write {} over the page at {}: reading it back failed ({error}), \
                     so this write cannot be shown to preserve its format. Move that page aside \
                     and the next run writes it again.",
                    Format::of(content).describe(),
                    full.display()
                ),
            )));
        }
    };
    let writing = Format::of(content);
    let present = Format::of(&existing);
    if provenance == Provenance::Generated && matches!(present, Format::Illegible) {
        return Ok(());
    }
    match (&present, &writing) {
        (Format::Untyped, _) => return Ok(()),
        (Format::Declared(present), Format::Declared(writing)) if present == writing => {
            return Ok(());
        }
        _ => {}
    }
    Err(VaultError::Io(std::io::Error::other(format!(
        "refusing to write {} over {} at {} — two page formats cannot share one file, and this \
         write replaces it wholesale. Either that page's `type` was edited by hand, in which case \
         restore it or move the page aside and the next run writes it again; or two directories \
         the vault addresses separately are one directory on disk, which `lore validate` reports \
         when the two are `vault.dirs` roots. This will keep failing until one of those is \
         resolved.",
        writing.describe(),
        present.describe(),
        full.display()
    ))))
}

/// Atomic, durable writer for vault pages. Both methods publish through the single
/// `lk_core::fs::write_atomic` (temp + fsync + rename + dir-fsync, per-writer-unique
/// temp), so the durability and unique-temp guarantees are single-sourced — there is no
/// second atomic-write implementation that could drift. `write_page` is the async ingest
/// path; `write_page_sync` is the sync path (`lk-graph`).
pub struct VaultWriter {
    root: PathBuf,
}

impl VaultWriter {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Write a page whose content includes material nothing can re-derive.
    pub async fn write_page(&self, rel_path: &Path, content: &str) -> Result<(), VaultError> {
        self.write(rel_path, content, Provenance::Authored).await
    }

    /// Write a page rendered wholesale from config and the vault, like the wiki catalog, timeline,
    /// map and page-format schema. Named separately rather than taking a flag so the four callers
    /// that may claim it are the four the compiler can point at.
    pub async fn write_generated_page(
        &self,
        rel_path: &Path,
        content: &str,
    ) -> Result<(), VaultError> {
        self.write(rel_path, content, Provenance::Generated).await
    }

    async fn write(
        &self,
        rel_path: &Path,
        content: &str,
        provenance: Provenance,
    ) -> Result<(), VaultError> {
        let full = self.root.join(rel_path);
        let content = content.to_owned();
        // File I/O is blocking and `write_atomic` fsyncs, so publish on the blocking pool
        // rather than reimplementing a non-fsyncing async variant (which is exactly the
        // duplicate that would let the durability guarantee drift).
        tokio::task::spawn_blocking(move || write_blocking(&full, &content, provenance))
            .await
            .map_err(std::io::Error::other)??;
        Ok(())
    }

    /// Sync sibling of [`Self::write_page`] for callers outside a tokio runtime
    /// (e.g. `lk-graph`, which is purely sync). Routes through the same
    /// `lk_core::fs::write_atomic`, so it is not a second atomic-write implementation.
    pub fn write_page_sync(&self, rel_path: &Path, content: &str) -> Result<(), VaultError> {
        write_blocking(&self.root.join(rel_path), content, Provenance::Authored)
    }

    /// Sync sibling of [`Self::write_generated_page`].
    pub fn write_generated_page_sync(
        &self,
        rel_path: &Path,
        content: &str,
    ) -> Result<(), VaultError> {
        write_blocking(&self.root.join(rel_path), content, Provenance::Generated)
    }
}

/// The one place a vault page is guarded and published, so the async and sync paths cannot drift
/// on either.
fn write_blocking(full: &Path, content: &str, provenance: Provenance) -> Result<(), VaultError> {
    refuse_format_change(full, content, provenance)?;
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent)?;
    }
    lk_core::fs::write_atomic(full, content.as_bytes(), None)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn setup() -> (TempDir, VaultWriter) {
        let dir = TempDir::new().unwrap();
        let writer = VaultWriter::new(dir.path());
        (dir, writer)
    }

    fn page(page_type: &str, body: &str) -> String {
        format!("---\nid: p\ntype: {page_type}\ntitle: \"P\"\n---\n\n{body}\n")
    }

    /// A page's format comes from where it sits, so two formats on one path is a
    /// contradiction — and the write replaces the whole file, so proceeding destroys what was
    /// there. Reproduced in the field: two `vault.dirs` roots the filesystem treats as one
    /// directory put a daily digest and a curated concept page on the same file, and ingest
    /// overwrote hand-written synthesis, an established category and a citation count, exit 0,
    /// with nothing in `lint` or `doctor` naming a loss.
    #[tokio::test]
    async fn a_write_that_would_change_a_pages_format_is_refused() {
        let (dir, writer) = setup().await;
        let rel = Path::new("concepts/2026-07-29.md");
        let curated = page("concept", "## 핵심\n\nhand-written");
        writer.write_page(rel, &curated).await.unwrap();

        let err = writer
            .write_page(rel, &page("daily", "## 요약"))
            .await
            .expect_err("a daily page must not replace a concept page");
        let message = format!("{err}");
        assert!(message.contains("refusing to write"), "{message}");
        assert!(
            message.contains("'daily'") && message.contains("'concept'"),
            "{message}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join(rel)).unwrap(),
            curated,
            "the page that was there must be untouched"
        );

        // The same format is the ordinary re-render and must go through.
        writer
            .write_page(rel, &page("concept", "## 핵심\n\nrefreshed"))
            .await
            .unwrap();
        // And the sync path shares the guard.
        writer
            .write_page_sync(rel, &page("daily", "## 요약"))
            .expect_err("the sync writer refuses too");
        // A page declaring no format states none to contradict, so the untyped catalog and
        // timeline rewrite themselves, and a page whose frontmatter was deleted is re-rendered.
        let plain = Path::new("wiki/index.md");
        writer.write_page(plain, "# Index\n").await.unwrap();
        writer.write_page(plain, "# Index v2\n").await.unwrap();
        writer
            .write_page(plain, &page("concept", "## 핵심"))
            .await
            .unwrap();

        // The other reachable cause is a hand-edited `type`, which locks the format that owns
        // the page out of it — so the refusal has to say how to get back, and both routes back
        // must work: restore the type, or move the page aside.
        assert!(message.contains("edited by hand"), "{message}");
        assert!(message.contains("move the page aside"), "{message}");
        std::fs::remove_file(dir.path().join(rel)).unwrap();
        writer
            .write_page(rel, &page("daily", "## 요약"))
            .await
            .expect("moving the page aside clears the refusal");
    }

    /// The guard compares what each side of the write says its format is, so the cases where a
    /// side says nothing decide as much as the cases where it says something else. An untyped
    /// write over a typed page replaces it just as wholesale — `lore wiki index` and `lore wiki
    /// log` render no `type` at all, and reaching a concept page through a folded directory is
    /// how they would arrive at one. A target that cannot be parsed says nothing to check, and
    /// proceeding because the answer is unavailable is the same loss with an extra step.
    #[tokio::test]
    async fn a_write_is_refused_unless_the_format_it_leaves_is_the_one_already_there() {
        let (dir, writer) = setup().await;
        let curated = page("concept", "## 핵심\n\nhand-written");

        let typed = Path::new("concepts/index.md");
        writer.write_page(typed, &curated).await.unwrap();
        let err = writer
            .write_page(typed, "# Index\n")
            .await
            .expect_err("an untyped catalog must not replace a concept page");
        assert!(format!("{err}").contains("no `type`"), "{err}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join(typed)).unwrap(),
            curated
        );

        for (name, bytes) in [
            (
                "unclosed.md",
                "---\ntype: concept\n\n# no closing delimiter\n",
            ),
            (
                "listed.md",
                "---\ntype: [concept]\n---\n\n# a type that is not a name\n",
            ),
        ] {
            let rel = Path::new("concepts").join(name);
            std::fs::write(dir.path().join(&rel), bytes).unwrap();
            let err = writer
                .write_page(&rel, &page("daily", "## 요약"))
                .await
                .expect_err("a target whose format cannot be read must not be overwritten");
            assert!(format!("{err}").contains("cannot be read"), "{err}");
            assert_eq!(
                std::fs::read_to_string(dir.path().join(&rel)).unwrap(),
                bytes
            );
        }

        // Bytes that cannot be READ, as distinct from bytes that cannot be parsed. Only one read
        // error means "nothing is there" — every other one means something is there that this
        // could not look at, and proceeding on that is the same overwrite. A directory at the
        // target path produces such an error on every platform.
        let occupied = Path::new("concepts/occupied.md");
        std::fs::create_dir(dir.path().join(occupied)).unwrap();
        let err = writer
            .write_page(occupied, &page("daily", "## 요약"))
            .await
            .expect_err("a target that cannot be read must not be overwritten");
        let message = format!("{err}");
        assert!(message.contains("reading it back failed"), "{message}");
        assert!(
            dir.path().join(occupied).is_dir(),
            "what was there must be untouched"
        );
    }

    /// A page rendered wholesale from config holds nothing a user wrote, so refusing to overwrite
    /// one because its current bytes cannot be read is a defect the guard introduced: `lore
    /// schema` could not repair its own output once that output was truncated by hand. What still
    /// refuses is another DECLARED format at the same path — damage is recoverable, a
    /// contradiction is not.
    #[tokio::test]
    async fn a_generated_page_repairs_itself_but_still_refuses_another_format() {
        let (dir, writer) = setup().await;
        let generated = Path::new("wiki/AGENTS.md");
        let rendered = page("schema", "# Page Formats\n");

        writer
            .write_generated_page(generated, &rendered)
            .await
            .unwrap();

        // Truncated by hand: the frontmatter no longer closes, so nothing can be read from it.
        std::fs::write(dir.path().join(generated), "---\ntype: schema\n").unwrap();
        writer
            .write_generated_page(generated, &rendered)
            .await
            .expect("a generated page is reproducible, so damage to it is repairable");
        assert_eq!(
            std::fs::read_to_string(dir.path().join(generated)).unwrap(),
            rendered
        );

        // Two formats on one path is still a contradiction, generated or not.
        std::fs::write(dir.path().join(generated), page("concept", "## 핵심")).unwrap();
        let err = writer
            .write_generated_page(generated, &rendered)
            .await
            .expect_err("a concept page at this path is not this page damaged");
        assert!(format!("{err}").contains("refusing to write"), "{err}");

        // The sync sibling shares the rule.
        std::fs::write(dir.path().join(generated), "---\ntype: schema\n").unwrap();
        writer
            .write_generated_page_sync(generated, &rendered)
            .unwrap();
    }

    #[tokio::test]
    async fn write_and_read_back() {
        let (dir, writer) = setup().await;
        let rel = Path::new("daily/test/2026-05-23.md");
        writer.write_page(rel, "# Hello").await.unwrap();
        let content = tokio::fs::read_to_string(dir.path().join(rel))
            .await
            .unwrap();
        assert_eq!(content, "# Hello");
    }

    #[test]
    fn sync_write_creates_parents_and_writes_atomically() {
        let dir = TempDir::new().unwrap();
        let writer = VaultWriter::new(dir.path());
        let rel = Path::new("wiki/concepts/a.md");
        writer.write_page_sync(rel, "# Hello").unwrap();
        let content = std::fs::read_to_string(dir.path().join(rel)).unwrap();
        assert_eq!(content, "# Hello");

        // No `.tmp` leftover after a successful write.
        let parent = dir.path().join("wiki/concepts");
        let leftover = std::fs::read_dir(&parent)
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| {
                e.path()
                    .extension()
                    .is_some_and(|ext| ext.to_string_lossy().ends_with(".tmp"))
            });
        assert!(!leftover, "temp file leaked into vault");
    }
}
