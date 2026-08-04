mod index;
mod log;
mod section;
mod store;
mod template;
mod timeline;
mod writer;

// Frontmatter parsing is pure and lives in lk-core; re-exported so the vault crate's
// public surface exposes the page types alongside the I/O that produces them.
pub use lk_core::frontmatter::{Frontmatter, VaultPage, parse_page};

pub use self::index::{build_index, write_index};
pub use self::log::{IngestLog, LogEntry, LogStatus};
pub use self::section::{
    PageSection, SectionKey, clear_llm_input, record_llm_input, replace_section, resolve_section,
    section_body, section_headings, set_frontmatter_field, set_llm_input,
};
pub use self::store::{FsVault, InMemoryVault, VaultStore};
pub use self::template::TemplateEngine;
pub use self::timeline::{build_timeline, write_timeline};
pub use self::writer::VaultWriter;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum VaultError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("frontmatter: {0}")]
    Frontmatter(String),
    #[error("template: {0}")]
    Template(#[from] minijinja::Error),
    #[error("serialization: {0}")]
    Serialization(String),
}
