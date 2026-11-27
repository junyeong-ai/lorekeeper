//! Link graph analysis for Obsidian vaults: hubs, orphans, community clustering,
//! slug normalization, and the JSON graph export.
//!
//! Pure domain logic (slugify, frontmatter parsing, link extraction) is delegated to
//! `lk-core`; this crate owns only the graph structure and I/O (walkdir/rayon vault scan,
//! file rename).

pub mod audit;
pub mod backlinks;
pub mod cluster;
pub mod concept_lint;
pub mod export;
pub mod graph;
pub mod merge;
pub mod normalize;
pub mod output;
pub mod scan;

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    #[error(transparent)]
    AuditAssessment(#[from] audit::AuditAssessmentError),
    #[error("audit screening inputs changed; reread the current subject and candidate cards")]
    AuditScreeningChanged,
    /// A page an audit derivation read (its subject, evidence, context or a scoped review) was
    /// edited or removed after the derivation: a recording or adoption guarded by it answers
    /// earlier inputs.
    #[error("audit read dependency changed: {0}")]
    AuditInputChanged(String),
    /// A screening reply's subject is no longer an audit subject of the vault: a writer
    /// removed it (or retyped it) after its packet was issued.
    #[error("audit subject left the vault after its screening packet: {0}")]
    AuditSubjectRemoved(String),
    /// Discovery adoption found the subject's assessment current: its legacy derivation is
    /// what its receipt verifies, so it is not adopted.
    #[error("the audit subject's assessment is current; its discovery is not adopted")]
    AuditSubjectCurrent,
    #[error(
        "audit screening requires {required} {boundary} bytes, above the complete configured bound {maximum}"
    )]
    AuditScreeningCapacity {
        boundary: audit::selection::CapacityBoundary,
        required: usize,
        maximum: usize,
    },
    #[error("graph input read cancelled")]
    Cancelled,
    #[error(transparent)]
    CaptureChanged(#[from] Box<lk_vault::CitationCaptureError>),
    #[error("failed to read page {path}: {error}")]
    UnreadablePage {
        path: PathBuf,
        #[source]
        error: scan::PageReadError,
    },
    #[error("cannot reconcile evidence while source page is unreadable: {0}")]
    UnreadableSource(PathBuf),
    #[error("scan directory not found: {0}")]
    ScanDirNotFound(PathBuf),
    #[error("invalid exclude pattern '{0}': {1}")]
    InvalidExclude(String, String),
    #[error(
        "vault directory '{configured}' is spelled '{on_disk}' on disk, and this filesystem \
         keeps the two apart — reading one and writing the other would address \
         different directories. Rename it, or point the config at '{on_disk}'"
    )]
    DirSpelling {
        configured: PathBuf,
        on_disk: PathBuf,
    },
    #[error(transparent)]
    Unresynthesizable(#[from] backlinks::Unresynthesizable),
    #[error("I/O error: {0}")]
    Io(String),
}

impl GraphError {
    pub fn into_vault_error(self) -> lk_vault::VaultError {
        match self {
            Self::Cancelled => lk_vault::VaultError::Cancelled,
            Self::CaptureChanged(change) => lk_vault::VaultError::CitationCapture(change),
            other => std::io::Error::other(other).into(),
        }
    }
}

impl From<lk_vault::VaultError> for GraphError {
    fn from(error: lk_vault::VaultError) -> Self {
        match error {
            lk_vault::VaultError::Cancelled => Self::Cancelled,
            other => Self::Io(other.to_string()),
        }
    }
}
