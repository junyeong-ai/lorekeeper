use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, SystemTime};

use crate::VaultError;

/// Exact bytes and vault-relative owner checked in the current verification phase.
/// The owner is an address, preserving distinct hard links rather than inode identity.
#[derive(Debug)]
pub struct VerifiedFileRevision {
    pub owner: PathBuf,
    pub file_revision: String,
}

/// Cooperative cancellation between synchronous read operations. A pending kernel
/// operation must return before the next check; cancellation never yields a partial capture.
#[derive(Clone, Debug, Default)]
pub struct ReadControl {
    cancelled: Option<Arc<AtomicBool>>,
    page_texts: Option<Arc<PageTexts>>,
}

impl ReadControl {
    pub fn new(cancelled: Arc<AtomicBool>) -> Self {
        Self {
            cancelled: Some(cancelled),
            page_texts: None,
        }
    }

    /// Let this control's discovery traversals reuse page texts an earlier traversal read.
    /// Clones share the reuse; guarded publication reads never consult it.
    pub fn with_page_reuse(mut self) -> Self {
        self.page_texts = Some(Arc::new(PageTexts::new(UNSETTLED, REUSE_BUDGET)));
        self
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
    }

    pub fn check(&self) -> Result<(), VaultError> {
        if self.is_cancelled() {
            Err(VaultError::Cancelled)
        } else {
            Ok(())
        }
    }

    pub fn read_to_string(&self, path: &Path) -> Result<String, VaultError> {
        use std::io::Read;
        self.check()?;
        let file = std::fs::File::open(path);
        self.check()?;
        let mut file = file?;
        let mut bytes = Vec::new();
        let mut buffer = [0; 65536];
        loop {
            self.check()?;
            let result = file.read(&mut buffer);
            self.check()?;
            let size = match result {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                result => result?,
            };
            self.check()?;
            if size == 0 {
                return String::from_utf8(bytes).map_err(|error| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, error).into()
                });
            }
            bytes.extend_from_slice(&buffer[..size]);
        }
    }

    /// Page reuse that keeps even files written moments before their read, so tests can
    /// exercise reuse without waiting out the settling window.
    #[cfg(test)]
    pub(crate) fn with_immediate_page_reuse(mut self) -> Self {
        self.page_texts = Some(Arc::new(PageTexts::new(Duration::ZERO, REUSE_BUDGET)));
        self
    }

    /// A discovery page's complete text, and whether an earlier traversal supplied it.
    /// Callers that guard publication or record evidence read with `read_to_string`.
    pub(crate) fn page_text(&self, path: &Path) -> Result<(Arc<str>, bool), VaultError> {
        let Some(texts) = &self.page_texts else {
            return Ok((self.read_to_string(path)?.into(), false));
        };
        self.check()?;
        let stamp = FileStamp::of(path);
        if let Some(text) = stamp.and_then(|stamp| texts.find(path, stamp)) {
            return Ok((text, true));
        }
        let started = SystemTime::now();
        let text: Arc<str> = self.read_to_string(path)?.into();
        let settled = stamp.filter(|stamp| {
            stamp.settled_before(started, texts.unsettled) && FileStamp::of(path) == Some(*stamp)
        });
        texts.replace(path, settled.map(|stamp| (stamp, text.clone())));
        Ok((text, false))
    }

    /// The text an earlier traversal read, while the file still carries the same stamp.
    pub(crate) fn reusable_page_text(&self, path: &Path) -> Option<Arc<str>> {
        let texts = self.page_texts.as_ref()?;
        texts.find(path, FileStamp::of(path)?)
    }
}

/// Page texts keyed by path. An entry is reused only while the file keeps the device,
/// inode, size and modification and status-change times it had when read. A file
/// modified within `UNSETTLED` before its read is never kept: a later write in the same
/// timestamp tick could leave every stamp field unchanged.
///
/// Kept texts total at most `budget` bytes. Once it is full, further pages are read on
/// every traversal rather than evicting others: traversals walk the corpus in the same
/// order, so evicting the oldest entry would discard each page just before it is needed
/// again, while refusing admission keeps reuse for the pages that fit.
struct PageTexts {
    unsettled: Duration,
    budget: usize,
    entries: Mutex<Entries>,
}

#[derive(Default)]
struct Entries {
    texts: HashMap<PathBuf, (FileStamp, Arc<str>)>,
    bytes: usize,
}

impl std::fmt::Debug for PageTexts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (pages, bytes) = self
            .entries
            .lock()
            .map_or((0, 0), |entries| (entries.texts.len(), entries.bytes));
        f.debug_struct("PageTexts")
            .field("pages", &pages)
            .field("bytes", &bytes)
            .finish()
    }
}

impl PageTexts {
    fn new(unsettled: Duration, budget: usize) -> Self {
        Self {
            unsettled,
            budget,
            entries: Mutex::default(),
        }
    }

    fn find(&self, path: &Path, stamp: FileStamp) -> Option<Arc<str>> {
        let entries = self.entries.lock().ok()?;
        entries
            .texts
            .get(path)
            .filter(|(kept, _)| *kept == stamp)
            .map(|(_, text)| text.clone())
    }

    /// Drops the path's previous text, which can no longer match, and keeps `read` when
    /// it is present and fits the budget.
    fn replace(&self, path: &Path, read: Option<(FileStamp, Arc<str>)>) {
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        if let Some((_, previous)) = entries.texts.remove(path) {
            entries.bytes -= previous.len();
        }
        if let Some((stamp, text)) = read
            && entries.bytes + text.len() <= self.budget
        {
            entries.bytes += text.len();
            entries.texts.insert(path.to_path_buf(), (stamp, text));
        }
    }
}

const UNSETTLED: Duration = Duration::from_secs(2);

/// Three times a 7,400-page vault's 43 MiB of Markdown.
const REUSE_BUDGET: usize = 128 << 20;

/// The status-change time cannot be set by `utimes`, so it bounds the last write even
/// when the modification time was rewound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileStamp {
    device: u64,
    inode: u64,
    length: u64,
    modified: SystemTime,
    changed: SystemTime,
}

impl FileStamp {
    #[cfg(unix)]
    fn of(path: &Path) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(path).ok()?;
        let changed = SystemTime::UNIX_EPOCH.checked_add(Duration::new(
            u64::try_from(metadata.ctime()).ok()?,
            u32::try_from(metadata.ctime_nsec()).ok()?,
        ))?;
        Some(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            modified: metadata.modified().ok()?,
            changed,
        })
    }

    #[cfg(not(unix))]
    fn of(_path: &Path) -> Option<Self> {
        None
    }

    fn settled_before(self, read_started: SystemTime, unsettled: Duration) -> bool {
        read_started
            .checked_sub(unsettled)
            .is_some_and(|settled| self.modified.max(self.changed) < settled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controlled_reads_keep_complete_unicode_and_reject_invalid_or_cancelled_input() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source.md");
        let raw = "Exact 한글 🦀\r\n".repeat(20000);
        std::fs::write(&path, &raw).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let read = ReadControl::new(cancelled.clone());
        assert_eq!(read.read_to_string(&path).unwrap(), raw);
        std::fs::write(&path, [0xff]).unwrap();
        assert!(
            matches!(read.read_to_string(&path), Err(VaultError::Io(error)) if error.kind() == std::io::ErrorKind::InvalidData)
        );
        cancelled.store(true, Ordering::Relaxed);
        assert!(matches!(
            read.read_to_string(&path),
            Err(VaultError::Cancelled)
        ));
        assert!(matches!(
            read.read_to_string(&directory.path().join("absent")),
            Err(VaultError::Cancelled)
        ));
    }

    fn reusing(unsettled: Duration, budget: usize) -> ReadControl {
        ReadControl {
            cancelled: None,
            page_texts: Some(Arc::new(PageTexts::new(unsettled, budget))),
        }
    }

    #[cfg(unix)]
    #[test]
    fn page_texts_are_reused_only_while_the_file_keeps_its_stamp() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("page.md");
        std::fs::write(&path, "first").unwrap();
        let read = ReadControl::default().with_immediate_page_reuse();
        assert_eq!(read.page_text(&path).unwrap(), ("first".into(), false));
        assert_eq!(read.page_text(&path).unwrap(), ("first".into(), true));
        assert_eq!(read.reusable_page_text(&path).as_deref(), Some("first"));

        std::fs::write(&path, "other").unwrap();
        assert_eq!(read.reusable_page_text(&path), None);
        assert_eq!(read.page_text(&path).unwrap(), ("other".into(), false));

        let replacement = directory.path().join("replacement.md");
        std::fs::write(&replacement, "moved").unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert_eq!(read.page_text(&path).unwrap(), ("moved".into(), false));

        std::fs::remove_file(&path).unwrap();
        assert_eq!(read.reusable_page_text(&path), None);
        assert!(read.page_text(&path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn recently_changed_pages_and_plain_controls_are_always_read() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("page.md");
        std::fs::write(&path, "fresh").unwrap();
        let read = reusing(Duration::from_secs(3600), REUSE_BUDGET);
        for _ in 0..2 {
            assert_eq!(read.page_text(&path).unwrap(), ("fresh".into(), false));
        }
        let plain = ReadControl::default();
        for _ in 0..2 {
            assert_eq!(plain.page_text(&path).unwrap(), ("fresh".into(), false));
        }
        assert_eq!(plain.reusable_page_text(&path), None);

        let cancelled = Arc::new(AtomicBool::new(true));
        let read = ReadControl::new(cancelled).with_page_reuse();
        assert!(matches!(read.page_text(&path), Err(VaultError::Cancelled)));
    }

    #[cfg(unix)]
    #[test]
    fn a_full_reuse_budget_keeps_earlier_pages_and_rereads_the_rest() {
        let directory = tempfile::tempdir().unwrap();
        let [first, second] = ["first.md", "second.md"].map(|name| directory.path().join(name));
        std::fs::write(&first, "1234").unwrap();
        std::fs::write(&second, "5678").unwrap();
        let read = reusing(Duration::ZERO, 6);
        assert_eq!(read.page_text(&first).unwrap(), ("1234".into(), false));
        for _ in 0..2 {
            assert_eq!(read.page_text(&second).unwrap(), ("5678".into(), false));
        }
        assert_eq!(read.page_text(&first).unwrap(), ("1234".into(), true));

        std::fs::write(&first, "1").unwrap();
        assert_eq!(read.page_text(&first).unwrap(), ("1".into(), false));
        assert_eq!(read.page_text(&second).unwrap(), ("5678".into(), false));
        assert_eq!(read.page_text(&second).unwrap(), ("5678".into(), true));
        assert_eq!(read.page_text(&first).unwrap(), ("1".into(), true));
    }
}
