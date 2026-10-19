//! Recognize only machine-rendered Sources bodies. This describes presentation ownership,
//! never citation authority: current evidence always comes from source links in backlinks.
use std::path::Path;

use lk_core::i18n::Locale;
use lk_core::link;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SourcesBody {
    /// An exact legacy generated list, whose complete prior addresses can support a diff.
    Legacy(Vec<String>),
    Compact,
    Authored,
}

pub(crate) fn render(locale: Locale) -> &'static str {
    match locale {
        Locale::Ko => {
            "<!-- lorekeeper:concept-sources:v1 -->\n\n전체 출처는 `lore wiki prepare`로 스냅샷 ID를 받은 다음 `lore wiki citations EXACT_CONCEPT_PATH --snapshot ID`로 조회합니다."
        }
        Locale::En => {
            "<!-- lorekeeper:concept-sources:v1 -->\n\nRun `lore wiki prepare` for a snapshot ID, then `lore wiki citations EXACT_CONCEPT_PATH --snapshot ID` for complete source citations."
        }
    }
}

/// Permit the empty scaffold and known compact bodies. A legacy body must round-trip
/// every complete line through the shared link parser and exact old renderer grammar,
/// including relative-address encoding, escaped labels and sorted unique addresses.
/// Mixed prose, code-wrapped links, indentation, link titles and anchors are authored
/// content; inline-code text inside an emitted link's label remains part of that label.
pub(crate) fn classify(body: &str, concept: &Path) -> SourcesBody {
    if body.trim().is_empty() {
        return SourcesBody::Legacy(Vec::new());
    }
    let lines = body.lines().collect::<Vec<_>>();
    let first = lines
        .iter()
        .position(|line| !line.is_empty())
        .expect("nonblank body");
    let last = lines
        .iter()
        .rposition(|line| !line.is_empty())
        .expect("nonblank body");
    let lines = &lines[first..=last];
    let text = lines.join("\n");
    if Locale::ALL.iter().any(|locale| render(*locale) == text) {
        return SourcesBody::Compact;
    }
    let mut sources = Vec::new();
    for line in lines {
        let Some(value) = line
            .strip_prefix("- ")
            .and_then(link::parse_standalone_page_link)
        else {
            return SourcesBody::Authored;
        };
        let Some(path) = link::resolve_dest(concept, &value.dest)
            .filter(|path| path.extension().is_some_and(|extension| extension == "md"))
        else {
            return SourcesBody::Authored;
        };
        let canonical = format!(
            "- {}",
            link::md_link(&value.text, &link::relative_dest(concept, &path))
        );
        let source = crate::scan::page_id(&path);
        if canonical != *line || sources.last().is_some_and(|previous| previous >= &source) {
            return SourcesBody::Authored;
        }
        sources.push(source);
    }
    SourcesBody::Legacy(sources)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_generated_labels_can_contain_inline_code_without_claiming_code_blocks() {
        let concept = Path::new("wiki/concepts/topic.md");
        for label in [
            "MCP `JSON-RPC` transport",
            "`gate` [validation]",
            "``nested ` code``",
        ] {
            let generated = format!("- {}", link::md_link(label, "../../daily/report.md"));
            assert_eq!(
                classify(&generated, concept),
                SourcesBody::Legacy(vec!["daily/report".into()])
            );
            for authored in [
                format!("{generated}\nAuthored explanation."),
                format!("```markdown\n{generated}\n```"),
                format!("- `{}`", link::md_link(label, "../../daily/report.md")),
            ] {
                assert_eq!(
                    classify(&authored, concept),
                    SourcesBody::Authored,
                    "{authored}"
                );
            }
        }
    }

    #[test]
    fn ownership_requires_exact_generated_structure_in_every_locale() {
        let concept = Path::new("wiki/concepts/nested/topic.md");
        let first = "- [Source \\[one\\]](../../../daily/a%20b.md)";
        let second = "- [Other](../../../daily/z.md)";
        assert_eq!(
            classify(&format!("\n{first}\n{second}\n\n"), concept),
            SourcesBody::Legacy(vec!["daily/a b".into(), "daily/z".into()])
        );
        assert_eq!(classify("\n\n", concept), SourcesBody::Legacy(Vec::new()));
        for locale in Locale::ALL {
            assert_eq!(
                classify(&format!("\n{}\n\n", render(*locale)), concept),
                SourcesBody::Compact
            );
            assert_eq!(
                classify(&render(*locale).replace('\n', "\r\n"), concept),
                SourcesBody::Compact
            );
        }
        for authored in [
            format!("  {first}"),
            format!("{first}  "),
            format!("{first}\n\n{second}"),
            format!("{first}\n{first}"),
            format!("{second}\n{first}"),
            format!("{first}\nAuthored explanation."),
            format!("```markdown\n{first}\n```"),
            "- [Source](../../../daily/a.md#section)".into(),
            "- [Source](../../../daily/a.md \"Authored title\")".into(),
            "- [Source](../../../daily/./a.md)".into(),
            "- [Source](https://example.test/a.md)".into(),
            "<!-- lorekeeper:concept-sources:v1 -->\nAuthored explanation.".into(),
            format!("{}\nAuthored explanation.", render(Locale::En)),
        ] {
            assert_eq!(
                classify(&authored, concept),
                SourcesBody::Authored,
                "{authored}"
            );
        }
    }
}
