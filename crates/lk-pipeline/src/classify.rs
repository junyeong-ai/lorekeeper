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
        let author = event.author.as_deref().unwrap_or_default().to_lowercase();
        let meta = event.metadata.to_string().to_lowercase();

        let matched = email
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
/// Jira id) as a matched needle. Includes alphanumerics (Hangul/CJK included via
/// `is_alphanumeric`) plus the punctuation that appears *inside* emails and
/// usernames. `@` is deliberately excluded: it is part of an email but also a
/// mention sigil (`<@U123>`), and `.` alone already blocks the only false positive
/// it guarded (`test@example.com` inside `test@example.com.au`), so excluding it
/// avoids a false negative on `@`-prefixed identifiers without reintroducing that.
fn is_identifier_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '.' | '_' | '%' | '+' | '-')
}

/// Substring match that requires the needle to NOT be flanked by identifier
/// characters on either side — a language-agnostic token boundary. Prevents the
/// false positives a plain `contains` produces: name "kim" matching "kimberly",
/// "이준영" matching "이준영팀" (that person's *team*), or email
/// "test@example.com" matching "test@example.com.au" (a different domain).
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

pub fn classify_by_keywords(
    events: &mut [Event],
    classify: &std::collections::BTreeMap<String, Vec<String>>,
) {
    if classify.is_empty() {
        return;
    }

    for event in events {
        if event.classification.is_some() {
            continue;
        }

        let text = format!("{} {}", event.title, event.body).to_lowercase();

        for (category, keywords) in classify {
            let matched = keywords
                .iter()
                .filter(|kw| !kw.trim().is_empty())
                .any(|kw| text.contains(&kw.to_lowercase()));

            if matched {
                event.classification = Some(category.clone());
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
            classification: None,
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
    fn cjk_name_does_not_match_team_suffix() {
        let identity = Identity {
            name: "이준영".into(),
            email: "e@x.com".into(),
            slack_id: None,
            jira_id: None,
        };
        // "이준영팀" (Lee Junyeong's *team*) is not the person's own work.
        let mut events = vec![make_event("회의", Some("이준영팀"))];
        flag_personal(&mut events, &identity);
        assert!(!events[0].is_personal);

        let mut events2 = vec![make_event("회의", Some("이준영"))];
        flag_personal(&mut events2, &identity);
        assert!(events2[0].is_personal);
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
        let mut classify = std::collections::BTreeMap::new();
        classify.insert(
            "action_required".to_string(),
            vec!["please review".to_string(), "검토 요청".to_string()],
        );
        classify.insert("decisions".to_string(), vec!["approved".to_string()]);
        let mut events = vec![make_event("Please review this PR", None)];
        classify_by_keywords(&mut events, &classify);
        assert_eq!(events[0].classification.as_deref(), Some("action_required"));
    }
}
