use wi_core::config::SourceType;
use wi_core::event::{Event, EventId, RawItem};

pub fn normalize(
    source_id: &str,
    source_type: SourceType,
    items: Vec<RawItem>,
    timezone: &jiff::tz::TimeZone,
) -> Vec<Event> {
    items
        .into_iter()
        .map(|item| {
            let zoned = item.timestamp.to_zoned(timezone.clone());
            let date = zoned.date();

            let hash_input = item
                .external_id
                .as_deref()
                .map(String::from)
                .unwrap_or_else(|| format!("{}{}", item.title, item.body));

            let id = EventId::new(source_id, date, &hash_input);

            Event {
                id,
                source_id: source_id.to_string(),
                source_type,
                date,
                title: item.title,
                body: item.body,
                url: item.url,
                author: item.author,
                labels: vec![],
                classification: None,
                is_personal: false,
                metadata: item.metadata,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_assigns_event_id() {
        let items = vec![RawItem {
            external_id: Some("MSG-001".into()),
            title: "Test".into(),
            body: "body".into(),
            url: None,
            author: None,
            timestamp: jiff::Timestamp::now(),
            metadata: serde_json::Value::Null,
        }];

        let tz = jiff::tz::TimeZone::UTC;
        let events = normalize("email-digest", SourceType::Gmail, items, &tz);
        assert_eq!(events.len(), 1);
        assert!(events[0].id.as_str().starts_with("email-digest:"));
    }

    #[test]
    fn stable_id_for_same_external_id() {
        let ts = jiff::Timestamp::now();
        let make = || RawItem {
            external_id: Some("JIRA-123".into()),
            title: "Title".into(),
            body: "different body".into(),
            url: None,
            author: None,
            timestamp: ts,
            metadata: serde_json::Value::Null,
        };

        let tz = jiff::tz::TimeZone::UTC;
        let a = normalize("my-tasks", SourceType::Jira, vec![make()], &tz);
        let b = normalize("my-tasks", SourceType::Jira, vec![make()], &tz);
        assert_eq!(a[0].id, b[0].id);
    }

    #[test]
    fn timezone_affects_date() {
        // 2026-05-22T23:00:00Z is 2026-05-23T08:00:00 KST
        let ts: jiff::Timestamp = "2026-05-22T23:00:00Z".parse().unwrap();
        let item = RawItem {
            external_id: Some("X".into()),
            title: "X".into(),
            body: String::new(),
            url: None,
            author: None,
            timestamp: ts,
            metadata: serde_json::Value::Null,
        };

        let utc = jiff::tz::TimeZone::UTC;
        let kst = jiff::tz::TimeZone::get("Asia/Seoul").unwrap();

        let utc_events = normalize("s", SourceType::Gmail, vec![item.clone()], &utc);
        let kst_events = normalize("s", SourceType::Gmail, vec![item], &kst);

        assert_eq!(utc_events[0].date, jiff::civil::date(2026, 5, 22));
        assert_eq!(kst_events[0].date, jiff::civil::date(2026, 5, 23));
    }
}
