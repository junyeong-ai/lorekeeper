//! Content-addressed cache for LLM-filled page sections.
//!
//! Daily pages are materialized views with two kinds of fields:
//! - **Structural** (frontmatter, raw event list, headings) — Rust-owned, re-rendered
//!   on every ingest.
//! - **Semantic** (summary body, refined event bodies, concept wiki-links) — LLM-owned,
//!   preserved across re-renders, and invalidated by the BLAKE3-128 hash of the LLM
//!   input recorded in the page's `llm_inputs` frontmatter.
//!
//! On each ingest the pipeline computes the hash of the LLM input it WOULD send. If
//! that hash matches the one already recorded in the existing page AND the section is
//! filled, no LLM task is enqueued; the new render splices the existing body back in.
//! Otherwise the task is enqueued and the section is left empty for `/lore-process`
//! to fill.
//!
//! **Two cache shapes** (see `TargetKind::cache_shape`):
//! - `FillEmpty` (summary, concepts, narratives): the section starts empty, so a
//!   non-empty body IS the completion signal. The pipeline pre-stamps the hash in
//!   `llm_inputs.<key>` at render time and [`lookup`] returns cached when that hash
//!   matches and the body is filled.
//! - `InPlace` (the daily event list): the render already populates the section, so
//!   it is non-empty from the first ingest and emptiness cannot mean "not done". The
//!   pipeline still pre-stamps `llm_inputs.<key>` with the *current-input* hash — this
//!   is the stale-task reference point, identical in role to the fill-empty case — but
//!   completion is tracked by a SECOND field (`completion_key`) that `/lore-process`
//!   writes once it has actually rewritten the bodies. [`lookup_in_place`] returns
//!   cached only when that completion stamp equals the current-input hash.
//!
//! The single page-side invariant either shape upholds: `llm_inputs.<key>` always
//! equals the current input's hash, so a queued task whose `cache_hash` differs is
//! unambiguously stale (a newer ingest re-rendered the page) and is dropped — there is
//! never a window where a stale task is indistinguishable from a current one.
//!
//! Manual re-processing is mechanism-free: deleting the section body (fill-empty) or
//! the `completion_key` line (in-place) forces a re-enqueue on the next ingest. No
//! `--force-llm` flag, no out-of-band cache invalidation API.

use lk_core::frontmatter::{VaultPage, field};
use lk_vault::section_body;

/// Per-section cache decision. The pipeline computes one of these for every LLM
/// task it could enqueue.
#[derive(Debug, Clone)]
pub struct SectionDecision {
    /// The request's `cache_hash` (BLAKE3-128 of its cache identity) — written into
    /// the new render's `llm_inputs.<key>` frontmatter regardless of whether the task
    /// is enqueued, so subsequent runs have a reference point.
    pub hash: String,
    /// True when the cached page has this same hash AND the section is filled,
    /// meaning the LLM task is redundant and should be skipped.
    pub cached: bool,
    /// Existing section body to splice back into the freshly rendered page when
    /// `cached`. Stripped of section framing whitespace so it can be handed to
    /// `replace_section`.
    pub preserved_body: Option<String>,
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

/// Decide whether the LLM task identified by `(key, heading, hash)` can be skipped
/// because the existing page already carries an identical hash and a filled body.
/// `heading` is the text after `## ` — the same form `lk_vault::section` functions
/// accept everywhere.
pub fn lookup(
    existing: Option<&VaultPage>,
    key: &str,
    heading: &str,
    hash: String,
) -> SectionDecision {
    let Some(page) = existing else {
        return SectionDecision {
            hash,
            cached: false,
            preserved_body: None,
        };
    };

    if stored_hash(Some(page), key) != Some(&hash) {
        return SectionDecision {
            hash,
            cached: false,
            preserved_body: None,
        };
    }

    let Some(body) = section_body(&page.body, heading) else {
        return SectionDecision {
            hash,
            cached: false,
            preserved_body: None,
        };
    };

    let trimmed = body.trim_matches('\n');
    if trimmed.trim().is_empty() {
        return SectionDecision {
            hash,
            cached: false,
            preserved_body: None,
        };
    }

    SectionDecision {
        hash,
        cached: true,
        preserved_body: Some(trimmed.to_string()),
    }
}

/// Cache decision for an `InPlace` rewrite (see `TargetKind::cache_shape`). The
/// section is structurally non-empty from the render, so — unlike [`lookup`] —
/// emptiness can't gate completion. The sole signal is `completion_key`: the task is
/// cached exactly when `/lore-process` has stamped it equal to the current-input
/// `hash`. On a hit the current (already-rewritten) body is preserved and spliced
/// back over the fresh render.
pub fn lookup_in_place(
    existing: Option<&VaultPage>,
    completion_key: &str,
    heading: &str,
    hash: String,
) -> SectionDecision {
    if stored_hash(existing, completion_key) != Some(&hash) {
        return SectionDecision {
            hash,
            cached: false,
            preserved_body: None,
        };
    }

    // Completion stamp matches the current input → the on-disk body is the refined
    // one; preserve it. Body emptiness is deliberately NOT consulted (the section is
    // structurally non-empty), so — unlike a fill-empty section — clearing the body
    // does not force a re-run. The single re-run lever is deleting the completion
    // line, which drops the stamp and lands us in the uncached branch above.
    let Some(body) = existing.and_then(|p| section_body(&p.body, heading)) else {
        // Stamp says done, but the section heading isn't present — the rendered
        // heading drifted from the one the stamp was written against (e.g. a custom
        // `--template-dir` renamed it). Reporting a hit would freeze the raw,
        // unrefined event list forever (the task is never re-enqueued, and with no
        // preserved body the splice-site drift warning never fires either). Force a
        // re-run and surface it.
        tracing::warn!(
            completion_key,
            heading,
            "in-place section marked done but heading not found (custom-template drift?); re-enqueueing"
        );
        return SectionDecision {
            hash,
            cached: false,
            preserved_body: None,
        };
    };

    SectionDecision {
        hash,
        cached: true,
        preserved_body: Some(body.trim_matches('\n').to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lk_core::frontmatter::parse_page;

    fn page_with(frontmatter: &str, body: &str) -> VaultPage {
        let content = format!("---\n{frontmatter}---\n\n{body}");
        parse_page(&content).expect("test fixture parses")
    }

    #[test]
    fn missing_page_is_uncached() {
        let d = lookup(None, "summary", "요약", "abc".into());
        assert!(d.enqueue());
        assert!(d.preserved_body.is_none());
    }

    #[test]
    fn missing_llm_inputs_frontmatter_is_uncached() {
        let page = page_with("id: x\n", "## 요약\n\nbody\n");
        let d = lookup(Some(&page), "summary", "요약", "abc".into());
        assert!(d.enqueue());
    }

    #[test]
    fn mismatched_hash_is_uncached() {
        let page = page_with(
            "id: x\nllm_inputs:\n  summary: stale\n",
            "## 요약\n\nbody\n",
        );
        let d = lookup(Some(&page), "summary", "요약", "fresh".into());
        assert!(d.enqueue());
    }

    #[test]
    fn matching_hash_with_empty_body_is_uncached() {
        // Section heading present but body empty — user manually cleared it to
        // force a re-run. The empty body must NOT count as cached.
        let page = page_with(
            "id: x\nllm_inputs:\n  summary: abc\n",
            "## 요약\n\n\n## 출처\n",
        );
        let d = lookup(Some(&page), "summary", "요약", "abc".into());
        assert!(d.enqueue());
    }

    #[test]
    fn matching_hash_with_filled_body_hits_cache() {
        let page = page_with(
            "id: x\nllm_inputs:\n  summary: abc\n",
            "## 요약\n\nreal content\n\n## 출처\n",
        );
        let d = lookup(Some(&page), "summary", "요약", "abc".into());
        assert!(!d.enqueue());
        assert_eq!(d.preserved_body.as_deref(), Some("real content"));
    }

    #[test]
    fn whitespace_only_body_is_uncached() {
        let page = page_with(
            "id: x\nllm_inputs:\n  summary: abc\n",
            "## 요약\n\n   \n  \n\n## 출처\n",
        );
        let d = lookup(Some(&page), "summary", "요약", "abc".into());
        assert!(d.enqueue());
    }

    #[test]
    fn in_place_matching_stamp_with_heading_hits_cache() {
        let page = page_with(
            "id: x\nllm_inputs:\n  refine_events: abc\n  refine_events_done: abc\n",
            "## 주요 이벤트\n\n### A\n\nrefined body\n\n## 관련 개념\n",
        );
        let d = lookup_in_place(
            Some(&page),
            "refine_events_done",
            "주요 이벤트",
            "abc".into(),
        );
        assert!(!d.enqueue());
        assert_eq!(d.preserved_body.as_deref(), Some("### A\n\nrefined body"));
    }

    #[test]
    fn in_place_stamp_set_but_heading_missing_is_uncached() {
        // Completion stamp matches, but the events heading drifted (e.g. a custom
        // template renamed it). This must NOT count as cached — otherwise the raw
        // event list would freeze and the refine task would never re-enqueue.
        let page = page_with(
            "id: x\nllm_inputs:\n  refine_events: abc\n  refine_events_done: abc\n",
            "## Key Events\n\n### A\n\nbody\n",
        );
        let d = lookup_in_place(
            Some(&page),
            "refine_events_done",
            "주요 이벤트",
            "abc".into(),
        );
        assert!(
            d.enqueue(),
            "missing heading must force re-enqueue, not a silent hit"
        );
        assert!(d.preserved_body.is_none());
    }

    #[test]
    fn in_place_missing_stamp_is_uncached() {
        let page = page_with(
            "id: x\nllm_inputs:\n  refine_events: abc\n",
            "## 주요 이벤트\n\n### A\n\nbody\n",
        );
        let d = lookup_in_place(
            Some(&page),
            "refine_events_done",
            "주요 이벤트",
            "abc".into(),
        );
        assert!(d.enqueue());
    }

    #[test]
    fn multi_line_body_preserves_internal_blank_lines() {
        let page = page_with(
            "id: x\nllm_inputs:\n  refine_events: abc\n",
            "## 주요 이벤트\n\n### A\n\nbody a\n\n### B\n\nbody b\n\n## 관련 개념\n",
        );
        let d = lookup(Some(&page), "refine_events", "주요 이벤트", "abc".into());
        assert!(!d.enqueue());
        let preserved = d.preserved_body.unwrap();
        assert!(preserved.contains("### A"));
        assert!(preserved.contains("### B"));
        assert!(preserved.contains("body a"));
        assert!(preserved.contains("body b"));
    }
}
