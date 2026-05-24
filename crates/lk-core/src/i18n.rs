//! Output-language strings. Source data (mail, Slack, Jira bodies) always stays in its
//! original language — only the structural labels, section headers, and page titles that
//! Lorekeeper *adds* are localized. Add a language by adding a `Strings` constant and a
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
    // Gmail digest
    pub action_required: &'static str,
    pub decisions: &'static str,
    pub project_updates: &'static str,
    pub knowledge_sharing: &'static str,
    pub meeting_followup: &'static str,
    pub statistics: &'static str,
    pub total_received: &'static str,
    pub meaningful_mail: &'static str,
    pub filter_rate: &'static str,
    /// Unit suffix for counts (Korean "건"); empty in languages that don't use one.
    pub count_unit: &'static str,
    // Personal / synthesis sections
    pub key_summary: &'static str,
    pub work_categories: &'static str,
    pub key_themes_this_week: &'static str,
    pub source_summaries: &'static str,
    pub new_concepts: &'static str,
    pub overall_summary: &'static str,
    pub quarterly_breakdown: &'static str,
    pub monthly_breakdown: &'static str,
    pub category_distribution: &'static str,
    pub top_achievements: &'static str,
    pub team_contribution: &'static str,
    pub growth_areas: &'static str,
    pub next_quarter: &'static str,
    // Quarterly table columns
    pub col_category: &'static str,
    pub col_count: &'static str,
    pub col_ratio: &'static str,
    // Concept page
    pub concept_synthesis: &'static str,
    pub concept_sources: &'static str,
    pub concept_meta: &'static str,
    pub related: &'static str,
    pub first_seen: &'static str,
    pub last_seen: &'static str,
    pub reference_count: &'static str,
    pub confidence: &'static str,
    pub original_message: &'static str,
    // Inline field labels (adapters / renderers)
    pub from: &'static str,
    pub status: &'static str,
    pub period: &'static str,
    pub location: &'static str,
    pub attendees: &'static str,
    /// Thread-reply marker; used as `--- {thread_replies} {n} ---`.
    pub thread_replies: &'static str,
    pub uncategorized: &'static str,
    // Page-title labels (dynamic date/number appended by the caller/template)
    pub gmail_title: &'static str,
    pub calendar_title: &'static str,
    pub drive_title: &'static str,
    pub slack_channel_title: &'static str,
    pub slack_search_title: &'static str,
    pub jira_title: &'static str,
    pub work_log_title: &'static str,
    pub weekly_synthesis_title: &'static str,
    pub weekly_personal_title: &'static str,
    // Synthesis placeholders
    pub pages_this_week: &'static str,
    // Work-log sections
    pub topic_summary: &'static str,
    pub details: &'static str,
    // Document page sections
    pub document_content: &'static str,
    // Wiki index categories (lore wiki index output)
    pub index_concepts: &'static str,
    pub index_documents: &'static str,
    pub index_daily: &'static str,
    pub index_worklog: &'static str,
    pub index_synthesis: &'static str,
    // Document page title
    pub document_title: &'static str,
}

static KO: Strings = Strings {
    summary: "요약",
    key_events: "주요 이벤트",
    key_messages: "주요 메시지",
    related_concepts: "관련 개념",
    action_required: "조치 필요",
    decisions: "의사결정",
    project_updates: "프로젝트 업데이트",
    knowledge_sharing: "지식 공유",
    meeting_followup: "미팅 후속",
    statistics: "통계",
    total_received: "전체 수신",
    meaningful_mail: "의미 있는 메일",
    filter_rate: "필터링률",
    count_unit: "건",
    key_summary: "핵심 요약",
    work_categories: "업무 카테고리",
    key_themes_this_week: "이번 주 핵심 주제",
    source_summaries: "소스별 요약",
    new_concepts: "새로 등장한 개념",
    overall_summary: "종합 요약",
    quarterly_breakdown: "분기별 요약",
    monthly_breakdown: "월별 요약",
    category_distribution: "업무 카테고리 분포",
    top_achievements: "주요 성과 Top 5",
    team_contribution: "팀 내 기여도",
    growth_areas: "성장 영역",
    next_quarter: "다음 분기 방향",
    col_category: "카테고리",
    col_count: "건수",
    col_ratio: "비중",
    concept_synthesis: "핵심",
    concept_sources: "출처",
    concept_meta: "메타",
    related: "관련",
    first_seen: "처음 등장",
    last_seen: "최근 등장",
    reference_count: "참조 횟수",
    confidence: "추출 방식",
    original_message: "원본 메시지",
    from: "발신",
    status: "상태",
    period: "기간",
    location: "위치",
    attendees: "참석자",
    thread_replies: "쓰레드 답글",
    uncategorized: "기타",
    gmail_title: "이메일 다이제스트",
    calendar_title: "내 일정",
    drive_title: "문서 브리핑",
    slack_channel_title: "팀 활동",
    slack_search_title: "키워드 트렌드",
    jira_title: "내 Jira 업무",
    work_log_title: "업무 기록",
    weekly_synthesis_title: "주간 종합",
    weekly_personal_title: "내 주간 업무",
    pages_this_week: "이번 주 페이지",
    topic_summary: "주제별 요약",
    details: "상세",
    document_content: "내용",
    index_concepts: "개념",
    index_documents: "문서",
    index_daily: "일일",
    index_worklog: "업무 로그",
    index_synthesis: "종합",
    document_title: "수집 문서",
};

static EN: Strings = Strings {
    summary: "Summary",
    key_events: "Key Events",
    key_messages: "Key Messages",
    related_concepts: "Related Concepts",
    action_required: "Action Required",
    decisions: "Decisions",
    project_updates: "Project Updates",
    knowledge_sharing: "Knowledge Sharing",
    meeting_followup: "Meeting Follow-up",
    statistics: "Statistics",
    total_received: "Total received",
    meaningful_mail: "Meaningful",
    filter_rate: "filter rate",
    count_unit: "",
    key_summary: "Summary",
    work_categories: "Work Categories",
    key_themes_this_week: "Key Themes This Week",
    source_summaries: "By Source",
    new_concepts: "New Concepts",
    overall_summary: "Overview",
    quarterly_breakdown: "By Quarter",
    monthly_breakdown: "By Month",
    category_distribution: "Category Distribution",
    top_achievements: "Top 5 Achievements",
    team_contribution: "Team Contribution",
    growth_areas: "Growth Areas",
    next_quarter: "Next Quarter",
    col_category: "Category",
    col_count: "Count",
    col_ratio: "Share",
    concept_synthesis: "Synthesis",
    concept_sources: "Sources",
    concept_meta: "Metadata",
    related: "Related",
    first_seen: "First seen",
    last_seen: "Last seen",
    reference_count: "References",
    confidence: "Extraction",
    original_message: "Original message",
    from: "from",
    status: "Status",
    period: "Period",
    location: "Location",
    attendees: "Attendees",
    thread_replies: "thread replies",
    uncategorized: "Other",
    gmail_title: "Email Digest",
    calendar_title: "My Schedule",
    drive_title: "Document Briefing",
    slack_channel_title: "Team Activity",
    slack_search_title: "Keyword Trends",
    jira_title: "My Jira",
    work_log_title: "Work Log",
    weekly_synthesis_title: "Weekly Synthesis",
    weekly_personal_title: "My Week",
    pages_this_week: "pages this week",
    topic_summary: "Topics",
    details: "Details",
    document_content: "Content",
    index_concepts: "Concepts",
    index_documents: "Documents",
    index_daily: "Daily",
    index_worklog: "Work Log",
    index_synthesis: "Synthesis",
    document_title: "Document",
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
    }

    #[test]
    fn strings_localized() {
        assert_eq!(Locale::Ko.strings().summary, "요약");
        assert_eq!(Locale::En.strings().summary, "Summary");
        assert_eq!(Locale::En.strings().count_unit, "");
    }
}
