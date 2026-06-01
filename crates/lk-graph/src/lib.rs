//! Wikilink graph analysis for Obsidian vaults: hubs, orphans, community clustering,
//! index-sync, slug normalization, and the JSON graph export.
//!
//! frontmatter parsing, wikilink extraction) are delegated to `lk-core`; this crate
//! owns only the graph structure and I/O (walkdir/rayon vault scan, index-file fixup,
//! file rename).

pub mod alias;
pub mod audit;
pub mod backlinks;
pub mod cache;
pub mod cluster;
pub mod concept_lint;
pub mod export;
pub mod graph;
pub mod index_drift;
pub mod merge;
pub mod normalize;
pub mod output;
pub mod scan;
pub mod stale;

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    #[error("scan directory not found: {0}")]
    ScanDirNotFound(PathBuf),
    #[error("invalid exclude pattern '{0}': {1}")]
    InvalidExclude(String, String),
    #[error("I/O error: {0}")]
    Io(String),
}
