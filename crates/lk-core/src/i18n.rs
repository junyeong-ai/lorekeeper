//! Output-language strings. Source data (mail, Slack, Jira bodies) always stays in its
//! original language — only the structural labels, section headers, and page titles that
//! Lorekeeper *adds* are localized. Add a language by adding a `Strings` constant and a
//! `Locale` arm; everything else is compiler-checked.

/// Output language for added labels/headers. `Ko` is the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, strum::EnumIter)]
pub enum Locale {
    #[default]
    Ko,
    En,
}

impl Locale {
    /// Every locale variant. The single place to enumerate locales — code that needs
    /// to consider all of them (e.g. finding a section heading authored under a
    /// since-changed `vault.locale`) iterates this instead of hardcoding the list.
    pub const ALL: &'static [Locale] = &[Locale::Ko, Locale::En];

    /// Parse a BCP-47-ish tag by its primary language subtag (`ko`, `ko-KR`,
    /// `en`, `en-US`). Matches against `tag()` over `ALL`, so a new language is
    /// recognized the moment its `Locale` arm exists — no separate parse table.
    /// `None` for an unrecognized tag; callers that need a total mapping use
    /// `from_tag`, and `Config::validate` rejects an unrecognized `vault.locale`.
    pub fn try_from_tag(tag: &str) -> Option<Self> {
        let primary = tag
            .split(['-', '_'])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        Locale::ALL.iter().copied().find(|l| l.tag() == primary)
    }

    /// Total tag parse: an unrecognized or absent tag falls back to the default
    /// locale. `vault.locale` is validated at config load, so in practice the
    /// fallback only applies when the field is entirely absent.
    pub fn from_tag(tag: Option<&str>) -> Self {
        tag.and_then(Self::try_from_tag).unwrap_or_default()
    }

    pub fn tag(self) -> &'static str {
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

    pub fn weekly_title(self, year: i16, week: u8) -> String {
        match self {
            Locale::Ko => format!("{year}년 {week}주차 성과 리뷰"),
            Locale::En => format!("{year}-W{week:02} Performance Review"),
        }
    }

    pub fn monthly_title(self, year: i16, month: i8) -> String {
        match self {
            Locale::Ko => format!("{year}년 {month}월 성과 리뷰"),
            Locale::En => format!("{year}-{month:02} Performance Review"),
        }
    }

    pub fn quarterly_title(self, year: i16, quarter: u8) -> String {
        match self {
            Locale::Ko => format!("{year}년 {quarter}분기 성과 리뷰"),
            Locale::En => format!("{year}-Q{quarter} Performance Review"),
        }
    }

    pub fn annual_title(self, year: i16) -> String {
        match self {
            Locale::Ko => format!("{year}년 연간 성과 리뷰"),
            Locale::En => format!("{year} Annual Performance Review"),
        }
    }
}

/// Localized structural labels. These are the strings Lorekeeper emits around content;
/// the content itself is never touched. `Serialize` so it injects into the template
/// context as `i18n.*` (templates use `{{ i18n.summary }}` etc).
#[derive(serde::Serialize)]
pub struct Strings {
    // Daily section headers
    pub summary: &'static str,
    pub key_events: &'static str,
    pub key_messages: &'static str,
    pub related_concepts: &'static str,
    // Personal / synthesis sections
    pub key_summary: &'static str,
    pub key_themes_this_week: &'static str,
    pub overall_summary: &'static str,
    pub quarterly_breakdown: &'static str,
    pub monthly_breakdown: &'static str,
    pub category_distribution: &'static str,
    // Quarterly table columns
    pub col_category: &'static str,
    pub col_count: &'static str,
    pub col_ratio: &'static str,
    // Concept page
    pub concept_synthesis: &'static str,
    pub concept_sources: &'static str,
    pub related: &'static str,
    pub first_seen: &'static str,
    pub last_seen: &'static str,
    pub source_count: &'static str,
    pub original_message: &'static str,
    // Inline field labels (adapters / renderers)
    pub status: &'static str,
    pub period: &'static str,
    pub location: &'static str,
    pub attendees: &'static str,
    pub meeting_notes: &'static str,
    pub original_link: &'static str,
    /// Thread-reply marker; used as `--- {thread_replies} {n} ---`.
    pub thread_replies: &'static str,
    pub uncategorized: &'static str,
    /// Placeholder title for a source item that arrives without one (a calendar
    /// event with no summary, a Jira issue with no summary).
    pub untitled: &'static str,
    // Page-title labels (dynamic date/number appended by the caller/template)
    pub gmail_title: &'static str,
    pub calendar_title: &'static str,
    pub drive_title: &'static str,
    pub slack_channel_title: &'static str,
    pub slack_search_title: &'static str,
    pub jira_title: &'static str,
    pub confluence_title: &'static str,
    pub rss_title: &'static str,
    pub work_log_title: &'static str,
    pub weekly_synthesis_title: &'static str,
    // Work-log sections
    pub topic_summary: &'static str,
    // Document page sections
    pub document_content: &'static str,
    // Exploration page sections
    pub exploration_question: &'static str,
    pub exploration_synthesis: &'static str,
    pub exploration_grounding: &'static str,
    // Wiki index categories (lore wiki index output)
    pub index_concepts: &'static str,
    pub index_documents: &'static str,
    pub index_explorations: &'static str,
    pub index_daily: &'static str,
    pub index_work_log: &'static str,
    pub index_synthesis: &'static str,
    // Knowledge map (lore wiki map)
    pub map_title: &'static str,
    pub index_title: &'static str,
    pub log_title: &'static str,
    pub map_intro: &'static str,
    pub map_empty: &'static str,
    pub map_links: &'static str,
}

static KO: Strings = Strings {
    summary: "요약",
    key_events: "주요 이벤트",
    key_messages: "주요 메시지",
    related_concepts: "관련 개념",
    key_summary: "핵심 요약",
    key_themes_this_week: "이번 주 핵심 주제",
    overall_summary: "종합 요약",
    quarterly_breakdown: "분기별 요약",
    monthly_breakdown: "월별 요약",
    category_distribution: "카테고리 분포",
    col_category: "카테고리",
    col_count: "건수",
    col_ratio: "비중",
    concept_synthesis: "핵심",
    concept_sources: "출처",
    related: "관련",
    first_seen: "처음 등장",
    last_seen: "최근 등장",
    source_count: "참조 횟수",
    original_message: "원본 메시지",
    status: "상태",
    period: "기간",
    location: "위치",
    attendees: "참석자",
    meeting_notes: "회의록",
    original_link: "원본",
    thread_replies: "쓰레드 답글",
    uncategorized: "기타",
    untitled: "(제목 없음)",
    gmail_title: "이메일 다이제스트",
    calendar_title: "내 일정",
    drive_title: "문서 브리핑",
    slack_channel_title: "팀 활동",
    slack_search_title: "키워드 트렌드",
    jira_title: "내 Jira 업무",
    confluence_title: "내 위키 문서",
    rss_title: "뉴스",
    work_log_title: "업무 기록",
    weekly_synthesis_title: "주간 종합",
    topic_summary: "주제별 요약",
    document_content: "내용",
    exploration_question: "질문",
    exploration_synthesis: "종합",
    exploration_grounding: "근거",
    index_concepts: "개념",
    index_documents: "문서",
    index_explorations: "탐구",
    index_daily: "일일",
    index_work_log: "업무 로그",
    index_synthesis: "종합",
    map_title: "지식 맵",
    index_title: "위키 인덱스",
    log_title: "지식 로그",
    map_intro: "출처 간 인용으로 함께 묶이는 개념 클러스터입니다 (`lore wiki map`이 매번 다시 생성). 임베딩 없이 vault를 탐색하는 진입점입니다.",
    map_empty: "(아직 클러스터가 없습니다 — 개념 간 연결이 쌓이면 채워집니다)",
    map_links: "연결",
};

static EN: Strings = Strings {
    summary: "Summary",
    key_events: "Key Events",
    key_messages: "Key Messages",
    related_concepts: "Related Concepts",
    key_summary: "Summary",
    key_themes_this_week: "Key Themes This Week",
    overall_summary: "Overview",
    quarterly_breakdown: "By Quarter",
    monthly_breakdown: "By Month",
    category_distribution: "Category Distribution",
    col_category: "Category",
    col_count: "Count",
    col_ratio: "Share",
    concept_synthesis: "Synthesis",
    concept_sources: "Sources",
    related: "Related",
    first_seen: "First seen",
    last_seen: "Last seen",
    source_count: "References",
    original_message: "Original message",
    status: "Status",
    period: "Period",
    location: "Location",
    attendees: "Attendees",
    meeting_notes: "Meeting Notes",
    original_link: "Source",
    thread_replies: "thread replies",
    uncategorized: "Other",
    untitled: "(untitled)",
    gmail_title: "Email Digest",
    calendar_title: "My Schedule",
    drive_title: "Document Briefing",
    slack_channel_title: "Team Activity",
    slack_search_title: "Keyword Trends",
    jira_title: "My Jira",
    confluence_title: "My Wiki Pages",
    rss_title: "News",
    work_log_title: "Work Log",
    weekly_synthesis_title: "Weekly Synthesis",
    topic_summary: "Topics",
    document_content: "Content",
    exploration_question: "Question",
    exploration_synthesis: "Synthesis",
    exploration_grounding: "Grounding",
    index_concepts: "Concepts",
    index_documents: "Documents",
    index_explorations: "Explorations",
    index_daily: "Daily",
    index_work_log: "Work Log",
    index_synthesis: "Synthesis",
    map_title: "Knowledge Map",
    index_title: "Wiki Index",
    log_title: "Knowledge Log",
    map_intro: "Concept clusters grouped by cross-source citation (regenerated by `lore wiki map`). A navigation entry point for traversing the vault without embeddings.",
    map_empty: "(no clusters yet — this fills in as links accumulate between concepts)",
    map_links: "links",
};

#[cfg(test)]
mod tests {
    use super::*;

    /// `ALL` is what every "consider each locale" path iterates — resolving a section heading a
    /// page was authored under, and the render tests that check both languages. A variant added
    /// to the enum and left out of it is a locale the vault silently stops recognizing headings
    /// for, which reads as a page with no such section rather than as a missing locale.
    #[test]
    fn all_locales_are_in_all() {
        use strum::IntoEnumIterator;

        let listed: Vec<Locale> = Locale::ALL.to_vec();
        let declared: Vec<Locale> = Locale::iter().collect();
        assert_eq!(
            listed, declared,
            "Locale::ALL must be every variant, in declaration order"
        );
    }

    #[test]
    fn from_tag_maps_language_subtag() {
        assert_eq!(Locale::from_tag(Some("en")), Locale::En);
        assert_eq!(Locale::from_tag(Some("en-US")), Locale::En);
        assert_eq!(Locale::from_tag(Some("ko-KR")), Locale::Ko);
        assert_eq!(Locale::from_tag(None), Locale::Ko);
        assert_eq!(Locale::from_tag(Some("fr")), Locale::Ko); // unknown → default
    }

    #[test]
    fn try_from_tag_recognizes_only_known_primary_subtags() {
        assert_eq!(Locale::try_from_tag("ko"), Some(Locale::Ko));
        assert_eq!(Locale::try_from_tag("EN-us"), Some(Locale::En)); // case-insensitive
        assert_eq!(Locale::try_from_tag("ko_KR"), Some(Locale::Ko)); // underscore subtag
        assert_eq!(Locale::try_from_tag("english"), None); // not a primary subtag
        assert_eq!(Locale::try_from_tag("fr"), None);
        assert_eq!(Locale::try_from_tag(""), None);
    }

    #[test]
    fn dynamic_titles_differ_by_locale() {
        assert_eq!(Locale::Ko.weekly_title(2026, 21), "2026년 21주차 성과 리뷰");
        assert_eq!(
            Locale::En.weekly_title(2026, 21),
            "2026-W21 Performance Review"
        );
        assert_eq!(Locale::Ko.monthly_title(2026, 5), "2026년 5월 성과 리뷰");
        assert_eq!(
            Locale::En.monthly_title(2026, 5),
            "2026-05 Performance Review"
        );
        assert_eq!(
            Locale::En.quarterly_title(2026, 2),
            "2026-Q2 Performance Review"
        );
    }

    #[test]
    fn strings_localized() {
        assert_eq!(Locale::Ko.strings().summary, "요약");
        assert_eq!(Locale::En.strings().summary, "Summary");
    }
}
