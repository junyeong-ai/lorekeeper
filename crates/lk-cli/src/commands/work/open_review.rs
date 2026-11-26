//! The open reviews a concept synthesis answers to. A synthesis capture lists every open
//! review naming the concept; the result names, in `review_refs`, those whose question it
//! leaves open, and for an evidence-gap review the defining claims still unsupported in
//! `gap_claims`. Publication renders the callouts beneath the synthesis from those references
//! and the capture, and records them on the concept (`llm_inputs.synthesis_reviews`), so the
//! marker and every reference are Rust's, and a callout follows its reviews from that record
//! rather than from prose.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use lk_core::i18n::Locale;
use lk_vault::ReviewKind;
use serde::{Deserialize, Serialize};

/// The `llm_inputs` key recording the reviews a published synthesis references.
pub(in crate::commands) const PUBLISHED_KEY: &str = "synthesis_reviews";

/// An open review naming the synthesized concept: its address, title and kind, and whether the
/// concept's citations changed since the review judged them. A review that records no judged
/// citations counts as changed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OpenReview {
    pub path: String,
    pub title: String,
    pub kind: Option<ReviewKind>,
    pub citations_changed: bool,
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
        let address = stored.to_string_lossy().into_owned();
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

/// What a synthesis publication adds for its referenced reviews: the callouts beneath its body
/// and the `llm_inputs.synthesis_reviews` record, each absent when it references none.
#[derive(Debug, Default)]
pub(in crate::commands) struct PublishedReviews {
    pub callout: Option<String>,
    pub record: Option<String>,
}

/// The `llm_inputs.synthesis_reviews` value recording each referenced review and its kind, or
/// `None` when the synthesis references none.
pub(super) fn record(reviews: &[OpenReview], refs: &[String]) -> Option<String> {
    let recorded = reviews
        .iter()
        .filter(|review| refs.contains(&review.path))
        .map(|review| (review.path.clone(), review.kind))
        .collect::<BTreeMap<_, _>>();
    (!recorded.is_empty())
        .then(|| serde_json::to_string(&recorded).expect("review record serializes"))
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

/// The reviews the published synthesis of `concept` (page `raw`) references, with the kind
/// each was referenced as: its `llm_inputs.synthesis_reviews` record. A synthesis published
/// before that record existed is read from the evidence-gap callout in its synthesis section:
/// the reference lines of a root block quote opening with the gap marker, never a quote inside
/// code or a link inside a claim.
fn published(concept: &str, raw: &str) -> Vec<(PathBuf, Option<ReviewKind>)> {
    let Ok(page) = lk_core::frontmatter::parse_page(raw) else {
        return Vec::new();
    };
    if let Some(recorded) = page
        .frontmatter
        .get(lk_core::frontmatter::field::LLM_INPUTS)
        .and_then(|inputs| inputs.get(PUBLISHED_KEY))
    {
        return serde_json::from_value::<BTreeMap<String, Option<ReviewKind>>>(recorded.clone())
            .map(|recorded| {
                recorded
                    .into_iter()
                    .map(|(path, kind)| (PathBuf::from(path), kind))
                    .collect()
            })
            .unwrap_or_default();
    }
    let Some(section) = lk_vault::resolve_section_in_body(&page.body, |s| s.concept_synthesis)
    else {
        return Vec::new();
    };
    let labels = Locale::ALL
        .iter()
        .map(|locale| format!("> {}: ", locale.strings().evidence_gap_review))
        .collect::<Vec<_>>();
    let mut references = Vec::new();
    for quote in lk_core::markdown::root_block_quotes(section.body) {
        let mut lines = lk_core::markdown::lines_with_endings(&section.body[quote])
            .map(|line| line.trim_end_matches(['\r', '\n']));
        if !lines
            .next()
            .is_some_and(|line| line.starts_with("> [!gap] "))
        {
            continue;
        }
        references.extend(
            lines
                .filter_map(|line| {
                    labels
                        .iter()
                        .find_map(|label| line.strip_prefix(label.as_str()))
                })
                .filter_map(lk_core::link::parse_standalone_page_link)
                .filter_map(|link| lk_core::link::resolve_dest(Path::new(concept), &link.dest))
                .map(|path| (path, Some(ReviewKind::EvidenceGap))),
        );
    }
    references
}

/// Whether the synthesis published on `concept` (page `raw`) references a review that no longer
/// matches its callout: removed or no longer a review, resolved, of another kind than it was
/// referenced as, or about other subjects. That synthesis is owed again. A page that cannot be
/// read or parsed, or whose review metadata is invalid, is left to review health; a recorded
/// address outside the vault is no review.
pub(in crate::commands) fn references_closed_review(
    root: &Path,
    concept: &str,
    raw: &str,
) -> miette::Result<bool> {
    let owner = |address: &Path| lk_vault::resolve_page_owner(root, address).ok();
    let concept_owner = owner(Path::new(concept));
    for (reference, kind) in published(concept, raw) {
        let Ok(path) = lk_vault::page_path(root, &reference) else {
            return Ok(true);
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
                return Ok(true);
            }
            Err(error) => return Err(miette::miette!("read {}: {error}", reference.display())),
        };
        let Ok(page) = lk_core::frontmatter::parse_page(&raw) else {
            continue;
        };
        let Some(metadata) = page.frontmatter.get("review").cloned() else {
            return Ok(true);
        };
        let Ok(review) = serde_json::from_value::<lk_vault::KnowledgeReview>(metadata) else {
            continue;
        };
        let names_concept = review
            .subjects
            .iter()
            .any(|subject| owner(Path::new(subject)) == concept_owner);
        if review.status != lk_vault::ReviewStatus::Open || review.kind != kind || !names_concept {
            return Ok(true);
        }
    }
    Ok(false)
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
            citations_changed,
        }
    }

    #[test]
    fn a_published_record_follows_each_review_and_a_legacy_callout_only_its_synthesis_quote() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        std::fs::create_dir_all(root.join("wiki/concepts")).unwrap();
        std::fs::create_dir_all(root.join("wiki/explorations")).unwrap();
        let concept = "wiki/concepts/topic.md";
        std::fs::write(root.join(concept), "---\ntype: concept\n---\n").unwrap();
        let write = |name: &str, kind: &str, status: &str| {
            std::fs::write(
                root.join(format!("wiki/explorations/{name}.md")),
                review(kind, status, concept),
            )
            .unwrap()
        };
        let recorded = |record: &str| {
            format!(
                "---\ntype: concept\nllm_inputs:\n  synthesis_reviews: {record}\n---\n## Synthesis\nBody.\n"
            )
        };
        let page = recorded(
            r#"{"wiki/explorations/gap.md":"evidence-gap","wiki/explorations/conflict.md":"conflict"}"#,
        );
        write("gap", "evidence-gap", "open");
        write("conflict", "conflict", "open");
        assert!(!references_closed_review(root, concept, &page).unwrap());
        write("conflict", "conflict", "resolved");
        assert!(references_closed_review(root, concept, &page).unwrap());
        write("conflict", "evidence-gap", "open");
        assert!(
            references_closed_review(root, concept, &page).unwrap(),
            "a review of another kind than it was referenced as"
        );
        write("conflict", "conflict", "open");
        std::fs::remove_file(root.join("wiki/explorations/gap.md")).unwrap();
        assert!(references_closed_review(root, concept, &page).unwrap());
        std::fs::write(
            root.join("wiki/explorations/gap.md"),
            "---\ntype: exploration\ntitle: Gap\n---\nNo longer a review.\n",
        )
        .unwrap();
        assert!(
            references_closed_review(root, concept, &page).unwrap(),
            "an exploration without review metadata is no longer a review"
        );
        assert!(
            references_closed_review(root, concept, &recorded(r#"{"../outside.md":"conflict"}"#))
                .unwrap(),
            "an address outside the vault is no review"
        );

        write("gap", "evidence-gap", "resolved");
        let callout = "> [!gap] These defining claims await supporting evidence.\n> - See [the old question](../explorations/conflict.md).\n>\n> Evidence gap review: [Gap](../explorations/gap.md)\n";
        let legacy = |synthesis: &str, related: &str| {
            format!("---\ntype: concept\n---\n## Synthesis\n{synthesis}\n\n## Related\n{related}\n")
        };
        assert!(
            references_closed_review(root, concept, &legacy(&format!("Body.\n\n{callout}"), ""))
                .unwrap()
        );
        assert!(
            !references_closed_review(root, concept, &legacy(&format!("```\n{callout}```"), ""))
                .unwrap(),
            "a quoted example inside code is no callout"
        );
        assert!(
            !references_closed_review(root, concept, &legacy("Body.", callout)).unwrap(),
            "a callout outside the synthesis section is not the synthesis's"
        );
        write("gap", "evidence-gap", "open");
        assert!(
            !references_closed_review(root, concept, &legacy(&format!("Body.\n\n{callout}"), ""))
                .unwrap(),
            "an open gap keeps its callout, and a link inside a claim is no reference"
        );
        assert!(!references_closed_review(root, concept, &recorded("{}")).unwrap());
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
            record(&reviews, &both).unwrap(),
            r#"{"wiki/explorations/conflict.md":"conflict","wiki/explorations/gap.md":"evidence-gap"}"#
        );
        assert_eq!(record(&reviews, &[]), None);
    }
}
