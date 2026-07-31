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
/// A generated page gets no exemption from that, though it was given one and had to be taken
/// back. The argument for it — a page re-derived wholesale from config holds nothing a user wrote
/// — is about the page this tool WRITES, not about whatever occupies the path; and when the bytes
/// there cannot be read, telling those apart is precisely what is impossible. A hand-written
/// `wiki/index.md` with one malformed frontmatter delimiter was destroyed by `lore wiki index`,
/// exit 0. What the exemption bought was not having to move a damaged file aside by hand, which is
/// what the refusal already says to do.
///
/// Every page PUBLISHED into the vault goes through here — there is no second path to keep a
/// list of. An in-place edit ([`VaultWriter::edit_page_sync`]) does not, and cannot be the write
/// this refuses: its content is the page it read. Neither is the vault's operational state
/// (`.lorekeeper/`: the queue, the event logs, the ingest log) or `credentials.json`, none of
/// which is a page.
///
/// The check and the publish are separate syscalls, so two concurrent processes writing
/// different formats to one path can both pass it; the vault has no cross-process lock, and
/// every writer in it is check-then-write.
fn refuse_format_change(full: &Path, content: &str) -> Result<(), VaultError> {
    let existing = match std::fs::read_to_string(full) {
        Ok(existing) => existing,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
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

/// Atomic, durable writer for vault pages. Every method publishes through the single
/// `lk_core::fs::write_atomic` (temp + fsync + rename + dir-fsync, per-writer-unique temp), so
/// the durability and unique-temp guarantees are single-sourced — there is no second
/// atomic-write implementation that could drift.
///
/// Two contracts, because a page is replaced for two different reasons. [`Self::write_page`]
/// and [`Self::write_page_sync`] PUBLISH a page rendered from somewhere else — a template, a
/// catalog derivation — and cannot know what occupies the path, so they carry
/// [`refuse_format_change`]. [`Self::edit_page_sync`] rewrites a page from its own bytes, where
/// that question has no meaning: the content IS that page, transformed.
pub struct VaultWriter {
    root: PathBuf,
}

impl VaultWriter {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub async fn write_page(&self, rel_path: &Path, content: &str) -> Result<(), VaultError> {
        let full = self.root.join(rel_path);
        let content = content.to_owned();
        // File I/O is blocking and `write_atomic` fsyncs, so publish on the blocking pool
        // rather than reimplementing a non-fsyncing async variant (which is exactly the
        // duplicate that would let the durability guarantee drift).
        tokio::task::spawn_blocking(move || write_blocking(&full, &content))
            .await
            .map_err(std::io::Error::other)??;
        Ok(())
    }

    /// Sync sibling of [`Self::write_page`] for callers outside a tokio runtime
    /// (e.g. `lk-graph`, which is purely sync). Routes through the same
    /// `lk_core::fs::write_atomic`, so it is not a second atomic-write implementation.
    pub fn write_page_sync(&self, rel_path: &Path, content: &str) -> Result<(), VaultError> {
        write_blocking(&self.root.join(rel_path), content)
    }

    /// Rewrite a page from content derived from its own bytes: a link repointed at a renamed
    /// target, a frontmatter field set. Same atomic publish, no format check.
    ///
    /// The check would have nothing to compare. It asks whether a page of one format is about
    /// to be replaced by a page of ANOTHER, and an edit produces the page it read — including
    /// when those bytes are illegible, which is when a caller most needs to repoint a link out
    /// of them. Refusing there abandons the sweep half-done over a page nobody can parse, and
    /// leaves a citation pointing at a concept that was just deleted.
    pub fn edit_page_sync(&self, rel_path: &Path, content: &str) -> Result<(), VaultError> {
        let full = self.root.join(rel_path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)?;
        }
        lk_core::fs::write_atomic(&full, content.as_bytes(), None)?;
        Ok(())
    }
}

/// The one place a vault page is guarded and published, so the async and sync paths cannot drift
/// on either.
fn write_blocking(full: &Path, content: &str) -> Result<(), VaultError> {
    refuse_format_change(full, content)?;
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

    /// A page whose format cannot be read is refused even when the write regenerates that page
    /// wholesale from config — the exemption that allowed it was withdrawn.
    ///
    /// The exemption's argument was that a generated page holds nothing a user wrote, which is
    /// about the page this tool WRITES, not about whatever occupies the path. A hand-written
    /// `wiki/index.md` with one malformed frontmatter delimiter parses as illegible exactly like a
    /// truncated `AGENTS.md` does, and `lore wiki index` destroyed it, exit 0. Nothing in the bytes
    /// distinguishes the two, so the safe direction is the loud one — and the refusal already says
    /// to move the page aside, which is all the exemption saved.
    #[tokio::test]
    async fn a_generated_page_is_refused_over_bytes_whose_format_cannot_be_read() {
        let (dir, writer) = setup().await;
        let catalog = Path::new("wiki/index.md");
        let authored = "---\ntype: concept\n\n# My hand-written index\n\nDo not erase this.\n";
        std::fs::create_dir_all(dir.path().join("wiki")).unwrap();
        std::fs::write(dir.path().join(catalog), authored).unwrap();

        let err = writer
            .write_page(catalog, "# The catalog\n")
            .await
            .expect_err("an unreadable format is not licence to replace the file");
        assert!(format!("{err}").contains("cannot be read"), "{err}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join(catalog)).unwrap(),
            authored,
            "the hand-written page must survive"
        );

        // Moving it aside is the remedy the refusal names, and it works.
        std::fs::remove_file(dir.path().join(catalog)).unwrap();
        writer.write_page(catalog, "# The catalog\n").await.unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join(catalog)).unwrap(),
            "# The catalog\n"
        );
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

    /// An edit publishes content the page itself produced, so the format question has no
    /// answer to compare — and refusing there is what abandons a repointing sweep half-done
    /// over the one page nobody can parse.
    #[test]
    fn an_edit_rewrites_a_page_whose_frontmatter_will_not_parse() {
        let dir = TempDir::new().unwrap();
        let rel = std::path::Path::new("daily/notes/2026-07-31.md");
        let full = dir.path().join(rel);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        let unparseable = "---\nid: notes\ntitle: unclosed\n\n# Notes\n\n[a](old.md)\n";
        std::fs::write(&full, unparseable).unwrap();

        let writer = VaultWriter::new(dir.path());
        let edited = unparseable.replace("old.md", "new.md");
        writer.edit_page_sync(rel, &edited).unwrap();
        assert_eq!(std::fs::read_to_string(&full).unwrap(), edited);

        // The publish path still refuses: it cannot show it is preserving a format the bytes
        // never stated, and its content did not come from them.
        let refusal = writer
            .write_page_sync(rel, "---\ntype: concept\n---\n\n# Rendered\n")
            .expect_err("a wholesale write over an illegible page is refused");
        assert!(format!("{refusal}").contains("two page formats cannot share one file"));
    }
}
