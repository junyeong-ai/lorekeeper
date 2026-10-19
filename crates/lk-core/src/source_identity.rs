use serde::{Deserialize, Serialize};

use crate::config::SourceType;

/// Stable document ownership, independent of title, URL, folder and content revision.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum SourceIdentity {
    Repository {
        repository: String,
        document: String,
    },
    Provider {
        provider: SourceType,
        namespace: String,
        document: String,
    },
}

impl SourceIdentity {
    pub fn from_value(value: serde_json::Value) -> Result<Self, String> {
        let identity: Self = serde_json::from_value(value)
            .map_err(|error| format!("invalid source_identity: {error}"))?;
        let scope = match &identity {
            Self::Repository { repository, .. } => repository,
            Self::Provider { namespace, .. } => namespace,
        };
        if scope.trim().is_empty() || identity.document().trim().is_empty() {
            return Err("source_identity requires a nonblank namespace and document".into());
        }
        Ok(identity)
    }

    pub fn document(&self) -> &str {
        match self {
            Self::Repository { document, .. } | Self::Provider { document, .. } => document,
        }
    }

    /// A typed namespace key; no provider ID is inferred from an address or display name.
    pub fn key(&self) -> (String, String) {
        let scope = match self {
            Self::Repository { repository, .. } => serde_json::json!(["repository", repository]),
            Self::Provider {
                provider,
                namespace,
                ..
            } => serde_json::json!(["provider", provider, namespace]),
        };
        (scope.to_string(), self.document().into())
    }

    pub fn with_document(&self, document: &str) -> Self {
        let mut identity = self.clone();
        match &mut identity {
            Self::Repository {
                document: target, ..
            }
            | Self::Provider {
                document: target, ..
            } => *target = document.into(),
        }
        identity
    }
}

impl std::fmt::Display for SourceIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{}",
            serde_json::to_string(self).map_err(|_| std::fmt::Error)?
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn native_namespaces_do_not_collide_or_accept_guessed_fields() {
        let repository =
            SourceIdentity::from_value(json!({"repository":"files","document":"same"})).unwrap();
        let provider = SourceIdentity::from_value(
            json!({"provider":"google-drive","namespace":"files","document":"same"}),
        )
        .unwrap();
        assert_ne!(repository.key(), provider.key());
        assert_eq!(provider.with_document("next").document(), "next");
        for invalid in [
            json!({"repository":"","document":"same"}),
            json!({"provider":"google-drive","namespace":"files","document":" "}),
            json!({"provider":"google-drive","document":"same"}),
            json!({"repository":"r","document":"same","url":"guessed"}),
            json!({"provider":"google-drive","namespace":"files","document":"same","repository":"r"}),
        ] {
            assert!(SourceIdentity::from_value(invalid).is_err());
        }
    }
}
