use std::collections::BTreeMap;

use lk_core::config::VaultDirs;
use lk_core::frontmatter::Frontmatter;
use lk_core::source_identity::SourceIdentity;
use lk_vault::VaultStore;

use crate::PipelineError;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DocumentIdentity {
    Stable(SourceIdentity),
    Location(String),
}

#[derive(Default)]
pub struct DocumentOwnership {
    addresses: BTreeMap<DocumentIdentity, Vec<String>>,
}

impl DocumentOwnership {
    pub async fn load(reader: &dyn VaultStore, dirs: &VaultDirs) -> Result<Self, PipelineError> {
        let directory = lk_core::vault_path::documents_dir(dirs);
        let mut ownership = Self::default();
        for path in reader.list_markdown_recursive(&directory).await? {
            let metadata = reader.read_frontmatter(&path).await?.ok_or_else(|| {
                PipelineError::Render(format!(
                    "document disappeared during address lookup: {}",
                    path.display()
                ))
            })?;
            if let Some(identity) = DocumentIdentity::from_page(&metadata)? {
                let address = path
                    .strip_prefix(&directory)
                    .map_err(|error| PipelineError::Render(error.to_string()))?
                    .with_extension("");
                let slug = address.to_str().ok_or_else(|| {
                    PipelineError::Render(format!(
                        "document address is not UTF-8: {}",
                        path.display()
                    ))
                })?;
                ownership
                    .addresses
                    .entry(identity)
                    .or_default()
                    .push(slug.replace(std::path::MAIN_SEPARATOR, "/"));
            }
        }
        Ok(ownership)
    }

    pub fn resolve(&self, identity: &DocumentIdentity) -> Result<Option<&str>, PipelineError> {
        match self.get(identity) {
            Some(addresses) if addresses.len() == 1 => Ok(Some(&addresses[0])),
            Some(addresses) => Err(PipelineError::Render(format!(
                "document source {identity} is claimed by multiple pages: {}",
                addresses.join(", ")
            ))),
            None => Ok(None),
        }
    }

    pub(crate) fn get(&self, identity: &DocumentIdentity) -> Option<&Vec<String>> {
        self.addresses.get(identity)
    }

    pub(crate) fn insert(&mut self, identity: DocumentIdentity, addresses: Vec<String>) {
        self.addresses.insert(identity, addresses);
    }
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
        if let Some(file) = file.filter(|value| !value.is_empty()) {
            return file_location(file).map(Self::Location).map(Some);
        }
        Ok(url
            .filter(|value| !value.is_empty())
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

fn file_location(declared: &str) -> Result<String, PipelineError> {
    let path = std::path::Path::new(declared);
    if !path.is_absolute() {
        return Ok(declared.into());
    }
    let failure = |error| {
        PipelineError::Render(format!(
            "resolve document source_file '{declared}': {error}"
        ))
    };
    let canonical = match path.canonicalize() {
        Ok(canonical) => canonical,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
                return Ok(declared.into());
            };
            match parent.canonicalize() {
                // Manual retirement removes the entry but leaves its inbox. Only
                // that verified parent can establish the archived leaf's location.
                Ok(parent) => parent.join(name),
                // An unavailable historical parent proves no alias identity. Keep
                // its declared location rather than infer it from a higher ancestor.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(declared.into());
                }
                Err(error) => return Err(failure(error)),
            }
        }
        Err(error) => return Err(failure(error)),
    };
    canonical.to_str().map(str::to_owned).ok_or_else(|| {
        PipelineError::Render(format!(
            "document source_file '{declared}' resolves to a non-UTF-8 location"
        ))
    })
}

impl std::fmt::Display for DocumentIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stable(identity) => identity.fmt(formatter),
            Self::Location(location) => formatter.write_str(location),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn location(path: &std::path::Path) -> Result<Option<DocumentIdentity>, PipelineError> {
        DocumentIdentity::from_fields(None, path.to_str(), None)
    }

    #[cfg(unix)]
    #[test]
    fn file_ownership_resolves_existing_and_archived_inbox_aliases() {
        let temp = tempfile::tempdir().unwrap();
        let inbox = temp.path().join("inbox");
        let alias = temp.path().join("alias");
        std::fs::create_dir(&inbox).unwrap();
        std::os::unix::fs::symlink(&inbox, &alias).unwrap();
        let entry = inbox.join("report.md");
        std::fs::write(&entry, "original").unwrap();
        let owner = location(&entry).unwrap();
        assert_eq!(location(&alias.join("report.md")).unwrap(), owner);

        std::fs::remove_file(&entry).unwrap();
        assert_eq!(location(&alias.join("report.md")).unwrap(), owner);
        assert_eq!(location(&entry).unwrap(), owner);
    }

    #[test]
    fn unavailable_historical_parent_keeps_declared_location() {
        let temp = tempfile::tempdir().unwrap();
        let declared = temp.path().join("removed-inbox/report.md");
        assert_eq!(
            location(&declared).unwrap(),
            Some(DocumentIdentity::Location(
                declared.to_str().unwrap().into()
            ))
        );
        assert_eq!(
            DocumentIdentity::from_fields(None, Some("relative/report.md"), None).unwrap(),
            Some(DocumentIdentity::Location("relative/report.md".into()))
        );
    }

    #[test]
    fn invalid_file_parent_is_not_silently_a_new_owner() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("not-a-directory");
        std::fs::write(&file, "x").unwrap();
        assert!(location(&file.join("report.md")).is_err());

        let stable = serde_json::json!({"repository":"repo", "document":"report"});
        assert!(matches!(
            DocumentIdentity::from_fields(Some(&stable), file.join("report.md").to_str(), None)
                .unwrap(),
            Some(DocumentIdentity::Stable(_))
        ));
        let url = "https://example.com/report.md";
        assert_eq!(
            DocumentIdentity::from_fields(None, None, Some(url)).unwrap(),
            Some(DocumentIdentity::Location(url.into()))
        );
    }

    #[test]
    fn distinct_hardlink_locations_keep_distinct_owners() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first.md");
        let second = temp.path().join("second.md");
        std::fs::write(&first, "same bytes").unwrap();
        std::fs::hard_link(&first, &second).unwrap();
        assert_ne!(location(&first).unwrap(), location(&second).unwrap());
    }
}
