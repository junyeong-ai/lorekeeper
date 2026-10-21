use std::fs::{File, Metadata, OpenOptions, TryLockError};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use lk_core::content::{InputProjection, InputUnit};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use thiserror::Error;

const ENTRY_VERSION: u8 = 1;
const MAX_CACHE_ENTRY_BYTES: usize = 512 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheAccess {
    ReadOnly,
    ReadWrite,
}

#[derive(Clone, Debug)]
pub struct ProjectionCache {
    root: PathBuf,
    access: CacheAccess,
    anchor: Option<PathBuf>,
}

#[derive(Debug, Error)]
pub enum CacheError {
    #[error("projection cache identity must contain 64 lowercase hexadecimal characters")]
    InvalidIdentity,
    #[error("projection cache unit number must be positive")]
    InvalidNumber,
    #[error("projection cache byte limit must be positive")]
    InvalidLimit,
    #[error("projection cache is already in use")]
    Busy,
    #[error("projection cache is read-only")]
    ReadOnly,
    #[error("projection cache entry is invalid")]
    InvalidEntry,
    #[error("projection cache entry exceeds the permitted byte limit")]
    Limit,
    #[error("projection cache entry already contains different bytes")]
    Occupied,
    #[error("projection cache IO: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug)]
pub(super) struct CacheClaim {
    directory: PathBuf,
    identity: String,
    access: CacheAccess,
    max_output_bytes: usize,
    _lock: Option<File>,
}

impl Drop for CacheClaim {
    fn drop(&mut self) {
        if let Some(lock) = &self._lock {
            let _ = lock.unlock();
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CacheEntry<T> {
    version: u8,
    identity: String,
    number: usize,
    payload_revision: String,
    payload: T,
}

impl ProjectionCache {
    pub fn for_vault(vault_root: &Path, access: CacheAccess) -> Self {
        let selected = if vault_root.is_absolute() {
            vault_root.to_path_buf()
        } else {
            Path::new(".").join(vault_root)
        };
        let anchor = lk_core::fs::canonical_prefix(&selected);
        Self {
            root: anchor.join(lk_core::vault_path::PROJECTION_CACHE_DIR),
            access,
            anchor: Some(anchor),
        }
    }

    #[cfg(test)]
    pub(super) fn new(root: PathBuf, access: CacheAccess) -> Self {
        Self {
            root,
            access,
            anchor: None,
        }
    }

    pub(super) fn is_writable(&self) -> bool {
        self.access == CacheAccess::ReadWrite
    }

    pub(super) fn claim(
        &self,
        identity: &str,
        max_output_bytes: usize,
    ) -> Result<CacheClaim, CacheError> {
        if !is_identity(identity) {
            return Err(CacheError::InvalidIdentity);
        }
        if max_output_bytes == 0 {
            return Err(CacheError::InvalidLimit);
        }
        self.validate_namespace()?;
        let writable = self.is_writable();
        let root_present = directory(&self.root, writable)?;
        let lock = if writable {
            let lock_path = self.root.join(format!("{identity}.lock"));
            require_regular_if_present(&lock_path)?;
            let mut options = OpenOptions::new();
            options.read(true).write(true).create(true).truncate(false);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let file = options.open(&lock_path)?;
            verify_opened(&lock_path, &file)?;
            match file.try_lock() {
                Ok(()) => {}
                Err(TryLockError::WouldBlock) => return Err(CacheError::Busy),
                Err(TryLockError::Error(error)) => return Err(CacheError::Io(error)),
            }
            Some(file)
        } else {
            None
        };
        let path = self.root.join(identity);
        if root_present {
            directory(&path, writable)?;
        }
        Ok(CacheClaim {
            directory: path,
            identity: identity.into(),
            access: self.access,
            max_output_bytes: max_output_bytes.min(MAX_CACHE_ENTRY_BYTES),
            _lock: lock,
        })
    }

    fn validate_namespace(&self) -> Result<(), CacheError> {
        let Some(anchor) = &self.anchor else {
            return Ok(());
        };
        if lk_core::fs::canonical_prefix(anchor) != *anchor {
            return Err(CacheError::InvalidEntry);
        }
        let mut path = anchor.clone();
        if !directory(&path, false)? {
            return Ok(());
        }
        for component in Path::new(lk_core::vault_path::PROJECTION_CACHE_DIR).components() {
            path.push(component);
            if !directory(&path, false)? {
                break;
            }
        }
        Ok(())
    }
}

pub fn prune(vault_root: &Path, cutoff: SystemTime) -> Result<usize, CacheError> {
    let cache = ProjectionCache::for_vault(vault_root, CacheAccess::ReadWrite);
    prune_cache(&cache, cutoff)
}

fn prune_cache(cache: &ProjectionCache, cutoff: SystemTime) -> Result<usize, CacheError> {
    cache.validate_namespace()?;
    if !directory(&cache.root, false)? {
        return Ok(0);
    }
    let mut removed = 0;
    for entry in std::fs::read_dir(&cache.root)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(identity) = name.to_str().filter(|name| is_identity(name)) else {
            continue;
        };
        if !entry.file_type()?.is_dir() {
            return Err(CacheError::InvalidEntry);
        }
        let claim = match cache.claim(identity, MAX_CACHE_ENTRY_BYTES) {
            Ok(claim) => claim,
            Err(CacheError::Busy) => continue,
            Err(error) => return Err(error),
        };
        let mut changed = false;
        for entry in std::fs::read_dir(&claim.directory)? {
            let entry = entry?;
            let name = entry.file_name();
            let number = if name == "projection.json" {
                0
            } else if let Some(number) = name
                .to_str()
                .and_then(|name| name.strip_suffix(".json"))
                .and_then(|number| number.parse::<usize>().ok())
                .filter(|number| *number > 0 && name == format!("{number}.json").as_str())
            {
                number
            } else {
                continue;
            };
            let metadata = entry.path().symlink_metadata()?;
            if !metadata.file_type().is_file() {
                return Err(CacheError::InvalidEntry);
            }
            if metadata.modified()? >= cutoff {
                continue;
            }
            let completed = if number == 0 {
                claim
                    .load_entry::<InputProjection>(&entry.path(), number, false)?
                    .is_some()
            } else {
                claim
                    .load_entry::<InputUnit>(&entry.path(), number, false)?
                    .is_some()
            };
            if completed {
                std::fs::remove_file(entry.path())?;
                removed += 1;
                changed = true;
            }
        }
        #[cfg(unix)]
        if changed {
            File::open(&claim.directory)?.sync_all()?;
        }
        #[cfg(not(unix))]
        let _ = changed;
    }
    Ok(removed)
}

fn is_identity(identity: &str) -> bool {
    identity.len() == 64
        && identity
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

impl CacheClaim {
    pub(super) fn load_unit(&self, number: usize) -> Result<Option<InputUnit>, CacheError> {
        let path = self.unit_path(number)?;
        self.load_entry(&path, number, true)
    }

    pub(super) fn store_unit(&self, number: usize, unit: &InputUnit) -> Result<(), CacheError> {
        let path = self.unit_path(number)?;
        self.store_entry(&path, number, unit)
    }

    pub(super) fn load_projection(&self) -> Result<Option<InputProjection>, CacheError> {
        self.load_entry(&self.directory.join("projection.json"), 0, true)
    }

    pub(super) fn store_projection(&self, projection: &InputProjection) -> Result<(), CacheError> {
        self.store_entry(&self.directory.join("projection.json"), 0, projection)
    }

    fn load_entry<T: DeserializeOwned + Serialize>(
        &self,
        path: &Path,
        number: usize,
        refresh_last_use: bool,
    ) -> Result<Option<T>, CacheError> {
        if !directory(&self.directory, false)? || !require_regular_if_present(path)? {
            return Ok(None);
        }
        let writable = refresh_last_use && self.access == CacheAccess::ReadWrite;
        let mut file = match OpenOptions::new().read(true).write(writable).open(path) {
            Ok(file) => file,
            Err(error)
                if self.access == CacheAccess::ReadOnly
                    && error.kind() == std::io::ErrorKind::NotFound =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };
        let Some(metadata) = self.verify_loaded(path, &file)? else {
            return Ok(None);
        };
        if metadata.len() > self.max_output_bytes as u64 {
            return Err(CacheError::Limit);
        }
        let mut bytes = Vec::new();
        (&mut file)
            .take((self.max_output_bytes as u64).saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() > self.max_output_bytes {
            return Err(CacheError::Limit);
        }
        let entry: CacheEntry<T> =
            serde_json::from_slice(&bytes).map_err(|_| CacheError::InvalidEntry)?;
        if entry.version != ENTRY_VERSION
            || entry.identity != self.identity
            || entry.number != number
            || entry.payload_revision != payload_revision(&entry.payload)?
        {
            return Err(CacheError::InvalidEntry);
        }
        if writable {
            file.set_modified(SystemTime::now())?;
        }
        Ok(Some(entry.payload))
    }

    fn verify_loaded(&self, path: &Path, file: &File) -> Result<Option<Metadata>, CacheError> {
        match verify_opened(path, file) {
            Ok(metadata) => Ok(Some(metadata)),
            Err(CacheError::Io(error))
                if self.access == CacheAccess::ReadOnly
                    && error.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    fn store_entry<T: DeserializeOwned + Serialize>(
        &self,
        path: &Path,
        number: usize,
        payload: &T,
    ) -> Result<(), CacheError> {
        if self.access != CacheAccess::ReadWrite {
            return Err(CacheError::ReadOnly);
        }
        if !directory(&self.directory, false)? {
            return Err(CacheError::InvalidEntry);
        }
        if require_regular_if_present(path)? {
            self.load_entry::<T>(path, number, false)?;
        }
        let entry = CacheEntry {
            version: ENTRY_VERSION,
            identity: self.identity.clone(),
            number,
            payload_revision: payload_revision(payload)?,
            payload,
        };
        let mut writer = LimitedBuffer {
            bytes: Vec::new(),
            limit: self.max_output_bytes,
            exceeded: false,
        };
        if serde_json::to_writer(&mut writer, &entry).is_err() {
            return Err(if writer.exceeded {
                CacheError::Limit
            } else {
                CacheError::InvalidEntry
            });
        }
        match lk_core::fs::publish_exclusive(path, &writer.bytes, Some(0o600))? {
            lk_core::fs::PublishOutcome::Created | lk_core::fs::PublishOutcome::Reused => Ok(()),
            lk_core::fs::PublishOutcome::Occupied => Err(CacheError::Occupied),
        }
    }

    fn unit_path(&self, number: usize) -> Result<PathBuf, CacheError> {
        if number == 0 {
            return Err(CacheError::InvalidNumber);
        }
        Ok(self.directory.join(format!("{number}.json")))
    }
}

fn directory(path: &Path, create: bool) -> Result<bool, CacheError> {
    match path.symlink_metadata() {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(true),
        Ok(_) => Err(CacheError::InvalidEntry),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !create => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                directory(parent, true)?;
            }
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(path) {
                Ok(()) => {
                    #[cfg(unix)]
                    if let Some(parent) = path.parent()
                        && !parent.as_os_str().is_empty()
                    {
                        File::open(parent)?.sync_all()?;
                    }
                    Ok(true)
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    directory(path, false)
                }
                Err(error) => Err(error.into()),
            }
        }
        Err(error) => Err(error.into()),
    }
}

fn require_regular_if_present(path: &Path) -> Result<bool, CacheError> {
    match path.symlink_metadata() {
        Ok(metadata) if metadata.file_type().is_file() => Ok(true),
        Ok(_) => Err(CacheError::InvalidEntry),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn verify_opened(path: &Path, file: &File) -> Result<Metadata, CacheError> {
    let named = path.symlink_metadata()?;
    let opened = file.metadata()?;
    if !named.file_type().is_file() || !opened.is_file() {
        return Err(CacheError::InvalidEntry);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if named.dev() != opened.dev()
            || named.ino() != opened.ino()
            || opened.permissions().mode() & 0o077 != 0
        {
            return Err(CacheError::InvalidEntry);
        }
    }
    Ok(opened)
}

fn payload_revision(payload: &impl Serialize) -> Result<String, CacheError> {
    struct HashWriter(blake3::Hasher);
    impl Write for HashWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = HashWriter(blake3::Hasher::new());
    serde_json::to_writer(&mut writer, payload).map_err(|_| CacheError::InvalidEntry)?;
    Ok(writer.0.finalize().to_hex().to_string())
}

struct LimitedBuffer {
    bytes: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl Write for LimitedBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            self.exceeded = true;
            return Err(std::io::Error::other(
                "projection cache byte limit exceeded",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDENTITY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn unit(text: &str) -> InputUnit {
        serde_json::from_value(serde_json::json!({
            "location": {"kind": "page", "number": 1},
            "method": "native", "representation": "markdown", "text": text,
        }))
        .unwrap()
    }

    fn projection(text: &str) -> InputProjection {
        serde_json::from_value(serde_json::json!({
            "policy": "document-projection-v3", "format": "pdf", "packages": {},
            "ocr": null, "units": [unit(text)], "warnings": [],
        }))
        .unwrap()
    }

    fn cache(root: &Path, access: CacheAccess) -> ProjectionCache {
        ProjectionCache::new(root.join("cache"), access)
    }

    fn open_writable(path: impl AsRef<Path>) -> File {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap()
    }

    #[test]
    fn readonly_misses_and_reads_never_create_files_or_claims() {
        let temporary = tempfile::tempdir().unwrap();
        let reader = cache(temporary.path(), CacheAccess::ReadOnly);
        let claim = reader.claim(IDENTITY, 4096).unwrap();
        assert!(claim.load_unit(1).unwrap().is_none());
        assert!(matches!(
            claim.store_unit(1, &unit("")),
            Err(CacheError::ReadOnly)
        ));
        assert!(!reader.root.exists());

        let writer = cache(temporary.path(), CacheAccess::ReadWrite);
        writer
            .claim(IDENTITY, 4096)
            .unwrap()
            .store_unit(1, &unit("source"))
            .unwrap();
        let mut before: Vec<_> = std::fs::read_dir(&reader.root)
            .unwrap()
            .map(|p| p.unwrap().file_name())
            .collect();
        before.sort();
        let other = "a".repeat(64);
        assert!(
            reader
                .claim(&other, 4096)
                .unwrap()
                .load_unit(1)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            reader
                .claim(IDENTITY, 4096)
                .unwrap()
                .load_unit(1)
                .unwrap()
                .unwrap()
                .text,
            "source"
        );
        let mut after: Vec<_> = std::fs::read_dir(&reader.root)
            .unwrap()
            .map(|p| p.unwrap().file_name())
            .collect();
        after.sort();
        assert_eq!(before, after);
    }

    #[test]
    fn anchored_readonly_and_maintenance_leave_an_absent_vault_absent() {
        let temporary = tempfile::tempdir().unwrap();
        let vault = temporary.path().join("not-created/vault");
        let cache = ProjectionCache::for_vault(&vault, CacheAccess::ReadOnly);
        assert!(
            cache
                .claim(IDENTITY, 4096)
                .unwrap()
                .load_unit(1)
                .unwrap()
                .is_none()
        );
        assert_eq!(prune(&vault, SystemTime::now()).unwrap(), 0);
        assert!(!temporary.path().join("not-created").exists());
    }

    #[test]
    fn readonly_reuse_treats_an_entry_removed_after_opening_as_a_miss() {
        let temporary = tempfile::tempdir().unwrap();
        let writer = cache(temporary.path(), CacheAccess::ReadWrite)
            .claim(IDENTITY, 4096)
            .unwrap();
        writer.store_unit(1, &unit("source")).unwrap();
        let reader = cache(temporary.path(), CacheAccess::ReadOnly)
            .claim(IDENTITY, 4096)
            .unwrap();
        let path = writer.unit_path(1).unwrap();
        let opened = File::open(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(reader.verify_loaded(&path, &opened).unwrap().is_none());
        assert!(reader.load_unit(1).unwrap().is_none());
        assert!(
            matches!(writer.verify_loaded(&path, &opened), Err(CacheError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound)
        );
    }

    #[test]
    fn dropping_a_claim_unlocks_descriptors_duplicated_before_a_child_executes() {
        let temporary = tempfile::tempdir().unwrap();
        let store = cache(temporary.path(), CacheAccess::ReadWrite);
        let claim = store.claim(IDENTITY, 4096).unwrap();
        let inherited = claim._lock.as_ref().unwrap().try_clone().unwrap();
        drop(claim);
        let next = store.claim(IDENTITY, 4096).unwrap();
        drop(inherited);
        assert!(matches!(store.claim(IDENTITY, 4096), Err(CacheError::Busy)));
        drop(next);
        assert!(store.claim(IDENTITY, 4096).is_ok());
    }

    #[test]
    fn publication_is_immutable_and_empty_units_are_completed() {
        let temporary = tempfile::tempdir().unwrap();
        let claim = cache(temporary.path(), CacheAccess::ReadWrite)
            .claim(IDENTITY, 4096)
            .unwrap();
        claim.store_unit(1, &unit("")).unwrap();
        claim.store_unit(1, &unit("")).unwrap();
        assert_eq!(claim.load_unit(1).unwrap().unwrap().text, "");
        assert!(matches!(
            claim.store_unit(1, &unit("changed")),
            Err(CacheError::Occupied)
        ));
        assert_eq!(claim.load_unit(1).unwrap().unwrap().text, "");
        assert!(claim.load_unit(2).unwrap().is_none());
    }

    #[test]
    fn full_projections_roundtrip_without_confusing_unit_number_zero() {
        let temporary = tempfile::tempdir().unwrap();
        let store = cache(temporary.path(), CacheAccess::ReadWrite);
        let claim = store.claim(IDENTITY, 4096).unwrap();
        assert!(claim.load_projection().unwrap().is_none());
        claim.store_projection(&projection("source")).unwrap();
        claim.store_projection(&projection("source")).unwrap();
        assert_eq!(
            claim.load_projection().unwrap().unwrap().units[0].text,
            "source"
        );
        assert!(matches!(
            claim.store_projection(&projection("changed")),
            Err(CacheError::Occupied)
        ));
        assert!(matches!(claim.load_unit(0), Err(CacheError::InvalidNumber)));
        let reader = cache(temporary.path(), CacheAccess::ReadOnly)
            .claim(IDENTITY, 4096)
            .unwrap();
        assert!(matches!(
            reader.store_projection(&projection("source")),
            Err(CacheError::ReadOnly)
        ));
        assert_eq!(
            reader.load_projection().unwrap().unwrap().units[0].text,
            "source"
        );
        let path = claim.directory.join("projection.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        value["number"] = serde_json::json!(1);
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(matches!(
            claim.load_projection(),
            Err(CacheError::InvalidEntry)
        ));
    }

    #[test]
    fn claim_refuses_contention_and_releases_on_drop() {
        let temporary = tempfile::tempdir().unwrap();
        let store = cache(temporary.path(), CacheAccess::ReadWrite);
        let claim = store.claim(IDENTITY, 4096).unwrap();
        assert!(matches!(store.claim(IDENTITY, 4096), Err(CacheError::Busy)));
        assert!(store.claim(&"a".repeat(64), 4096).is_ok());
        assert!(
            cache(temporary.path(), CacheAccess::ReadOnly)
                .claim(IDENTITY, 4096)
                .is_ok()
        );
        drop(claim);
        assert!(store.claim(IDENTITY, 4096).is_ok());
    }

    #[test]
    fn malformed_identity_version_number_and_unit_digest_refuse_reuse() {
        let temporary = tempfile::tempdir().unwrap();
        let store = cache(temporary.path(), CacheAccess::ReadWrite);
        for identity in ["", "..", "0123", &"A".repeat(64), &"g".repeat(64)] {
            assert!(matches!(
                store.claim(identity, 4096),
                Err(CacheError::InvalidIdentity)
            ));
        }
        assert!(!store.root.exists());
        assert!(matches!(
            store.claim(IDENTITY, 0),
            Err(CacheError::InvalidLimit)
        ));
        let claim = store.claim(IDENTITY, 4096).unwrap();
        assert!(matches!(claim.load_unit(0), Err(CacheError::InvalidNumber)));
        claim.store_unit(1, &unit("original")).unwrap();
        let path = claim.unit_path(1).unwrap();
        let original = std::fs::read(&path).unwrap();
        for (pointer, replacement) in [
            ("/version", serde_json::json!(2)),
            ("/identity", serde_json::json!("a".repeat(64))),
            ("/number", serde_json::json!(2)),
            ("/payload/text", serde_json::json!("modified")),
            ("/payload_revision", serde_json::json!("")),
        ] {
            let mut value: serde_json::Value = serde_json::from_slice(&original).unwrap();
            *value.pointer_mut(pointer).unwrap() = replacement;
            std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
            let old = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1);
            open_writable(&path).set_modified(old).unwrap();
            assert!(
                matches!(claim.load_unit(1), Err(CacheError::InvalidEntry)),
                "{pointer}"
            );
            assert_eq!(path.metadata().unwrap().modified().unwrap(), old);
        }
        std::fs::write(&path, b"{\"unit\":").unwrap();
        assert!(matches!(claim.load_unit(1), Err(CacheError::InvalidEntry)));
    }

    #[test]
    fn current_byte_limit_bounds_reads_and_publication() {
        let temporary = tempfile::tempdir().unwrap();
        let store = cache(temporary.path(), CacheAccess::ReadWrite);
        let claim = store.claim(IDENTITY, 4096).unwrap();
        claim.store_unit(1, &unit("source")).unwrap();
        let length = std::fs::metadata(claim.unit_path(1).unwrap())
            .unwrap()
            .len() as usize;
        let reader = cache(temporary.path(), CacheAccess::ReadOnly);
        assert!(
            reader
                .claim(IDENTITY, length)
                .unwrap()
                .load_unit(1)
                .unwrap()
                .is_some()
        );
        assert!(matches!(
            reader.claim(IDENTITY, length - 1).unwrap().load_unit(1),
            Err(CacheError::Limit)
        ));
        drop(claim);
        let claim = store.claim(IDENTITY, length - 1).unwrap();
        assert!(matches!(
            claim.store_unit(2, &unit("source")),
            Err(CacheError::Limit)
        ));
        assert!(!claim.unit_path(2).unwrap().exists());
    }

    #[test]
    fn retention_verifies_stored_entries_after_current_admission_limits_are_lowered() {
        use std::time::Duration;
        let temporary = tempfile::tempdir().unwrap();
        let store = ProjectionCache::for_vault(temporary.path(), CacheAccess::ReadWrite);
        let claim = store.claim(IDENTITY, 4096).unwrap();
        let text = "source ".repeat(128);
        claim.store_unit(1, &unit(&text)).unwrap();
        claim.store_projection(&projection(&text)).unwrap();
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
        for path in [
            claim.unit_path(1).unwrap(),
            claim.directory.join("projection.json"),
        ] {
            open_writable(path).set_modified(old).unwrap();
        }
        drop(claim);

        let reader = ProjectionCache::for_vault(temporary.path(), CacheAccess::ReadOnly);
        let current = reader.claim(IDENTITY, 64).unwrap();
        assert!(matches!(current.load_unit(1), Err(CacheError::Limit)));
        assert!(matches!(current.load_projection(), Err(CacheError::Limit)));
        assert_eq!(
            prune(temporary.path(), old + Duration::from_secs(1)).unwrap(),
            2
        );
        assert!(current.load_unit(1).unwrap().is_none());
        assert!(current.load_projection().unwrap().is_none());

        let bounded = reader.claim(IDENTITY, usize::MAX).unwrap();
        assert_eq!(bounded.max_output_bytes, MAX_CACHE_ENTRY_BYTES);
    }

    #[test]
    fn normal_writable_use_renews_retention_without_refreshing_inspection_or_pruning() {
        use std::time::Duration;
        let temporary = tempfile::tempdir().unwrap();
        let writer = ProjectionCache::for_vault(temporary.path(), CacheAccess::ReadWrite);
        let claim = writer.claim(IDENTITY, 4096).unwrap();
        claim.store_unit(1, &unit("hot")).unwrap();
        claim.store_unit(2, &unit("inspected")).unwrap();
        claim.store_unit(3, &unit("unused")).unwrap();
        claim
            .store_projection(&projection("hot projection"))
            .unwrap();
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
        let cutoff = old + Duration::from_secs(1);
        let hot_path = claim.unit_path(1).unwrap();
        let projection_path = claim.directory.join("projection.json");
        let inspected = open_writable(claim.unit_path(2).unwrap());
        let unused = open_writable(claim.unit_path(3).unwrap());
        for path in [&hot_path, &projection_path] {
            open_writable(path).set_modified(old).unwrap();
        }
        inspected.set_modified(old).unwrap();
        unused.set_modified(old).unwrap();
        let hot_bytes = std::fs::read(&hot_path).unwrap();
        let projection_bytes = std::fs::read(&projection_path).unwrap();

        assert_eq!(claim.load_unit(1).unwrap().unwrap().text, "hot");
        assert!(claim.load_projection().unwrap().is_some());
        assert!(hot_path.metadata().unwrap().modified().unwrap() > cutoff);
        assert!(projection_path.metadata().unwrap().modified().unwrap() > cutoff);
        assert_eq!(std::fs::read(&hot_path).unwrap(), hot_bytes);
        assert_eq!(std::fs::read(&projection_path).unwrap(), projection_bytes);

        let reader = ProjectionCache::for_vault(temporary.path(), CacheAccess::ReadOnly);
        assert_eq!(
            reader
                .claim(IDENTITY, 4096)
                .unwrap()
                .load_unit(2)
                .unwrap()
                .unwrap()
                .text,
            "inspected"
        );
        assert_eq!(inspected.metadata().unwrap().modified().unwrap(), old);
        drop(claim);
        assert_eq!(prune(temporary.path(), cutoff).unwrap(), 2);
        assert_eq!(inspected.metadata().unwrap().modified().unwrap(), old);
        assert_eq!(unused.metadata().unwrap().modified().unwrap(), old);
        let remaining = reader.claim(IDENTITY, 4096).unwrap();
        assert!(remaining.load_unit(1).unwrap().is_some());
        assert!(remaining.load_projection().unwrap().is_some());
        assert!(remaining.load_unit(2).unwrap().is_none());
        assert!(remaining.load_unit(3).unwrap().is_none());
    }

    #[test]
    fn pruning_keeps_active_recent_and_unknown_entries_and_stable_claims() {
        use std::time::Duration;
        let temporary = tempfile::tempdir().unwrap();
        let store = cache(temporary.path(), CacheAccess::ReadWrite);
        let claim = store.claim(IDENTITY, 4096).unwrap();
        claim.store_unit(1, &unit("expired")).unwrap();
        claim.store_unit(2, &unit("recent")).unwrap();
        claim
            .store_projection(&projection("expired native projection"))
            .unwrap();
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
        let cutoff = SystemTime::UNIX_EPOCH + Duration::from_secs(2);
        open_writable(claim.unit_path(1).unwrap())
            .set_modified(old)
            .unwrap();
        open_writable(claim.directory.join("projection.json"))
            .set_modified(old)
            .unwrap();
        let unknown = claim.directory.join("notes.json");
        std::fs::write(&unknown, b"unowned").unwrap();
        let active = store.claim(&"a".repeat(64), 4096).unwrap();
        active.store_unit(1, &unit("active")).unwrap();
        open_writable(active.unit_path(1).unwrap())
            .set_modified(old)
            .unwrap();
        drop(claim);

        assert_eq!(prune_cache(&store, cutoff).unwrap(), 2);
        let claim = store.claim(IDENTITY, 4096).unwrap();
        assert!(claim.load_unit(1).unwrap().is_none());
        assert!(claim.load_projection().unwrap().is_none());
        assert_eq!(claim.load_unit(2).unwrap().unwrap().text, "recent");
        assert_eq!(active.load_unit(1).unwrap().unwrap().text, "active");
        assert!(unknown.exists());
        assert!(claim.directory.exists());
        assert!(store.root.join(format!("{IDENTITY}.lock")).exists());
        assert!(matches!(store.claim(IDENTITY, 4096), Err(CacheError::Busy)));
    }

    #[test]
    fn pruning_refuses_and_preserves_malformed_expired_entries() {
        use std::time::Duration;
        let temporary = tempfile::tempdir().unwrap();
        let store = cache(temporary.path(), CacheAccess::ReadWrite);
        let claim = store.claim(IDENTITY, 4096).unwrap();
        claim.store_unit(1, &unit("source")).unwrap();
        let path = claim.unit_path(1).unwrap();
        std::fs::write(&path, b"{ malformed").unwrap();
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
        open_writable(&path).set_modified(old).unwrap();
        drop(claim);
        assert!(matches!(
            prune_cache(&store, old + Duration::from_secs(1)),
            Err(CacheError::InvalidEntry)
        ));
        assert_eq!(std::fs::read(&path).unwrap(), b"{ malformed");
        let missing = temporary.path().join("missing");
        assert_eq!(prune(&missing, old).unwrap(), 0);
        assert!(!missing.exists());
    }

    #[test]
    fn nonregular_roots_locks_and_units_are_errors() {
        let temporary = tempfile::tempdir().unwrap();
        let store = cache(temporary.path(), CacheAccess::ReadWrite);
        std::fs::write(&store.root, b"not a directory").unwrap();
        assert!(matches!(
            store.claim(IDENTITY, 4096),
            Err(CacheError::InvalidEntry)
        ));
        std::fs::remove_file(&store.root).unwrap();
        std::fs::create_dir(&store.root).unwrap();
        let lock = store.root.join(format!("{IDENTITY}.lock"));
        std::fs::create_dir(&lock).unwrap();
        assert!(matches!(
            store.claim(IDENTITY, 4096),
            Err(CacheError::InvalidEntry)
        ));
        std::fs::remove_dir(&lock).unwrap();
        let claim = store.claim(IDENTITY, 4096).unwrap();
        std::fs::create_dir(claim.unit_path(1).unwrap()).unwrap();
        assert!(matches!(claim.load_unit(1), Err(CacheError::InvalidEntry)));
        assert!(matches!(
            claim.store_unit(1, &unit("source")),
            Err(CacheError::InvalidEntry)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_roots_directories_locks_and_units_are_refused() {
        use std::os::unix::fs::symlink;
        let temporary = tempfile::tempdir().unwrap();
        let outside = temporary.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let store = cache(temporary.path(), CacheAccess::ReadWrite);
        symlink(&outside, &store.root).unwrap();
        assert!(matches!(
            store.claim(IDENTITY, 4096),
            Err(CacheError::InvalidEntry)
        ));
        assert!(matches!(
            cache(temporary.path(), CacheAccess::ReadOnly).claim(IDENTITY, 4096),
            Err(CacheError::InvalidEntry)
        ));
        std::fs::remove_file(&store.root).unwrap();
        std::fs::create_dir(&store.root).unwrap();
        let lock = store.root.join(format!("{IDENTITY}.lock"));
        symlink(outside.join("lock"), &lock).unwrap();
        assert!(matches!(
            store.claim(IDENTITY, 4096),
            Err(CacheError::InvalidEntry)
        ));
        std::fs::remove_file(&lock).unwrap();
        let directory = store.root.join(IDENTITY);
        symlink(&outside, &directory).unwrap();
        assert!(matches!(
            store.claim(IDENTITY, 4096),
            Err(CacheError::InvalidEntry)
        ));
        std::fs::remove_file(&directory).unwrap();
        let claim = store.claim(IDENTITY, 4096).unwrap();
        symlink(outside.join("unit"), claim.unit_path(1).unwrap()).unwrap();
        assert!(matches!(claim.load_unit(1), Err(CacheError::InvalidEntry)));
        assert!(matches!(
            claim.store_unit(1, &unit("source")),
            Err(CacheError::InvalidEntry)
        ));
        assert!(!outside.join("lock").exists());
        assert!(!outside.join("unit").exists());
    }

    #[cfg(unix)]
    #[test]
    fn anchored_cache_refuses_each_owned_ancestor_alias_without_outside_writes() {
        use std::os::unix::fs::symlink;
        for owned in [
            ".lorekeeper",
            ".lorekeeper/cache",
            lk_core::vault_path::PROJECTION_CACHE_DIR,
        ] {
            let temporary = tempfile::tempdir().unwrap();
            let vault = temporary.path().join("vault");
            let outside = temporary.path().join("outside");
            std::fs::create_dir(&vault).unwrap();
            std::fs::create_dir(&outside).unwrap();
            let alias = vault.join(owned);
            std::fs::create_dir_all(alias.parent().unwrap()).unwrap();
            symlink(&outside, &alias).unwrap();
            for access in [CacheAccess::ReadOnly, CacheAccess::ReadWrite] {
                let cache = ProjectionCache::for_vault(&vault, access);
                assert!(
                    matches!(cache.claim(IDENTITY, 4096), Err(CacheError::InvalidEntry)),
                    "{owned}"
                );
            }
            assert!(
                matches!(
                    prune(&vault, SystemTime::now()),
                    Err(CacheError::InvalidEntry)
                ),
                "{owned}"
            );
            assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0, "{owned}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn anchored_cache_accepts_a_vault_alias_and_prunes_the_same_owned_namespace() {
        use std::os::unix::fs::symlink;
        use std::time::Duration;
        let temporary = tempfile::tempdir().unwrap();
        let vault = temporary.path().join("vault");
        let alias = temporary.path().join("vault-alias");
        std::fs::create_dir(&vault).unwrap();
        symlink(&vault, &alias).unwrap();
        let writer = ProjectionCache::for_vault(&alias, CacheAccess::ReadWrite);
        let claim = writer.claim(IDENTITY, 4096).unwrap();
        claim.store_unit(1, &unit("source")).unwrap();
        let reader = ProjectionCache::for_vault(&vault, CacheAccess::ReadOnly);
        assert_eq!(
            reader
                .claim(IDENTITY, 4096)
                .unwrap()
                .load_unit(1)
                .unwrap()
                .unwrap()
                .text,
            "source"
        );
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
        open_writable(claim.unit_path(1).unwrap())
            .set_modified(old)
            .unwrap();
        drop(claim);
        assert_eq!(prune(&alias, old + Duration::from_secs(1)).unwrap(), 1);
        assert!(
            reader
                .claim(IDENTITY, 4096)
                .unwrap()
                .load_unit(1)
                .unwrap()
                .is_none()
        );
        assert!(
            vault
                .join(lk_core::vault_path::PROJECTION_CACHE_DIR)
                .exists()
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_uncreated_vault_anchor_cannot_be_redirected_after_selection() {
        use std::os::unix::fs::symlink;
        let temporary = tempfile::tempdir().unwrap();
        let parent = temporary.path().join("uncreated");
        let outside = temporary.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let cache = ProjectionCache::for_vault(&parent.join("vault"), CacheAccess::ReadWrite);
        symlink(&outside, &parent).unwrap();
        assert!(matches!(
            cache.claim(IDENTITY, 4096),
            Err(CacheError::InvalidEntry)
        ));
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn newly_created_cache_files_and_directories_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let temporary = tempfile::tempdir().unwrap();
        let store = cache(temporary.path(), CacheAccess::ReadWrite);
        let claim = store.claim(IDENTITY, 4096).unwrap();
        claim.store_unit(1, &unit("source")).unwrap();
        for path in [&store.root, &claim.directory] {
            assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o700);
        }
        for path in [
            store.root.join(format!("{IDENTITY}.lock")),
            claim.unit_path(1).unwrap(),
        ] {
            assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        }
    }
}
