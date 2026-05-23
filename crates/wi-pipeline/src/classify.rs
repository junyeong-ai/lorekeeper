use wi_core::config::Identity;
use wi_core::event::Event;

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
    let email = identity.email.to_lowercase();
    let name = identity.name.to_lowercase();
    let slack_id = identity.slack_id.as_deref().map(str::to_lowercase);
    let jira_id = identity.jira_id.as_deref().map(str::to_lowercase);

    for event in events {
        let author = event.author.as_deref().unwrap_or_default().to_lowercase();
        let meta = event.metadata.to_string().to_lowercase();

        let matched = author.contains(&email)
            || author.contains(&name)
            || meta.contains(&email)
            || slack_id
                .as_deref()
                .is_some_and(|sid| author.contains(sid) || meta.contains(sid))
            || jira_id.as_deref().is_some_and(|jid| author.contains(jid));

        if matched {
            event.is_personal = true;
            if !event.labels.contains(&"personal".to_string()) {
                event.labels.push("personal".into());
            }
        }
    }
}

pub fn classify_by_keywords(events: &mut [Event], params: &serde_json::Value) {
    let map = match params.get("classify").and_then(|v| v.as_object()) {
        Some(m) => m,
        None => return,
    };

    for event in events {
        if event.classification.is_some() {
            continue;
        }

        let text = format!("{} {}", event.title, event.body).to_lowercase();

        for (category, kw_val) in map {
            let matched = kw_val
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str())
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
    use wi_core::config::SourceType;
    use wi_core::event::EventId;

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
    fn keyword_classification() {
        let params = serde_json::json!({
            "classify": {
                "action_required": ["please review", "검토 요청"],
                "decisions": ["approved"]
            }
        });
        let mut events = vec![make_event("Please review this PR", None)];
        classify_by_keywords(&mut events, &params);
        assert_eq!(events[0].classification.as_deref(), Some("action_required"));
    }
}
