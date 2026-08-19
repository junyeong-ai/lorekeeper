/// How a release packages the archive for a platform. It decides more than a file extension:
/// a `.tar.gz` is read in-process here, so its platforms can replace their own binary, while a
/// `.zip` is unpacked by the PowerShell installer and Windows cannot rename over a running
/// executable at all. Deriving self-update support from the format keeps that from becoming a
/// second list nothing compares to this one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Archive {
    TarGz,
    Zip,
}

impl Archive {
    pub fn extension(self) -> &'static str {
        match self {
            Archive::TarGz => "tar.gz",
            Archive::Zip => "zip",
        }
    }
}

/// A platform the release publishes an archive for.
///
/// The triple is the `{target}` in `lore-v{version}-{target}`, so this table and the release
/// matrix name the same set or an install is a 404 — the first thing a new user sees. The
/// `uname` spellings are how `scripts/install.sh` selects a triple before any binary exists;
/// they are held here so the two answers to "which archive is this machine's" are one table
/// with a test between them, rather than two that agree until one is edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReleaseTarget {
    pub triple: &'static str,
    /// `uname -s`, lowercased.
    pub os: &'static str,
    /// Every `uname -m` spelling that selects this target.
    pub machines: &'static [&'static str],
    pub archive: Archive,
}

impl ReleaseTarget {
    pub const ALL: [ReleaseTarget; 4] = [
        ReleaseTarget {
            triple: "x86_64-unknown-linux-musl",
            os: "linux",
            machines: &["x86_64"],
            archive: Archive::TarGz,
        },
        ReleaseTarget {
            triple: "aarch64-unknown-linux-musl",
            os: "linux",
            machines: &["aarch64", "arm64"],
            archive: Archive::TarGz,
        },
        ReleaseTarget {
            triple: "aarch64-apple-darwin",
            os: "darwin",
            machines: &["arm64"],
            archive: Archive::TarGz,
        },
        ReleaseTarget {
            triple: "x86_64-pc-windows-msvc",
            os: "windows",
            machines: &["x86_64"],
            archive: Archive::Zip,
        },
    ];

    /// Whether a binary on this platform can replace itself.
    ///
    /// On Unix `rename(2)` publishes the new file atomically and the kernel keeps the running
    /// inode alive until the process exits. Windows refuses to replace an open executable, so
    /// the swap there is a different algorithm rather than the same one with a different
    /// unpacker — and a half-implemented one leaves an installation with no binary at all.
    pub fn is_self_replaceable(&self) -> bool {
        self.archive == Archive::TarGz
    }

    /// The target whose archive runs on this machine, or `None` where the release publishes
    /// none.
    ///
    /// A `gnu` Linux build resolves to the `musl` archive on purpose: statically linked, it is
    /// what runs on both, and the release publishes only the one.
    pub fn current() -> Option<&'static ReleaseTarget> {
        let triple = if cfg!(all(target_arch = "x86_64", target_os = "linux")) {
            "x86_64-unknown-linux-musl"
        } else if cfg!(all(target_arch = "aarch64", target_os = "linux")) {
            "aarch64-unknown-linux-musl"
        } else if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
            "aarch64-apple-darwin"
        } else if cfg!(all(target_arch = "x86_64", target_os = "windows")) {
            "x86_64-pc-windows-msvc"
        } else {
            return None;
        };
        Self::ALL.iter().find(|t| t.triple == triple)
    }

    /// Why this machine has no archive, phrased as what to do instead. Callers reach for this
    /// only where [`current`](Self::current) answered `None`, so it never has a triple to name.
    pub fn unsupported_reason() -> String {
        format!(
            "no published release for {}/{} — build from a checkout with `cargo build --release -p lore`",
            std::env::consts::OS,
            std::env::consts::ARCH,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_triple_is_named_once() {
        let mut triples: Vec<&str> = ReleaseTarget::ALL.iter().map(|t| t.triple).collect();
        triples.sort_unstable();
        let unique = {
            let mut copy = triples.clone();
            copy.dedup();
            copy
        };
        assert_eq!(triples, unique, "a triple appears twice in the table");
    }

    /// The `uname` pair is the key the shell installer selects by, so two targets claiming the
    /// same pair would make that selection depend on arm order in a `case` — one of them
    /// unreachable, and which one not decidable from this table.
    #[test]
    fn no_two_targets_answer_to_one_uname_pair() {
        let mut pairs: Vec<(&str, &str)> = ReleaseTarget::ALL
            .iter()
            .flat_map(|t| t.machines.iter().map(move |m| (t.os, *m)))
            .collect();
        pairs.sort_unstable();
        let len = pairs.len();
        pairs.dedup();
        assert_eq!(
            len,
            pairs.len(),
            "two targets answer to one os/machine pair"
        );
    }

    #[test]
    fn this_build_resolves_to_an_archive_the_release_publishes() {
        // Every platform the suite runs on is one the release builds for; a host outside the
        // table would make `current()` `None`, which is what `unsupported_reason` is for.
        let target = ReleaseTarget::current().expect("test hosts are release targets");
        assert!(ReleaseTarget::ALL.contains(target));
    }
}
