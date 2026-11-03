use super::CapturedConceptCitations;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use lk_vault::{
    CaptureChange, CaptureChangeCounts, CaptureFileState, CapturePath, CaptureResolutionState,
    CitationCaptureError,
};

const EXAMPLES_PER_COMPONENT: usize = 3;

fn changes<'a, K: Ord, V: PartialEq>(
    expected: &'a BTreeMap<K, V>,
    current: &'a BTreeMap<K, V>,
) -> impl Iterator<Item = (&'a K, Option<&'a V>, Option<&'a V>)> {
    expected
        .iter()
        .filter_map(move |(key, before)| {
            let after = current.get(key);
            (after != Some(before)).then_some((key, Some(before), after))
        })
        .chain(current.iter().filter_map(move |(key, after)| {
            (!expected.contains_key(key)).then_some((key, None, Some(after)))
        }))
}

fn count(counts: &mut CaptureChangeCounts, expected: bool, current: bool) {
    match (expected, current) {
        (false, true) => counts.added += 1,
        (true, false) => counts.removed += 1,
        _ => counts.changed += 1,
    }
}

fn example(
    examples: &mut Vec<CaptureChange>,
    omitted: &mut usize,
    group_start: usize,
    build: impl FnOnce() -> CaptureChange,
) {
    if examples.len() - group_start < EXAMPLES_PER_COMPONENT {
        examples.push(build());
    } else {
        *omitted += 1;
    }
}

fn file(value: Option<&Option<String>>) -> CaptureFileState {
    match value {
        None => CaptureFileState::Absent,
        Some(None) => CaptureFileState::Unreadable,
        Some(Some(revision)) => CaptureFileState::Revision {
            revision: revision.clone(),
        },
    }
}

fn resolution(value: Option<&Option<PathBuf>>, root: &Path) -> CaptureResolutionState {
    match value {
        None => CaptureResolutionState::Absent,
        Some(None) => CaptureResolutionState::Missing,
        Some(Some(path)) => CaptureResolutionState::Owner {
            address: CapturePath::build(path.strip_prefix(root).unwrap_or(path)),
        },
    }
}

impl CapturedConceptCitations {
    pub(super) fn capture_change(&self, current: &Self) -> CitationCaptureError {
        let mut pages = CaptureChangeCounts::default();
        let mut readability_changed = 0;
        let mut resolutions = CaptureChangeCounts::default();
        let mut evidence = CaptureChangeCounts::default();
        let mut examples = Vec::new();
        let mut omitted = 0;
        let group_start = examples.len();
        for (path, before, after) in changes(&self.file_revisions, &current.file_revisions) {
            count(&mut pages, before.is_some(), after.is_some());
            if matches!(
                (before, after),
                (Some(Some(_)), Some(None)) | (Some(None), Some(Some(_)))
            ) {
                readability_changed += 1;
            }
            example(&mut examples, &mut omitted, group_start, || {
                CaptureChange::Page {
                    path: CapturePath::build(path),
                    expected: file(before),
                    current: file(after),
                }
            });
        }
        let group_start = examples.len();
        for (path, before, after) in changes(&self.resolutions, &current.resolutions) {
            count(&mut resolutions, before.is_some(), after.is_some());
            example(&mut examples, &mut omitted, group_start, || {
                CaptureChange::Resolution {
                    path: CapturePath::build(path),
                    expected: resolution(before, &self.root),
                    current: resolution(after, &current.root),
                }
            });
        }
        let expected_concepts = self
            .projection
            .concepts
            .iter()
            .map(|concept| (&concept.path, concept))
            .collect::<BTreeMap<_, _>>();
        let current_concepts = current
            .projection
            .concepts
            .iter()
            .map(|concept| (&concept.path, concept))
            .collect::<BTreeMap<_, _>>();
        let evidence_order_changed = expected_concepts.keys().eq(current_concepts.keys())
            && !self
                .projection
                .concepts
                .iter()
                .map(|concept| &concept.path)
                .eq(current
                    .projection
                    .concepts
                    .iter()
                    .map(|concept| &concept.path));
        let group_start = examples.len();
        for (path, before, after) in changes(&expected_concepts, &current_concepts) {
            count(&mut evidence, before.is_some(), after.is_some());
            let revision = |value: &&lk_vault::ConceptCitationSet| {
                lk_core::concept::file_revision(
                    &serde_json::to_string(value).expect("concept capture serializes"),
                )
            };
            example(&mut examples, &mut omitted, group_start, || {
                CaptureChange::Evidence {
                    path: CapturePath::build(path),
                    expected_digest: before.map(revision),
                    current_digest: after.map(revision),
                }
            });
        }
        CitationCaptureError::Graph {
            expected_revision: self.projection.input_revision.clone(),
            current_revision: current.projection.input_revision.clone(),
            root_changed: self.root != current.root,
            pages,
            readability_changed,
            resolutions,
            evidence,
            evidence_order_changed,
            examples,
            omitted,
        }
    }
}
