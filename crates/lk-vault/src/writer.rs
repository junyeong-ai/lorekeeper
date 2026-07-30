use std::path::{Path, PathBuf};

use crate::VaultError;

/// The page format a rendered page declares, or `None` when it carries no frontmatter type.
fn declared_type(content: &str) -> Option<String> {
    lk_core::frontmatter::parse_page(content)
        .ok()?
        .frontmatter
        .get("type")?
        .as_str()
        .map(str::to_owned)
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
/// Judged from the frontmatter both sides declare, so it holds for any route to a collision
/// rather than the one that exposed it — and the other reachable route is a hand-edited `type`,
/// which locks the format that owns a page out of writing it. That is the safe direction and it
/// is loud, so the refusal names both causes and how to get back from either; a failure a user
/// cannot clear is not an improvement on a silent loss.
///
/// A target with no readable type, or content declaring none, is not a contradiction and is
/// allowed: an unparseable page is a different problem and this is not the place that reports
/// it.
fn refuse_format_change(full: &Path, content: &str) -> Result<(), VaultError> {
    let Some(writing) = declared_type(content) else {
        return Ok(());
    };
    let Ok(existing) = std::fs::read_to_string(full) else {
        return Ok(());
    };
    let Some(existing_type) = declared_type(&existing) else {
        return Ok(());
    };
    if existing_type == writing {
        return Ok(());
    }
    Err(VaultError::Io(std::io::Error::other(format!(
        "refusing to write a '{writing}' page over the '{existing_type}' page at {} — two page \
         formats cannot share one file, and this write replaces it wholesale. Either that \
         page's `type` was edited by hand, in which case restore it to '{writing}' or move the \
         page aside and the next run writes it again; or two vault.dirs roots resolve to one \
         directory, which `lore validate` reports.",
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

    pub async fn write_page(&self, rel_path: &Path, content: &str) -> Result<(), VaultError> {
        let full = self.root.join(rel_path);
        let content = content.to_owned();
        // File I/O is blocking and `write_atomic` fsyncs, so publish on the blocking pool
        // rather than reimplementing a non-fsyncing async variant (which is exactly the
        // duplicate that would let the durability guarantee drift).
        tokio::task::spawn_blocking(move || -> Result<(), VaultError> {
            refuse_format_change(&full, &content)?;
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent)?;
            }
            lk_core::fs::write_atomic(&full, content.as_bytes(), None)?;
            Ok(())
        })
        .await
        .map_err(std::io::Error::other)??;
        Ok(())
    }

    /// Sync sibling of [`Self::write_page`] for callers outside a tokio runtime
    /// (e.g. `lk-graph`, which is purely sync). Routes through the same
    /// `lk_core::fs::write_atomic`, so it is not a second atomic-write implementation.
    pub fn write_page_sync(&self, rel_path: &Path, content: &str) -> Result<(), VaultError> {
        let full = self.root.join(rel_path);
        refuse_format_change(&full, content)?;
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)?;
        }
        lk_core::fs::write_atomic(&full, content.as_bytes(), None)?;
        Ok(())
    }
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
        // A page with no declared type is not a contradiction either way.
        let plain = Path::new("wiki/index.md");
        writer.write_page(plain, "# Index\n").await.unwrap();
        writer.write_page(plain, "# Index v2\n").await.unwrap();

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
