mod address;
mod body_search;
mod brief;
mod document;
mod index;
mod links;
mod log;
mod mutation;
mod navigation;
mod provenance;
mod review;
mod search;
mod search_index;
mod section;
mod store;
mod template;
mod timeline;
mod writer;

// Frontmatter parsing is pure and lives in lk-core; re-exported so the vault crate's
// public surface exposes the page types alongside the I/O that produces them.
pub use lk_core::frontmatter::{Frontmatter, VaultPage, parse_page};

pub use self::address::{ExistingPageResolver, page_path, writable_page_path};
pub use self::body_search::{
    BodyContextLine, BodySearchHit, BodySearchOptions, BodySearchResult, search_body,
};
pub use self::brief::{Brief, BriefEntry, build_brief};
pub use self::document::{
    DocumentContent, DocumentOptions, DocumentSection, DocumentView, inspect_document,
};
pub use self::index::{IndexCatalog, build_catalog, build_index, write_index};
pub use self::links::{LinkDetail, LinkDetails, LinkOptions, inspect_links};
pub use self::log::{IngestLog, LogEntry, LogStatus};
pub use self::mutation::{MutationPlan, MutationSession};
pub use self::navigation::{
    NAVIGATION_PAGE_ENTRIES, NavigationCatalog, NavigationKind, NavigationRow,
};
pub use self::provenance::{
    ProvenanceDirection, ProvenanceOptions, ProvenanceReport, inspect_provenance,
};
pub use self::review::{
    EvidenceReport, KnowledgeReview, ReviewEffectiveState, ReviewReport, ReviewStatus,
    archive_evidence, inspect_evidence, list_reviews, load_archived_evidence, publish_document,
    publish_document_expected, same_existing_file,
};
pub use self::search::{MatchField, SearchHit, SearchOptions, SearchResult, search};
pub use self::search_index::{SearchSnapshot, prepare_search};
pub use self::section::{
    PageSection, SectionKey, clear_llm_input, record_llm_input, replace_section, resolve_section,
    section_body, section_headings, set_frontmatter_field, set_llm_input,
};
pub use self::store::{FsVault, InMemoryVault, VaultStore, load_concept_registry};
pub use self::template::{TemplateEngine, embedded_templates};
pub use self::timeline::{build_timeline, build_timeline_catalog, write_timeline};
pub use self::writer::VaultWriter;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum VaultError {
    #[error("search index: {0}")]
    SearchIndex(#[from] rusqlite::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("frontmatter: {0}")]
    Frontmatter(String),
    #[error("template: {0}")]
    Template(#[from] minijinja::Error),
    #[error("serialization: {0}")]
    Serialization(String),
}
