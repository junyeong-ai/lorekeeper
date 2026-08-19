use lk_core::config::SourceType;
use lk_core::event::{Event, EventId, RawItem};
use lk_core::markdown::demote_headings;
use lk_core::text::collapse_blank_lines;

/// Floor heading level for normalized source bodies. Vault pages nest a body under
/// `# Title` (H1) → `## Section` (H2) → `### Event` (H3), so an embedded body whose
/// own headings reach H1–H3 would collide with that structure — in particular a
/// body `## Heading` would be mistaken for a section boundary by `lk-vault::section`,
/// corrupting cache section-extraction and `/lore-process` edits. Demoting every
/// body heading to at least H4 keeps embedded content strictly inside its event.
const BODY_HEADING_FLOOR: usize = 4;

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

            // Text hygiene is owned HERE, once for every adapter: titles arrive with
            // wire noise (a Subject header's leading space) that would corrupt the
            // markdown the templates build around them (`**{title}**`, `### {title}`).
            let title = item.title.trim().to_string();
            let body = demote_headings(&collapse_blank_lines(&item.body), BODY_HEADING_FLOOR);

            // Identity prefers the adapter's stable `external_id`. The fallback —
            // hashing the NORMALIZED title + body — is intentionally lossy: two raw
            // items differing only in wire whitespace collapse to one observation.
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
                labels: vec![],
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
            external_id: Some("MSG-001".into()),
            title: "Test".into(),
            body: "body".into(),
            url: None,
            author: None,
            timestamp: jiff::Timestamp::now(),
            is_self: false,
            open_work: None,
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
            external_id: None,
            title: title.into(),
            body: body.into(),
            url: None,
            author: None,
            timestamp: ts,
            is_self: false,
            open_work: None,
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
            external_id: Some("JIRA-123".into()),
            title: "Title".into(),
            body: "different body".into(),
            url: None,
            author: None,
            timestamp: ts,
            is_self: false,
            open_work: None,
            metadata: serde_json::Value::Null,
        };

        let tz = jiff::tz::TimeZone::UTC;
        let a = normalize_events("my-tasks", SourceType::Jira, vec![make()], &tz);
        let b = normalize_events("my-tasks", SourceType::Jira, vec![make()], &tz);
        assert_eq!(a[0].id, b[0].id);
    }

    #[test]
    fn demotes_source_body_headings_to_avoid_section_collision() {
        // A Jira/manual/RSS body with a level-2 heading would otherwise collide with
        // the `## {section}` page structure. Normalize must demote it below H3.
        let tz = jiff::tz::TimeZone::UTC;
        let item = RawItem {
            external_id: Some("X".into()),
            title: "Issue".into(),
            body: "## Plan\n\nstep one\n\n### Sub\n\ndetail\n".into(),
            url: None,
            author: None,
            timestamp: jiff::Timestamp::now(),
            is_self: false,
            open_work: None,
            metadata: serde_json::Value::Null,
        };
        let events = normalize_events("my-tasks", SourceType::Jira, vec![item], &tz);
        assert!(
            !events[0].body.lines().any(|l| l.trim_end() == "## Plan"),
            "body H2 must be demoted so it can't be read as a section boundary:\n{}",
            events[0].body
        );
        assert!(
            events[0].body.contains("#### Plan"),
            "H2 should demote to H4:\n{}",
            events[0].body
        );
    }

    #[test]
    fn title_wire_whitespace_is_trimmed() {
        // A Subject header arriving as " [Action] …" would render as `** [Action]…**`
        // — CommonMark refuses `**` followed by whitespace, so the bold breaks.
        // Normalize owns title hygiene for every adapter.
        let tz = jiff::tz::TimeZone::UTC;
        let item = RawItem {
            external_id: Some("X".into()),
            title: "  [Action] subject \n".into(),
            body: String::new(),
            url: None,
            author: None,
            timestamp: jiff::Timestamp::now(),
            is_self: false,
            open_work: None,
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
            external_id: None,
            title: title.into(),
            body: "b".into(),
            url: None,
            author: None,
            timestamp: ts,
            is_self: false,
            open_work: None,
            metadata: serde_json::Value::Null,
        };
        let a = normalize_events("s", SourceType::Gmail, vec![mk("T")], &tz);
        let b = normalize_events("s", SourceType::Gmail, vec![mk(" T ")], &tz);
        assert_eq!(a[0].id, b[0].id);
    }

    // Adversarial property tests: assert the normalization guarantees hold for ANY
    // source item, closing the "messy source field reaches the page" class by
    // contract rather than by example.
    mod properties {
        use super::*;
        use proptest::prelude::*;

        fn raw(title: &str, body: &str) -> RawItem {
            RawItem {
                external_id: None,
                title: title.into(),
                body: body.into(),
                url: None,
                author: None,
                timestamp: jiff::Timestamp::UNIX_EPOCH,
                is_self: false,
                open_work: None,
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
            fn body_headings_never_collide_with_page_structure(
                level in 1usize..=6,
                heading in r#"[ \PC]{0,30}"#,
            ) {
                // Any source-body heading is demoted to at least H4, so it can never be
                // mistaken for a page/section/event heading (H1–H3) by `lk-vault::section`.
                let tz = jiff::tz::TimeZone::UTC;
                let body = format!("{} Heading {}\n\nbody\n", "#".repeat(level), heading);
                let ev = normalize_events("s", SourceType::Gmail, vec![raw("t", &body)], &tz);
                for line in ev[0].body.lines() {
                    let hashes = line.chars().take_while(|c| *c == '#').count();
                    let is_heading = (1..=6).contains(&hashes)
                        && line[hashes..].starts_with(' ');
                    prop_assert!(
                        !(is_heading && hashes < 4),
                        "body heading shallower than H4 survived: {:?}",
                        line
                    );
                }
            }
        }
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
            is_self: false,
            open_work: None,
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
