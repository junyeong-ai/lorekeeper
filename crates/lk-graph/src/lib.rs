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
    /// edited or removed after the derivation: a recording guarded by it answers earlier
    /// inputs.
    #[error("audit read dependency changed: {0}")]
    AuditInputChanged(String),
    /// While an exact selection was measured, `input`, a page `subject`'s inputs read, was
    /// edited or removed after the subject was derived: that subject's planning drift.
    #[error("audit subject {subject} changed while it was planned: {input}")]
    AuditSubjectInputChanged { subject: String, input: String },
    /// An audit subject no page of the vault holds: a writer removed it after it was planned
    /// (a screening reply's subject removed or retyped after its packet was issued, a batch
    /// slot or an exact selection whose page is gone), or an explicit selection names no page.
    /// The path is the subject as it was selected.
    #[error("audit subject is not in the vault: {0}")]
    AuditSubjectRemoved(String),
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
    /// A vault read or publication failure, kept typed so its caller can tell a guard that
    /// found a page changed (`VaultError::MutationConflict`) from a fault.
    #[error(transparent)]
    Vault(lk_vault::VaultError),
    #[error("I/O error: {0}")]
    Io(String),
}

impl GraphError {
    pub fn into_vault_error(self) -> lk_vault::VaultError {
        match self {
            Self::Cancelled => lk_vault::VaultError::Cancelled,
            Self::CaptureChanged(change) => lk_vault::VaultError::CitationCapture(change),
            Self::Vault(error) => error,
            other => std::io::Error::other(other).into(),
        }
    }
}

impl From<lk_vault::VaultError> for GraphError {
    fn from(error: lk_vault::VaultError) -> Self {
        match error {
            lk_vault::VaultError::Cancelled => Self::Cancelled,
            other => Self::Vault(other),
        }
    }
}
