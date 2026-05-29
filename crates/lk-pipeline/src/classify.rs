use lk_core::config::Identity;
use lk_core::event::Event;

pub fn assign_static_labels(events: &mut [Event], labels: &[String]) {
    for event in events {
        for label in labels {
            if !event.labels.contains(label) {
                event.labels.push(label.clone());
            }
        }
    }
}

pub fn flag_personal(events: &mut [Event], identity: &Identity) {
    // Empty/whitespace identity tokens would make matching degenerate (a blank or
    // all-space needle), flagging unrelated events as personal. Drop them up front.
    let nonblank = |s: String| if s.trim().is_empty() { None } else { Some(s) };
    let email = nonblank(identity.email.to_lowercase());
    let name = nonblank(identity.name.to_lowercase());
    let slack_id = identity
        .slack_id
        .as_deref()
        .map(str::to_lowercase)
        .and_then(nonblank);
    let jira_id = identity
        .jira_id
        .as_deref()
        .map(str::to_lowercase)
        .and_then(nonblank);

    for event in events {
        let is_self = event
            .metadata
            .get("is_self")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let author = event.author.as_deref().unwrap_or_default().to_lowercase();
        let meta = event.metadata.to_string().to_lowercase();

        let matched = is_self
            || email
                .as_deref()
                .is_some_and(|e| contains_bounded(&author, e) || contains_bounded(&meta, e))
            || name
                .as_deref()
                .is_some_and(|n| contains_bounded(&author, n))
            || slack_id
                .as_deref()
                .is_some_and(|sid| contains_bounded(&author, sid) || contains_bounded(&meta, sid))
            || jira_id
                .as_deref()
                .is_some_and(|jid| contains_bounded(&author, jid) || contains_bounded(&meta, jid));

        if matched {
            event.is_personal = true;
            if !event.labels.contains(&"personal".to_string()) {
                event.labels.push("personal".into());
            }
        }
    }
}

/// A character that can be part of the same identifier token (name, email, Slack/
/// Jira id) as a matched needle. **ASCII only** — alphanumerics plus the punctuation
/// that appears *inside* emails and usernames. CJK scripts are deliberately NOT
/// identifier characters: unlike space-delimited Latin text, agglutinative Korean
/// writes content morphemes with no separator (the particle in "검토를", the honorific
/// in "이준영님"), so treating an adjacent Hangul syllable as "same token" would make
/// every particle/honorific suppress a real match. Excluding CJK means a CJK needle
/// matches as a substring (correct for morpheme-joined text) while ASCII keeps strict
/// token boundaries. `@` is excluded so a Slack `<@U123>` mention still matches a user
/// id; `.` already blocks the `test@example.com` vs `...com.au` case.
fn is_identifier_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '%' | '+' | '-')
}

/// Substring match that requires the needle to NOT be flanked by ASCII identifier
/// characters on either side. Prevents the false positives a plain `contains`
/// produces in Latin text — name "kim" matching "kimberly", keyword "AI" matching
/// "FAIR", email "test@example.com" matching "test@example.com.au" — while a CJK
/// needle (no ASCII boundary chars around it) matches as a substring, so Korean
/// keywords/names match across attached particles ("검토" in "검토를", "이준영" in
/// "이준영님").
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

    for event in events {
        if event.work_category.is_some() {
            continue;
        }

        let text = format!("{} {}", event.title, event.body).to_lowercase();

        for rule in rules {
            let matched = rule
                .keywords
                .iter()
                .filter(|kw| !kw.trim().is_empty())
                .any(|kw| contains_bounded(&text, &kw.to_lowercase()));

            if matched {
                event.work_category = Some(rule.category.clone());
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

    fn make_event(title: &str, author: Option<&str>) -> Event {
        Event {
            id: EventId::new("test", jiff::civil::date(2026, 5, 23), title),
            source_id: "test".into(),
            source_type: SourceType::Gmail,
            date: jiff::civil::date(2026, 5, 23),
            title: title.into(),
            body: String::new(),
            url: None,
            author: author.map(String::from),
            labels: vec![],
            work_category: None,
            is_personal: false,
            content_hash: lk_core::event::content_hash(title, ""),
            metadata: serde_json::Value::Null,
        }
    }

    #[test]
    fn static_labels() {
        let mut events = vec![make_event("Test", None)];
        assign_static_labels(&mut events, &["ai-industry".into()]);
        assert_eq!(events[0].labels, vec!["ai-industry"]);
    }

    #[test]
    fn personal_by_email() {
        let identity = Identity {
            name: "Test User".into(),
            email: "test@example.com".into(),
            slack_id: None,
            jira_id: None,
        };
        let mut events = vec![make_event("Hello", Some("test@example.com"))];
        flag_personal(&mut events, &identity);
        assert!(events[0].is_personal);
    }

    #[test]
    fn name_substring_does_not_false_positive() {
        let identity = Identity {
            name: "Kim".into(),
            email: "kim@example.com".into(),
            slack_id: None,
            jira_id: None,
        };
        // "Kimberly" must NOT be flagged as Kim's personal work.
        let mut events = vec![make_event("Status", Some("Kimberly Park"))];
        flag_personal(&mut events, &identity);
        assert!(!events[0].is_personal);

        // Standalone "Kim" as a whole token must still match.
        let mut events2 = vec![make_event("Status", Some("Kim"))];
        flag_personal(&mut events2, &identity);
        assert!(events2[0].is_personal);
    }

    #[test]
    fn slack_id_matches_next_to_sigil() {
        let identity = Identity {
            name: "X".into(),
            email: "x@y.com".into(),
            slack_id: Some("U0123456789".into()),
            jira_id: None,
        };
        // A Slack user id adjacent to the `@` mention sigil must still match — `@`
        // is not treated as part of the identifier token.
        let mut events = vec![make_event("hi", Some("<@U0123456789>"))];
        flag_personal(&mut events, &identity);
        assert!(events[0].is_personal);
    }

    #[test]
    fn email_does_not_match_longer_domain() {
        let identity = Identity {
            name: "X".into(),
            email: "test@example.com".into(),
            slack_id: None,
            jira_id: None,
        };
        // A different address that merely starts with the identity email.
        let mut events = vec![make_event("Msg", Some("test@example.com.au"))];
        flag_personal(&mut events, &identity);
        assert!(!events[0].is_personal);

        // The exact address (delimited by angle brackets) still matches.
        let mut events2 = vec![make_event("Msg", Some("Foo <test@example.com>"))];
        flag_personal(&mut events2, &identity);
        assert!(events2[0].is_personal);
    }

    #[test]
    fn cjk_name_matches_across_attached_particles() {
        let identity = Identity {
            name: "이준영".into(),
            email: "e@x.com".into(),
            slack_id: None,
            jira_id: None,
        };
        // Korean attaches honorifics/particles with no separator; the name must still
        // match (the ASCII token-boundary model would wrongly miss these).
        for author in ["이준영", "이준영님", "이준영님께", "이준영이", "이준영의"]
        {
            let mut events = vec![make_event("회의", Some(author))];
            flag_personal(&mut events, &identity);
            assert!(events[0].is_personal, "name must match in {author:?}");
        }
        // A different name that does not contain the identity name must not match.
        let mut other = vec![make_event("회의", Some("박서준 보고"))];
        flag_personal(&mut other, &identity);
        assert!(!other[0].is_personal);
    }

    #[test]
    fn cjk_keyword_matches_across_attached_particles() {
        let rules = vec![lk_core::config::ClassifyRule {
            category: "action_required".into(),
            keywords: vec!["검토".into()],
        }];
        // "검토" must classify "검토를"/"재검토"/"검토중" — the particle/affix is part
        // of a different morpheme, not the same Latin-style token.
        for title in ["검토를 부탁드립니다", "재검토가 필요합니다", "검토중입니다"]
        {
            let mut events = vec![make_event(title, None)];
            classify_by_keywords(&mut events, &rules);
            assert_eq!(
                events[0].work_category.as_deref(),
                Some("action_required"),
                "keyword must match in {title:?}"
            );
        }
    }

    #[test]
    fn empty_identity_does_not_flag_everything() {
        let identity = Identity {
            name: String::new(),
            email: String::new(),
            slack_id: Some(String::new()),
            jira_id: None,
        };
        let mut events = vec![make_event("Random news", Some("someone@else.com"))];
        flag_personal(&mut events, &identity);
        assert!(
            !events[0].is_personal,
            "blank identity tokens must not match every event"
        );
    }

    #[test]
    fn keyword_classification() {
        let rules = vec![
            lk_core::config::ClassifyRule {
                category: "action_required".into(),
                keywords: vec!["please review".into(), "검토 요청".into()],
            },
            lk_core::config::ClassifyRule {
                category: "decisions".into(),
                keywords: vec!["approved".into()],
            },
        ];
        let mut events = vec![make_event("Please review this PR", None)];
        classify_by_keywords(&mut events, &rules);
        assert_eq!(events[0].work_category.as_deref(), Some("action_required"));
    }

    #[test]
    fn keyword_classification_rejects_substring_false_positives() {
        let rules = vec![lk_core::config::ClassifyRule {
            category: "ai_topic".into(),
            keywords: vec!["AI".into()],
        }];

        // "FAIR" and "MAIL" contain "AI" as a substring but not as a token.
        let mut events = vec![
            make_event("FAIR conference recap", None),
            make_event("Check your MAIL inbox", None),
        ];
        classify_by_keywords(&mut events, &rules);
        assert!(
            events[0].work_category.is_none(),
            "FAIR must not match keyword AI"
        );
        assert!(
            events[1].work_category.is_none(),
            "MAIL must not match keyword AI"
        );

        // Standalone "AI" as a whole token matches.
        let mut events2 = vec![make_event("AI research update", None)];
        classify_by_keywords(&mut events2, &rules);
        assert_eq!(events2[0].work_category.as_deref(), Some("ai_topic"));
    }
}
