use lk_core::frontmatter::Frontmatter;
use lk_core::source_identity::SourceIdentity;

use crate::PipelineError;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum DocumentIdentity {
    Stable(SourceIdentity),
    Location(String),
}

impl DocumentIdentity {
    pub fn from_fields(
        stable: Option<&serde_json::Value>,
        file: Option<&str>,
        url: Option<&str>,
    ) -> Result<Option<Self>, PipelineError> {
        if let Some(stable) = stable.filter(|value| !value.is_null()) {
            return SourceIdentity::from_value(stable.clone())
                .map(Self::Stable)
                .map(Some)
                .map_err(PipelineError::Render);
        }
        Ok(file
            .filter(|value| !value.is_empty())
            .or_else(|| url.filter(|value| !value.is_empty()))
            .map(|value| Self::Location(value.into())))
    }

    pub fn from_page(frontmatter: &Frontmatter) -> Result<Option<Self>, PipelineError> {
        Self::from_fields(
            frontmatter.get("source_identity"),
            frontmatter
                .get("source_file")
                .and_then(|value| value.as_str()),
            frontmatter
                .get("source_url")
                .and_then(|value| value.as_str()),
        )
    }
}

impl std::fmt::Display for DocumentIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stable(identity) => identity.fmt(formatter),
            Self::Location(location) => formatter.write_str(location),
        }
    }
}
