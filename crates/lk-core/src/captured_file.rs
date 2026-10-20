use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;

use crate::manual_original::OriginalDigest;

/// Owned, private acquisition bytes; source paths never stand in for captured content.
#[derive(Debug)]
pub struct CapturedFile {
    file: tempfile::NamedTempFile,
    bytes: usize,
    revision: OriginalDigest,
}

impl CapturedFile {
    pub fn capture(mut reader: impl Read, limit: usize) -> io::Result<Self> {
        let mut file = tempfile::Builder::new()
            .prefix("lorekeeper-input-")
            .tempfile()?;
        let mut hash = blake3::Hasher::new();
        let mut bytes = 0usize;
        let mut buffer = [0; 65536];
        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            bytes = bytes
                .checked_add(count)
                .filter(|size| *size <= limit)
                .ok_or_else(|| {
                    io::Error::other(format!(
                        "captured input exceeds configured {limit}-byte limit"
                    ))
                })?;
            file.write_all(&buffer[..count])?;
            hash.update(&buffer[..count]);
        }
        file.flush()?;
        Ok(Self {
            file,
            bytes,
            revision: OriginalDigest {
                scheme: "blake3-256".into(),
                value: hash.finalize().to_hex().to_string(),
            },
        })
    }

    pub fn path(&self) -> &Path {
        self.file.path()
    }

    pub fn byte_len(&self) -> usize {
        self.bytes
    }

    pub fn revision(&self) -> &OriginalDigest {
        &self.revision
    }

    pub fn open(&self) -> io::Result<File> {
        let file = File::open(self.path())?;
        if !file.metadata()?.is_file() || file.metadata()?.len() != self.bytes as u64 {
            return Err(io::Error::other(
                "captured input length or file type changed",
            ));
        }
        Ok(file)
    }

    pub fn reader(&self) -> io::Result<VerifiedReader<File>> {
        Ok(VerifiedReader {
            inner: self.open()?,
            expected_bytes: self.bytes,
            expected_revision: self.revision.clone(),
            bytes: 0,
            hash: blake3::Hasher::new(),
            complete: false,
        })
    }

    pub fn verify(&self) -> io::Result<()> {
        let mut file = self.open()?;
        let (bytes, revision) = hash_reader((&mut file).take(self.bytes as u64 + 1))?;
        if bytes != self.bytes || revision != self.revision {
            return Err(io::Error::other("captured input digest changed"));
        }
        Ok(())
    }

    pub fn read_bytes(&self, limit: usize) -> io::Result<Vec<u8>> {
        if self.bytes > limit {
            return Err(io::Error::other(format!(
                "captured text exceeds {limit}-byte conversion limit"
            )));
        }
        let mut bytes = Vec::with_capacity(self.bytes);
        self.open()?
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() != self.bytes || OriginalDigest::of(&bytes) != self.revision {
            return Err(io::Error::other("captured input bytes changed"));
        }
        Ok(bytes)
    }
}

pub struct VerifiedReader<R> {
    inner: R,
    expected_bytes: usize,
    expected_revision: OriginalDigest,
    bytes: usize,
    hash: blake3::Hasher,
    complete: bool,
}

impl<R: Read> Read for VerifiedReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() || self.complete {
            return Ok(0);
        }
        let count = self.inner.read(buffer)?;
        if count == 0 {
            if self.bytes != self.expected_bytes
                || self.hash.finalize().to_hex().as_str() != self.expected_revision.value
            {
                return Err(io::Error::other("captured input digest or length changed"));
            }
            self.complete = true;
        } else {
            self.bytes = self
                .bytes
                .checked_add(count)
                .filter(|bytes| *bytes <= self.expected_bytes)
                .ok_or_else(|| io::Error::other("captured input grew during reading"))?;
            self.hash.update(&buffer[..count]);
        }
        Ok(count)
    }
}

pub fn hash_reader(mut reader: impl Read) -> io::Result<(usize, OriginalDigest)> {
    let mut hash = blake3::Hasher::new();
    let mut bytes = 0usize;
    let mut buffer = [0; 65536];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        bytes = bytes
            .checked_add(count)
            .ok_or_else(|| io::Error::other("input size overflow"))?;
        hash.update(&buffer[..count]);
    }
    Ok((
        bytes,
        OriginalDigest {
            scheme: "blake3-256".into(),
            value: hash.finalize().to_hex().to_string(),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_owns_exact_private_bytes_and_removes_only_its_scratch_file() {
        let captured = CapturedFile::capture(&b"original"[..], 8).unwrap();
        let path = captured.path().to_owned();
        assert_eq!(captured.byte_len(), 8);
        assert_eq!(captured.revision(), &OriginalDigest::of(b"original"));
        captured.verify().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        }
        std::fs::write(&path, b"tampered").unwrap();
        assert!(captured.verify().is_err());
        drop(captured);
        assert!(!path.exists());
        assert!(CapturedFile::capture(&b"oversized"[..], 8).is_err());
    }
}
