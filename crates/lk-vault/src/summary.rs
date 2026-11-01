//! Short opening statements shared by search results and daily briefs.

const MAX_SUMMARY_BYTES: usize = 200;

/// Keep the first sentence, bounded at a UTF-8 and word boundary.
pub(crate) fn truncate_summary(s: &str) -> String {
    match opening(s.trim()) {
        (head, true) => format!("{head}…"),
        (first, false) => first.to_string(),
    }
}

/// A bounded preview of `s` that reaches byte `end`, where a match finishes.
///
/// The opening sentence answers when it already reaches that far. Otherwise the window is
/// centred on the match, starts and stops at word boundaries where the text has them, and is
/// marked `…` on each side it clips.
pub(crate) fn truncate_around(s: &str, end: usize) -> String {
    let lead = s.len() - s.trim_start().len();
    let s = s.trim();
    let end = end.saturating_sub(lead).min(s.len());
    if let (shown, clipped) = opening(s)
        && end <= shown.len()
    {
        return if clipped {
            format!("{shown}…")
        } else {
            shown.to_string()
        };
    }
    let mut start = s.floor_char_boundary(end.saturating_sub(MAX_SUMMARY_BYTES / 2));
    if !s[..start].ends_with(char::is_whitespace)
        && let Some(gap) = s[start..end].find(char::is_whitespace)
    {
        start += gap;
    }
    start += s[start..].len() - s[start..].trim_start().len();
    let limit = s.floor_char_boundary(start + MAX_SUMMARY_BYTES);
    let stop = if limit >= s.len() {
        s.len()
    } else if limit <= end {
        limit
    } else {
        s[end..limit]
            .rfind(char::is_whitespace)
            .map_or(limit, |gap| end + gap)
    };
    format!(
        "{}{}{}",
        if start > 0 { "…" } else { "" },
        s[start..stop].trim_end(),
        if stop < s.len() { "…" } else { "" }
    )
}

/// The first sentence of `s`, or its word-bounded head when that sentence exceeds the bound;
/// `true` when the head clipped it.
fn opening(s: &str) -> (&str, bool) {
    let first = first_sentence(s);
    if first.len() <= MAX_SUMMARY_BYTES {
        return (first, false);
    }
    let head = &s[..s.floor_char_boundary(MAX_SUMMARY_BYTES)];
    (
        head.rsplit_once(' ').map_or(head, |(a, _)| a).trim_end(),
        true,
    )
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

    #[test]
    fn a_match_past_the_opening_is_shown_inside_a_bounded_window() {
        let text = format!(
            "Opening sentence. {}Helios retains logs. {}",
            "filler ".repeat(40),
            "tail ".repeat(40)
        );
        let end = text.find("Helios").unwrap() + "Helios".len();
        let excerpt = truncate_around(&text, end);
        assert!(excerpt.starts_with("…filler"), "{excerpt}");
        assert!(excerpt.contains("Helios retains logs."), "{excerpt}");
        assert!(excerpt.ends_with("tail…"), "{excerpt}");
        assert!(excerpt.len() <= MAX_SUMMARY_BYTES + 2 * '…'.len_utf8());
        assert_eq!(truncate_around(&text, "Opening".len()), "Opening sentence.");
    }

    #[test]
    fn a_window_without_word_boundaries_stays_valid_utf8() {
        let text = "가".repeat(300);
        let excerpt = truncate_around(&text, 600);
        assert!(excerpt.starts_with('…') && excerpt.ends_with('…'));
        assert!(excerpt.len() <= MAX_SUMMARY_BYTES + 2 * '…'.len_utf8());
    }
}
