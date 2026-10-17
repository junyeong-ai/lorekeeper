use lk_core::frontmatter::Frontmatter;
use serde::Deserialize;

use crate::PipelineError;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepositoryDocument {
    pub repository: String,
    pub document: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum DocumentIdentity {
    Repository(RepositoryDocument),
    Location(String),
}

impl DocumentIdentity {
    pub fn from_fields(
        stable: Option<&serde_json::Value>,
        file: Option<&str>,
        url: Option<&str>,
    ) -> Result<Option<Self>, PipelineError> {
        if let Some(stable) = stable.filter(|value| !value.is_null()) {
            let identity: RepositoryDocument =
                serde_json::from_value(stable.clone()).map_err(|error| {
                    PipelineError::Render(format!("invalid source_identity: {error}"))
                })?;
            if identity.repository.trim().is_empty() || identity.document.trim().is_empty() {
                return Err(PipelineError::Render(
                    "source_identity repository and document must be nonempty".into(),
                ));
            }
            return Ok(Some(Self::Repository(identity)));
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
            Self::Repository(identity) => write!(
                formatter,
                "repository {:?}, document {:?}",
                identity.repository, identity.document
            ),
            Self::Location(location) => formatter.write_str(location),
        }
    }
}
