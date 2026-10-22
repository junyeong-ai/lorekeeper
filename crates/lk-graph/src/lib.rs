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
mod sources;

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum GraphError {
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
    #[error("I/O error: {0}")]
    Io(String),
}
