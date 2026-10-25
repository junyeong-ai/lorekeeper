use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

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
}

impl ReadControl {
    pub fn new(cancelled: Arc<AtomicBool>) -> Self {
        Self {
            cancelled: Some(cancelled),
        }
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

    pub(crate) fn read_to_string(&self, path: &Path) -> Result<String, VaultError> {
        use std::io::Read;
        self.check()?;
        let mut file = std::fs::File::open(path)?;
        let mut bytes = Vec::new();
        let mut buffer = [0; 65536];
        loop {
            self.check()?;
            let size = match file.read(&mut buffer) {
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
}
