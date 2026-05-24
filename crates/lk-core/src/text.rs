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
