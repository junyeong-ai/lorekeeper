use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use lk_core::concept::{Citation, identity_key, source_revision};
use lk_core::config::VaultDirs;
use serde::{Deserialize, Serialize};

use crate::{SearchOptions, VaultError, search};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryQuery {
    pub query: String,
    #[serde(default)]
    pub phrase: bool,
    #[serde(default)]
    pub spacing: bool,
}

impl DiscoveryQuery {
    pub fn validate(&self) -> Result<(), VaultError> {
        if self.query.trim().is_empty() || self.query.len() > 4096 || self.query.contains('\0') {
            return Err(std::io::Error::other(
                "discovery query must be nonblank, at most 4096 UTF-8 bytes and contain no NUL",
            )
            .into());
        }
        Ok(())
    }
}

/// Complete lexical candidates, including retired knowledge. Matches are context,
/// never inferred citations, equivalence or contradiction judgments.
pub fn discover_queries(
    root: &Path,
    dirs: &VaultDirs,
    queries: &[DiscoveryQuery],
) -> Result<BTreeMap<DiscoveryQuery, Vec<Citation>>, VaultError> {
    for query in queries {
        query.validate()?;
    }
    if queries.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut vocabulary = Vec::new();
    search::visit_collections(
        root,
        dirs,
        vec![lk_core::vault_path::concepts_dir(dirs)],
        |mut page, _| {
            page.page.body = String::new();
            page.page.original = None;
            vocabulary.push(page);
            Ok(())
        },
    )?;
    let plans = queries
        .iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|query| {
            let identity = identity_key(&query.query);
            let spellings = search::spellings_of(identity.as_deref(), &vocabulary);
            let terms = search::query_terms(&query.query, query.phrase);
            (query, identity, spellings, terms)
        })
        .collect::<Vec<_>>();
    let mut result = BTreeMap::from_iter(
        plans
            .iter()
            .map(|(query, ..)| ((*query).clone(), Vec::new())),
    );
    search::visit_pages(root, dirs, false, |mut page, _| {
        page.projection = Some(page.match_projection().into_owned());
        let mut revision = None;
        for (query, identity, spellings, terms) in &plans {
            let options = SearchOptions {
                query: Some(&query.query),
                phrase: query.phrase,
                spacing: query.spacing,
                knowledge_status: None,
                ..Default::default()
            };
            if search::match_query(&page, &options, spellings, terms, identity.as_deref()).is_some()
            {
                let revision = revision.get_or_insert_with(|| source_revision(&page.page));
                result
                    .get_mut(*query)
                    .expect("selected query")
                    .push(Citation::at_revision(&page.rel_path, revision.clone()));
            }
        }
        Ok(())
    })?;
    for matches in result.values_mut() {
        matches.sort();
        matches.dedup();
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_uses_declared_aliases_and_preserves_unlinked_retired_candidates() {
        let root = tempfile::tempdir().unwrap();
        let files = [
            (
                "concepts/policy.md",
                "---\ntype: concept\ntitle: Retention policy\naliases: [보존 정책]\n---\n## Core\nA scoped policy.\n",
            ),
            (
                "documents/a.md",
                "---\ntype: document\n---\n보존 정책은 운영에서 30일이다.\n",
            ),
            (
                "documents/b.md",
                "---\ntype: document\nknowledge_status: retired\n---\n보존 정책은 운영에서 7일이다.\n",
            ),
            (
                "documents/unrelated.md",
                "---\ntype: document\n---\nDifferent material.\n",
            ),
        ];
        for (path, text) in files {
            let path = root.path().join("wiki").join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let query = DiscoveryQuery {
            query: "Retention policy".into(),
            phrase: false,
            spacing: false,
        };
        let matches = discover_queries(
            root.path(),
            &VaultDirs::default(),
            std::slice::from_ref(&query),
        )
        .unwrap();
        assert_eq!(
            matches[&query]
                .iter()
                .map(|c| c.path.as_str())
                .collect::<Vec<_>>(),
            [
                "wiki/concepts/policy.md",
                "wiki/documents/a.md",
                "wiki/documents/b.md"
            ]
        );
        let invalid = DiscoveryQuery {
            query: " ".into(),
            ..query
        };
        assert!(discover_queries(root.path(), &VaultDirs::default(), &[invalid]).is_err());
    }
}
