mod index;
mod log;
mod reader;
mod template;
mod writer;

// Frontmatter parsing is pure and lives in lk-core; re-exported here so existing
// `lk_vault::{Frontmatter, Page}` call sites keep working.
pub use lk_core::frontmatter::{Frontmatter, Page, parse_page};

pub use self::index::{build_index, first_line_under_heading, write_index};
pub use self::log::{IngestLog, LogEntry, LogStatus};
pub use self::reader::VaultReader;
pub use self::template::TemplateEngine;
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
