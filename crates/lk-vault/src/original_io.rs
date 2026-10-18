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
                let mut builder = std::fs::DirBuilder::new();
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
    let mut file = std::fs::File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(error("original must be a regular file"));
    }
    let mut bytes = Vec::new();
    (&mut file).take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(error("original exceeds supported byte limit"));
    }
    Ok(bytes)
}
