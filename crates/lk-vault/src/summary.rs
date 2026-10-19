//! Short opening statements shared by search results and daily briefs.

const MAX_SUMMARY_BYTES: usize = 200;

/// Keep the first sentence, bounded at a UTF-8 and word boundary.
pub(crate) fn truncate_summary(s: &str) -> String {
    let s = s.trim();
    let first = first_sentence(s);
    if first.len() <= MAX_SUMMARY_BYTES {
        return first.to_string();
    }
    let mut cut = MAX_SUMMARY_BYTES;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    let head = &s[..cut];
    let head = head.rsplit_once(' ').map_or(head, |(a, _)| a).trim_end();
    format!("{head}…")
}

fn first_sentence(s: &str) -> &str {
    for (idx, ch) in s.char_indices() {
        if matches!(ch, '.' | '?' | '!' | '。') {
            let rest = &s[idx + ch.len_utf8()..];
            if rest.is_empty() || rest.starts_with(char::is_whitespace) {
                return &s[..idx + ch.len_utf8()];
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_first_sentence_without_splitting_decimal_or_domain() {
        assert_eq!(
            truncate_summary("  First sentence. Second sentence.  "),
            "First sentence."
        );
        assert_eq!(
            truncate_summary("Version 1.5 at example.com works! More."),
            "Version 1.5 at example.com works!"
        );
        assert_eq!(
            truncate_summary("첫 문장입니다。 다음 문장입니다。"),
            "첫 문장입니다。"
        );
    }

    #[test]
    fn long_openings_remain_bounded_and_valid_utf8() {
        assert!(truncate_summary(&"가".repeat(200)).len() <= MAX_SUMMARY_BYTES + '…'.len_utf8());
        let opening = format!("{} tail", "word ".repeat(60));
        let summary = truncate_summary(&opening);
        assert!(summary.ends_with("word…"));
        assert!(summary.len() <= MAX_SUMMARY_BYTES + '…'.len_utf8());
    }
}
