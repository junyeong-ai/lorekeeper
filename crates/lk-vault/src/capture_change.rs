//! Bounded diagnostics for a rejected graph/search citation capture.
use std::path::{Component, Path};

use serde::Serialize;

/// A diagnostic address digest is not a page revision or publication authority.
#[derive(Debug, Clone, Serialize)]
pub struct CapturePath {
    pub path: Option<String>,
    pub digest: String,
}

impl CapturePath {
    pub fn build(path: &Path) -> Self {
        let encoded = serde_json::to_string(path.as_os_str().as_encoded_bytes())
            .expect("path bytes serialize");
        Self {
            path: path
                .to_str()
                .filter(|value| {
                    value.len() <= 128
                        && !value.is_empty()
                        && path
                            .components()
                            .all(|part| matches!(part, Component::Normal(_)))
                })
                .map(str::to_owned),
            digest: lk_core::concept::file_revision(&encoded),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum CaptureRevision {
    Exact { revision: String },
    Invalid { digest: String, bytes: usize },
}

impl CaptureRevision {
    pub fn build(revision: &str) -> Self {
        if revision.len() == 64 && revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            Self::Exact {
                revision: revision.into(),
            }
        } else {
            Self::Invalid {
                digest: lk_core::concept::file_revision(revision),
                bytes: revision.len(),
            }
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CaptureChangeCounts {
    pub added: usize,
    pub removed: usize,
    pub changed: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum CaptureFileState {
    Absent,
    Unreadable,
    Revision { revision: String },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum CaptureResolutionState {
    Absent,
    Missing,
    Owner { address: CapturePath },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "component", rename_all = "kebab-case")]
pub enum CaptureChange {
    Page {
        path: CapturePath,
        expected: CaptureFileState,
        current: CaptureFileState,
    },
    Resolution {
        path: CapturePath,
        expected: CaptureResolutionState,
        current: CaptureResolutionState,
    },
    Evidence {
        path: CapturePath,
        expected_digest: Option<String>,
        current_digest: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "phase", rename_all = "kebab-case")]
pub enum CitationCaptureError {
    SelectedPage {
        path: CapturePath,
        /// The graph captured this address; None means its read was unavailable.
        expected_revision: Option<String>,
        current_revision: CaptureRevision,
    },
    Graph {
        expected_revision: String,
        current_revision: String,
        root_changed: bool,
        pages: CaptureChangeCounts,
        /// Transitions between readable and unreadable are included in pages.changed.
        readability_changed: usize,
        resolutions: CaptureChangeCounts,
        evidence: CaptureChangeCounts,
        /// Reordering of the same concept-address set; membership is counted separately.
        evidence_order_changed: bool,
        examples: Vec<CaptureChange>,
        omitted: usize,
    },
}

impl std::fmt::Display for CitationCaptureError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let encoded = serde_json::to_string(self).map_err(|_| std::fmt::Error)?;
        write!(formatter, "citation capture changed: {encoded}")
    }
}

impl std::error::Error for CitationCaptureError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_paths_are_bounded_lossless_or_explicitly_digest_only() {
        let accepted = "a".repeat(128);
        assert_eq!(
            CapturePath::build(Path::new(&accepted)).path.as_deref(),
            Some(accepted.as_str())
        );
        for value in [
            "a".repeat(129),
            "가".repeat(43),
            "/private/account/file".into(),
            "../outside".into(),
        ] {
            let result = CapturePath::build(Path::new(&value));
            assert!(result.path.is_none());
            assert_eq!(result.digest.len(), 64);
        }
        assert_ne!(
            CapturePath::build(Path::new("é.md")).digest,
            CapturePath::build(Path::new("e\u{301}.md")).digest
        );
        assert_ne!(
            CapturePath::build(Path::new("A.md")).digest,
            CapturePath::build(Path::new("a.md")).digest
        );
        let escaped =
            serde_json::to_string(&CapturePath::build(Path::new("line\nname.md"))).unwrap();
        assert!(!escaped.contains('\n'));
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let path = std::path::PathBuf::from(std::ffi::OsString::from_vec(vec![255]));
            assert!(CapturePath::build(&path).path.is_none());
        }
    }

    #[test]
    fn invalid_revision_diagnostics_keep_size_and_digest_without_answer_text() {
        let private = "Private text.".repeat(1000);
        let encoded = serde_json::to_string(&CaptureRevision::build(&private)).unwrap();
        assert!(!encoded.contains("Private text."));
        assert!(encoded.len() < 160);
        assert!(matches!(
            CaptureRevision::build(&"a".repeat(64)),
            CaptureRevision::Exact { .. }
        ));
    }

    #[test]
    fn capture_diagnostic_rendering_remains_bounded_with_escaped_addresses() {
        let address = Path::new("\n".repeat(128).as_str()).to_path_buf();
        let examples = (0..9)
            .map(|_| CaptureChange::Resolution {
                path: CapturePath::build(&address),
                expected: CaptureResolutionState::Owner {
                    address: CapturePath::build(&address),
                },
                current: CaptureResolutionState::Owner {
                    address: CapturePath::build(&address),
                },
            })
            .collect();
        let change = CitationCaptureError::Graph {
            expected_revision: "a".repeat(64),
            current_revision: "b".repeat(64),
            root_changed: false,
            pages: CaptureChangeCounts::default(),
            readability_changed: 0,
            resolutions: CaptureChangeCounts::default(),
            evidence: CaptureChangeCounts::default(),
            evidence_order_changed: false,
            examples,
            omitted: usize::MAX,
        };
        let raw = change.to_string();
        assert!(raw.len() < 32768);
        assert!(!raw.contains('\n'));
    }
}
