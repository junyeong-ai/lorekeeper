use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClassifyRule {
    /// Daily-page grouping bucket assigned to a matching event (e.g.
    /// `action_required`). A presentation axis — it controls which section the
    /// event renders under on the daily page.
    pub category: String,
    pub keywords: Vec<String>,
    /// Optional EXPLICIT bridge to the performance taxonomy: when set, a matching
    /// event also gets this `performance_category`, contributing to the work-log /
    /// review distribution. Must be one of `performance.performance_categories`
    /// (validated at load). Omitted = this rule is grouping-only and the event's
    /// performance category falls back to the per-source-type map. This keeps the two
    /// taxonomies orthogonal while making the content→performance link visible and
    /// opt-in per rule, rather than a fragile string coincidence.
    #[serde(default)]
    pub performance_category: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub vault: VaultConfig,
    pub identity: Identity,
    pub sources: BTreeMap<String, SourceConfig>,
    #[serde(default)]
    pub performance: PerformanceConfig,
    #[serde(default)]
    pub synthesis: SynthesisConfig,
    #[serde(default)]
    pub llm: LlmConfig,
    /// Concept extraction and categorization settings.
    #[serde(default)]
    pub concepts: ConceptConfig,
    /// Wikilink graph analysis settings (consumed by `lore graph` / `lk-graph`).
    /// Optional and fully defaulted; absent in config.yaml means "use defaults".
    #[serde(default)]
    pub graph: GraphConfig,
    /// Retention horizons for `lore maintenance` pruning.
    #[serde(default)]
    pub maintenance: MaintenanceConfig,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path)?;
        let mut config: Self = serde_yaml_ng::from_str(&content)?;
        config.validate()?;
        config.vault.dirs.normalize();
        config.graph.apply_vault_defaults(&config.vault.dirs);
        if let Some(parent) = path.parent() {
            config.vault.resolve_relative_to(parent);
        }
        Ok(config)
    }

    pub fn enabled_sources(&self) -> impl Iterator<Item = (&str, &SourceConfig)> {
        self.sources
            .iter()
            .filter(|(_, c)| c.enabled)
            .map(|(id, c)| (id.as_str(), c))
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.vault.root.trim().is_empty() {
            return Err(ConfigError::Validation(
                "vault.root must not be empty or whitespace-only".into(),
            ));
        }

        if self.sources.is_empty() {
            return Err(ConfigError::Validation("no sources defined".into()));
        }
        for id in self.sources.keys() {
            if id.is_empty() {
                return Err(ConfigError::Validation("source ID cannot be empty".into()));
            }
            if id.contains('/') || id.contains('\\') {
                return Err(ConfigError::Validation(format!(
                    "source ID '{id}' must not contain path separators"
                )));
            }
            if id == "." || id == ".." {
                return Err(ConfigError::Validation(format!(
                    "source ID '{id}' is a path traversal component"
                )));
            }
        }

        for (id, sc) in &self.sources {
            // Earlier rules' lowercased keywords, for the reachability proof below.
            let mut earlier: Vec<(&str, String)> = Vec::new();
            for rule in &sc.classify {
                if rule.category.trim().is_empty() {
                    return Err(ConfigError::Validation(format!(
                        "sources.{id}.classify: rule has a blank category name"
                    )));
                }
                // Lowercased exactly as `classify_by_keywords` prepares them, so the
                // reachability check below sees the same keywords matching will.
                let lowered: Vec<String> = rule
                    .keywords
                    .iter()
                    .filter(|kw| !kw.trim().is_empty())
                    .map(|kw| kw.to_lowercase())
                    .collect();
                if lowered.is_empty() {
                    return Err(ConfigError::Validation(format!(
                        "sources.{id}.classify: category '{}' has no non-blank keywords",
                        rule.category
                    )));
                }
                // The performance bridge must target a real performance category, or
                // the work-log distribution would silently drop the rule's events into
                // "uncategorized" — the same fail-fast contract as the source maps.
                if let Some(pc) = &rule.performance_category
                    && !self.performance.performance_categories.contains(pc)
                {
                    return Err(ConfigError::Validation(format!(
                        "sources.{id}.classify: rule '{}' performance_category '{pc}' is not in \
                         performance.performance_categories",
                        rule.category
                    )));
                }
                // Reachability proof: rules are first-match-wins, so this rule is dead
                // config iff every event it would match is claimed by an earlier rule.
                // That holds whenever EVERY keyword bounded-contains some earlier rule's
                // keyword (`contains_bounded` is transitive across containment — see its
                // doc), e.g. an earlier `ai` shadows a later `ai ethics`. A sufficient
                // condition only — a rule with one unshadowed keyword is reachable and
                // passes — so this can never reject a working priority ordering. The fix
                // is to put the more specific rule first, or delete the dead one.
                let shadowed: Option<Vec<(&str, &(&str, String))>> = lowered
                    .iter()
                    .map(|kw| {
                        earlier
                            .iter()
                            .find(|(_, prior)| crate::text::contains_bounded(kw, prior))
                            .map(|hit| (kw.as_str(), hit))
                    })
                    .collect();
                if let Some(hits) = shadowed
                    && let Some((kw, (prior_cat, prior_kw))) = hits.first()
                {
                    return Err(ConfigError::Validation(format!(
                        "sources.{id}.classify: rule '{}' is unreachable — every keyword is \
                         already matched by an earlier rule (keyword '{kw}' is covered by rule \
                         '{prior_cat}'s keyword '{prior_kw}'); put the more specific rule first \
                         or remove it",
                        rule.category
                    )));
                }
                earlier.extend(lowered.into_iter().map(|kw| (rule.category.as_str(), kw)));
            }
        }

        if self.sources.values().all(|sc| !sc.enabled) {
            tracing::warn!("all sources are disabled; ingest will have nothing to do");
        }

        if self.identity.name.trim().is_empty() {
            return Err(ConfigError::Validation(
                "identity.name must not be empty".into(),
            ));
        }
        if self.identity.email.trim().is_empty() {
            return Err(ConfigError::Validation(
                "identity.email must not be empty".into(),
            ));
        }

        self.vault.dirs.validate()?;

        if let Some(tz_name) = self.vault.timezone.as_deref()
            && tz_name != "system"
        {
            jiff::tz::TimeZone::get(tz_name)
                .map_err(|_| ConfigError::Validation(format!("invalid timezone: '{tz_name}'")))?;
        }

        if let Some(tag) = self.vault.locale.as_deref()
            && crate::i18n::Locale::try_from_tag(tag).is_none()
        {
            let supported: Vec<&str> = crate::i18n::Locale::ALL.iter().map(|l| l.tag()).collect();
            return Err(ConfigError::Validation(format!(
                "unsupported vault.locale: '{tag}' (supported: {})",
                supported.join(", ")
            )));
        }

        if !(0.1..=5.0).contains(&self.graph.cluster.resolution) {
            return Err(ConfigError::Validation(format!(
                "graph.cluster.resolution must be in [0.1, 5.0], got {}",
                self.graph.cluster.resolution
            )));
        }

        // Count thresholds whose `0` value is degenerate (everything-is-a-hub /
        // no-community-filter / split-every-category). Rejected up front so the
        // "out-of-range thresholds" guard holds for the integer knobs too.
        if self.graph.metrics.min_hub_degree == 0 {
            return Err(ConfigError::Validation(
                "graph.metrics.min_hub_degree must be >= 1".into(),
            ));
        }
        if !(0.0..=1.0).contains(&self.graph.metrics.concept_near_duplicate_threshold) {
            return Err(ConfigError::Validation(format!(
                "graph.metrics.concept_near_duplicate_threshold must be in [0.0, 1.0], got {}",
                self.graph.metrics.concept_near_duplicate_threshold
            )));
        }
        if self.graph.cluster.min_community_size == 0 {
            return Err(ConfigError::Validation(
                "graph.cluster.min_community_size must be >= 1".into(),
            ));
        }
        if self.graph.cluster.max_iterations == 0 {
            return Err(ConfigError::Validation(
                "graph.cluster.max_iterations must be >= 1".into(),
            ));
        }
        if self.graph.cluster.suggest_min_shared_neighbors == 0 {
            return Err(ConfigError::Validation(
                "graph.cluster.suggest_min_shared_neighbors must be >= 1".into(),
            ));
        }
        if self.concepts.index_split_threshold == 0 {
            return Err(ConfigError::Validation(
                "concepts.index_split_threshold must be >= 1".into(),
            ));
        }

        if self.maintenance.retention_days <= 0 {
            return Err(ConfigError::Validation(
                "maintenance.retention_days must be >= 1".into(),
            ));
        }

        for src_id in &self.synthesis.weekly.include_sources {
            if !self.sources.contains_key(src_id) {
                return Err(ConfigError::Validation(format!(
                    "synthesis.weekly.include_sources references unknown source: '{src_id}'"
                )));
            }
        }

        for src_id in self.performance.source_category_map.keys() {
            if !self.sources.contains_key(src_id) {
                return Err(ConfigError::Validation(format!(
                    "performance.source_category_map references unknown source: '{src_id}'"
                )));
            }
        }
        for category in self
            .performance
            .source_category_map
            .values()
            .chain(self.performance.source_type_category_map.values())
        {
            if !self.performance.performance_categories.contains(category) {
                return Err(ConfigError::Validation(format!(
                    "category '{category}' in source mapping is not in \
                     performance.performance_categories"
                )));
            }
        }

        for (id, sc) in &self.sources {
            if let Some(ref sched) = sc.schedule {
                validate_cron(sched)
                    .map_err(|e| ConfigError::Validation(format!("sources.{id}.schedule: {e}")))?;
            }
        }
        for (period, sched) in self.synthesis.schedules() {
            validate_cron(sched).map_err(|e| {
                ConfigError::Validation(format!("synthesis.{period}.schedule: {e}"))
            })?;
        }

        // A scheduled personal-review period can never run while the performance
        // subsystem is off; reject the contradiction rather than emit a no-op cron.
        // (Weekly is exempt — its cross-source themes run independently of performance.)
        if !self.performance.enabled {
            for (period, cfg) in [
                ("monthly", &self.synthesis.monthly),
                ("quarterly", &self.synthesis.quarterly),
                ("annual", &self.synthesis.annual),
            ] {
                if cfg.enabled && cfg.schedule.is_some() {
                    return Err(ConfigError::Validation(format!(
                        "synthesis.{period} is scheduled but performance.enabled is false — \
                         the review can never run; enable performance or clear \
                         synthesis.{period}.schedule"
                    )));
                }
            }
        }

        {
            let mut seen = std::collections::HashSet::new();
            for cat in &self.concepts.categories {
                if cat.id.trim().is_empty() {
                    return Err(ConfigError::Validation(
                        "concepts.categories: category id must not be empty".into(),
                    ));
                }
                if !cat
                    .id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-')
                {
                    return Err(ConfigError::Validation(format!(
                        "concepts.categories: category id '{}' must contain only ASCII alphanumeric characters and hyphens",
                        cat.id
                    )));
                }
                if cat.label.trim().is_empty() {
                    return Err(ConfigError::Validation(format!(
                        "concepts.categories: category '{}' has an empty label",
                        cat.id
                    )));
                }
                if !seen.insert(&cat.id) {
                    return Err(ConfigError::Validation(format!(
                        "concepts.categories: duplicate category id '{}'",
                        cat.id
                    )));
                }
            }
        }

        for dir in &self.graph.scope.dirs {
            let s = dir.to_string_lossy();
            if s.is_empty() || dir.is_absolute() || s.contains("..") {
                return Err(ConfigError::Validation(format!(
                    "graph.scope.dirs entry '{s}' must be a relative path without '..'"
                )));
            }
        }

        Ok(())
    }
}

fn validate_cron(expr: &str) -> Result<(), String> {
    let parts: Vec<&str> = expr.split_whitespace().collect();
    if parts.len() != 5 {
        return Err(format!(
            "expected 5 space-separated fields, got {}: '{expr}'",
            parts.len()
        ));
    }
    let ranges: [(u8, u8); 5] = [(0, 59), (0, 23), (1, 31), (1, 12), (0, 7)];
    let names = ["minute", "hour", "day-of-month", "month", "day-of-week"];

    for (i, field) in parts.iter().enumerate() {
        if field.is_empty() {
            return Err(format!("{}: empty field", names[i]));
        }
        validate_cron_field(field, ranges[i].0, ranges[i].1)
            .map_err(|e| format!("{}: {e}", names[i]))?;
    }
    Ok(())
}

fn validate_cron_field(field: &str, min: u8, max: u8) -> Result<(), String> {
    if field == "*" {
        return Ok(());
    }
    if let Some(step_str) = field.strip_prefix("*/") {
        let step: u8 = step_str
            .parse()
            .map_err(|_| format!("invalid step: '{field}'"))?;
        if step == 0 {
            return Err(format!("step cannot be zero: '{field}'"));
        }
        return Ok(());
    }

    for part in field.split(',') {
        let (base, step_part) = match part.split_once('/') {
            Some((b, s)) => (b, Some(s)),
            None => (part, None),
        };
        if let Some(s) = step_part {
            let step: u8 = s.parse().map_err(|_| format!("invalid step: '{part}'"))?;
            if step == 0 {
                return Err(format!("step cannot be zero: '{part}'"));
            }
        }
        if let Some((start_str, end_str)) = base.split_once('-') {
            let start: u8 = start_str
                .parse()
                .map_err(|_| format!("invalid range start: '{part}'"))?;
            let end: u8 = end_str
                .parse()
                .map_err(|_| format!("invalid range end: '{part}'"))?;
            if start < min || end > max || start > end {
                return Err(format!(
                    "range out of bounds: '{part}' (allowed: {min}-{max})"
                ));
            }
        } else {
            let v: u8 = base
                .parse()
                .map_err(|_| format!("invalid value: '{part}'"))?;
            if v < min || v > max {
                return Err(format!("value out of bounds: '{v}' (allowed: {min}-{max})"));
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultConfig {
    pub root: String,
    #[serde(default)]
    pub dirs: VaultDirs,
    #[serde(default)]
    pub timezone: Option<String>,
    /// Output language for the labels/headers Lorekeeper *adds* (e.g. "ko", "en").
    /// Source content (mail/Slack/Jira bodies) is never translated. Absent → Ko;
    /// an unrecognized tag is rejected at load (see `Config::validate`).
    #[serde(default)]
    pub locale: Option<String>,
}

impl VaultConfig {
    pub fn root_path(&self) -> PathBuf {
        expand_tilde(&self.root)
    }

    /// Output locale for added labels/headers, parsed from `vault.locale`.
    pub fn locale(&self) -> crate::i18n::Locale {
        crate::i18n::Locale::from_tag(self.locale.as_deref())
    }

    /// Resolve a relative vault root against the config file's parent directory so the
    /// vault location is anchored to the config file, not to the process CWD.
    pub(crate) fn resolve_relative_to(&mut self, base: &Path) {
        let expanded = expand_tilde(&self.root);
        if expanded.is_relative() {
            let absolute = base.join(&expanded);
            self.root = absolute.to_string_lossy().into_owned();
        } else {
            self.root = expanded.to_string_lossy().into_owned();
        }
    }

    pub fn timezone(&self) -> jiff::tz::TimeZone {
        match self.timezone.as_deref() {
            Some("system") | None => jiff::tz::TimeZone::system(),
            Some(name) => jiff::tz::TimeZone::get(name).expect("valid timezone"),
        }
    }
}

/// Vault directory layout. Three top-level roots — `daily`, `wiki`, and
/// `synthesis` — plus a `personal` root for all individual performance tracking.
/// `weekly`, `monthly`, `quarterly`, and `annual` are time-period names used as
/// subdirectories within both `personal` (e.g. `me/weekly/`) and `synthesis`
/// (e.g. `synthesis/weekly/`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VaultDirs {
    pub daily: String,
    pub weekly: String,
    pub monthly: String,
    pub quarterly: String,
    pub annual: String,
    pub personal: String,
    pub synthesis: String,
    pub wiki: String,
}

impl Default for VaultDirs {
    fn default() -> Self {
        Self {
            daily: "daily".into(),
            weekly: "weekly".into(),
            monthly: "monthly".into(),
            quarterly: "quarterly".into(),
            annual: "annual".into(),
            personal: "me".into(),
            synthesis: "synthesis".into(),
            wiki: "wiki".into(),
        }
    }
}

impl VaultDirs {
    fn validate(&self) -> Result<(), ConfigError> {
        let fields = [
            ("daily", &self.daily),
            ("weekly", &self.weekly),
            ("monthly", &self.monthly),
            ("quarterly", &self.quarterly),
            ("annual", &self.annual),
            ("personal", &self.personal),
            ("synthesis", &self.synthesis),
            ("wiki", &self.wiki),
        ];
        for (name, value) in fields {
            validate_vault_dir(name, value)?;
        }
        Ok(())
    }

    pub fn normalize(&mut self) {
        use std::path::{Component, Path as StdPath};
        for field in [
            &mut self.daily,
            &mut self.weekly,
            &mut self.monthly,
            &mut self.quarterly,
            &mut self.annual,
            &mut self.personal,
            &mut self.synthesis,
            &mut self.wiki,
        ] {
            let normalized: String = StdPath::new(field.as_str())
                .components()
                .filter_map(|c| match c {
                    Component::Normal(s) => Some(s.to_string_lossy()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("/");
            if normalized != *field {
                *field = normalized;
            }
        }
    }
}

fn validate_vault_dir(field: &str, value: &str) -> Result<(), ConfigError> {
    use std::path::{Component, Path};

    if value.is_empty() {
        return Err(ConfigError::Validation(format!(
            "vault.dirs.{field} must not be empty"
        )));
    }
    let path = Path::new(value);
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir => {}
            _ => {
                return Err(ConfigError::Validation(format!(
                    "vault.dirs.{field} ('{value}') must be a relative path inside the vault \
                     (no '..', absolute, or drive-prefixed segments)"
                )));
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub name: String,
    pub email: String,
    /// Slack user id (`U…`), matched against message authors to attribute the user's
    /// own posts. Jira ownership needs no config: the adapter compares against the
    /// authenticated account from `/myself`.
    #[serde(default)]
    pub slack_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceConfig {
    #[serde(rename = "type")]
    pub source_type: SourceType,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub schedule: Option<String>,
    #[serde(default = "empty_object")]
    pub params: serde_json::Value,
    /// Ordered rules for deterministic keyword classification. A source-level
    /// concern (read by the pipeline), kept out of `params` so adapter params can
    /// reject unknown keys without colliding with this cross-cutting field.
    /// First matching rule wins; order is preserved.
    #[serde(default)]
    pub classify: Vec<ClassifyRule>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub extract_concepts: bool,
    /// Optional natural-language relevance criterion. When set, the LLM keeps only
    /// concepts (and summary content) matching it — so a broad source (e.g. a news
    /// aggregator) contributes focused knowledge instead of off-topic noise.
    #[serde(default)]
    pub focus: Option<String>,
    #[serde(default)]
    pub track_personal: bool,
}

impl SourceConfig {
    /// The relevance focus, normalized: blank or whitespace-only is treated as
    /// "no focus". Single source of truth so every consumer — both LLM provider
    /// paths and the queue-draining skill — sees the same `Option`, never a
    /// spurious empty-string filter.
    pub fn normalized_focus(&self) -> Option<String> {
        self.focus
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    }
}

fn yes() -> bool {
    true
}

fn empty_object() -> serde_json::Value {
    serde_json::Value::Object(Default::default())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceType {
    GoogleDrive,
    Gmail,
    SlackChannel,
    SlackSearch,
    Jira,
    GoogleCalendar,
    /// RSS/Atom feed reader for external knowledge sources (vendor blogs, news
    /// aggregators). Public HTTP, no credentials. One source can poll many feeds.
    Rss,
    /// User-curated inbox: files dropped into `<vault>/inbox/` are picked up,
    /// processed through the same pipeline as automated sources, and archived.
    Manual,
}

impl SourceType {
    pub fn events_heading(self, strings: &crate::i18n::Strings) -> &'static str {
        match self {
            SourceType::SlackChannel | SourceType::SlackSearch => strings.key_messages,
            _ => strings.key_events,
        }
    }

    /// Whether the source fetches a rolling, capped window that CANNOT completely
    /// re-fetch a past day. An RSS feed drops old entries as new ones arrive, so an item
    /// observed on day N is gone from the feed before a later run re-renders page N —
    /// rendering from the fetch alone would silently lose it. Streaming sources therefore
    /// project their daily pages from the per-date event log (accumulate, never deplete).
    ///
    /// Complete-refetch sources (Gmail/Jira/Calendar/Slack/Drive) reproduce their whole
    /// window on demand via date-range queries, so they render directly from the fetch and
    /// keep no log — there is nothing to accumulate. `Manual` is neither: it produces
    /// document pages (one per inbox file), not a daily aggregation.
    pub fn is_streaming(self) -> bool {
        matches!(self, SourceType::Rss)
    }

    /// Default Jinja template filename for this source type. User overrides
    /// (`{source-id}.md.jinja`) take precedence at render time; this is the
    /// type-level fallback.
    pub fn default_template_name(self) -> &'static str {
        match self {
            SourceType::Gmail => "gmail.md.jinja",
            SourceType::GoogleDrive => "google-drive.md.jinja",
            SourceType::GoogleCalendar => "google-calendar.md.jinja",
            SourceType::SlackChannel => "slack-channel.md.jinja",
            SourceType::SlackSearch => "slack-search.md.jinja",
            SourceType::Jira => "jira.md.jinja",
            SourceType::Rss => "rss.md.jinja",
            SourceType::Manual => "document.md.jinja",
        }
    }

    /// Categories surfaced as dedicated highlight sections ABOVE the full event list on
    /// this source's daily page. For each entry the renderer exposes `{category}_items`
    /// (events whose `Event::category` matches) and `{category}_count` to the template.
    /// Every event still renders under the full event list regardless of category, so a
    /// category outside this set is never hidden — the buckets are a highlight view, not
    /// a filter. A source that returns `&[]` renders straight from the event list and
    /// pays for no filtering; the renderer carries no per-type branching.
    pub fn highlight_categories(self) -> &'static [&'static str] {
        match self {
            SourceType::Gmail => &[
                "action_required",
                "decisions",
                "project_updates",
                "knowledge_sharing",
                "meeting_followup",
            ],
            _ => &[],
        }
    }
}

impl std::fmt::Display for SourceType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = serde_json::to_value(self)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| format!("{self:?}"));
        f.write_str(&s)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PerformanceConfig {
    pub enabled: bool,
    pub performance_categories: Vec<String>,
    /// Per-source-ID category override (highest priority).
    pub source_category_map: BTreeMap<String, String>,
    /// Per-source-type default category (fallback when source_category_map has no entry).
    pub source_type_category_map: BTreeMap<SourceType, String>,
    /// Label used for events that match no category. When `None`, falls back
    /// to the locale-appropriate default via `uncategorized_label()`.
    pub uncategorized_label: Option<String>,
}

impl PerformanceConfig {
    /// Resolve the uncategorized label, falling back to the locale default.
    pub fn uncategorized_label(&self, locale: crate::i18n::Locale) -> &str {
        self.uncategorized_label
            .as_deref()
            .unwrap_or(locale.strings().uncategorized)
    }

    /// Resolve the performance category for an event. Precedence, most to least
    /// specific:
    /// 1. `source_category_map[source_id]` — an explicit per-source intent.
    /// 2. `performance_category` — the event's own content signal, set by a
    ///    `classify` rule's `performance_category` bridge. This OUTRANKS the per-type
    ///    map so a genuine content signal (e.g. a Jira issue whose body marks it as
    ///    `innovation`) wins over the coarse "all Jira = project-delivery" default.
    /// 3. `source_type_category_map[source_type]` — the coarse fallback.
    ///
    /// All three already draw from `performance_categories` (the rule bridge and the
    /// maps are validated against it at load), so no membership re-check is needed here.
    pub fn resolve_category(
        &self,
        source_id: &str,
        source_type: SourceType,
        performance_category: Option<&str>,
    ) -> Option<String> {
        if let Some(c) = self.source_category_map.get(source_id) {
            return Some(c.clone());
        }
        if let Some(c) = performance_category {
            return Some(c.to_string());
        }
        if let Some(c) = self.source_type_category_map.get(&source_type) {
            return Some(c.clone());
        }
        None
    }
}

impl Default for PerformanceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            performance_categories: vec![
                "project-delivery".into(),
                "technical-leadership".into(),
                "team-contribution".into(),
                "innovation".into(),
                "operational-excellence".into(),
            ],
            source_category_map: BTreeMap::new(),
            source_type_category_map: BTreeMap::new(),
            uncategorized_label: None,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SynthesisConfig {
    pub weekly: WeeklySynthesisConfig,
    pub monthly: PersonalReviewConfig,
    pub quarterly: PersonalReviewConfig,
    pub annual: PersonalReviewConfig,
}

impl SynthesisConfig {
    /// Iterator of `(period_name, cron_expression)` for enabled synthesis periods only.
    pub fn schedules(&self) -> impl Iterator<Item = (&'static str, &String)> {
        let entries: [(&'static str, bool, Option<&String>); 4] = [
            ("weekly", self.weekly.enabled, self.weekly.schedule.as_ref()),
            (
                "monthly",
                self.monthly.enabled,
                self.monthly.schedule.as_ref(),
            ),
            (
                "quarterly",
                self.quarterly.enabled,
                self.quarterly.schedule.as_ref(),
            ),
            ("annual", self.annual.enabled, self.annual.schedule.as_ref()),
        ];
        entries.into_iter().filter_map(|(name, enabled, sched)| {
            if enabled {
                sched.map(|s| (name, s))
            } else {
                None
            }
        })
    }
}

/// Weekly synthesis carries the cross-source themes page on top of the weekly review
/// narrative, so it alone takes `include_sources`. The other periods are
/// work-log-only performance reviews and share the leaner `PersonalReviewConfig`.
/// "Synthesis" is reserved for the cross-source weekly themes; the monthly/quarterly/
/// annual periods are personal performance *reviews*, named accordingly.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WeeklySynthesisConfig {
    pub enabled: bool,
    pub schedule: Option<String>,
    /// Sources rolled up into the cross-source weekly themes page. Opt-in: an empty
    /// list produces no themes page. List work/communication sources (team chat,
    /// tickets) where a weekly thematic recap adds value — not knowledge feeds, whose
    /// value already lives in the continuously-accumulated concept graph.
    pub include_sources: Vec<String>,
}

impl Default for WeeklySynthesisConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            schedule: None,
            include_sources: vec![],
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PersonalReviewConfig {
    pub enabled: bool,
    pub schedule: Option<String>,
}

impl Default for PersonalReviewConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            schedule: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LlmConfig {
    pub provider: LlmProvider,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            provider: LlmProvider::Queue,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LlmProvider {
    /// Emit JSONL queue tasks under `<vault>/.lorekeeper/queue/`. A Claude Code skill
    /// (`/lore-process`) consumes the queue and edits target pages via Obsidian MCP using
    /// Claude Code's native LLM session (no API key, no separate billing).
    Queue,
    /// No LLM work. Daily pages render without summary/concepts sections. Useful for
    /// development, CI, or vault-only sources where Rust templating is sufficient.
    Noop,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConceptConfig {
    pub categories: Vec<ConceptCategory>,
    /// When the total concept count exceeds this threshold, the wiki index
    /// splits into per-category sub-pages (`wiki/index/{category}.md`).
    /// Below the threshold, all concepts render inline in `wiki/index.md`.
    pub index_split_threshold: usize,
}

impl Default for ConceptConfig {
    fn default() -> Self {
        Self {
            categories: Vec::new(),
            index_split_threshold: 100,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConceptCategory {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MaintenanceConfig {
    /// Retention horizon (days) for `lore maintenance`: prunes the ingest log, drained
    /// `queue/processed/` files, and per-date streaming event logs older than this.
    /// User-facing pages are permanent — this only trims operational history. Dropping a
    /// streaming source's long-past event log forfeits re-projecting that day, never live
    /// data: the frozen page already holds its items.
    pub retention_days: i64,
}

impl Default for MaintenanceConfig {
    fn default() -> Self {
        Self { retention_days: 90 }
    }
}

/// Wikilink graph analysis configuration, consumed by `lore graph` / `lk-graph`.
/// Splits into analysis `scope`, structural `metrics`, and `cluster` settings.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GraphConfig {
    pub scope: GraphScopeConfig,
    pub metrics: GraphMetricsConfig,
    pub cluster: GraphClusterConfig,
}

impl GraphConfig {
    pub fn apply_vault_defaults(&mut self, vault_dirs: &VaultDirs) {
        if self.scope.dirs.is_empty() {
            self.scope.dirs = vec![PathBuf::from(&vault_dirs.wiki)];
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GraphScopeConfig {
    /// Vault-relative directories for structural analysis
    /// (`hubs`/`cluster`/`suggest-links`). Derived from `vault.dirs.wiki` when
    /// absent. Integrity checks (`broken`/`orphans`/`index-sync`) resolve against
    /// the full vault regardless.
    pub dirs: Vec<PathBuf>,
    /// Glob patterns (matched against vault-relative paths) to exclude from the scan.
    pub exclude: Vec<String>,
    /// Whether the walker follows symlinks.
    pub follow_links: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GraphMetricsConfig {
    /// Minimum total degree for a page to count as a hub in `lint`.
    pub min_hub_degree: usize,
    /// Page ids never reported as orphans (e.g. index/MOC pages).
    pub orphan_exclude: Vec<String>,
    /// Sørensen-Dice similarity (on separator-stripped slugs) at or above which two
    /// concept slugs are reported as near-duplicate merge candidates by `lint`.
    /// Higher = fewer, higher-confidence pairs.
    pub concept_near_duplicate_threshold: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GraphClusterConfig {
    /// Louvain resolution parameter; higher favors more, smaller communities.
    pub resolution: f64,
    /// Maximum local-moving passes per aggregation level before that level stops.
    pub max_iterations: usize,
    /// Communities smaller than this are dropped from results.
    pub min_community_size: usize,
    /// `suggest-links` only proposes a pair sharing at least this many neighbors.
    /// The graph is dominated by daily/document→concept edges, so a single shared
    /// neighbor usually means "co-cited by one note", not a real relationship —
    /// the default of 2 suppresses that co-citation noise.
    pub suggest_min_shared_neighbors: usize,
}

impl Default for GraphMetricsConfig {
    fn default() -> Self {
        Self {
            min_hub_degree: 5,
            orphan_exclude: Vec::new(),
            // 0.6 catches genuine spelling variants (`vector-db` ~ `vector-database`
            // = 0.6) that fragment the graph. Version families (`gpt-4`/`gpt-4o`) are
            // excluded separately by `is_version_variant`, so this can favor recall
            // without the model-version false positives that would otherwise need a
            // higher cutoff.
            concept_near_duplicate_threshold: 0.6,
        }
    }
}

impl Default for GraphClusterConfig {
    fn default() -> Self {
        Self {
            resolution: 1.0,
            max_iterations: 100,
            min_community_size: 1,
            suggest_min_shared_neighbors: 2,
        }
    }
}

/// Expand `~` / a leading `~/` to the user's home directory. The single source
/// for tilde expansion in user-supplied config paths (`vault.root`, the manual
/// source's `inbox_dir`) — config values are written by humans in a shell
/// mindset, but nothing else expands `~` for us.
pub fn expand_tilde(path: &str) -> PathBuf {
    if path == "~"
        && let Ok(home) = std::env::var("HOME")
    {
        return PathBuf::from(home);
    }
    if let Some(rest) = path.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_example_config() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config.example.yaml");
        if path.exists() {
            let config = Config::load(&path).expect("should parse example config");
            assert!(!config.sources.is_empty());
            assert!(config.sources.contains_key("ai-news"));
        }
    }

    #[test]
    fn normalized_focus_treats_blank_as_none() {
        let mk = |f: Option<&str>| SourceConfig {
            source_type: SourceType::Rss,
            enabled: true,
            schedule: None,
            params: empty_object(),
            classify: vec![],
            labels: vec![],
            extract_concepts: true,
            focus: f.map(str::to_owned),
            track_personal: false,
        };
        assert_eq!(mk(None).normalized_focus(), None);
        assert_eq!(mk(Some("")).normalized_focus(), None);
        assert_eq!(mk(Some("   ")).normalized_focus(), None);
        assert_eq!(
            mk(Some("  AI/ML only  ")).normalized_focus(),
            Some("AI/ML only".to_string())
        );
    }

    #[test]
    fn source_type_serde_roundtrip() {
        let json = serde_json::to_string(&SourceType::GoogleDrive).unwrap();
        assert_eq!(json, "\"google-drive\"");
        let parsed: SourceType = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, SourceType::GoogleDrive);
    }

    #[test]
    fn reject_source_id_with_path_separator() {
        let yaml = r#"
vault:
  root: /tmp/vault
identity:
  name: test
  email: test@test.com
sources:
  bad/id:
    type: gmail
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn expand_tilde_works() {
        let expanded = expand_tilde("~/Documents");
        assert!(!expanded.to_string_lossy().contains('~'));
        // A bare `~` is the home directory itself — shell convention, and the
        // `~/...` form's natural degenerate case.
        if let Ok(home) = std::env::var("HOME") {
            assert_eq!(expand_tilde("~"), PathBuf::from(home));
        }
        // `~` is only special as the whole first component.
        assert_eq!(expand_tilde("a/~/b"), PathBuf::from("a/~/b"));
    }

    #[test]
    fn validate_rejects_vault_dir_traversal() {
        let yaml = r#"
vault:
  root: /tmp/vault
  dirs:
    daily: "../../etc"
identity:
  name: test
  email: test@test.com
sources:
  s1:
    type: gmail
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.validate().is_err(), "'..' segment must be rejected");
    }

    #[test]
    fn near_duplicate_threshold_default_catches_real_spelling_variants() {
        // 0.6 is deliberate: `vector-db` ~ `vector-database` scores exactly 0.6, and
        // version families are excluded separately by `is_version_variant`, so the
        // default favors recall without model-version false positives. A bump to 0.7
        // would silently stop catching that canonical variant — trip this test first.
        assert_eq!(
            GraphMetricsConfig::default().concept_near_duplicate_threshold,
            0.6
        );
    }

    #[test]
    fn validate_rejects_absolute_vault_dir() {
        let yaml = r#"
vault:
  root: /tmp/vault
  dirs:
    wiki: "/etc/passwd"
identity:
  name: test
  email: test@test.com
sources:
  s1:
    type: gmail
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.validate().is_err(), "absolute path must be rejected");
    }

    #[test]
    fn validate_accepts_nested_vault_dir() {
        let yaml = r#"
vault:
  root: /tmp/vault
  dirs:
    daily: "sources/daily"
identity:
  name: test
  email: test@test.com
sources:
  s1:
    type: gmail
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(
            config.validate().is_ok(),
            "a normal nested relative path must be allowed"
        );
    }

    #[test]
    fn default_llm_provider_is_queue() {
        assert_eq!(LlmConfig::default().provider, LlmProvider::Queue);
    }

    #[test]
    fn maintenance_retention_defaults_to_90_and_rejects_non_positive() {
        let base = |days: &str| {
            format!(
                r#"
vault:
  root: /tmp/vault
identity:
  name: test
  email: test@test.com
sources:
  s1:
    type: gmail
maintenance:
  retention_days: {days}
"#
            )
        };
        assert_eq!(MaintenanceConfig::default().retention_days, 90);
        let ok: Config = serde_yaml_ng::from_str(&base("120")).unwrap();
        assert!(ok.validate().is_ok());
        let bad: Config = serde_yaml_ng::from_str(&base("0")).unwrap();
        assert!(
            bad.validate().is_err(),
            "retention_days=0 should be rejected"
        );
    }

    #[test]
    fn validate_rejects_invalid_timezone() {
        let yaml = r#"
vault:
  root: /tmp/vault
  timezone: Not/A/Zone
identity:
  name: test
  email: test@test.com
sources:
  s1:
    type: gmail
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_rejects_unrecognized_locale() {
        let yaml = r#"
vault:
  root: /tmp/vault
  locale: fr
identity:
  name: test
  email: test@test.com
sources:
  s1:
    type: gmail
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("vault.locale"));
    }

    #[test]
    fn validate_rejects_bad_cron_field_count() {
        let yaml = r#"
vault:
  root: /tmp/vault
identity:
  name: test
  email: test@test.com
sources:
  s1:
    type: gmail
    schedule: "0 7 *"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_rejects_cron_out_of_range() {
        let yaml = r#"
vault:
  root: /tmp/vault
identity:
  name: test
  email: test@test.com
sources:
  s1:
    type: gmail
    schedule: "0 25 * * *"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.validate().is_err(), "hour=25 should be rejected");
    }

    #[test]
    fn validate_rejects_zero_step() {
        let yaml = r#"
vault:
  root: /tmp/vault
identity:
  name: test
  email: test@test.com
sources:
  s1:
    type: gmail
    schedule: "*/0 * * * *"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_rejects_non_numeric_step() {
        let yaml = r#"
vault:
  root: /tmp/vault
identity:
  name: test
  email: test@test.com
sources:
  s1:
    type: gmail
    schedule: "0-29/foo * * * *"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn schedules_excludes_disabled() {
        let yaml = r#"
vault:
  root: /tmp/vault
identity:
  name: test
  email: test@test.com
sources:
  s1:
    type: gmail
synthesis:
  weekly:
    enabled: false
    schedule: "0 8 * * 1"
  monthly:
    enabled: true
    schedule: "0 8 1 * *"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        config.validate().unwrap();
        let names: Vec<_> = config.synthesis.schedules().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["monthly"], "disabled weekly should be excluded");
    }

    #[test]
    fn relative_vault_root_resolved_against_config_dir() {
        let dir = std::env::temp_dir().join("wi-config-relative-test");
        std::fs::create_dir_all(&dir).unwrap();
        let cfg_path = dir.join("config.yaml");
        std::fs::write(
            &cfg_path,
            r#"
vault:
  root: my-vault
identity:
  name: test
  email: test@test.com
sources:
  s1:
    type: gmail
"#,
        )
        .unwrap();
        let config = Config::load(&cfg_path).unwrap();
        let resolved = config.vault.root_path();
        assert!(
            resolved.starts_with(&dir),
            "vault root should be anchored to config dir, got: {}",
            resolved.display()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_accepts_cron_with_range() {
        let yaml = r#"
vault:
  root: /tmp/vault
identity:
  name: test
  email: test@test.com
sources:
  s1:
    type: gmail
    schedule: "0 7 * * 1-5"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_rejects_unknown_synthesis_source() {
        let yaml = r#"
vault:
  root: /tmp/vault
identity:
  name: test
  email: test@test.com
sources:
  s1:
    type: gmail
synthesis:
  weekly:
    include_sources: [nonexistent]
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_rejects_scheduled_review_when_performance_disabled() {
        let yaml = r#"
vault:
  root: /tmp/vault
identity:
  name: test
  email: test@test.com
sources:
  s1:
    type: gmail
performance:
  enabled: false
synthesis:
  monthly:
    schedule: "0 8 1 * *"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(
            config.validate().is_err(),
            "scheduling a personal review with performance disabled must be rejected"
        );
    }

    #[test]
    fn validate_rejects_unknown_category() {
        let yaml = r#"
vault:
  root: /tmp/vault
identity:
  name: test
  email: test@test.com
sources:
  s1:
    type: jira
performance:
  source_category_map:
    s1: nonexistent-category
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn resolve_category_prefers_source_id() {
        let mut perf = PerformanceConfig::default();
        perf.source_category_map
            .insert("my-tasks".into(), "innovation".into());
        let cat = perf.resolve_category("my-tasks", SourceType::Jira, None);
        assert_eq!(cat.as_deref(), Some("innovation"));
    }

    #[test]
    fn resolve_category_falls_back_to_source_type() {
        let mut perf = PerformanceConfig::default();
        perf.source_type_category_map
            .insert(SourceType::Jira, "project-delivery".into());
        let cat = perf.resolve_category("other-jira", SourceType::Jira, None);
        assert_eq!(cat.as_deref(), Some("project-delivery"));
    }

    #[test]
    fn resolve_category_content_signal_outranks_source_type_map() {
        // A `performance_category` from a classify-rule bridge must win over the
        // coarse per-source-type default: a Jira issue the content marks as
        // `innovation` is innovation, not the blanket "all Jira = project-delivery".
        let mut perf = PerformanceConfig::default();
        perf.source_type_category_map
            .insert(SourceType::Jira, "project-delivery".into());
        assert_eq!(
            perf.resolve_category("any", SourceType::Jira, Some("innovation"))
                .as_deref(),
            Some("innovation"),
            "content signal must outrank the per-source-type fallback"
        );
        assert_eq!(
            perf.resolve_category("any", SourceType::Jira, None)
                .as_deref(),
            Some("project-delivery"),
            "with no content signal the per-source-type default applies"
        );
    }

    #[test]
    fn graph_config_defaults_when_absent() {
        let yaml = r#"
vault:
  root: /tmp/vault
identity:
  name: test
  email: test@test.com
sources:
  s1:
    type: gmail
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.graph.scope.dirs.is_empty());
        assert_eq!(config.graph.metrics.min_hub_degree, 5);
        assert_eq!(config.graph.cluster.resolution, 1.0);
        assert_eq!(config.graph.cluster.max_iterations, 100);
        assert_eq!(config.graph.cluster.min_community_size, 1);
    }

    #[test]
    fn graph_config_parses_overrides() {
        let yaml = r#"
vault:
  root: /tmp/vault
identity:
  name: test
  email: test@test.com
sources:
  s1:
    type: gmail
graph:
  scope:
    dirs: [wiki, docs]
    exclude: ["wiki/log.md"]
    follow_links: true
  metrics:
    min_hub_degree: 3
    orphan_exclude: ["wiki/index"]
  cluster:
    resolution: 1.5
    max_iterations: 50
    min_community_size: 2
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.graph.scope.dirs.len(), 2);
        assert!(config.graph.scope.follow_links);
        assert_eq!(config.graph.metrics.min_hub_degree, 3);
        assert_eq!(config.graph.metrics.orphan_exclude, vec!["wiki/index"]);
        assert_eq!(config.graph.cluster.resolution, 1.5);
        assert_eq!(config.graph.cluster.min_community_size, 2);
    }

    #[test]
    fn validate_rejects_resolution_out_of_range() {
        let yaml = r#"
vault:
  root: /tmp/vault
identity:
  name: test
  email: test@test.com
sources:
  s1:
    type: gmail
graph:
  cluster:
    resolution: 10.0
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(
            config.validate().is_err(),
            "resolution=10.0 should be rejected"
        );

        let yaml_low = r#"
vault:
  root: /tmp/vault
identity:
  name: test
  email: test@test.com
sources:
  s1:
    type: gmail
graph:
  cluster:
    resolution: 0.05
"#;
        let config_low: Config = serde_yaml_ng::from_str(yaml_low).unwrap();
        assert!(
            config_low.validate().is_err(),
            "resolution=0.05 should be rejected"
        );
    }

    #[test]
    fn validate_rejects_classify_rule_with_empty_keywords() {
        let yaml = r#"
vault:
  root: /tmp/vault
identity:
  name: test
  email: test@test.com
sources:
  s1:
    type: gmail
    classify:
      - category: action_required
        keywords: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(
            config.validate().is_err(),
            "classify rule with empty keywords must be rejected"
        );
    }

    #[test]
    fn validate_rejects_unreachable_classify_rule() {
        // First-match-wins: an earlier "ai" claims every event a later "AI ethics"
        // would match (case-insensitive, bounded containment) — the later rule is
        // provably dead config and must fail at load, not silently never fire.
        let yaml = r#"
vault:
  root: /tmp/vault
identity:
  name: test
  email: test@test.com
sources:
  s1:
    type: gmail
    classify:
      - category: ai_topic
        keywords: ["ai"]
      - category: ethics
        keywords: ["AI ethics"]
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(
            err.contains("unreachable") && err.contains("ethics"),
            "fully-shadowed rule must be rejected with the shadowing pair named: {err}"
        );
    }

    #[test]
    fn validate_accepts_specific_rule_before_general() {
        // The correct priority ordering — specific first, general after — must pass:
        // "ai" alone (no "ethics") still reaches the second rule.
        let yaml = r#"
vault:
  root: /tmp/vault
identity:
  name: test
  email: test@test.com
sources:
  s1:
    type: gmail
    classify:
      - category: ethics
        keywords: ["ai ethics"]
      - category: ai_topic
        keywords: ["ai"]
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(
            config.validate().is_ok(),
            "specific-before-general ordering must be accepted"
        );
    }

    #[test]
    fn validate_accepts_partially_overlapping_rule() {
        // One keyword shadowed, one reachable → the rule still fires on "fairness"
        // and must pass. The check is a sufficient condition only — it can never
        // reject a rule that has any reachable keyword.
        let yaml = r#"
vault:
  root: /tmp/vault
identity:
  name: test
  email: test@test.com
sources:
  s1:
    type: gmail
    classify:
      - category: ai_topic
        keywords: ["ai"]
      - category: ethics
        keywords: ["ai ethics", "fairness"]
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(
            config.validate().is_ok(),
            "a rule with at least one unshadowed keyword is reachable"
        );
    }

    #[test]
    fn validate_rejects_duplicate_classify_rule_keywords() {
        // An exact duplicate keyword in a later rule is the simplest shadow: the
        // earlier rule always wins, so the later rule never fires.
        let yaml = r#"
vault:
  root: /tmp/vault
identity:
  name: test
  email: test@test.com
sources:
  s1:
    type: gmail
    classify:
      - category: action_required
        keywords: ["검토"]
      - category: review_request
        keywords: ["검토"]
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(
            config.validate().is_err(),
            "a later rule whose only keyword duplicates an earlier rule's must be rejected"
        );
    }

    #[test]
    fn validate_rejects_duplicate_concept_category() {
        let yaml = r#"
vault:
  root: /tmp/vault
identity:
  name: test
  email: test@test.com
sources:
  s1:
    type: gmail
concepts:
  categories:
    - id: ai-ml
      label: "AI/ML"
    - id: ai-ml
      label: "Duplicate"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(
            config.validate().is_err(),
            "duplicate concept category id must be rejected"
        );
    }

    #[test]
    fn validate_rejects_concept_category_with_special_chars() {
        let yaml = r#"
vault:
  root: /tmp/vault
identity:
  name: test
  email: test@test.com
sources:
  s1:
    type: gmail
concepts:
  categories:
    - id: "ai/ml"
      label: "AI/ML"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(
            config.validate().is_err(),
            "category id with '/' must be rejected"
        );
    }

    #[test]
    fn validate_accepts_empty_concept_categories() {
        let yaml = r#"
vault:
  root: /tmp/vault
identity:
  name: test
  email: test@test.com
sources:
  s1:
    type: gmail
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.validate().is_ok());
        assert!(config.concepts.categories.is_empty());
    }

    #[test]
    fn graph_config_rejects_unknown_field() {
        let yaml = r#"
vault:
  root: /tmp/vault
identity:
  name: test
  email: test@test.com
sources:
  s1:
    type: gmail
graph:
  metrics:
    typo_field: 1
"#;
        let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
        assert!(result.is_err());
    }

    #[test]
    fn vault_dirs_normalize_cleans_paths() {
        let mut dirs = VaultDirs {
            wiki: "./wiki/".into(),
            daily: "daily/".into(),
            personal: "./me".into(),
            synthesis: "synthesis".into(),
            weekly: "././weekly//".into(),
            ..Default::default()
        };
        dirs.normalize();
        assert_eq!(dirs.wiki, "wiki");
        assert_eq!(dirs.daily, "daily");
        assert_eq!(dirs.personal, "me");
        assert_eq!(dirs.synthesis, "synthesis");
        assert_eq!(dirs.weekly, "weekly");
    }
}
