use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Frontmatter {
    #[serde(flatten)]
    pub fields: BTreeMap<String, serde_json::Value>,
}

impl Frontmatter {
    pub fn get(&self, key: &str) -> Option<&serde_json::Value> {
        self.fields.get(key)
    }
}

#[derive(Debug, Clone)]
pub struct Page {
    pub frontmatter: Frontmatter,
    pub body: String,
}

/// Parse a markdown page into its YAML frontmatter and body.
///
/// Frontmatter is recognized only when the document's FIRST line is exactly `---`
/// and a later line is exactly `---` (delimiters must stand alone on their line).
/// This avoids two false detections a substring scan would make: a body that merely
/// begins with whitespace then `---`, and a `---` appearing inside a YAML value or a
/// `---not-a-delimiter` line. CRLF is normalized to LF up front.
pub fn parse_page(content: &str) -> Result<Page, String> {
    let normalized = content.replace("\r\n", "\n");

    let Some(rest) = normalized.strip_prefix("---\n") else {
        return Ok(Page {
            frontmatter: Frontmatter::default(),
            body: normalized,
        });
    };

    // Locate the closing delimiter: a line equal to exactly "---".
    let mut offset = 0usize;
    let mut closing = None;
    for line in rest.split_inclusive('\n') {
        if line.strip_suffix('\n').unwrap_or(line) == "---" {
            closing = Some(offset);
            break;
        }
        offset += line.len();
    }
    let Some(closing) = closing else {
        return Err("unclosed frontmatter block".to_string());
    };

    let yaml_str = &rest[..closing];
    let after_delim = &rest[closing..];
    let body = after_delim
        .strip_prefix("---\n")
        .or_else(|| after_delim.strip_prefix("---"))
        .unwrap_or(after_delim);
    let body = body.strip_prefix('\n').unwrap_or(body);

    let fields: BTreeMap<String, serde_json::Value> =
        serde_yaml_ng::from_str(yaml_str).map_err(|e| format!("invalid frontmatter YAML: {e}"))?;

    Ok(Page {
        frontmatter: Frontmatter { fields },
        body: body.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let original = "---\nid: test-123\ntitle: \"Hello World\"\nlabels:\n- ai\n- ml\n---\n\n# Body\n\nContent here.\n";
        let page = parse_page(original).unwrap();
        assert_eq!(
            page.frontmatter.get("id").and_then(|v| v.as_str()),
            Some("test-123")
        );
        assert!(page.body.contains("# Body"));
    }

    #[test]
    fn no_frontmatter() {
        let page = parse_page("# Just a heading\n\nContent.").unwrap();
        assert!(page.frontmatter.fields.is_empty());
        assert!(page.body.contains("Just a heading"));
    }

    #[test]
    fn crlf_normalized() {
        let crlf = "---\r\nid: test\r\ntitle: \"CRLF\"\r\n---\r\n\r\n# Body\r\n\r\nContent.\r\n";
        let page = parse_page(crlf).unwrap();
        assert_eq!(
            page.frontmatter.get("id").and_then(|v| v.as_str()),
            Some("test")
        );
        assert!(!page.body.contains('\r'));
        assert!(page.body.contains("# Body"));
    }

    #[test]
    fn crlf_without_frontmatter() {
        let page = parse_page("# Title\r\n\r\nContent.\r\n").unwrap();
        assert!(page.frontmatter.fields.is_empty());
        assert!(!page.body.contains('\r'));
    }

    #[test]
    fn leading_whitespace_before_dashes_is_not_frontmatter() {
        // A body that merely starts with whitespace then `---` must not be parsed
        // as frontmatter.
        let page = parse_page("  \n---\nnot: frontmatter\n").unwrap();
        assert!(page.frontmatter.fields.is_empty());
        assert!(page.body.contains("not: frontmatter"));
    }

    #[test]
    fn dashes_inside_yaml_value_do_not_close_early() {
        // An indented `---` inside a block scalar is not a standalone delimiter.
        let doc = "---\nnote: |\n  ---\n  still in yaml\nid: x\n---\n\nbody\n";
        let page = parse_page(doc).unwrap();
        assert_eq!(
            page.frontmatter.get("id").and_then(|v| v.as_str()),
            Some("x")
        );
        assert_eq!(page.body, "body\n");
    }

    #[test]
    fn unclosed_frontmatter_errors() {
        assert!(parse_page("---\nid: x\nno closing delimiter\n").is_err());
    }
}
