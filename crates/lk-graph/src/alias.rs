//! Alias-conflict lint for concept pages.
//!
//! A concept may declare `aliases` so a bare `[[synonym]]` link resolves to it (see
//! the alias layer in [`crate::scan::VaultExistence`] and [`crate::graph::WikiGraph`]).
//! Resolution is deterministic — a real page always wins, and among concepts the
//! first to claim an alias wins — but a *silent* first-wins would let a genuine
//! ambiguity calcify. This lint surfaces the two ways an alias declaration goes wrong
//! so `graph lint` can report them. Pure read; it reports, a human decides.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use lk_core::concept::slugify;
use lk_core::config::VaultDirs;
use serde::Serialize;

use crate::scan::{ScannedPage, VaultExistence, is_concept_page};

/// One alias declaration that cannot resolve as intended.
#[derive(Debug, Clone, Serialize)]
pub struct AliasConflict {
    /// The alias slug at the center of the conflict.
    pub alias: String,
    /// The concept slugs that declare this alias, sorted (one entry for a shadow,
    /// two or more for a duplicate).
    pub claimants: Vec<String>,
    /// What makes the declaration wrong.
    pub kind: AliasConflictKind,
}

/// The two ways a declared alias fails to resolve as the author intended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AliasConflictKind {
    /// Two or more concepts declare the same alias; a bare `[[alias]]` resolves to
    /// only one of them (the first by slug order). Remove the alias from all but one.
    Duplicate,
    /// The alias equals an existing page's real slug, so it never resolves — the real
    /// page always wins. The declaration is inert; remove or rename it.
    ShadowsRealPage,
}

/// Surface alias declarations that cannot resolve as intended: a `Duplicate` alias
/// claimed by multiple concepts, or one that `ShadowsRealPage` (collides with a real
/// page slug). Deterministic and read-only — output is sorted by alias slug.
pub fn find_alias_conflicts(
    pages: &[ScannedPage],
    existence: &VaultExistence,
    dirs: &VaultDirs,
) -> Vec<AliasConflict> {
    // alias slug → declaring concept slugs (BTree for deterministic order).
    let mut claims: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for page in pages {
        if !is_concept_page(&page.path, dirs) {
            continue;
        }
        let Some(concept_slug) = real_slug(&page.path) else {
            continue;
        };
        for alias in &page.aliases {
            claims
                .entry(alias.clone())
                .or_default()
                .insert(concept_slug.clone());
        }
    }

    let mut conflicts = Vec::new();
    for (alias, claimants) in claims {
        // Full-vault real-page membership (via `existence`) — an alias equal to any real
        // page id/slug, in scope or not, is inert because the real page always wins. A
        // self-alias is dropped at scan time, so a hit here is a DIFFERENT page.
        if existence.is_real_page(&alias) {
            conflicts.push(AliasConflict {
                alias,
                claimants: claimants.into_iter().collect(),
                kind: AliasConflictKind::ShadowsRealPage,
            });
        } else if claimants.len() >= 2 {
            conflicts.push(AliasConflict {
                alias,
                claimants: claimants.into_iter().collect(),
                kind: AliasConflictKind::Duplicate,
            });
        }
    }
    conflicts
}

/// The slugified file stem of a page — its bare-link resolution key.
fn real_slug(path: &Path) -> Option<String> {
    path.file_stem().and_then(|s| s.to_str()).and_then(slugify)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn concept(slug: &str, aliases: &[&str]) -> ScannedPage {
        ScannedPage {
            id: format!("wiki/concepts/{slug}"),
            path: PathBuf::from(format!("wiki/concepts/{slug}.md")),
            title: slug.to_owned(),
            outgoing: vec![],
            aliases: aliases.iter().map(|a| (*a).to_owned()).collect(),
        }
    }

    fn conflicts(pages: &[ScannedPage]) -> Vec<AliasConflict> {
        let existence = VaultExistence::from_pages(pages, &VaultDirs::default());
        find_alias_conflicts(pages, &existence, &VaultDirs::default())
    }

    #[test]
    fn no_conflicts_when_aliases_are_distinct() {
        let pages = vec![
            concept("kubernetes", &["k8s"]),
            concept("postgres", &["postgresql"]),
        ];
        assert!(conflicts(&pages).is_empty());
    }

    #[test]
    fn duplicate_alias_across_concepts_is_flagged() {
        let pages = vec![
            concept("kubernetes", &["k8s"]),
            concept("kafka-streams", &["k8s"]),
        ];
        let conflicts = conflicts(&pages);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].alias, "k8s");
        assert_eq!(conflicts[0].kind, AliasConflictKind::Duplicate);
        assert_eq!(conflicts[0].claimants, vec!["kafka-streams", "kubernetes"]);
    }

    #[test]
    fn alias_shadowing_a_real_page_is_flagged() {
        // `helm` declares an alias `kubernetes`, but a real `kubernetes` concept
        // exists — the alias is inert.
        let pages = vec![concept("kubernetes", &[]), concept("helm", &["kubernetes"])];
        let conflicts = conflicts(&pages);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].alias, "kubernetes");
        assert_eq!(conflicts[0].kind, AliasConflictKind::ShadowsRealPage);
        assert_eq!(conflicts[0].claimants, vec!["helm"]);
    }

    #[test]
    fn non_concept_aliases_are_ignored() {
        // Only concept pages declare resolvable aliases; a stray alias on a daily
        // page is not a concept-graph conflict.
        let mut daily = concept("ignored", &["k8s"]);
        daily.id = "daily/x/2026-05-22".to_owned();
        daily.path = PathBuf::from("daily/x/2026-05-22.md");
        let pages = vec![concept("kubernetes", &["k8s"]), daily];
        assert!(conflicts(&pages).is_empty());
    }
}
