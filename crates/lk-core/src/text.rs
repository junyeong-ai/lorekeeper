/// A character that can be part of the same keyword token as a matched needle —
/// the standard `\w` word-character set: ASCII alphanumerics plus `_`. Hyphens,
/// dots, and other punctuation are treated as token boundaries, so a keyword
/// matches inside a compound: `AI` matches `AI-powered`, `GPT` matches `GPT-4`,
/// `node` matches `node.js`. The original false positive this guards against
/// (`AI` inside `FAIR`) is still rejected by the alphanumeric boundary alone.
/// CJK scripts are deliberately NOT identifier characters: unlike space-delimited
/// Latin text, agglutinative Korean writes content morphemes with no separator
/// (the particle in "검토를"), so treating an adjacent Hangul syllable as "same
/// token" would make every particle/affix suppress a real keyword match. Excluding
/// CJK means a CJK keyword matches as a substring (correct for morpheme-joined
/// text) while ASCII keeps strict token boundaries.
fn is_identifier_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Substring match that requires the needle to NOT be flanked by ASCII identifier
/// characters on either side. Prevents the false positives a plain `contains`
/// produces in Latin text — keyword "AI" matching "FAIR" — while a CJK needle (no
/// ASCII boundary chars around it) matches as a substring, so Korean keywords
/// match across attached particles ("검토" in "검토를", "재검토"). CJK has no word
/// boundary to anchor against, so a short CJK keyword behaves like `contains` and also
/// matches inside a larger compound ("검토" in "미검토") — configure CJK keywords
/// specific enough that this is the intended grouping.
///
/// Single source for keyword matching: `lk-pipeline::classify` matches events with
/// it, and `Config::validate` proves classify-rule reachability with it — the
/// boundary semantics can never drift between the two.
///
/// The match is transitive across containment: if `a` bounded-contains `b` and `b`
/// bounded-contains `c`, then `a` bounded-contains `c` — the flanking characters of
/// the inner occurrence come either from `b` itself (non-identifier by `b`'s match)
/// or from `a` at `b`'s edges (non-identifier by `a`'s match). Rule-shadowing
/// validation relies on this implication.
pub fn contains_bounded(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let mut from = 0;
    while let Some(rel) = haystack[from..].find(needle) {
        let start = from + rel;
        let end = start + needle.len();
        let before_ok = haystack[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !is_identifier_char(c));
        let after_ok = haystack[end..]
            .chars()
            .next()
            .is_none_or(|c| !is_identifier_char(c));
        if before_ok && after_ok {
            return true;
        }
        from = start + needle.chars().next().map_or(1, char::len_utf8);
    }
    false
}

/// Squeeze 3+ consecutive newlines down to a paragraph break so converted bodies
/// don't accumulate excess vertical whitespace in vault pages. Carriage returns
/// are stripped so `\r\n` sequences are treated as a single `\n`.
pub fn collapse_blank_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut newlines = 0;
    for ch in text.chars() {
        if ch == '\r' {
            continue;
        }
        if ch == '\n' {
            newlines += 1;
            if newlines <= 2 {
                out.push(ch);
            }
        } else {
            newlines = 0;
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_rejects_inner_substring() {
        // "ai" inside "fair"/"mail" is flanked by identifier chars — not a token.
        assert!(!contains_bounded("fair conference", "ai"));
        assert!(!contains_bounded("check your mail", "ai"));
        // "ml" at the end of "html" is preceded by an identifier char.
        assert!(!contains_bounded("html runtime", "ml"));
    }

    #[test]
    fn bounded_matches_whole_token_and_compounds() {
        assert!(contains_bounded("ai research", "ai"));
        assert!(contains_bounded("ai-powered platform", "ai"));
        assert!(contains_bounded("gpt-4 launch", "gpt"));
        assert!(contains_bounded("node.js runtime", "node"));
    }

    #[test]
    fn cjk_needle_matches_as_substring() {
        // CJK chars are not identifier chars, so morpheme-joined text matches.
        assert!(contains_bounded("재검토가 필요합니다", "검토"));
        assert!(contains_bounded("검토를 부탁드립니다", "검토"));
    }

    #[test]
    fn empty_needle_never_matches() {
        assert!(!contains_bounded("anything", ""));
    }

    #[test]
    fn containment_implication_holds_at_edges() {
        // The transitivity contract rule-shadowing relies on: a needle bounded
        // inside a longer keyword stays bounded in any text that keyword matches.
        assert!(contains_bounded("ai ethics", "ai")); // needle at keyword start
        assert!(contains_bounded("applied ai", "ai")); // needle at keyword end
    }

    #[test]
    fn preserves_single_blank_line() {
        assert_eq!(collapse_blank_lines("a\n\nb"), "a\n\nb");
    }

    #[test]
    fn collapses_triple_newlines() {
        assert_eq!(collapse_blank_lines("a\n\n\nb"), "a\n\nb");
    }

    #[test]
    fn collapses_many_newlines() {
        assert_eq!(collapse_blank_lines("a\n\n\n\n\nb"), "a\n\nb");
    }

    #[test]
    fn handles_crlf() {
        assert_eq!(collapse_blank_lines("a\r\n\r\n\r\nb"), "a\n\nb");
    }

    #[test]
    fn strips_bare_cr() {
        assert_eq!(collapse_blank_lines("a\r\rb"), "ab");
    }
}
