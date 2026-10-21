//! Content-addressed cache for LLM-filled page sections.
//!
//! Daily pages are materialized views with two kinds of fields:
//! - **Structural** (frontmatter, raw event list, headings) — Rust-owned, re-rendered
//!   on every ingest.
//! - **Semantic** (summary body, refined event bodies, concept wiki-links) — LLM-owned,
//!   preserved across re-renders, and invalidated by the BLAKE3-128 hash of the LLM
//!   input recorded in the page's `llm_inputs` frontmatter.
//!
//! Completion is uniformly **marker-signalled** (see `TargetKind::completion_key`). On
//! each ingest the pipeline pre-stamps `llm_inputs.<key>` with the hash of the LLM input
//! it WOULD send — the stale-task reference point — and `/lore-process` stamps the
//! companion `llm_inputs.<key>_done` once it has finished, even when the result is empty.
//! [`lookup`] returns cached when that `*_done` marker equals the current input hash
//! and the owned section exists, regardless of its body: an empty-but-done result
//! (an extraction that found nothing, a focus-filtered summary, a trivial-only work-log)
//! stays cached instead of re-enqueueing forever. There is no body-emptiness completion signal anywhere — that
//! would require a per-kind "can this be empty?" judgment, and any kind misjudged
//! "always non-empty" would re-enqueue every empty result forever.
//!
//! On a cache hit the new render splices the existing body back in (preserving the
//! LLM-owned content) and re-emits the `*_done` marker; on a miss the task is enqueued,
//! the section is left empty for the skill, and a stale marker is dropped rather than
//! carried forward. `llm_inputs.<key>` always equals the current input's hash, so a
//! queued task whose `cache_hash` differs is unambiguously stale (a newer ingest
//! re-rendered the page) and is dropped — never a window where a stale task is
//! indistinguishable from a current one.
//!
//! Explicit regeneration retains the validated task input and invalidates completion
//! together with a new request through the CLI publication journal.

use lk_core::frontmatter::{VaultPage, field};
use lk_queue::TargetKind;
use lk_vault::{SectionKey, resolve_section};

/// Per-section cache decision. The pipeline computes one of these for every LLM
/// task it could enqueue.
#[derive(Debug, Clone)]
pub struct SectionDecision {
    /// Provenance follows preserved output; explicit requests follow only their input.
    pub provenance: std::collections::BTreeMap<String, String>,
    /// The request's `cache_hash` (BLAKE3-128 of its cache identity) — written into
    /// the new render's `llm_inputs.<key>` frontmatter regardless of whether the task
    /// is enqueued, so subsequent runs have a reference point.
    pub hash: String,
    /// True when the completion marker equals this hash and the owned section exists.
    /// The body may be empty; its presence does not establish completion.
    pub cached: bool,
    /// Existing section body to splice back into the freshly rendered page when
    /// `cached`. Stripped of section framing whitespace so it can be handed to
    /// `replace_section`.
    pub preserved_body: Option<String>,
    /// The section held content that this render will REPLACE with an empty one.
    ///
    /// Only ever true on a cache miss, which is the pipeline's normal path: the section is
    /// re-enqueued and a drain fills it again. It matters because a miss also describes a
    /// section somebody answered without recording it — a body written by hand, or by a drain
    /// that never stamped — and that content is not recoverable once the page is rewritten.
    /// Reported rather than inferred, so a caller can say what it is about to discard instead of
    /// leaving it to be noticed afterwards.
    pub discarding: Option<String>,
}

impl SectionDecision {
    pub fn enqueue(&self) -> bool {
        !self.cached
    }
}

/// The hash recorded in `existing`'s `llm_inputs.<key>` frontmatter, if any. The
/// single reader of that frontmatter shape — every cache decision goes through here.
pub fn stored_hash<'a>(existing: Option<&'a VaultPage>, key: &str) -> Option<&'a str> {
    existing?
        .frontmatter
        .get(field::LLM_INPUTS)
        .and_then(|v| v.get(key))
        .and_then(|v| v.as_str())
}

/// Decide whether the LLM task for this section can be skipped because the existing
/// page declares the task's format and carries its completion marker for this `hash`.
/// `heading` names the section as a [`Heading`] selector, so the existing body is found
/// under whichever locale the page was authored in.
///
/// Body emptiness is deliberately NOT consulted: a matching marker and an existing
/// section complete even an empty result (an extraction that found nothing, a
/// focus-filtered summary, a trivial-only work-log), avoiding repeated enqueueing.
/// On a hit the existing body is preserved and spliced over the fresh render.
pub fn lookup(
    existing: Option<&VaultPage>,
    kind: TargetKind,
    heading: impl SectionKey,
    hash: String,
) -> SectionDecision {
    let existing_body =
        existing.and_then(|page| resolve_section(&page.body, heading).map(|s| s.body));
    let input_key = kind.llm_inputs_key();
    let completion_key = kind.completion_key();
    let compatible = existing.is_some_and(|page| {
        page.frontmatter
            .get("type")
            .and_then(|value| value.as_str())
            == Some(kind.page_format())
    });
    let mut provenance = std::collections::BTreeMap::new();
    let request_input = format!("{input_key}_policy_requested_input");
    let request = format!("{input_key}_policy_requested");
    if compatible
        && stored_hash(existing, input_key) == Some(&hash)
        && stored_hash(existing, &request_input) == Some(&hash)
        && let Some(policy) = stored_hash(existing, &request)
    {
        provenance.insert(request, policy.to_owned());
        provenance.insert(request_input, hash.clone());
    }

    if !compatible || stored_hash(existing, &completion_key) != Some(&hash) {
        return SectionDecision {
            provenance,
            hash,
            cached: false,
            preserved_body: None,
            discarding: existing_body
                .map(str::trim)
                .filter(|body| !body.is_empty())
                .map(str::to_owned),
        };
    }

    let Some(body) = existing_body else {
        // Marker says done, but no locale's spelling of the section is present — the
        // rendered heading drifted from the one the marker was written against (a custom
        // `--template-dir` renamed it). Reporting a hit would freeze the stale body
        // forever (the task is never re-enqueued, and with no preserved body the
        // splice-site drift warning never fires either). Force a re-run and surface it.
        tracing::warn!(
            completion_key = %completion_key,
            "section marked done but heading not found (custom-template drift?); re-enqueueing"
        );
        return SectionDecision {
            provenance,
            hash,
            cached: false,
            preserved_body: None,
            discarding: None,
        };
    };

    let policy_key = format!("{input_key}_policy_done");
    if let Some(policy) = stored_hash(existing, &policy_key) {
        provenance.insert(policy_key, policy.to_owned());
    }
    SectionDecision {
        provenance,
        hash,
        cached: true,
        preserved_body: Some(body.trim_matches('\n').to_string()),
        discarding: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lk_core::frontmatter::parse_page;

    fn page_with(frontmatter: &str, body: &str) -> VaultPage {
        let content = format!("---\ntype: daily\n{frontmatter}---\n\n{body}");
        parse_page(&content).expect("test fixture parses")
    }

    #[test]
    fn missing_page_is_uncached() {
        let d = lookup(None, TargetKind::DailySummary, |s| s.summary, "abc".into());
        assert!(d.enqueue());
        assert!(d.preserved_body.is_none());
    }

    #[test]
    fn missing_marker_is_uncached() {
        // Input hash present but the `*_done` marker absent — the skill hasn't
        // finished this input yet.
        let page = page_with("id: x\nllm_inputs:\n  summary: abc\n", "## 요약\n\nbody\n");
        let d = lookup(
            Some(&page),
            TargetKind::DailySummary,
            |s| s.summary,
            "abc".into(),
        );
        assert!(d.enqueue());
    }

    #[test]
    fn mismatched_marker_is_uncached() {
        let page = page_with(
            "id: x\nllm_inputs:\n  summary: fresh\n  summary_done: stale\n",
            "## 요약\n\nbody\n",
        );
        let d = lookup(
            Some(&page),
            TargetKind::DailySummary,
            |s| s.summary,
            "fresh".into(),
        );
        assert!(d.enqueue());
    }

    #[test]
    fn matching_marker_with_filled_body_hits_cache() {
        let page = page_with(
            "id: x\nllm_inputs:\n  summary: abc\n  summary_done: abc\n",
            "## 요약\n\nreal content\n\n## 출처\n",
        );
        let d = lookup(
            Some(&page),
            TargetKind::DailySummary,
            |s| s.summary,
            "abc".into(),
        );
        assert!(!d.enqueue());
        assert_eq!(d.preserved_body.as_deref(), Some("real content"));
    }

    #[test]
    fn matching_marker_with_empty_body_hits_cache() {
        // The whole point of marker-signalled completion: an empty result (a
        // focus-filtered summary that matched nothing, an extraction that found
        // nothing) is DONE once its marker is stamped. Body emptiness is never
        // consulted, so it must NOT re-enqueue.
        let page = page_with(
            "id: x\nllm_inputs:\n  concepts: abc\n  concepts_done: abc\n",
            "## 관련 개념\n\n\n## 출처\n",
        );
        let d = lookup(
            Some(&page),
            TargetKind::DailyConcepts,
            |s| s.related_concepts,
            "abc".into(),
        );
        assert!(!d.enqueue(), "empty-but-done section must stay cached");
        assert_eq!(d.preserved_body.as_deref(), Some(""));
    }

    #[test]
    fn cache_reuse_requires_the_tasks_declared_page_format() {
        for kind in [
            TargetKind::DailySummary,
            TargetKind::DocumentSummary,
            TargetKind::WeeklyReviewNarrative,
            TargetKind::AnnualReviewNarrative,
        ] {
            for declaration in [
                String::new(),
                "type: null\n".into(),
                "type: [document]\n".into(),
                "type: exploration\n".into(),
                format!("type: {}\n", kind.page_format()),
            ] {
                let key = kind.llm_inputs_key();
                let done = kind.completion_key();
                let page = parse_page(&format!(
                    "---\n{declaration}llm_inputs:\n  {key}: current\n  {done}: current\n  {key}_policy_done: historical\n---\n## Summary\n"
                ))
                .unwrap();
                let decision = lookup(Some(&page), kind, |s| s.summary, "current".into());
                let compatible = declaration == format!("type: {}\n", kind.page_format());
                assert_eq!(decision.cached, compatible, "{kind:?}: {declaration}");
                assert_eq!(
                    decision.preserved_body.as_deref(),
                    compatible.then_some(""),
                    "{kind:?}: {declaration}"
                );
                assert_eq!(decision.provenance.is_empty(), !compatible);
            }
        }
    }

    #[test]
    fn marker_set_but_heading_missing_is_uncached() {
        // Completion marker matches, but the section sits under a heading NO locale writes —
        // a custom template renamed it. This must NOT count as cached, or the body would
        // freeze and the task would never re-enqueue.
        let page = page_with(
            "id: x\nllm_inputs:\n  refine_events: abc\n  refine_events_done: abc\n",
            "## Daily Rundown\n\n### A\n\nbody\n",
        );
        let d = lookup(
            Some(&page),
            TargetKind::DailyRefineEvents,
            |s| s.key_events,
            "abc".into(),
        );
        assert!(
            d.enqueue(),
            "missing heading must force re-enqueue, not a silent hit"
        );
        assert!(d.preserved_body.is_none());
    }

    /// A `vault.locale` switch renames every heading at once. READ must still find the body —
    /// the rule `lk_vault::index` and every concept reader already follow — because here the
    /// alternative is not an omitted summary line but an answered, marker-stamped section
    /// overwritten empty, on every page in the vault, reported as a clean run.
    #[test]
    fn a_body_authored_under_another_locale_is_still_found() {
        let ko_page = page_with(
            "id: x\nllm_inputs:\n  summary: abc\n  summary_done: abc\n",
            "## 요약\n\nThe durable record.\n\n## 내용\n",
        );
        let hit = lookup(
            Some(&ko_page),
            TargetKind::DailySummary,
            |s| s.summary,
            "abc".into(),
        );
        assert!(!hit.enqueue(), "the same input is still answered");
        assert_eq!(hit.preserved_body.as_deref(), Some("The durable record."));

        // A locale is part of the request, so the switch also changes the hash: the section is
        // legitimately re-enqueued to be rewritten in the new language — and what that costs
        // is now reported instead of discovered.
        let miss = lookup(
            Some(&ko_page),
            TargetKind::DailySummary,
            |s| s.summary,
            "new".into(),
        );
        assert!(miss.enqueue());
        assert_eq!(miss.discarding.as_deref(), Some("The durable record."));
    }

    #[test]
    fn multi_line_body_preserves_internal_blank_lines() {
        let page = page_with(
            "id: x\nllm_inputs:\n  refine_events: abc\n  refine_events_done: abc\n",
            "## 주요 이벤트\n\n### A\n\nbody a\n\n### B\n\nbody b\n\n## 관련 개념\n",
        );
        let d = lookup(
            Some(&page),
            TargetKind::DailyRefineEvents,
            |s| s.key_events,
            "abc".into(),
        );
        assert!(!d.enqueue());
        let preserved = d.preserved_body.unwrap();
        assert!(preserved.contains("### A"));
        assert!(preserved.contains("### B"));
        assert!(preserved.contains("body a"));
        assert!(preserved.contains("body b"));
    }
}
