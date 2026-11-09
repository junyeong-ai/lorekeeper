//! Private path and bounded-read boundary shared by the two typed original stores.
use crate::VaultError;
use std::io::Read;
use std::path::{Path, PathBuf};
fn error(e: impl std::fmt::Display) -> VaultError {
    VaultError::Original(e.to_string())
}
pub(crate) fn store_path(
    root: &Path,
    relative: &Path,
    namespace: &str,
    create: bool,
) -> Result<PathBuf, VaultError> {
    lk_core::manual_original::validate_member_path(&relative.to_string_lossy()).map_err(error)?;
    if !relative.starts_with(namespace) {
        return Err(error("outside original store"));
    }
    let mut full = root.canonicalize()?;
    let components: Vec<_> = relative.components().collect();
    for (index, c) in components.iter().enumerate() {
        full.push(c.as_os_str());
        let last = index + 1 == components.len();
        match full.symlink_metadata() {
            Ok(m) if m.file_type().is_symlink() => return Err(error("symlink in original store")),
            Ok(m) if !last && !m.is_dir() => {
                return Err(error("original store parent is not a directory"));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !last && create => {
                let builder = std::fs::DirBuilder::new();
                #[cfg(unix)]
                let mut builder = builder;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    builder.mode(0o700);
                }
                builder.create(&full)?;
                #[cfg(unix)]
                std::fs::File::open(full.parent().ok_or_else(|| error("original parent"))?)?
                    .sync_all()?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && last => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(full)
}
pub(crate) fn read_bytes(path: &Path, limit: usize) -> Result<Vec<u8>, VaultError> {
    read_bytes_controlled(path, limit, &crate::ReadControl::default())
}

pub(crate) fn read_bytes_controlled(
    path: &Path,
    limit: usize,
    read: &crate::ReadControl,
) -> Result<Vec<u8>, VaultError> {
    read.check()?;
    let file = lk_core::fs::open_regular_file(path);
    read.check()?;
    let mut file = file?.take(limit as u64 + 1);
    #[cfg(test)]
    read_tests::record_read(path);
    let mut bytes = Vec::new();
    let mut buffer = [0; 65536];
    loop {
        read.check()?;
        let result = file.read(&mut buffer);
        read.check()?;
        let count = match result {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    if bytes.len() > limit {
        return Err(error("original exceeds supported byte limit"));
    }
    read.check()?;
    Ok(bytes)
}

#[cfg(test)]
pub(crate) mod read_tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    thread_local! {
        static READS: std::cell::RefCell<Option<Vec<PathBuf>>> = const { std::cell::RefCell::new(None) };
        pub(crate) static CANCEL_READ: std::cell::RefCell<Option<Arc<AtomicBool>>> = const { std::cell::RefCell::new(None) };
    }

    pub(crate) struct ReadObservation;

    pub(crate) fn observe() -> ReadObservation {
        READS.with_borrow_mut(|paths| {
            assert!(
                paths.is_none(),
                "original read observation is already active"
            );
            *paths = Some(Vec::new());
        });
        ReadObservation
    }

    impl ReadObservation {
        pub(crate) fn paths(&self) -> Vec<PathBuf> {
            READS.with_borrow(|paths| paths.as_ref().expect("active read observation").clone())
        }
    }

    impl Drop for ReadObservation {
        fn drop(&mut self) {
            READS.with_borrow_mut(|paths| *paths = None);
            CANCEL_READ.with_borrow_mut(|flag| *flag = None);
        }
    }

    pub(super) fn record_read(path: &Path) {
        READS.with_borrow_mut(|paths| {
            if let Some(paths) = paths {
                paths.push(path.to_owned());
            }
        });
        CANCEL_READ.with_borrow_mut(|flag| {
            if let Some(flag) = flag.take() {
                flag.store(true, Ordering::Relaxed);
            }
        });
    }

    #[test]
    fn controlled_original_reads_preserve_bounds_missing_and_cancellation() {
        let _observation = observe();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("original");
        std::fs::write(&path, [0, 255, 1]).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let read = crate::ReadControl::new(cancelled.clone());
        assert_eq!(read_bytes_controlled(&path, 3, &read).unwrap(), [0, 255, 1]);
        assert!(read_bytes_controlled(&path, 2, &read).is_err());
        assert!(
            matches!(read_bytes_controlled(&directory.path().join("missing"), 3, &read), Err(VaultError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound)
        );
        CANCEL_READ.with_borrow_mut(|flag| *flag = Some(cancelled));
        assert!(matches!(
            read_bytes_controlled(&path, 3, &read),
            Err(VaultError::Cancelled)
        ));
    }
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn assert_refuses_fifo(
        path: PathBuf,
        read: impl FnOnce() -> Result<(), VaultError> + Send + 'static,
    ) {
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        let (sender, receiver) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || sender.send(read()).unwrap());
        let result = receiver.recv_timeout(std::time::Duration::from_secs(5));
        let _release = result.is_err().then(|| {
            std::fs::File::options()
                .read(true)
                .write(true)
                .open(path)
                .unwrap()
        });
        reader.join().unwrap();
        assert!(
            result
                .expect("original reader waited for a FIFO writer")
                .is_err()
        );
    }

    #[test]
    fn bounded_original_read_refuses_fifo_without_waiting() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("original");
        let selected = path.clone();
        assert_refuses_fifo(path, move || read_bytes(&selected, 64).map(|_| ()));
    }
}
