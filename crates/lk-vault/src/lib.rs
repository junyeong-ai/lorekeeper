mod frontmatter;
mod log;
mod reader;
mod template;
mod writer;

pub use self::frontmatter::{Frontmatter, Page};
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
