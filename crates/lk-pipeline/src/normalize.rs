use lk_core::config::SourceType;
use lk_core::event::{Event, EventId, RawItem};

pub fn normalize_events(
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

            // Wire padding is not part of a title. Bodies retain literal source
            // content; Markdown containment belongs to the fragment renderer.
            let title = item.title.trim().to_string();
            let body = item.body;

            // Identity prefers the adapter's stable `external_id`. The fallback —
            // hashing the trimmed title and exact body — retains literal body differences.
            // An adapter whose items have genuine distinct identity (a real message
            // id) MUST supply `external_id`; this path is for sources that have none.
            // The fields are a JSON array so the boundary is unambiguous — a plain
            // concatenation would collide (title "ab" + body "c" == "a" + "bc").
            let hash_input = match &item.external_id {
                Some(id) => id.clone(),
                None => serde_json::json!([title, body]).to_string(),
            };

            let id = EventId::new(source_id, date, &hash_input);

            Event {
                id,
                source_id: source_id.to_string(),
                source_type,
                timestamp: item.timestamp,
                date,
                title,
                body,
                url: item.url,
                author: item.author,
                labels: item.labels,
                category: None,
                performance_category: None,
                is_self: item.is_self,
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
            labels: Vec::new(),
            external_id: Some("MSG-001".into()),
            title: "Test".into(),
            body: "body".into(),
            url: None,
            author: None,
            timestamp: jiff::Timestamp::now(),
            is_self: false,
            work: None,
            metadata: serde_json::Value::Null,
        }];

        let tz = jiff::tz::TimeZone::UTC;
        let events = normalize_events("email-digest", SourceType::Gmail, items, &tz);
        assert_eq!(events.len(), 1);
        assert!(events[0].id.as_str().starts_with("email-digest:"));
    }

    #[test]
    fn title_body_boundary_is_unambiguous() {
        let tz = jiff::tz::TimeZone::UTC;
        let ts = jiff::Timestamp::now();
        let mk = |title: &str, body: &str| RawItem {
            labels: Vec::new(),
            external_id: None,
            title: title.into(),
            body: body.into(),
            url: None,
            author: None,
            timestamp: ts,
            is_self: false,
            work: None,
            metadata: serde_json::Value::Null,
        };
        // "ab"+"c" must not hash to the same id as "a"+"bc".
        let a = normalize_events("s", SourceType::Gmail, vec![mk("ab", "c")], &tz);
        let b = normalize_events("s", SourceType::Gmail, vec![mk("a", "bc")], &tz);
        assert_ne!(a[0].id, b[0].id);
    }

    #[test]
    fn stable_id_for_same_external_id() {
        let ts = jiff::Timestamp::now();
        let make = || RawItem {
            labels: Vec::new(),
            external_id: Some("JIRA-123".into()),
            title: "Title".into(),
            body: "different body".into(),
            url: None,
            author: None,
            timestamp: ts,
            is_self: false,
            work: None,
            metadata: serde_json::Value::Null,
        };

        let tz = jiff::tz::TimeZone::UTC;
        let a = normalize_events("my-tasks", SourceType::Jira, vec![make()], &tz);
        let mut changed = make();
        changed.body = "revised source body".into();
        let b = normalize_events("my-tasks", SourceType::Jira, vec![changed], &tz);
        assert_eq!(a[0].id, b[0].id);
        assert_ne!(a[0].body, b[0].body);
    }

    #[test]
    fn preserves_source_markdown_and_literal_content() {
        let tz = jiff::tz::TimeZone::UTC;
        for body in [
            "## Plan\n\nstep one\n\n### Sub\n\ndetail\n",
            "```python\nvalue = \"\"\"first\n\n\nsecond\"\"\"\n```\n",
            "<pre>\n# literal heading\n\n\ntext\n</pre>\n",
            "line one\r\n\r\n\r\nline two\r",
        ] {
            let item = RawItem {
                labels: Vec::new(),
                external_id: Some("X".into()),
                title: "Issue".into(),
                body: body.into(),
                url: None,
                author: None,
                timestamp: jiff::Timestamp::now(),
                is_self: false,
                work: None,
                metadata: serde_json::Value::Null,
            };
            let events = normalize_events("my-tasks", SourceType::Jira, vec![item], &tz);
            assert_eq!(events[0].body, body);
        }
    }

    #[test]
    fn literal_body_differences_distinguish_fallback_event_identity() {
        let item = RawItem {
            labels: Vec::new(),
            external_id: None,
            title: "Example".into(),
            body: "```python\nvalue = \"\"\"first\n\n\nsecond\"\"\"\n```".into(),
            url: None,
            author: None,
            timestamp: jiff::Timestamp::UNIX_EPOCH,
            is_self: false,
            work: None,
            metadata: serde_json::Value::Null,
        };
        let mut changed = item.clone();
        changed.body = changed.body.replace("\n\n\n", "\n\n");
        let events = normalize_events(
            "source",
            SourceType::Gmail,
            vec![item, changed],
            &jiff::tz::TimeZone::UTC,
        );
        assert_ne!(events[0].id, events[1].id);
    }

    #[test]
    fn title_wire_whitespace_is_trimmed() {
        // A Subject header arriving as " [Action] …" would render as `** [Action]…**`
        // — CommonMark refuses `**` followed by whitespace, so the bold breaks.
        // Normalize owns title hygiene for every adapter.
        let tz = jiff::tz::TimeZone::UTC;
        let item = RawItem {
            labels: Vec::new(),
            external_id: Some("X".into()),
            title: "  [Action] subject \n".into(),
            body: String::new(),
            url: None,
            author: None,
            timestamp: jiff::Timestamp::now(),
            is_self: false,
            work: None,
            metadata: serde_json::Value::Null,
        };
        let events = normalize_events("s", SourceType::Gmail, vec![item], &tz);
        assert_eq!(events[0].title, "[Action] subject");
    }

    #[test]
    fn whitespace_only_difference_is_the_same_identity_without_external_id() {
        // Identity hashes the NORMALIZED title/body, so wire whitespace can't fork
        // two EventIds for the same observation.
        let tz = jiff::tz::TimeZone::UTC;
        let ts = jiff::Timestamp::now();
        let mk = |title: &str| RawItem {
            labels: Vec::new(),
            external_id: None,
            title: title.into(),
            body: "b".into(),
            url: None,
            author: None,
            timestamp: ts,
            is_self: false,
            work: None,
            metadata: serde_json::Value::Null,
        };
        let a = normalize_events("s", SourceType::Gmail, vec![mk("T")], &tz);
        let b = normalize_events("s", SourceType::Gmail, vec![mk(" T ")], &tz);
        assert_eq!(a[0].id, b[0].id);
    }

    mod properties {
        use super::*;
        use proptest::prelude::*;

        fn raw(title: &str, body: &str) -> RawItem {
            RawItem {
                labels: Vec::new(),
                external_id: None,
                title: title.into(),
                body: body.into(),
                url: None,
                author: None,
                timestamp: jiff::Timestamp::UNIX_EPOCH,
                is_self: false,
                work: None,
                metadata: serde_json::Value::Null,
            }
        }

        proptest! {
            #[test]
            fn title_is_always_edge_trimmed(title in r#"[\PC]{0,60}"#) {
                // A normalized title never carries leading/trailing whitespace, so the
                // markdown the templates wrap around it (`**{title}**`, `### {title}`)
                // can never be broken by wire padding.
                let tz = jiff::tz::TimeZone::UTC;
                let ev = normalize_events("s", SourceType::Gmail, vec![raw(&title, "b")], &tz);
                let t = &ev[0].title;
                prop_assert_eq!(t.as_str(), t.trim(), "title not edge-trimmed: {:?}", t);
            }

            #[test]
            fn source_body_is_preserved(body in any::<String>()) {
                let tz = jiff::tz::TimeZone::UTC;
                let ev = normalize_events("s", SourceType::Gmail, vec![raw("t", &body)], &tz);
                prop_assert_eq!(ev[0].body.as_str(), body);
            }
        }
    }

    #[test]
    fn timezone_affects_date() {
        // 2026-05-22T23:00:00Z is 2026-05-23T08:00:00 KST
        let ts: jiff::Timestamp = "2026-05-22T23:00:00Z".parse().unwrap();
        let item = RawItem {
            labels: Vec::new(),
            external_id: Some("X".into()),
            title: "X".into(),
            body: String::new(),
            url: None,
            author: None,
            timestamp: ts,
            is_self: false,
            work: None,
            metadata: serde_json::Value::Null,
        };

        let utc = jiff::tz::TimeZone::UTC;
        let kst = jiff::tz::TimeZone::get("Asia/Seoul").unwrap();

        let utc_events = normalize_events("s", SourceType::Gmail, vec![item.clone()], &utc);
        let kst_events = normalize_events("s", SourceType::Gmail, vec![item], &kst);

        assert_eq!(utc_events[0].date, jiff::civil::date(2026, 5, 22));
        assert_eq!(kst_events[0].date, jiff::civil::date(2026, 5, 23));
    }
}
