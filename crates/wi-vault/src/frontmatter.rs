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

    pub fn set(&mut self, key: impl Into<String>, value: impl Into<serde_json::Value>) {
        self.fields.insert(key.into(), value.into());
    }
}

#[derive(Debug, Clone, Default)]
pub struct FrontmatterPatch {
    pub set: BTreeMap<String, serde_json::Value>,
    pub remove: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Page {
    pub frontmatter: Frontmatter,
    pub body: String,
}

pub fn parse_page(content: &str) -> Result<Page, String> {
    let normalized = content.replace("\r\n", "\n");
    let trimmed = normalized.trim_start();

    if !trimmed.starts_with("---") {
        return Ok(Page {
            frontmatter: Frontmatter::default(),
            body: normalized,
        });
    }

    let after_opening = &trimmed[3..];
    let after_opening = after_opening.strip_prefix('\n').unwrap_or(after_opening);

    let closing = after_opening
        .find("\n---")
        .ok_or_else(|| "unclosed frontmatter block".to_string())?;

    let yaml_str = &after_opening[..closing];
    let rest = &after_opening[closing + 4..];
    let body = rest.strip_prefix('\n').unwrap_or(rest);

    let fields: BTreeMap<String, serde_json::Value> =
        serde_yaml_ng::from_str(yaml_str).map_err(|e| format!("invalid frontmatter YAML: {e}"))?;

    Ok(Page {
        frontmatter: Frontmatter { fields },
        body: body.to_string(),
    })
}

pub fn serialize_page(frontmatter: &Frontmatter, body: &str) -> String {
    if frontmatter.fields.is_empty() {
        return body.to_string();
    }
    let yaml = serde_yaml_ng::to_string(&frontmatter.fields).unwrap_or_default();
    format!("---\n{yaml}---\n\n{body}")
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
}
