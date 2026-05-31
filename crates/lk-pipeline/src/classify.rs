use lk_core::event::Event;

pub fn assign_labels(events: &mut [Event], labels: &[String]) {
    for event in events {
        for label in labels {
            if !event.labels.contains(label) {
                event.labels.push(label.clone());
            }
        }
    }
}

/// Promote the adapter's authorship signal to a tracked personal event. Ownership
/// is decided at the source (where the structured author/assignee/organizer fields
/// live) and carried on `Event::is_self`; here it is gated by the source's
/// `track_personal` so only opted-in sources feed the work-log and performance
/// reviews. No text matching — a recipient, CC, or mention is never the author.
pub fn mark_personal(events: &mut [Event], track_personal: bool) {
    if !track_personal {
        return;
    }
    for event in events {
        if event.is_self {
            event.is_personal = true;
            if !event.labels.iter().any(|l| l == "personal") {
                event.labels.push("personal".into());
            }
        }
    }
}

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
/// match across attached particles ("검토" in "검토를", "재검토").
fn contains_bounded(haystack: &str, needle: &str) -> bool {
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

pub fn classify_by_keywords(events: &mut [Event], rules: &[lk_core::config::ClassifyRule]) {
    if rules.is_empty() {
        return;
    }

    // Lowercase each rule's non-blank keywords once up front rather than per event.
    let prepared: Vec<(&lk_core::config::ClassifyRule, Vec<String>)> = rules
        .iter()
        .map(|rule| {
            let keywords = rule
                .keywords
                .iter()
                .filter(|kw| !kw.trim().is_empty())
                .map(|kw| kw.to_lowercase())
                .collect();
            (rule, keywords)
        })
        .collect();

    for event in events {
        if event.classification.is_some() {
            continue;
        }

        let text = format!("{} {}", event.title, event.body).to_lowercase();

        for (rule, keywords) in &prepared {
            let matched = keywords.iter().any(|kw| contains_bounded(&text, kw));

            if matched {
                event.classification = Some(rule.category.clone());
                // A rule may also bridge to the performance taxonomy. Only set it
                // when the rule opts in; otherwise leave it None so `resolve_category`
                // falls back to the per-source-type map.
                if let Some(wc) = &rule.work_category {
                    event.performance_category = Some(wc.clone());
                }
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lk_core::config::SourceType;
    use lk_core::event::EventId;

    fn make_event(title: &str) -> Event {
        Event {
            id: EventId::new("test", jiff::civil::date(2026, 5, 23), title),
            source_id: "test".into(),
            source_type: SourceType::Gmail,
            date: jiff::civil::date(2026, 5, 23),
            title: title.into(),
            body: String::new(),
            url: None,
            author: None,
            labels: vec![],
            classification: None,
            performance_category: None,
            is_self: false,
            is_personal: false,
            content_hash: lk_core::event::content_hash(title, ""),
            metadata: serde_json::Value::Null,
        }
    }

    #[test]
    fn static_labels() {
        let mut events = vec![make_event("Test")];
        assign_labels(&mut events, &["ai-industry".into()]);
        assert_eq!(events[0].labels, vec!["ai-industry"]);
    }

    #[test]
    fn personal_follows_adapter_ownership_when_tracked() {
        let mut events = vec![make_event("Mine"), make_event("Theirs")];
        events[0].is_self = true;
        mark_personal(&mut events, true);
        assert!(events[0].is_personal);
        assert!(events[0].labels.iter().any(|l| l == "personal"));
        assert!(!events[1].is_personal);
        assert!(events[1].labels.is_empty());
    }

    #[test]
    fn personal_not_marked_when_tracking_off() {
        let mut events = vec![make_event("Mine")];
        events[0].is_self = true;
        mark_personal(&mut events, false);
        assert!(!events[0].is_personal);
        assert!(events[0].labels.is_empty());
    }

    #[test]
    fn keyword_classification() {
        let rules = vec![
            lk_core::config::ClassifyRule {
                category: "action_required".into(),
                keywords: vec!["please review".into(), "검토 요청".into()],
                work_category: None,
            },
            lk_core::config::ClassifyRule {
                category: "decisions".into(),
                keywords: vec!["approved".into()],
                work_category: None,
            },
        ];
        let mut events = vec![make_event("Please review this PR")];
        classify_by_keywords(&mut events, &rules);
        assert_eq!(events[0].classification.as_deref(), Some("action_required"));
    }

    #[test]
    fn classification_and_performance_bridge_are_separate_axes() {
        // A rule with a work_category bridge sets BOTH the daily-grouping
        // `classification` and the performance `performance_category`; a rule
        // without the bridge sets only `classification`.
        let rules = vec![
            lk_core::config::ClassifyRule {
                category: "decisions".into(),
                keywords: vec!["approved".into()],
                work_category: Some("technical-leadership".into()),
            },
            lk_core::config::ClassifyRule {
                category: "knowledge_sharing".into(),
                keywords: vec!["fyi".into()],
                work_category: None,
            },
        ];
        let mut bridged = vec![make_event("Change approved")];
        classify_by_keywords(&mut bridged, &rules);
        assert_eq!(bridged[0].classification.as_deref(), Some("decisions"));
        assert_eq!(
            bridged[0].performance_category.as_deref(),
            Some("technical-leadership"),
            "an opted-in rule bridges to the performance taxonomy"
        );

        let mut grouping_only = vec![make_event("FYI: new doc")];
        classify_by_keywords(&mut grouping_only, &rules);
        assert_eq!(
            grouping_only[0].classification.as_deref(),
            Some("knowledge_sharing")
        );
        assert!(
            grouping_only[0].performance_category.is_none(),
            "a grouping-only rule leaves performance_category for the source-type fallback"
        );
    }

    #[test]
    fn keyword_classification_rejects_substring_false_positives() {
        let rules = vec![lk_core::config::ClassifyRule {
            category: "ai_topic".into(),
            keywords: vec!["AI".into()],
            work_category: None,
        }];

        // "FAIR" and "MAIL" contain "AI" as a substring but not as a token.
        let mut events = vec![
            make_event("FAIR conference recap"),
            make_event("Check your MAIL inbox"),
        ];
        classify_by_keywords(&mut events, &rules);
        assert!(
            events[0].classification.is_none(),
            "FAIR must not match keyword AI"
        );
        assert!(
            events[1].classification.is_none(),
            "MAIL must not match keyword AI"
        );

        // Standalone "AI" as a whole token matches.
        let mut events2 = vec![make_event("AI research update")];
        classify_by_keywords(&mut events2, &rules);
        assert_eq!(events2[0].classification.as_deref(), Some("ai_topic"));
    }

    #[test]
    fn keyword_matches_inside_hyphen_and_dot_compounds() {
        // Hyphens/dots are token boundaries, so a keyword matches inside a compound —
        // the common shape in tech text (`AI-powered`, `GPT-4`, `node.js`).
        let rules = vec![lk_core::config::ClassifyRule {
            category: "ai_topic".into(),
            keywords: vec!["AI".into(), "GPT".into(), "node".into()],
            work_category: None,
        }];
        for title in ["AI-powered platform", "GPT-4 launch", "node.js runtime"] {
            let mut events = vec![make_event(title)];
            classify_by_keywords(&mut events, &rules);
            assert_eq!(
                events[0].classification.as_deref(),
                Some("ai_topic"),
                "keyword must match inside compound {title:?}"
            );
        }
    }

    #[test]
    fn cjk_keyword_matches_across_attached_particles() {
        let rules = vec![lk_core::config::ClassifyRule {
            category: "action_required".into(),
            keywords: vec!["검토".into()],
            work_category: None,
        }];
        // "검토" must classify "검토를"/"재검토"/"검토중" — the particle/affix is part
        // of a different morpheme, not the same Latin-style token.
        for title in ["검토를 부탁드립니다", "재검토가 필요합니다", "검토중입니다"]
        {
            let mut events = vec![make_event(title)];
            classify_by_keywords(&mut events, &rules);
            assert_eq!(
                events[0].classification.as_deref(),
                Some("action_required"),
                "keyword must match in {title:?}"
            );
        }
    }
}
