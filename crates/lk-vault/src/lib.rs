mod address;
mod body_search;
mod brief;
mod document;
mod indexed_links;
mod knowledge;
mod links;
mod log;
mod manual_original;
mod mutation;
mod navigation;
mod original_io;
mod provenance;
mod provenance_index;
mod review;
mod rss_evidence;
mod rss_original;
mod search;
mod search_index;
mod section;
mod store;
mod summary;
mod template;
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
pub use self::indexed_links::{
    IndexedLink, IndexedLinkOptions, IndexedLinks, LinkDirection, inspect_indexed_links,
};
pub use self::knowledge::{
    CategoryUpdate, ConceptCuration, CurationPlan, CurationResult, PageRevision, ReviewCuration,
    curate, preview_curation,
};
pub use self::links::{LinkDetail, LinkDetails, LinkOptions, inspect_links};
pub use self::log::{IngestLog, LogEntry, LogStatus};
pub use self::manual_original::{
    OriginalOptions, OriginalView, inspect_original, is_manual_original, publish_manual_original,
};
pub use self::mutation::{MutationPlan, MutationSession};
pub use self::navigation::{NAVIGATION_ENTRY_MARKER, plan_navigation_entry};
pub use self::provenance::{
    ProvenanceDirection, ProvenanceOptions, ProvenanceReport, inspect_provenance,
};
pub use self::review::{
    EvidenceReport, KnowledgeReview, ReviewEffectiveState, ReviewReport, ReviewStatus,
    archive_evidence, inspect_evidence, is_archived_evidence, list_reviews, load_archived_evidence,
    publish_document, publish_document_expected, publish_document_expected_with_plan,
    same_existing_file,
};
pub use self::rss_evidence::{
    RssEvidencePreview, accumulate_rss_evidence, guard_rss_citation, guard_rss_evidence,
    has_rss_citation, inspect_rss_citation, inspect_rss_evidence_inventory, load_rss_manifest,
    load_rss_selected_payload, repoint_rss_evidence, rss_concept_citations, rss_link_previews,
    validate_rss_citation, write_bindings,
};
pub use self::rss_original::{
    RssOriginalOptions, RssOriginalView, guard_rss_original, inspect_rss_original,
    publish_rss_original,
};
pub use self::search::{
    BrowseSort, CreatedStatus, MatchField, SearchHit, SearchOptions, SearchResult,
    parse_created_date, search,
};
pub use self::search_index::{SearchSnapshot, prepare_search};
pub use self::section::{
    PageSection, SectionKey, clear_llm_input, record_llm_input, replace_section, resolve_section,
    section_body, section_headings, set_frontmatter_field, set_llm_input,
};
pub use self::store::{
    FsVault, InMemoryVault, MarkdownInventory, VaultStore, load_concept_registry,
};
pub use self::template::{TemplateEngine, embedded_templates};
pub use self::writer::VaultWriter;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum VaultError {
    #[error("original: {0}")]
    Original(String),
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
