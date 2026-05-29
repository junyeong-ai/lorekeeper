//! Document-page `document_type` vocabulary. This is a FORMAT classification, not a
//! content one: the renderer derives it from a source file's extension and `lore
//! schema` lists the same slate in AGENTS.md, so the wiki skills that author document
//! pages by hand emit identical values. A document's subject-matter character (ADR,
//! troubleshooting, project-knowledge, …) belongs in `tags`, never here — keeping
//! `document_type` a small closed format set is what lets `lore wiki index` group
//! documents cleanly.

/// The closed `document_type` slate.
pub const DOCUMENT_TYPES: &[&str] = &["note", "report", "data"];

/// Classify a source file's extension into a `document_type`. Extension-less or
/// unrecognized formats are prose → `note`.
pub fn document_type_for_extension(ext: &str) -> &'static str {
    match ext {
        "html" | "htm" => "report",
        "json" => "data",
        _ => "note",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_maps_to_format_type() {
        assert_eq!(document_type_for_extension("html"), "report");
        assert_eq!(document_type_for_extension("htm"), "report");
        assert_eq!(document_type_for_extension("json"), "data");
        assert_eq!(document_type_for_extension("md"), "note");
        assert_eq!(document_type_for_extension("txt"), "note");
        assert_eq!(document_type_for_extension(""), "note");
    }

    #[test]
    fn slate_covers_every_mapped_value() {
        for ext in ["html", "json", "md", "weird"] {
            assert!(DOCUMENT_TYPES.contains(&document_type_for_extension(ext)));
        }
    }
}
