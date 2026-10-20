use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

use lk_core::concept::{Citation, identity_key, source_revision};
use lk_core::config::VaultDirs;
pub use lk_core::discovery::DiscoveryQuery;

use crate::{SearchOptions, VaultError, search};

/// Complete lexical candidates, including retired knowledge. Matches are context,
/// never inferred citations, equivalence or contradiction judgments.
pub fn discover_queries(
    root: &Path,
    dirs: &VaultDirs,
    queries: &[DiscoveryQuery],
) -> Result<BTreeMap<DiscoveryQuery, Vec<Citation>>, VaultError> {
    for query in queries {
        query.validate().map_err(std::io::Error::other)?;
    }
    if queries.is_empty() {
        return Ok(BTreeMap::new());
    }
    if let Some(matches) = indexed_queries(root, dirs, queries)? {
        return Ok(matches);
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

fn indexed_queries(
    root: &Path,
    dirs: &VaultDirs,
    queries: &[DiscoveryQuery],
) -> Result<Option<BTreeMap<DiscoveryQuery, Vec<Citation>>>, VaultError> {
    let snapshot = match crate::search_index::inspect_search_snapshot(root, dirs) {
        Ok(snapshot) => snapshot,
        Err(_) => return Ok(None),
    };
    let path = crate::page_path(root, Path::new(crate::search_index::INDEX_PATH))?;
    let mut connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let transaction = connection.transaction()?;
    crate::search_index::verify_snapshot(&transaction, root, dirs, &snapshot.snapshot)?;
    let mut statement = transaction.prepare(
        "SELECT path, json_extract(metadata, '$.file_revision') FROM pages WHERE knowledge = 1",
    )?;
    let mut captured = statement
        .query_map([], |row| {
            Ok((
                PathBuf::from(row.get::<_, String>(0)?),
                row.get::<_, String>(1)?,
            ))
        })?
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    let mut revisions = BTreeMap::new();
    let mut unchanged = true;
    search::visit_pages(root, dirs, false, |page, _| {
        unchanged &= captured.remove(&page.rel_path).as_deref() == Some(&page.file_revision);
        revisions.insert(page.rel_path, source_revision(&page.page));
        Ok(())
    })?;
    if !unchanged || !captured.is_empty() {
        return Ok(None);
    }
    let mut result = BTreeMap::new();
    for query in queries.iter().collect::<BTreeSet<_>>() {
        let options = SearchOptions {
            query: Some(&query.query),
            phrase: query.phrase,
            spacing: query.spacing,
            knowledge_status: None,
            ..Default::default()
        };
        let mut matches = Vec::new();
        crate::search_index::visit_query_candidates(
            &transaction,
            &options,
            |page, spellings, terms, identity| {
                if search::match_query(&page, &options, spellings, terms, identity).is_some() {
                    let revision = revisions.get(&page.rel_path).ok_or_else(|| {
                        std::io::Error::other(
                            "indexed discovery candidate is outside the verified scope",
                        )
                    })?;
                    matches.push(Citation::at_revision(&page.rel_path, revision.clone()));
                }
                Ok(())
            },
        )?;
        matches.sort();
        matches.dedup();
        result.insert(query.clone(), matches);
    }
    Ok(Some(result))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, path: &str, body: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn indexed_discovery_is_complete_and_requires_exact_current_knowledge_inputs() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let concept = "wiki/concepts/policy.md";
        let subject = "wiki/documents/a.md";
        let body =
            "---\ntype: document\n---\nProduction order retention is 30 days. AI is in scope.\n";
        write(
            root.path(),
            concept,
            "---\ntype: concept\ntitle: Retention policy\naliases: [보존 정책]\n---\nA production order retention policy.\n",
        );
        write(root.path(), subject, body);
        for index in 0..80 {
            write(
                root.path(),
                &format!("wiki/documents/retired-{index:02}.md"),
                "---\ntype: document\nknowledge_status: retired\n---\n보존 정책: production order retention was 7 days.\n",
            );
        }
        let queries = [
            DiscoveryQuery {
                query: "Retention policy".into(),
                phrase: false,
                spacing: false,
            },
            DiscoveryQuery {
                query: "production order retention".into(),
                phrase: true,
                spacing: false,
            },
            DiscoveryQuery {
                query: "보존정책".into(),
                phrase: false,
                spacing: true,
            },
            DiscoveryQuery {
                query: "AI".into(),
                phrase: false,
                spacing: false,
            },
        ];
        let live = discover_queries(root.path(), &dirs, &queries).unwrap();
        assert!(
            live[&queries[0]].len() > 50,
            "discovery is not ranked top-K retrieval"
        );
        let session = crate::MutationSession::acquire(root.path()).unwrap();
        crate::prepare_search(&session, &dirs).unwrap();
        let database = root.path().join(crate::search_index::INDEX_PATH);
        let before = std::fs::read(&database).unwrap();
        assert_eq!(
            indexed_queries(root.path(), &dirs, &queries).unwrap(),
            Some(live.clone())
        );
        assert_eq!(
            discover_queries(root.path(), &dirs, &queries).unwrap(),
            live
        );
        assert_eq!(
            std::fs::read(&database).unwrap(),
            before,
            "read-only discovery must not prepare or update a capture"
        );

        let modified = std::fs::metadata(root.path().join(subject))
            .unwrap()
            .modified()
            .unwrap();
        write(root.path(), subject, &body.replace("30 days", "70 days"));
        std::fs::File::options()
            .write(true)
            .open(root.path().join(subject))
            .unwrap()
            .set_modified(modified)
            .unwrap();
        assert!(
            indexed_queries(root.path(), &dirs, &queries)
                .unwrap()
                .is_none(),
            "same-size, same-mtime changes invalidate capture reuse"
        );
        let changed = discover_queries(root.path(), &dirs, &queries).unwrap();
        assert_ne!(changed[&queries[1]], live[&queries[1]]);
        crate::prepare_search(&session, &dirs).unwrap();
        assert_eq!(
            indexed_queries(root.path(), &dirs, &queries).unwrap(),
            Some(changed)
        );

        let added = "wiki/documents/new.md";
        write(root.path(), added, body);
        assert!(
            indexed_queries(root.path(), &dirs, &queries)
                .unwrap()
                .is_none()
        );
        let expanded = discover_queries(root.path(), &dirs, &queries).unwrap();
        assert!(
            expanded[&queries[1]]
                .iter()
                .any(|citation| citation.path == added)
        );
        crate::prepare_search(&session, &dirs).unwrap();
        std::fs::remove_file(root.path().join(added)).unwrap();
        assert!(
            indexed_queries(root.path(), &dirs, &queries)
                .unwrap()
                .is_none()
        );
        assert!(
            !discover_queries(root.path(), &dirs, &queries).unwrap()[&queries[1]]
                .iter()
                .any(|citation| citation.path == added)
        );
        crate::prepare_search(&session, &dirs).unwrap();
        write(
            root.path(),
            concept,
            "---\ntype: concept\ntitle: Retention policy\naliases: [A different name]\n---\nA production order retention policy.\n",
        );
        assert!(
            indexed_queries(root.path(), &dirs, &queries)
                .unwrap()
                .is_none()
        );
        let renamed = discover_queries(root.path(), &dirs, &queries).unwrap();
        assert!(
            renamed[&queries[0]].len() < live[&queries[0]].len(),
            "alias changes must alter lexical discovery immediately"
        );
    }

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
