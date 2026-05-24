use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedConcept {
    pub name: String,
    pub slug: String,
    pub confidence: Confidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    Extracted,
    Inferred,
}

impl std::fmt::Display for Confidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Extracted => f.write_str("extracted"),
            Self::Inferred => f.write_str("inferred"),
        }
    }
}

/// Normalize an arbitrary name into a path-safe slug.
///
/// Pipeline: **NFKC normalize → lowercase → keep `[alphanumeric + '-']`, mapping every
/// other character (whitespace, punctuation, …) to a separator → collapse runs of
/// separators into a single `-` and trim leading/trailing `-`**.
///
/// NFKC folds compatibility forms first, so composed and decomposed Hangul and
/// full-width characters all reduce to the same canonical slug (e.g. full-width `ＡＢ`
/// and ASCII `AB` both become `ab`). All concept slugs and graph node ids flow through
/// this one rule, so there is exactly one CJK-correct normalization in the workspace.
pub fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut pending_sep = false;
    for c in name.nfkc().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            if pending_sep && !out.is_empty() {
                out.push('-');
            }
            pending_sep = false;
            out.push(c);
        } else {
            // '-', whitespace, and punctuation are all separators; runs collapse to one.
            pending_sep = true;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Claude Code"), "claude-code");
        assert_eq!(slugify("GPT-4o"), "gpt-4o");
        assert_eq!(slugify("RAG (Retrieval)"), "rag-retrieval");
    }

    #[test]
    fn slugify_collapses_and_trims_separators() {
        assert_eq!(slugify("  spaced  out  "), "spaced-out");
        assert_eq!(slugify("a---b"), "a-b");
        assert_eq!(slugify("--leading-and-trailing--"), "leading-and-trailing");
        assert_eq!(slugify("under_score"), "under-score");
    }

    #[test]
    fn slugify_nfkc_fullwidth_matches_ascii() {
        // Full-width latin letters fold to ASCII under NFKC, so the slug matches.
        assert_eq!(slugify("\u{ff21}\u{ff22}"), "ab"); // Ａ Ｂ
        assert_eq!(slugify("\u{ff21}\u{ff22}"), slugify("AB"));
    }

    #[test]
    fn slugify_nfkc_hangul_composed_equals_decomposed() {
        // Composed (precomposed syllable) and decomposed (jamo sequence) Hangul must
        // produce the same slug once NFKC-normalized — the real CJK bug this fixes.
        let composed = "\u{d55c}\u{ae00}"; // 한글, precomposed
        let decomposed = "\u{1112}\u{1161}\u{11ab}\u{1100}\u{1173}\u{11af}"; // ㅎㅏㄴㄱㅡㄹ jamo
        assert_eq!(slugify(composed), slugify(decomposed));
        assert!(!slugify(composed).is_empty());
    }
}
