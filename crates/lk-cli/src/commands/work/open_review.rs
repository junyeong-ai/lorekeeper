//! The open reviews a concept synthesis answers to. A synthesis capture lists every open
//! review naming the concept; the result names, in `review_refs`, those whose question it
//! leaves open, and for an evidence-gap review the defining claims still unsupported in
//! `gap_claims`. Publication renders the callouts beneath the synthesis from those references
//! and the capture, and records them with each review's authored revision on the concept
//! (`llm_inputs.synthesis_reviews`, JSON text like every `llm_inputs` value, which a concept
//! rerender carries with the synthesis it describes; `{}` when it references none), so the marker and every
//! reference are Rust's, and a callout follows its reviews from that record, never from prose
//! whose author Rust cannot establish.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use lk_core::i18n::Locale;
use lk_vault::ReviewKind;
use serde::{Deserialize, Serialize};

/// An open review naming the synthesized concept: its address, title, kind and authored
/// revision (`lk_core::concept::source_revision`), and whether the concept's citations changed
/// since the review judged them. A review that records no judged citations counts as changed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OpenReview {
    pub path: String,
    pub title: String,
    pub kind: Option<ReviewKind>,
    pub revision: String,
    pub citations_changed: bool,
}

/// A referenced review as the published synthesis recorded it.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Recorded {
    kind: Option<ReviewKind>,
    revision: String,
}

impl OpenReview {
    fn is_gap(&self) -> bool {
        self.kind == Some(ReviewKind::EvidenceGap)
    }
}

/// The open reviews of `concept` among a synthesis capture's scoped `reviews`, for a request
/// answering the citations `citation_digest` names, each once under its verified owner's
/// address and read from the bytes that capture stored there. A review names the concept when
/// one of its subjects has the concept's owner. A resolved, unreadable or other-subject review is not one.
pub(super) fn open_reviews(
    root: &Path,
    concept: &str,
    citation_digest: &str,
    reviews: &[String],
    pages: &BTreeMap<PathBuf, String>,
) -> miette::Result<Vec<OpenReview>> {
    let owner = |address: &str| lk_vault::resolve_page_owner(root, Path::new(address)).ok();
    let Some(concept_owner) = owner(concept) else {
        return Ok(Vec::new());
    };
    let mut open = Vec::new();
    let mut owners = std::collections::BTreeSet::new();
    for path in reviews {
        let Some(stored) = owner(path).filter(|stored| owners.insert(stored.clone())) else {
            continue;
        };
        let address = lk_core::vault_path::address(&stored);
        let Some(page) = pages
            .get(&stored)
            .and_then(|raw| lk_core::frontmatter::parse_page(raw).ok())
        else {
            continue;
        };
        let Some(review) = page
            .frontmatter
            .get("review")
            .cloned()
            .and_then(|value| serde_json::from_value::<lk_vault::KnowledgeReview>(value).ok())
        else {
            continue;
        };
        if review.status == lk_vault::ReviewStatus::Open
            && review
                .subjects
                .iter()
                .any(|subject| owner(subject).as_ref() == Some(&concept_owner))
        {
            let title = page
                .frontmatter
                .get("title")
                .and_then(serde_json::Value::as_str)
                .map(|title| title.split_whitespace().collect::<Vec<_>>().join(" "))
                .filter(|title| !title.is_empty())
                .unwrap_or_else(|| address.clone());
            open.push(OpenReview {
                path: address,
                title,
                kind: review.kind,
                revision: lk_core::concept::source_revision(&page),
                citations_changed: review.judged_citation_digest.as_deref()
                    != Some(citation_digest),
            });
        }
    }
    Ok(open)
}

/// Check a result's references against the open reviews its scope holds. With reviews in
/// scope, `review_refs` lists those whose question the synthesis leaves open, each once; an
/// empty list answers that the citations now settle every one. A review judged against these
/// exact citations stays referenced, since no new evidence answers it. `gap_claims` carries
/// the defining claims, one nonblank line each, exactly when an evidence-gap review is
/// referenced.
pub(super) fn check_reply(
    reviews: &[OpenReview],
    refs: Option<&[String]>,
    claims: Option<&[String]>,
) -> Result<(), String> {
    let refs = match (reviews.is_empty(), refs) {
        (true, None) => &[][..],
        (true, Some(_)) => {
            return Err("review_refs need an open review of this concept in scope".into());
        }
        (false, None) => {
            return Err(
                "open reviews of this concept are in scope: list in review_refs each one whose question the synthesis leaves open, or [] when the citations now settle every one"
                    .into(),
            );
        }
        (false, Some(refs)) => refs,
    };
    let mut seen = Vec::new();
    for reference in refs {
        if !reviews.iter().any(|review| &review.path == reference) || seen.contains(&reference) {
            return Err(format!(
                "review_refs entry {reference} is not one distinct open review in scope"
            ));
        }
        seen.push(reference);
    }
    if let Some(unchanged) = reviews
        .iter()
        .find(|review| !review.citations_changed && !refs.contains(&review.path))
    {
        return Err(format!(
            "the citations are those the review {} judged; no new evidence answers it, so keep it in review_refs",
            unchanged.path
        ));
    }
    let gap = reviews
        .iter()
        .any(|review| review.is_gap() && refs.contains(&review.path));
    match (gap, claims) {
        (false, None) => Ok(()),
        (false, Some(_)) => {
            Err("gap_claims need an evidence-gap review listed in review_refs".into())
        }
        (true, Some(claims))
            if !claims.is_empty()
                && claims
                    .iter()
                    .all(|claim| !claim.trim().is_empty() && !claim.contains(['\n', '\r'])) =>
        {
            Ok(())
        }
        (true, _) => Err(
            "an evidence-gap review listed in review_refs needs the defining claims it names that remain unsupported in gap_claims, each one nonblank line"
                .into(),
        ),
    }
}

/// The callouts for the referenced reviews: the evidence-gap callout with its claims and a
/// reference to each gap review, then one callout referencing every other review. Each
/// reference is its own paragraph so no inline span in a claim or title reaches another.
pub(super) fn render(
    locale: Locale,
    concept: &str,
    reviews: &[OpenReview],
    refs: &[String],
    claims: &[String],
) -> Option<String> {
    let strings = locale.strings();
    let referenced = reviews
        .iter()
        .filter(|review| refs.contains(&review.path))
        .collect::<Vec<_>>();
    let reference = |label: &str, review: &OpenReview| {
        let destination = lk_core::link::relative_dest(Path::new(concept), Path::new(&review.path));
        format!(
            ">\n> {label}: {}\n",
            lk_core::link::md_link(&review.title, &destination)
        )
    };
    let mut callouts = Vec::new();
    let gaps = referenced.iter().filter(|review| review.is_gap());
    if gaps.clone().next().is_some() {
        let mut callout = format!("> [!gap] {}\n", strings.evidence_gap_lead);
        for claim in claims {
            callout.push_str(&format!("> - {}\n", claim.trim()));
        }
        for review in gaps {
            callout.push_str(&reference(strings.evidence_gap_review, review));
        }
        callouts.push(callout);
    }
    let others = referenced.iter().filter(|review| !review.is_gap());
    if others.clone().next().is_some() {
        let mut callout = format!("> [!question] {}\n", strings.open_review_lead);
        for review in others {
            callout.push_str(&reference(strings.open_review, review));
        }
        callouts.push(callout);
    }
    (!callouts.is_empty()).then(|| callouts.join("\n"))
}

/// What a synthesis publication adds for its referenced reviews: the callouts beneath its body,
/// absent when it references none, and the `llm_inputs.synthesis_reviews` record, which every
/// synthesis publication writes and no other result does.
#[derive(Debug, Default)]
pub(in crate::commands) struct PublishedReviews {
    pub callout: Option<String>,
    pub record: Option<String>,
}

/// The `llm_inputs.synthesis_reviews` value recording each referenced review's kind and
/// authored revision, `{}` when the synthesis references none.
pub(super) fn record(reviews: &[OpenReview], refs: &[String]) -> String {
    let recorded = reviews
        .iter()
        .filter(|review| refs.contains(&review.path))
        .map(|review| {
            (
                review.path.clone(),
                Recorded {
                    kind: review.kind,
                    revision: review.revision.clone(),
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    lk_core::concept::yaml_scalar(&recorded)
}

/// The open reviews a synthesis capture listed.
pub(super) fn from_context(context: &serde_json::Value) -> miette::Result<Vec<OpenReview>> {
    match context["purpose"]["parameters"].get("open_reviews") {
        Some(reviews) => serde_json::from_value(reviews.clone())
            .map_err(|error| miette::miette!("synthesis open reviews: {error}")),
        None => Ok(Vec::new()),
    }
}

/// The locale a synthesis context publishes in.
pub(super) fn locale(context: &serde_json::Value) -> miette::Result<Locale> {
    context["purpose"]["parameters"]["locale"]
        .as_str()
        .and_then(Locale::try_from_tag)
        .ok_or_else(|| miette::miette!("synthesis context has no supported locale"))
}

/// What a concept's published synthesis owes its referenced reviews.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::commands) enum PublishedState {
    /// No `llm_inputs.synthesis_reviews` record: published before references were recorded,
    /// or never synthesized. Nothing establishes that a callout on the page is Rust's.
    Unrecorded,
    /// Every recorded review is still the open review of the concept the synthesis read.
    Current,
    /// A recorded review was removed or is no longer a review, resolved, retyped, about other
    /// subjects or revised since, or the record itself is invalid: the synthesis is owed again.
    Owed,
}

/// The state of the synthesis published on `concept` (page `raw`) against its record. A review
/// page that cannot be parsed, or whose review metadata is invalid, is left to review health.
pub(in crate::commands) fn published_state(
    root: &Path,
    concept: &str,
    raw: &str,
) -> miette::Result<PublishedState> {
    let Some(recorded) = lk_core::frontmatter::parse_page(raw).ok().and_then(|page| {
        page.frontmatter
            .get(lk_core::frontmatter::field::LLM_INPUTS)
            .and_then(|inputs| inputs.get(lk_core::frontmatter::field::SYNTHESIS_REVIEWS))
            .cloned()
    }) else {
        return Ok(PublishedState::Unrecorded);
    };
    let Some(Ok(recorded)) = recorded
        .as_str()
        .map(serde_json::from_str::<BTreeMap<String, Recorded>>)
    else {
        return Ok(PublishedState::Owed);
    };
    let owner = |address: &Path| lk_vault::resolve_page_owner(root, address).ok();
    let concept_owner = owner(Path::new(concept));
    for (reference, recorded) in recorded {
        let Ok(path) = lk_vault::page_path(root, Path::new(&reference)) else {
            return Ok(PublishedState::Owed);
        };
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound
                        | std::io::ErrorKind::IsADirectory
                        | std::io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(PublishedState::Owed);
            }
            Err(error) => return Err(miette::miette!("read {reference}: {error}")),
        };
        let Ok(page) = lk_core::frontmatter::parse_page(&raw) else {
            continue;
        };
        let Some(metadata) = page.frontmatter.get("review").cloned() else {
            return Ok(PublishedState::Owed);
        };
        let Ok(review) = serde_json::from_value::<lk_vault::KnowledgeReview>(metadata) else {
            continue;
        };
        let names_concept = review
            .subjects
            .iter()
            .any(|subject| owner(Path::new(subject)) == concept_owner);
        if review.status != lk_vault::ReviewStatus::Open
            || review.kind != recorded.kind
            || !names_concept
            || lk_core::concept::source_revision(&page) != recorded.revision
        {
            return Ok(PublishedState::Owed);
        }
    }
    Ok(PublishedState::Current)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review(kind: &str, status: &str, subject: &str) -> String {
        format!(
            "---\ntype: exploration\ntitle: |-\n  Definition\n  gap\nreview:\n  status: {status}\n  kind: {kind}\n  subjects: [{subject}]\n  evidence: []\n  resolution: null\n---\nThe definition lacks support.\n"
        )
    }

    fn open(path: &str, kind: ReviewKind, citations_changed: bool) -> OpenReview {
        OpenReview {
            path: path.into(),
            title: "Title".into(),
            kind: Some(kind),
            revision: format!("{path}-revision"),
            citations_changed,
        }
    }

    #[test]
    fn a_published_record_follows_each_review_state_and_revision_and_prose_is_never_one() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        std::fs::create_dir_all(root.join("wiki/concepts")).unwrap();
        std::fs::create_dir_all(root.join("wiki/explorations")).unwrap();
        let concept = "wiki/concepts/topic.md";
        std::fs::write(root.join(concept), "---\ntype: concept\n---\n").unwrap();
        let write = |name: &str, raw: &str| {
            std::fs::write(root.join(format!("wiki/explorations/{name}.md")), raw).unwrap()
        };
        let revision = |raw: &str| {
            lk_core::concept::source_revision(&lk_core::frontmatter::parse_page(raw).unwrap())
        };
        let recorded = |record: &str| {
            format!(
                "---\ntype: concept\nllm_inputs:\n  synthesis_reviews: {}\n---\n## Synthesis\nBody.\n",
                serde_json::to_string(record).unwrap()
            )
        };
        let conflict = review("conflict", "open", concept);
        let gap = review("evidence-gap", "open", concept);
        write("conflict", &conflict);
        write("gap", &gap);
        let page = recorded(&format!(
            r#"{{"wiki/explorations/conflict.md":{{"kind":"conflict","revision":"{}"}},"wiki/explorations/gap.md":{{"kind":"evidence-gap","revision":"{}"}}}}"#,
            revision(&conflict),
            revision(&gap)
        ));
        let state = |page: &str| published_state(root, concept, page).unwrap();
        assert_eq!(state(&page), PublishedState::Current);
        write(
            "conflict",
            &conflict.replace("The definition lacks support.", "The scope is narrower."),
        );
        assert_eq!(
            state(&page),
            PublishedState::Owed,
            "an open review revised since publication owes the synthesis an update"
        );
        write(
            "conflict",
            &conflict.replace("status: open", "status: resolved"),
        );
        assert_eq!(state(&page), PublishedState::Owed);
        write("conflict", &review("evidence-gap", "open", concept));
        assert_eq!(state(&page), PublishedState::Owed, "retyped");
        write(
            "conflict",
            "---\ntype: exploration\ntitle: Gap\n---\nNo longer a review.\n",
        );
        assert_eq!(state(&page), PublishedState::Owed, "no longer a review");
        std::fs::remove_file(root.join("wiki/explorations/conflict.md")).unwrap();
        assert_eq!(state(&page), PublishedState::Owed, "removed");
        assert_eq!(
            state(&recorded(
                r#"{"../outside.md":{"kind":"conflict","revision":"r"}}"#
            )),
            PublishedState::Owed,
            "an address outside the vault is no review"
        );
        assert_eq!(state(&recorded("{}")), PublishedState::Current);

        let quoted = "---\ntype: concept\n---\n## Synthesis\nBody.\n\n> [!gap] These defining claims await supporting evidence.\n>\n> Evidence gap review: [Gap](../explorations/gap.md)\n";
        write("gap", &gap.replace("status: open", "status: resolved"));
        assert_eq!(
            state(quoted),
            PublishedState::Unrecorded,
            "a quote nothing establishes as Lore's never owes a rewrite"
        );
    }

    #[test]
    fn only_an_open_review_naming_the_concept_owner_is_in_scope() {
        let root = tempfile::tempdir().unwrap();
        let concepts = root.path().join("wiki/concepts");
        std::fs::create_dir_all(&concepts).unwrap();
        std::fs::create_dir_all(root.path().join("wiki/explorations")).unwrap();
        std::fs::write(concepts.join("topic.md"), "---\ntype: concept\n---\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("topic.md", concepts.join("topic-alias.md")).unwrap();
        let concept = "wiki/concepts/topic.md";
        let alias = if cfg!(unix) {
            "wiki/concepts/topic-alias.md"
        } else {
            concept
        };
        let reviews = [
            ("gap", review("evidence-gap", "open", alias)),
            ("conflict-alias", String::new()),
            ("conflict", review("conflict", "open", concept)),
            ("resolved", review("evidence-gap", "resolved", concept)),
            (
                "other",
                review("evidence-gap", "open", "wiki/concepts/other.md"),
            ),
        ];
        let mut pages = BTreeMap::new();
        let mut scoped = Vec::new();
        for (name, raw) in reviews {
            let path = format!("wiki/explorations/{name}.md");
            if name == "conflict-alias" {
                #[cfg(unix)]
                {
                    std::os::unix::fs::symlink("conflict.md", root.path().join(&path)).unwrap();
                    scoped.push(path);
                }
                continue;
            }
            std::fs::write(root.path().join(&path), &raw).unwrap();
            pages.insert(PathBuf::from(&path), raw);
            scoped.push(path);
        }
        let root = root.path().canonicalize().unwrap();
        let digest = "0123456789abcdef0123456789abcdef";
        let open = open_reviews(&root, concept, digest, &scoped, &pages).unwrap();
        assert_eq!(
            open.iter()
                .map(|review| review.path.as_str())
                .collect::<Vec<_>>(),
            ["wiki/explorations/gap.md", "wiki/explorations/conflict.md"],
            "a subject alias names the concept's owner, and a review reached through two \
             addresses is listed once"
        );
        assert_eq!(
            open[0].title, "Definition gap",
            "the title renders on one line"
        );
        let judged = review("evidence-gap", "open", concept).replace(
            "  resolution: null\n",
            &format!("  resolution: null\n  judged_citation_digest: {digest}\n"),
        );
        let path = PathBuf::from("wiki/explorations/gap.md");
        std::fs::write(root.join(&path), &judged).unwrap();
        let unchanged = open_reviews(
            &root,
            concept,
            digest,
            &[path.to_string_lossy().into_owned()],
            &BTreeMap::from([(path, judged)]),
        )
        .unwrap();
        assert!(!unchanged[0].citations_changed);
    }

    #[test]
    fn references_name_open_reviews_and_gap_claims_follow_a_referenced_gap() {
        let gap = open("wiki/explorations/gap.md", ReviewKind::EvidenceGap, true);
        let conflict = open("wiki/explorations/conflict.md", ReviewKind::Conflict, true);
        let reviews = [gap.clone(), conflict.clone()];
        let claims = ["A protocol is a mutual agreement.".to_owned()];
        let refs = |paths: &[&str]| {
            paths
                .iter()
                .map(|path| path.to_string())
                .collect::<Vec<_>>()
        };
        assert!(check_reply(&[], None, None).is_ok());
        assert!(check_reply(&[], Some(&[]), None).is_err());
        assert!(check_reply(&reviews, None, None).is_err());
        assert!(
            check_reply(&reviews, Some(&[]), None).is_ok(),
            "recovered evidence settles both"
        );
        assert!(check_reply(&reviews, Some(&refs(&[&conflict.path])), None).is_ok());
        assert!(check_reply(&reviews, Some(&refs(&[&gap.path])), None).is_err());
        assert!(check_reply(&reviews, Some(&refs(&[&gap.path])), Some(&claims)).is_ok());
        assert!(check_reply(&reviews, Some(&refs(&[&conflict.path])), Some(&claims)).is_err());
        assert!(
            check_reply(
                &reviews,
                Some(&refs(&[&gap.path, &gap.path])),
                Some(&claims)
            )
            .is_err()
        );
        assert!(check_reply(&reviews, Some(&refs(&["wiki/explorations/other.md"])), None).is_err());
        assert!(
            check_reply(
                &reviews,
                Some(&refs(&[&gap.path])),
                Some(&["two\nlines".to_owned()])
            )
            .is_err()
        );
        let judged = [open(&conflict.path, ReviewKind::Conflict, false)];
        assert!(
            check_reply(&judged, Some(&[]), None).is_err(),
            "the citations a review was judged against cannot settle it"
        );

        let concept = "wiki/concepts/topic.md";
        let both = refs(&[&gap.path, &conflict.path]);
        assert_eq!(
            render(Locale::En, concept, &reviews, &both, &claims).unwrap(),
            "> [!gap] These defining claims await supporting evidence.\n> - A protocol is a mutual agreement.\n>\n> Evidence gap review: [Title](../explorations/gap.md)\n\n> [!question] These reviews keep a question about this concept open.\n>\n> Open review: [Title](../explorations/conflict.md)\n"
        );
        assert_eq!(render(Locale::En, concept, &reviews, &[], &[]), None);
        assert_eq!(
            record(&reviews, &both),
            r#"{"wiki/explorations/conflict.md":{"kind":"conflict","revision":"wiki/explorations/conflict.md-revision"},"wiki/explorations/gap.md":{"kind":"evidence-gap","revision":"wiki/explorations/gap.md-revision"}}"#
        );
        assert_eq!(record(&reviews, &[]), "{}");
    }
}
