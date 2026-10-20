use serde::{Deserialize, Serialize};

/// Explicit lexical discovery instructions, never semantic identity or support.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryQuery {
    pub query: String,
    #[serde(default)]
    pub phrase: bool,
    #[serde(default)]
    pub spacing: bool,
}

#[derive(Debug, thiserror::Error)]
#[error("discovery query must be nonblank, at most 4096 UTF-8 bytes and contain no NUL")]
pub struct InvalidDiscoveryQuery;

impl DiscoveryQuery {
    pub fn validate(&self) -> Result<(), InvalidDiscoveryQuery> {
        if self.query.trim().is_empty() || self.query.len() > 4096 || self.query.contains('\0') {
            return Err(InvalidDiscoveryQuery);
        }
        Ok(())
    }
}
