use std::io::Read;

use crate::DistError;

/// The bytes of one named member of a gzipped tar, or `None` where the archive holds no such
/// member.
///
/// Reading one member rather than unpacking the archive is the whole safety argument: an
/// unpack writes paths the archive chose, so it needs a rule for `../`, for an absolute path,
/// for a symlink pointing out of the destination — three rules that have to be right, on bytes
/// fetched from the network. Nothing here writes a path the archive names. The member is
/// matched against a path the CALLER spelled, and everything else is skipped.
pub fn read_from_tar_gz(bytes: &[u8], member: &str) -> Result<Option<Vec<u8>>, DistError> {
    let decoder = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|e| DistError::Integrity(format!("archive is not a readable tar: {e}")))?;

    for entry in entries {
        let mut entry =
            entry.map_err(|e| DistError::Integrity(format!("archive entry is unreadable: {e}")))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry
            .path()
            .map_err(|e| DistError::Integrity(format!("archive entry has no readable path: {e}")))?
            .to_string_lossy()
            .replace('\\', "/");
        if path != member {
            continue;
        }
        let mut contents = Vec::with_capacity(entry.size() as usize);
        entry
            .read_to_end(&mut contents)
            .map_err(|e| DistError::Integrity(format!("reading `{member}` from archive: {e}")))?;
        return Ok(Some(contents));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tar_gz(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        for (path, contents) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append_data(&mut header, path, *contents).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn reads_the_named_member() {
        let archive = tar_gz(&[
            ("lore-v1.2.3-target/README.md", b"docs"),
            ("lore-v1.2.3-target/lore", b"\x7fELF"),
        ]);
        assert_eq!(
            read_from_tar_gz(&archive, "lore-v1.2.3-target/lore").unwrap(),
            Some(b"\x7fELF".to_vec())
        );
    }

    /// A member the caller did not name is not the binary, whatever it is called. The answer
    /// is `None` rather than "the only executable in there", because a release whose layout
    /// changed must fail loudly instead of installing whichever file happened to match a guess.
    #[test]
    fn a_missing_member_is_absent_rather_than_approximated() {
        let archive = tar_gz(&[("lore-v1.2.3-target/lore.exe", b"MZ")]);
        assert_eq!(
            read_from_tar_gz(&archive, "lore-v1.2.3-target/lore").unwrap(),
            None
        );
    }

    /// An archive whose entry names a path outside anything it could be unpacked into. The
    /// `tar` crate's own builder refuses to write one, so the header is laid out by hand —
    /// this is the case that has to hold against an archive produced by something with no such
    /// scruples, and a fixture that cannot express it would test nothing.
    ///
    /// The name is never joined onto a destination, so it is data that fails to match.
    #[test]
    fn a_traversing_path_is_just_a_name_that_does_not_match() {
        let archive = raw_tar_gz("../../../etc/profile", b"pwned");
        assert_eq!(
            read_from_tar_gz(&archive, "lore-v1.2.3-target/lore").unwrap(),
            None
        );
        assert_eq!(
            read_from_tar_gz(&archive, "../../../etc/profile").unwrap(),
            Some(b"pwned".to_vec()),
            "the entry is readable — what makes it harmless is that nothing writes its name"
        );
    }

    fn raw_tar_gz(name: &str, contents: &[u8]) -> Vec<u8> {
        use std::io::Write as _;

        let mut header = [0u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..108].copy_from_slice(b"0000644\0");
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        header[124..136].copy_from_slice(format!("{:011o}\0", contents.len()).as_bytes());
        header[136..148].copy_from_slice(b"00000000000\0");
        // The checksum is computed with its own field read as spaces.
        header[148..156].copy_from_slice(b"        ");
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        let sum: u32 = header.iter().map(|b| u32::from(*b)).sum();
        header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());

        let mut raw = header.to_vec();
        raw.extend_from_slice(contents);
        raw.resize(raw.len().div_ceil(512) * 512, 0);
        // Two zero blocks end an archive.
        raw.extend_from_slice(&[0u8; 1024]);

        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&raw).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn bytes_that_are_not_an_archive_are_an_integrity_failure() {
        assert!(matches!(
            read_from_tar_gz(b"not a gzip stream at all", "lore"),
            Err(DistError::Integrity(_))
        ));
    }
}
