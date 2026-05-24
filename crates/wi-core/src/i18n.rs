//! Output-language strings. Source data (mail, Slack, Jira bodies) always stays in its
//! original language — only the structural labels, section headers, and page titles that
//! wiki-ingest *adds* are localized. Add a language by adding a `Strings` constant and a
//! `Locale` arm; everything else is compiler-checked.

/// Output language for added labels/headers. `Ko` is the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Locale {
    #[default]
    Ko,
    En,
}

impl Locale {
    /// Parse a BCP-47-ish tag (`ko`, `ko-KR`, `en`, `en-US`); unknown/absent → `Ko`.
    pub fn from_tag(tag: Option<&str>) -> Self {
        match tag.map(str::to_ascii_lowercase).as_deref() {
            Some(t) if t.starts_with("en") => Locale::En,
            _ => Locale::Ko,
        }
    }

    /// BCP-47 code, e.g. for selecting a `templates/{code}/` directory.
    pub fn code(self) -> &'static str {
        match self {
            Locale::Ko => "ko",
            Locale::En => "en",
        }
    }

    pub fn strings(self) -> &'static Strings {
        match self {
            Locale::Ko => &KO,
            Locale::En => &EN,
        }
    }

    // ── Dynamic titles (word order differs per language, so format per arm) ──

    pub fn monthly_title(self, year: i16, month: i8) -> String {
        match self {
            Locale::Ko => format!("{year}년 {month}월 업무 요약"),
            Locale::En => format!("Work Summary {year}-{month:02}"),
        }
    }

    pub fn quarterly_title(self, year: i16, quarter: u8) -> String {
        match self {
            Locale::Ko => format!("{year}년 {quarter}분기 성과 리뷰"),
            Locale::En => format!("{year} Q{quarter} Performance Review"),
        }
    }

    pub fn annual_title(self, year: i16) -> String {
        match self {
            Locale::Ko => format!("{year}년 연간 성과 리뷰"),
            Locale::En => format!("{year} Annual Performance Review"),
        }
    }
}

/// Localized structural labels. These are the strings wiki-ingest emits around content;
/// the content itself is never touched. `Serialize` so it can be injected into the
/// template context as `i18n.*` (templates use `{{ i18n.summary }}` etc).
#[derive(serde::Serialize)]
pub struct Strings {
    // Section headers
    pub summary: &'static str,
    pub related_concepts: &'static str,
    pub key_events: &'static str,
    pub key_messages: &'static str,
    pub key_themes_this_week: &'static str,
    pub key_summary: &'static str,
    pub work_categories: &'static str,
    pub top_achievements: &'static str,
    pub overall_summary: &'static str,
    pub period_section: &'static str,
    // Inline field labels (adapters / renderers)
    pub status: &'static str,
    pub period: &'static str,
    pub location: &'static str,
    pub attendees: &'static str,
    /// Thread-reply marker; used as `--- {thread_replies} {n} ---`.
    pub thread_replies: &'static str,
    pub uncategorized: &'static str,
    // Page-title labels (the dynamic date/number is appended by the caller)
    pub work_log_title: &'static str,
    pub weekly_synthesis_title: &'static str,
    pub weekly_personal_title: &'static str,
}

static KO: Strings = Strings {
    summary: "요약",
    related_concepts: "관련 개념",
    key_events: "주요 이벤트",
    key_messages: "주요 메시지",
    key_themes_this_week: "이번 주 핵심 주제",
    key_summary: "핵심 요약",
    work_categories: "업무 카테고리",
    top_achievements: "주요 성과 Top 5",
    overall_summary: "종합 요약",
    period_section: "기간",
    status: "상태",
    period: "기간",
    location: "위치",
    attendees: "참석자",
    thread_replies: "쓰레드 답글",
    uncategorized: "기타",
    work_log_title: "업무 기록",
    weekly_synthesis_title: "주간 종합",
    weekly_personal_title: "내 주간 업무",
};

static EN: Strings = Strings {
    summary: "Summary",
    related_concepts: "Related Concepts",
    key_events: "Key Events",
    key_messages: "Key Messages",
    key_themes_this_week: "Key Themes This Week",
    key_summary: "Summary",
    work_categories: "Work Categories",
    top_achievements: "Top 5 Achievements",
    overall_summary: "Overview",
    period_section: "Period",
    status: "Status",
    period: "Period",
    location: "Location",
    attendees: "Attendees",
    thread_replies: "thread replies",
    uncategorized: "Other",
    work_log_title: "Work Log",
    weekly_synthesis_title: "Weekly Synthesis",
    weekly_personal_title: "My Week",
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_tag_maps_language_subtag() {
        assert_eq!(Locale::from_tag(Some("en")), Locale::En);
        assert_eq!(Locale::from_tag(Some("en-US")), Locale::En);
        assert_eq!(Locale::from_tag(Some("ko-KR")), Locale::Ko);
        assert_eq!(Locale::from_tag(None), Locale::Ko);
        assert_eq!(Locale::from_tag(Some("fr")), Locale::Ko); // unknown → default
    }

    #[test]
    fn dynamic_titles_differ_by_locale() {
        assert_eq!(Locale::Ko.monthly_title(2026, 5), "2026년 5월 업무 요약");
        assert_eq!(Locale::En.monthly_title(2026, 5), "Work Summary 2026-05");
        assert_eq!(Locale::En.strings().summary, "Summary");
        assert_eq!(Locale::Ko.strings().summary, "요약");
    }
}
