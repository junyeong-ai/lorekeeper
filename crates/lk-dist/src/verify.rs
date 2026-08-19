use std::path::Path;

use sha2::{Digest, Sha256};

use crate::DistError;
use crate::release::REPO;

pub fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// Hold `archive` to the digest its `.sha256` sidecar publishes.
///
/// Computed here rather than shelled out to `sha256sum`/`shasum`, which differ in flags
/// between platforms and are not guaranteed to exist on any of them. The sidecar's own format
/// is the one both of those tools write — `<digest>  <name>` — and the name is checked when it
/// is present, so a sidecar fetched for a different asset fails rather than passing on a digest
/// that describes something else.
pub fn verify_sidecar(archive: &[u8], sidecar: &str, expected_name: &str) -> Result<(), DistError> {
    let line = sidecar
        .lines()
        .find(|line| !line.trim().is_empty())
        .ok_or_else(|| {
            DistError::Integrity(format!(
                "the checksum published for {expected_name} is empty"
            ))
        })?;
    let mut fields = line.split_whitespace();
    let published = fields.next().ok_or_else(|| {
        DistError::Integrity(format!(
            "the checksum published for {expected_name} names no digest"
        ))
    })?;

    if published.len() != 64 || !published.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(DistError::Integrity(format!(
            "the checksum published for {expected_name} is not a SHA-256 digest"
        )));
    }

    // `sha256sum` marks a binary-mode digest with a leading `*` on the name; both tools omit
    // the name entirely when reading from stdin, which is a sidecar that describes whatever it
    // was pointed at and so cannot contradict this one.
    if let Some(named) = fields.next()
        && named.trim_start_matches('*') != expected_name
    {
        return Err(DistError::Integrity(format!(
            "the checksum fetched for {expected_name} describes {named}"
        )));
    }

    let computed = sha256_hex(archive);
    if !computed.eq_ignore_ascii_case(published) {
        return Err(DistError::Integrity(format!(
            "{expected_name} does not match its published checksum \
             (published {published}, downloaded {computed})"
        )));
    }
    Ok(())
}

/// Hold `archive` to the build provenance the release workflow attested.
///
/// This is the one link a checksum cannot make. The sidecar travels the same channel as the
/// archive, so it proves the bytes arrived intact and says nothing about who produced them — a
/// compromised release rewrites both. An attestation ties the archive to a run of this
/// repository's workflow.
///
/// Opt-in because it needs the `gh` CLI, and hard-failing when asked for: a verification the
/// caller requested and did not get is a failed update, never a warning.
pub fn verify_attestation(archive: &Path) -> Result<(), DistError> {
    let status = std::process::Command::new("gh")
        .arg("attestation")
        .arg("verify")
        .arg(archive)
        .args(["--repo", REPO])
        .status()
        .map_err(|e| {
            DistError::Integrity(format!(
                "attestation verification needs the `gh` CLI (https://cli.github.com): {e}"
            ))
        })?;
    if !status.success() {
        return Err(DistError::Integrity(format!(
            "{} does not verify against {REPO}'s build provenance",
            archive.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BYTES: &[u8] = b"lore";
    /// A well-formed digest of something other than `BYTES`.
    const OTHER_DIGEST: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    fn sidecar(digest: &str, name: &str) -> String {
        format!("{digest}  {name}\n")
    }

    #[test]
    fn a_matching_digest_passes() {
        let digest = sha256_hex(BYTES);
        verify_sidecar(BYTES, &sidecar(&digest, "lore.tar.gz"), "lore.tar.gz").unwrap();
    }

    #[test]
    fn an_uppercase_digest_is_the_same_digest() {
        let digest = sha256_hex(BYTES).to_uppercase();
        verify_sidecar(BYTES, &sidecar(&digest, "lore.tar.gz"), "lore.tar.gz").unwrap();
    }

    #[test]
    fn a_binary_mode_marker_is_not_part_of_the_name() {
        let digest = sha256_hex(BYTES);
        verify_sidecar(BYTES, &format!("{digest} *lore.tar.gz\n"), "lore.tar.gz").unwrap();
    }

    #[test]
    fn a_differing_digest_fails() {
        assert!(matches!(
            verify_sidecar(BYTES, &sidecar(OTHER_DIGEST, "lore.tar.gz"), "lore.tar.gz"),
            Err(DistError::Integrity(_))
        ));
    }

    /// The digest and the name are one claim. A sidecar fetched for another asset would
    /// otherwise be compared digest-only and fail with a message naming the wrong cause — or,
    /// if the two assets happened to be identical, pass while describing something else.
    #[test]
    fn a_sidecar_for_another_asset_fails_on_the_name() {
        let digest = sha256_hex(BYTES);
        let err = verify_sidecar(
            BYTES,
            &sidecar(&digest, "lore-v9.9.9-other.tar.gz"),
            "lore.tar.gz",
        );
        assert!(matches!(err, Err(DistError::Integrity(_))));
    }

    #[test]
    fn a_sidecar_that_publishes_no_digest_fails() {
        for malformed in [
            "",
            "\n",
            "not-a-digest  lore.tar.gz\n",
            "abc  lore.tar.gz\n",
        ] {
            assert!(
                matches!(
                    verify_sidecar(BYTES, malformed, "lore.tar.gz"),
                    Err(DistError::Integrity(_))
                ),
                "sidecar {malformed:?} must not verify"
            );
        }
    }

    #[test]
    fn the_digest_is_the_one_the_published_tools_write() {
        // `printf 'lore' | sha256sum`
        assert_eq!(
            sha256_hex(b"lore"),
            "b6598e838f350a97cb734eca208ce0cdc602dd60afbf65a3b8b65195cbd1a7fe"
        );
    }
}
