use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

/// A concept the LLM surfaced from a source. The page filename is always
/// `slugify(name)`, so the address a concept link points at is derivable from the
/// name alone — a citation and its page can never disagree on the slug.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedConcept {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
}

/// Normalize an arbitrary name into a path-safe slug.
///
/// Pipeline: **NFKC normalize → lowercase → map every non-alphanumeric char
/// (whitespace, punctuation, including literal `-`) to a separator → collapse runs of
/// separators into a single `-` and trim leading/trailing `-`**.
///
/// NFKC folds compatibility forms first, so composed and decomposed Hangul and
/// full-width characters all reduce to the same canonical slug (e.g. full-width `ＡＢ`
/// and ASCII `AB` both become `ab`). All concept slugs and graph node ids flow through
/// this one rule, so there is exactly one CJK-correct normalization in the workspace.
pub fn slugify(name: &str) -> Option<String> {
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
    if out.is_empty() { None } else { Some(out) }
}

/// The identity a name claims, as opposed to the ADDRESS [`slugify`] writes it at.
///
/// The two answer different questions. A slug has to stay readable as a filename, so it
/// keeps every separator; identity keeps only the separators that mean something. Between
/// letters a break is typography — `Vector DB`, `vector-db` and `vectordb` are one name
/// written three ways, and every defect they cause comes from treating them as three.
/// Between DIGITS a break is the name itself: positional notation makes `3-5` two numerals
/// and `35` one, so `Claude 3.5` and `Claude 35` are different names, as are `GPT-4.1`/
/// `GPT-41` and `Web 2.0`/`web20`. Dropping every separator would fold those together —
/// and version-numbered names are the single most common shape in a technology vault.
///
/// Nothing else is folded. Word order and every character are identity, so `agent-harness`
/// and `harness-agent`, `http` and `https`, `doc-hub` and `docs-hub` are all DIFFERENT
/// names — whether they name the same concept is a question about meaning, which this
/// cannot and must not answer.
///
/// Single-sourced because two consumers must agree exactly: `lk_pipeline`'s alias index
/// resolves an extracted name to the page that owns it, and `lk_graph`'s duplicate lint
/// reports two pages owning one name. The lint only reports, but the index ACTS — it can
/// fold an extraction into an established page — so the fold has to be one no reviewer
/// would overturn.
pub fn identity_key(name: &str) -> Option<String> {
    let slug = slugify(name)?;
    let mut key = String::with_capacity(slug.len());
    let mut rest = slug.chars().peekable();
    while let Some(c) = rest.next() {
        // slugify leaves no leading, trailing or repeated separator, so a `-` always sits
        // between two characters and both sides are readable here.
        if c != '-' {
            key.push(c);
        } else if key.chars().next_back().is_some_and(char::is_numeric)
            && rest.peek().copied().is_some_and(char::is_numeric)
        {
            key.push('-');
        }
    }
    Some(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Claude Code"), Some("claude-code".into()));
        assert_eq!(slugify("GPT-4o"), Some("gpt-4o".into()));
        assert_eq!(slugify("RAG (Retrieval)"), Some("rag-retrieval".into()));
    }

    #[test]
    fn slugify_collapses_and_trims_separators() {
        assert_eq!(slugify("  spaced  out  "), Some("spaced-out".into()));
        assert_eq!(slugify("a---b"), Some("a-b".into()));
        assert_eq!(
            slugify("--leading-and-trailing--"),
            Some("leading-and-trailing".into())
        );
        assert_eq!(slugify("under_score"), Some("under-score".into()));
    }

    #[test]
    fn identity_key_folds_a_break_between_letters() {
        // Everything slugify folds, plus where a break between letters falls.
        let vector_db = identity_key("Vector DB");
        assert_eq!(identity_key("vector-db"), vector_db);
        assert_eq!(identity_key("vectordb"), vector_db);
        assert_eq!(identity_key("VectorDB"), vector_db);
        assert_eq!(
            identity_key("Chain of Thought"),
            identity_key("chain-of-thought")
        );
        assert_eq!(identity_key("ＲＡＧ"), identity_key("rag")); // NFKC full-width
        assert_eq!(identity_key("Vite+"), identity_key("vite"));
        // A break on only ONE side of a digit is still typography.
        assert_eq!(identity_key("GPT-4o"), identity_key("gpt4o"));
        assert_eq!(identity_key("H-2"), identity_key("h2"));
        assert_eq!(identity_key("ISO-8601"), identity_key("iso8601"));
        assert_eq!(identity_key("S3 bucket"), identity_key("s3bucket"));
        assert_eq!(identity_key("---"), None);
    }

    #[test]
    fn identity_key_keeps_a_break_between_digits() {
        // Positional notation: `3-5` is two numerals, `35` is one, so the break IS the
        // name. Version-numbered names are the most common shape in a technology vault,
        // and folding these together would silently route one concept onto the other.
        assert_ne!(identity_key("Claude 3.5"), identity_key("Claude 35"));
        assert_ne!(identity_key("GPT-4.1"), identity_key("GPT-41"));
        assert_ne!(identity_key("Web 2.0"), identity_key("web20"));
        assert_ne!(identity_key("v1-0"), identity_key("v10"));
        // Spelling the same version differently still folds.
        assert_eq!(identity_key("Claude 3.5"), identity_key("claude-3-5"));
        assert_eq!(
            identity_key("Gemini 3.1 Flash"),
            identity_key("gemini-3-1-flash")
        );
    }

    #[test]
    fn identity_key_folds_nothing_about_meaning() {
        // Order and every character are identity; whether two of these name one concept
        // is a question about meaning, which this must not answer.
        assert_ne!(identity_key("agent harness"), identity_key("harness agent"));
        assert_ne!(identity_key("http"), identity_key("https"));
        assert_ne!(identity_key("doc-hub"), identity_key("docs-hub"));
        assert_ne!(identity_key("k8s"), identity_key("kubernetes"));
    }

    #[test]
    fn slugify_returns_none_for_empty() {
        assert_eq!(slugify(""), None);
        assert_eq!(slugify("---"), None);
        assert_eq!(slugify("   "), None);
    }

    #[test]
    fn slugify_nfkc_fullwidth_matches_ascii() {
        // Full-width latin letters fold to ASCII under NFKC, so the slug matches.
        assert_eq!(slugify("\u{ff21}\u{ff22}"), Some("ab".into())); // Ａ Ｂ
        assert_eq!(slugify("\u{ff21}\u{ff22}"), slugify("AB"));
    }

    #[test]
    fn slugify_nfkc_hangul_composed_equals_decomposed() {
        // Composed (precomposed syllable) and decomposed (jamo sequence) Hangul must
        // produce the same slug once NFKC-normalized — the real CJK bug this fixes.
        let composed = "\u{d55c}\u{ae00}"; // 한글, precomposed
        let decomposed = "\u{1112}\u{1161}\u{11ab}\u{1100}\u{1173}\u{11af}"; // ㅎㅏㄴㄱㅡㄹ jamo
        assert_eq!(slugify(composed), slugify(decomposed));
        assert!(slugify(composed).is_some());
    }
}
