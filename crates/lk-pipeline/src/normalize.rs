use lk_core::config::SourceType;
use lk_core::event::{Event, EventId, RawItem, content_hash};
use lk_core::markdown::demote_headings;
use lk_core::text::collapse_blank_lines;

/// Floor heading level for normalized source bodies. Vault pages nest a body under
/// `# Title` (H1) → `## Section` (H2) → `### Event` (H3), so an embedded body whose
/// own headings reach H1–H3 would collide with that structure — in particular a
/// body `## Heading` would be mistaken for a section boundary by `lk-vault::section`,
/// corrupting cache section-extraction and `/lore-process` edits. Demoting every
/// body heading to at least H4 keeps embedded content strictly inside its event.
const BODY_HEADING_FLOOR: usize = 4;

/// Strip Unicode emoji from text. Long-lived documents don't benefit from
/// decorative emoji (marketing 🎀, reactions 🎉) — they add visual noise and
/// break grep/search. Slack shortcode emoji are already stripped in
/// `slack_to_markdown`; this catches emoji that arrive as Unicode (Gmail
/// subjects, Calendar descriptions, Jira bodies).
fn strip_unicode_emoji(text: &str) -> String {
    text.chars()
        .filter(|c| {
            // Keep ASCII + extended Latin + CJK + Hangul + common symbols.
            // Drop emoji blocks: Emoticons, Dingbats, Symbols, Flags, Misc.
            let cp = *c as u32;
            !(0x1F600..=0x1F64F).contains(&cp)   // Emoticons
                && !(0x1F300..=0x1F5FF).contains(&cp) // Misc Symbols & Pictographs
                && !(0x1F680..=0x1F6FF).contains(&cp) // Transport & Map
                && !(0x1F900..=0x1F9FF).contains(&cp) // Supplemental Symbols
                && !(0x1FA00..=0x1FA6F).contains(&cp) // Chess, Extended-A
                && !(0x1FA70..=0x1FAFF).contains(&cp) // Symbols Extended-A
                && !(0x2600..=0x26FF).contains(&cp)    // Misc Symbols
                && !(0x2700..=0x27BF).contains(&cp)    // Dingbats
                && !(0xFE00..=0xFE0F).contains(&cp)    // Variation Selectors
                && !(0x200D..=0x200D).contains(&cp)    // ZWJ
                && !(0xE0020..=0xE007F).contains(&cp) // Tags
        })
        .collect()
}

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

            // Without an external_id, identity is derived from title + body. Serialize
            // them as a JSON array so the field boundary is unambiguous — a plain
            // concatenation would collide (title "ab" + body "c" == title "a" + body "bc").
            let hash_input = match &item.external_id {
                Some(id) => id.clone(),
                None => serde_json::json!([item.title, item.body]).to_string(),
            };

            let id = EventId::new(source_id, date, &hash_input);
            let title = strip_unicode_emoji(&item.title);
            let body = strip_unicode_emoji(&demote_headings(
                &collapse_blank_lines(&item.body),
                BODY_HEADING_FLOOR,
            ));
            let ch = content_hash(&title, &body);

            Event {
                id,
                source_id: source_id.to_string(),
                source_type,
                date,
                title,
                body,
                url: item.url,
                author: item.author,
                labels: vec![],
                classification: None,
                performance_category: None,
                is_self: item.is_self,
                is_personal: false,
                content_hash: ch,
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
            metadata: serde_json::Value::Null,
        }];

        let tz = jiff::tz::TimeZone::UTC;
        let events = normalize("email-digest", SourceType::Gmail, items, &tz);
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
            metadata: serde_json::Value::Null,
        };
        // "ab"+"c" must not hash to the same id as "a"+"bc".
        let a = normalize("s", SourceType::Gmail, vec![mk("ab", "c")], &tz);
        let b = normalize("s", SourceType::Gmail, vec![mk("a", "bc")], &tz);
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
            metadata: serde_json::Value::Null,
        };

        let tz = jiff::tz::TimeZone::UTC;
        let a = normalize("my-tasks", SourceType::Jira, vec![make()], &tz);
        let b = normalize("my-tasks", SourceType::Jira, vec![make()], &tz);
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
            metadata: serde_json::Value::Null,
        };
        let events = normalize("my-tasks", SourceType::Jira, vec![item], &tz);
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
