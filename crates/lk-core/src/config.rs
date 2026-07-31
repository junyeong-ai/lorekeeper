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
    /// review distribution. Must be one of `personal.performance_categories`
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
    /// When ingestion runs. A single daily `lore ingest` (all sources) is the scheduling
    /// unit: the work-log aggregates personal events across every source for a day, so all
    /// sources must be ingested in one run for it to be complete.
    #[serde(default)]
    pub ingest: IngestConfig,
    /// The personal-productivity module: work-log, performance reviews, and the
    /// contribution taxonomy. ABSENT (the default) means a pure, domain-neutral knowledge
    /// engine — no work-log, no reviews, no `is_personal`, no performance categories. Present
    /// opts a single knowledge worker's own activity into tracking. The core never depends
    /// on it; every personal behavior is gated on `config.personal.is_some()`.
    #[serde(default)]
    pub personal: Option<PersonalConfig>,
    #[serde(default)]
    pub synthesis: SynthesisConfig,
    #[serde(default)]
    pub llm: LlmConfig,
    /// Concept extraction and categorization settings.
    #[serde(default)]
    pub concepts: ConceptConfig,
    /// Link graph analysis settings (consumed by `lore graph` / `lk-graph`).
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

    fn validate(&mut self) -> Result<(), ConfigError> {
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
                // The performance bridge targets the personal contribution taxonomy, so it
                // requires a `personal:` section AND a real category there — else the rule's
                // events would silently fall into "uncategorized". Fail-fast on both.
                if let Some(pc) = &rule.performance_category {
                    match &self.personal {
                        None => {
                            return Err(ConfigError::Validation(format!(
                                "sources.{id}.classify: rule '{}' sets performance_category \
                                 '{pc}' but there is no `personal:` section — add one (with this \
                                 category in personal.performance_categories) or drop the bridge",
                                rule.category
                            )));
                        }
                        Some(personal) if !personal.performance_categories.contains(pc) => {
                            return Err(ConfigError::Validation(format!(
                                "sources.{id}.classify: rule '{}' performance_category '{pc}' is \
                                 not in personal.performance_categories",
                                rule.category
                            )));
                        }
                        Some(_) => {}
                    }
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

            let mut seen_highlight: Vec<&str> = Vec::new();
            for h in &sc.highlights {
                if h.category.trim().is_empty() || h.label.trim().is_empty() {
                    return Err(ConfigError::Validation(format!(
                        "sources.{id}.highlights: each entry needs a non-blank category and label"
                    )));
                }
                if seen_highlight.contains(&h.category.as_str()) {
                    return Err(ConfigError::Validation(format!(
                        "sources.{id}.highlights: duplicate category '{}'",
                        h.category
                    )));
                }
                // A highlight surfaces events a `classify` rule routed into `category`; a
                // category no rule produces renders a permanently-empty section, so reject
                // the typo at load instead of failing silently.
                if !sc.classify.iter().any(|r| r.category == h.category) {
                    return Err(ConfigError::Validation(format!(
                        "sources.{id}.highlights: category '{}' is not produced by any \
                         classify rule on this source",
                        h.category
                    )));
                }
                seen_highlight.push(&h.category);
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

        if let Some(personal) = &self.personal {
            for src_id in personal
                .tracked_sources
                .iter()
                .chain(personal.source_category_map.keys())
            {
                if !self.sources.contains_key(src_id) {
                    return Err(ConfigError::Validation(format!(
                        "personal references unknown source: '{src_id}'"
                    )));
                }
            }
            for category in personal
                .source_category_map
                .values()
                .chain(personal.source_type_category_map.values())
            {
                if !personal.performance_categories.contains(category) {
                    return Err(ConfigError::Validation(format!(
                        "category '{category}' in personal source mapping is not in \
                         personal.performance_categories"
                    )));
                }
            }
        }

        if let Some(ref sched) = self.ingest.schedule {
            validate_cron(sched)
                .map_err(|e| ConfigError::Validation(format!("ingest.schedule: {e}")))?;
        }
        if self.synthesis.weekly.enabled
            && let Some(sched) = &self.synthesis.weekly.schedule
        {
            validate_cron(sched)
                .map_err(|e| ConfigError::Validation(format!("synthesis.weekly.schedule: {e}")))?;
        }
        if let Some(personal) = &self.personal {
            for (period, sched) in personal.review_schedules() {
                validate_cron(sched).map_err(|e| {
                    ConfigError::Validation(format!("personal.{period}.schedule: {e}"))
                })?;
            }
        }
        if let Some(ref sched) = self.maintenance.schedule {
            validate_cron(sched)
                .map_err(|e| ConfigError::Validation(format!("maintenance.schedule: {e}")))?;
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
                // A label is a one-line display name, and every consumer treats it as one:
                // `lore config categories` and `lore wiki concepts` emit tab-separated records
                // one per line, and `index.md` renders it as a heading. A control character
                // makes it more than one — a label of `"AI\nfake"` put a category id `fake` in
                // the vocabulary an agent reads, which no config declares, so the page it then
                // wrote failed `lint` under `invalid_categories`. Refused where it enters,
                // rather than escaped in each consumer, which is where they would drift.
                if cat.label.chars().any(char::is_control) {
                    return Err(ConfigError::Validation(format!(
                        "concepts.categories: category '{}' has a label containing a control \
                         character (newline or tab) — a label is one line, and the commands that \
                         publish the vocabulary emit one record per line",
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

        for dir in &mut self.graph.scope.dirs {
            let normalized =
                normalize_vault_relative("graph.scope.dirs entry", &dir.to_string_lossy())?;
            *dir = PathBuf::from(normalized);
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
    // One code path for every member: a plain `*/n` is just a single-element list whose
    // base is `*`, handled below — no separate whole-field branch (which used to mis-parse
    // a list like `*/15,30` by treating `15,30` as one step number).
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
        if base == "*" {
            // `*` (optionally stepped, e.g. `*/15`) is a valid list member: the evaluator
            // (`crate::cron::parse_field`) expands `*` over [min,max] inside a comma list,
            // so the validator must accept the same grammar instead of parsing `*` as a
            // number (which would reject a schedule the engine can actually run).
        } else if let Some((start_str, end_str)) = base.split_once('-') {
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
        let absolute = if expanded.is_relative() {
            base.join(&expanded)
        } else {
            expanded
        };
        // Folded lexically, not canonicalized: `vault.root: ./vault` joined onto the config's
        // directory yields `…/base/./vault`, which is absolute and works but is a SECOND spelling
        // of one path. `lore config vault-root` and `schema-path` are contracts a script or a
        // skill reads and passes on, and two spellings of the same vault fail a string compare.
        // Canonicalizing instead would resolve symlinks and fail outright on a vault that does
        // not exist yet, which is the first run.
        self.root = fold_current_dir(&absolute).to_string_lossy().into_owned();
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
    fn validate(&mut self) -> Result<(), ConfigError> {
        let fields: [(&str, &mut String); 8] = [
            ("daily", &mut self.daily),
            ("weekly", &mut self.weekly),
            ("monthly", &mut self.monthly),
            ("quarterly", &mut self.quarterly),
            ("annual", &mut self.annual),
            ("personal", &mut self.personal),
            ("synthesis", &mut self.synthesis),
            ("wiki", &mut self.wiki),
        ];
        for (name, value) in fields {
            *value = normalize_vault_relative(&format!("vault.dirs.{name}"), value)?;
        }

        // `daily`, `personal`, `synthesis` and `wiki` each ROOT a path, and every consumer
        // classifies a page by which of them it sits under. Nested or equal roots make one
        // page answer to two of those questions at once: a concept under a `wiki` inside
        // `daily` is both a concept page and a daily page, so `lore graph backlinks-sync`
        // reads its curated `## Related` cross-references as provenance and inflates every
        // cited concept's `source_count`. `weekly`/`monthly`/`quarterly`/`annual` are leaf
        // names joined UNDER `personal` or `synthesis` (`{personal}/{weekly}`,
        // `{synthesis}/{weekly}`), so they are expected to repeat and are not compared.
        let roots = [
            ("daily", &self.daily),
            ("personal", &self.personal),
            ("synthesis", &self.synthesis),
            ("wiki", &self.wiki),
        ];
        for (index, (outer_name, outer)) in roots.iter().enumerate() {
            for (inner_name, inner) in &roots[index + 1..] {
                if is_within(outer, inner) || is_within(inner, outer) {
                    return Err(ConfigError::Validation(format!(
                        "vault.dirs.{outer_name} ('{outer}') and vault.dirs.{inner_name} \
                         ('{inner}') must name separate directories — one contains the other, \
                         so a page under it belongs to both and the link graph would count a \
                         concept's curated cross-references as citations"
                    )));
                }
            }
        }
        Ok(())
    }
}

/// Whether `outer`'s named segments are a prefix of `inner`'s, equal paths included.
///
/// Compared as segments rather than strings, so `wiki` is not read as containing
/// `wiki-archive`.
fn is_within(outer: &str, inner: &str) -> bool {
    use std::path::{Component, Path};

    let segments = |value: &str| -> Vec<std::ffi::OsString> {
        Path::new(value)
            .components()
            .filter_map(|component| match component {
                Component::Normal(segment) => Some(segment.to_owned()),
                _ => None,
            })
            .collect()
    };
    let (outer, inner) = (segments(outer), segments(inner));
    outer.len() <= inner.len() && inner[..outer.len()] == outer[..]
}

/// The single boundary every vault-relative config value passes through: `value` is checked
/// to be a non-empty relative path confined to the vault — no `..`, absolute, or
/// drive-prefixed segments — and returned as its named segments joined by `/`. `label` is the
/// full config-key prefix for error messages.
///
/// Checking and normalizing are one call because they are one decision. `Path` keeps a leading
/// `CurDir` (`./wiki` and `wiki` are unequal paths with unequal segment sequences), so a key
/// that were checked here but normalized elsewhere would hand consumers two spellings of one
/// directory, and every comparison against another key — `Path::starts_with`, a string prefix,
/// set membership — would answer no.
fn normalize_vault_relative(label: &str, value: &str) -> Result<String, ConfigError> {
    use std::path::{Component, Path};

    if value.is_empty() {
        return Err(ConfigError::Validation(format!(
            "{label} must not be empty"
        )));
    }
    // A `CurDir` segment (`.`/`./`) is noise around real segments and is dropped. A value that
    // is ALL `CurDir` (`.`, `./`) would leave nothing behind, silently aliasing the directory
    // to the vault root, so at least one named segment is required.
    let mut segments = Vec::new();
    for component in Path::new(value).components() {
        match component {
            Component::Normal(segment) => segments.push(segment.to_string_lossy().into_owned()),
            Component::CurDir => {}
            _ => {
                return Err(ConfigError::Validation(format!(
                    "{label} ('{value}') must be a relative path inside the vault \
                     (no '..', absolute, or drive-prefixed segments)"
                )));
            }
        }
    }
    if segments.is_empty() {
        return Err(ConfigError::Validation(format!(
            "{label} ('{value}') must name at least one directory segment, not just '.'"
        )));
    }
    Ok(segments.join("/"))
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
    #[serde(default = "empty_object")]
    pub params: serde_json::Value,
    /// Which entry of `credentials.atlassian` this source authenticates with. Optional
    /// when exactly one instance is configured; required to disambiguate when several are
    /// (a Cloud tenant alongside an on-prem wiki, say).
    ///
    /// Credential routing is a source-level concern the FACTORY resolves, not adapter data,
    /// so it lives here rather than in `params` — the same separation `classify` keeps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
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
    /// Daily-page highlight sections: each surfaces the events whose `Event::category`
    /// matches `category` (set by this source's `classify` rules) under its own `label`
    /// heading, above the full event list. The events still render in the full list, so a
    /// highlight never hides anything. Empty (the default) renders straight from the event
    /// list with no extra sections — no per-source-type branching in the core.
    #[serde(default)]
    pub highlights: Vec<HighlightSection>,
}

/// A daily-page highlight section: events classified into `category` (by this source's
/// `classify` rules) are surfaced under `label` above the full event list.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HighlightSection {
    /// The `Event::category` value (produced by a `classify` rule) to bucket here.
    pub category: String,
    /// The section heading rendered for this bucket.
    pub label: String,
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

// `EnumIter` lets the lk-vault template guard test assert every source type's
// `descriptor().default_template` is embedded — the one part of the "add a source type"
// checklist the compiler can't force (the same drift-proof pattern as `lk_queue::TargetKind`).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    strum::EnumIter,
)]
#[serde(rename_all = "kebab-case")]
pub enum SourceType {
    GoogleDrive,
    Gmail,
    SlackChannel,
    SlackSearch,
    Jira,
    /// Confluence Cloud pages selected by CQL. Unlike every other source's immutable
    /// items, a page is a living document: its version is folded into the event identity,
    /// so an edit re-enters the pipeline while an unchanged page dedups away.
    Confluence,
    GoogleCalendar,
    /// RSS/Atom feed reader for external knowledge sources (vendor blogs, news
    /// aggregators). Public HTTP, no credentials. One source can poll many feeds.
    Rss,
    /// User-curated inbox: files dropped into `<vault>/inbox/` are picked up,
    /// processed through the same pipeline as automated sources, and archived.
    Manual,
}

/// Whether a source's items read as "messages" (Slack) or "events" (everything
/// else). Selects the daily-page event-list heading from the i18n bundle — the one
/// per-variant trait that resolves against the active locale rather than a static
/// literal, so it carries its own resolver instead of widening `SourceDescriptor`
/// with an i18n dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Message,
    Event,
}

impl ItemKind {
    pub fn heading(self, strings: &crate::i18n::Strings) -> &'static str {
        match self {
            ItemKind::Message => strings.key_messages,
            ItemKind::Event => strings.key_events,
        }
    }
}

/// Static, per-variant traits of a source type, declared in one exhaustive place
/// (`SourceType::descriptor`). Every field is set per variant with no catch-all, so
/// adding a `SourceType` fills one struct literal the compiler forces complete — a
/// new variant can never silently inherit a default (e.g. a streaming source quietly
/// flagged non-streaming would lose scrolled-out items).
#[derive(Debug, Clone, Copy)]
pub struct SourceDescriptor {
    /// A rolling/capped feed that CANNOT completely re-fetch a past day (RSS). Such a
    /// source projects its daily pages from the per-date event log (accumulate, never
    /// deplete): an item observed on day N is gone from the feed before a later run
    /// re-renders page N, so rendering from the fetch alone would silently lose it.
    /// Complete-refetch sources reproduce their whole window on demand and keep no log;
    /// `Manual` produces document pages (one per inbox file), not a daily aggregation.
    pub streaming: bool,
    /// Type-level fallback Jinja template. A user `{source-id}.md.jinja` overrides it at
    /// render time.
    pub default_template: &'static str,
    /// Whether items read as messages or events (daily-page heading terminology).
    pub item_kind: ItemKind,
}

impl SourceType {
    /// The single source of truth for this variant's static traits. One exhaustive
    /// `match`, no catch-all: adding a `SourceType` is a compiler-forced, complete decision.
    pub const fn descriptor(self) -> SourceDescriptor {
        match self {
            SourceType::GoogleDrive => SourceDescriptor {
                streaming: false,
                default_template: "google-drive.md.jinja",
                item_kind: ItemKind::Event,
            },
            SourceType::Gmail => SourceDescriptor {
                streaming: false,
                default_template: "gmail.md.jinja",
                item_kind: ItemKind::Event,
            },
            SourceType::SlackChannel => SourceDescriptor {
                streaming: false,
                default_template: "slack-channel.md.jinja",
                item_kind: ItemKind::Message,
            },
            SourceType::SlackSearch => SourceDescriptor {
                streaming: false,
                default_template: "slack-search.md.jinja",
                item_kind: ItemKind::Message,
            },
            SourceType::Jira => SourceDescriptor {
                streaming: false,
                default_template: "jira.md.jinja",
                item_kind: ItemKind::Event,
            },
            // Not streaming: a CQL window re-queries any past day completely, so a daily
            // page rebuilds from the fetch alone — no event-log projection needed.
            SourceType::Confluence => SourceDescriptor {
                streaming: false,
                default_template: "confluence.md.jinja",
                item_kind: ItemKind::Event,
            },
            SourceType::GoogleCalendar => SourceDescriptor {
                streaming: false,
                default_template: "google-calendar.md.jinja",
                item_kind: ItemKind::Event,
            },
            SourceType::Rss => SourceDescriptor {
                streaming: true,
                default_template: "rss.md.jinja",
                item_kind: ItemKind::Event,
            },
            SourceType::Manual => SourceDescriptor {
                streaming: false,
                default_template: "document.md.jinja",
                item_kind: ItemKind::Event,
            },
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

/// The personal-productivity module (gated behind `Config.personal`). Owns the
/// contribution taxonomy, which sources feed the work-log, and the monthly/quarterly/annual
/// review schedules. Its mere PRESENCE enables the subsystem — there is no `enabled` flag,
/// because an absent module is the domain-neutral default.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PersonalConfig {
    /// Source IDs whose own-authored events feed the work-log and reviews. A source not
    /// listed here never contributes to personal pages — its knowledge still flows to the
    /// concept graph and daily pages. This is the single place "which activity is mine" is
    /// declared (the core `SourceConfig` carries no personal concept).
    pub tracked_sources: Vec<String>,
    /// The contribution taxonomy: the work-log / review category buckets.
    pub performance_categories: Vec<String>,
    /// Per-source-ID category override (highest priority).
    pub source_category_map: BTreeMap<String, String>,
    /// Per-source-type default category (fallback when source_category_map has no entry).
    pub source_type_category_map: BTreeMap<SourceType, String>,
    /// Label used for events that match no category. When `None`, falls back
    /// to the locale-appropriate default via `uncategorized_label()`.
    pub uncategorized_label: Option<String>,
    /// Monthly/quarterly/annual review enable + schedule. The weekly review rides the
    /// weekly-synthesis run (`lore synthesis weekly`), so it has no separate schedule here.
    pub monthly: PersonalReviewConfig,
    pub quarterly: PersonalReviewConfig,
    pub annual: PersonalReviewConfig,
}

impl PersonalConfig {
    /// Whether this source's own-authored events feed the work-log / reviews.
    pub fn is_tracked(&self, source_id: &str) -> bool {
        self.tracked_sources.iter().any(|s| s == source_id)
    }

    /// Resolve the uncategorized label, falling back to the locale default.
    pub fn uncategorized_label(&self, locale: crate::i18n::Locale) -> &str {
        self.uncategorized_label
            .as_deref()
            .unwrap_or(locale.strings().uncategorized)
    }

    /// Resolve the contribution category for an event. Precedence, most to least
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

    /// `(period_name, cron)` for each enabled, scheduled personal review.
    pub fn review_schedules(&self) -> impl Iterator<Item = (&'static str, &String)> {
        let entries: [(&'static str, &PersonalReviewConfig); 3] = [
            ("monthly", &self.monthly),
            ("quarterly", &self.quarterly),
            ("annual", &self.annual),
        ];
        entries.into_iter().filter_map(|(name, cfg)| {
            if cfg.enabled {
                cfg.schedule.as_ref().map(|s| (name, s))
            } else {
                None
            }
        })
    }
}

impl Default for PersonalConfig {
    fn default() -> Self {
        Self {
            tracked_sources: vec![],
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
            monthly: PersonalReviewConfig::default(),
            quarterly: PersonalReviewConfig::default(),
            annual: PersonalReviewConfig::default(),
        }
    }
}

/// Cross-source synthesis (domain-neutral knowledge synthesis, NOT personal review).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SynthesisConfig {
    pub weekly: WeeklySynthesisConfig,
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
    /// (`/lore-process`) consumes the queue and edits target pages in place using
    /// Claude Code's native LLM session (no API key, no separate billing).
    Queue,
    /// No LLM work. Daily pages render without summary/concepts sections. Useful for
    /// development, CI, or vault-only sources where Rust templating is sufficient.
    Noop,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConceptConfig {
    pub categories: Vec<ConceptCategory>,
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
    /// Retention horizon (days) for `lore maintenance`: prunes the ingest log and drained
    /// `queue/processed/` files older than this. Operational history only — user-facing
    /// pages AND the per-date streaming event logs (`.lorekeeper/events/`) are permanent.
    /// The event log is the raw layer a streaming source re-projects from; pruning it
    /// would silently break `lore ingest --date <past>` self-healing for those days.
    pub retention_days: i64,
    /// Optional cron expression; when set, `lore schedule` emits crontab lines for
    /// `lore maintenance` and `lore queue prune`, so retention pruning and dead-task
    /// cleanup run unattended instead of depending on the operator remembering them.
    pub schedule: Option<String>,
}

impl Default for MaintenanceConfig {
    fn default() -> Self {
        Self {
            retention_days: 90,
            schedule: None,
        }
    }
}

/// When `lore ingest` runs. Ingestion is scheduled as ONE run over every enabled source,
/// never per source: the work-log is a cross-source daily aggregate, so a per-source run
/// would render it from a subset and overwrite the complete page.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IngestConfig {
    /// Optional cron expression; when set, `lore schedule` emits a single crontab line
    /// running `lore ingest` (all sources). Absent → no scheduled ingest line.
    pub schedule: Option<String>,
}

/// Link graph analysis configuration, consumed by `lore graph` / `lk-graph`.
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
    /// Glob patterns (matched against vault-relative paths) narrowing the ANALYSIS: an excluded
    /// page is not a graph node and its own links are not checked. It still EXISTS — a link to
    /// it resolves, its citations still count, and a rename still repoints the links it wrote —
    /// because a page that ceased to exist for the sweeps that rewrite the vault would have its
    /// citation records deleted as unjustified.
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
/// mindset, but nothing else expands `~` for us. Home is `$HOME` on Unix and
/// `%USERPROFILE%` on Windows (which has no `HOME`) — the SAME cross-platform home
/// resolution as `xdg_config_path`, so `~/vault` in config expands on every platform.
pub fn expand_tilde(path: &str) -> PathBuf {
    let home = || {
        std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .filter(|h| !h.is_empty())
    };
    if path == "~"
        && let Some(home) = home()
    {
        return PathBuf::from(home);
    }
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = home()
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(path)
}

/// Re-spell an absolute path with its `.` components dropped.
///
/// `..` is deliberately NOT folded: resolving it lexically changes which directory a path names
/// when a symlink is involved, and this runs on a path that may not exist yet.
///
/// Rebuilding from `components()` also collapses redundant separators (`a//b`, a trailing `/`),
/// which serves the same purpose — one spelling per vault. It reaches one case where POSIX
/// permits a difference: a path beginning with exactly two slashes is implementation-defined,
/// and this makes `//host/share` into `/host/share`. Neither platform this ships to (Linux,
/// macOS) gives that spelling a separate meaning, so there is no path it redirects.
fn fold_current_dir(path: &Path) -> PathBuf {
    path.components()
        .filter(|component| !matches!(component, std::path::Component::CurDir))
        .collect()
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
            instance: None,
            enabled: true,
            params: empty_object(),
            classify: vec![],
            labels: vec![],
            extract_concepts: true,
            focus: f.map(str::to_owned),
            highlights: vec![],
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
        let mut config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn expand_tilde_works() {
        // Asked the way the function decides it, so the assertions hold with `HOME` unset or
        // empty — a stripped container, a launchd job — rather than only where a home exists.
        // Reading `var("HOME")` instead would admit `Ok("")`, which is exactly the case
        // `expand_tilde` treats as no home at all.
        match std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .filter(|home| !home.is_empty())
        {
            Some(home) => {
                assert_eq!(
                    expand_tilde("~/Documents"),
                    PathBuf::from(&home).join("Documents")
                );
                // A bare `~` is the home directory itself — shell convention, and the
                // `~/...` form's natural degenerate case.
                assert_eq!(expand_tilde("~"), PathBuf::from(home));
            }
            None => {
                // Nothing to expand to, so the path is left as written rather than losing its
                // first component to an empty prefix.
                assert_eq!(expand_tilde("~/Documents"), PathBuf::from("~/Documents"));
                assert_eq!(expand_tilde("~"), PathBuf::from("~"));
            }
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
        let mut config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.validate().is_err(), "'..' segment must be rejected");
    }

    /// Nested or equal top-level dirs make one page answer to two classifications, and the
    /// consequence is silent: `backlinks-sync` reads a concept's curated `## Related`
    /// cross-references as provenance and inflates every cited concept's `source_count`,
    /// reproducibly, on a config that otherwise loads clean.
    #[test]
    fn validate_rejects_a_vault_dir_nested_in_another() {
        let with = |dirs: &str| -> Config {
            serde_yaml_ng::from_str(&format!(
                "vault:\n  root: /tmp/vault\n  dirs:\n{dirs}identity:\n  name: t\n  \
                 email: t@t.com\nsources:\n  s1:\n    type: gmail\n"
            ))
            .unwrap()
        };
        // Containment is checked BOTH ways: the comparison walks a fixed field order, so a
        // one-directional test would pass while a root that contains an earlier one slipped
        // through. `daily` precedes `wiki`, so both arrangements are needed.
        for dirs in [
            "    daily: pages\n    wiki: pages/wiki\n",
            "    wiki: pages\n    daily: pages/daily\n",
            "    daily: x\n    wiki: x\n",
            "    personal: me\n    synthesis: me/synthesis\n",
            "    daily: ./pages\n    wiki: pages/wiki\n",
        ] {
            assert!(
                with(dirs).validate().is_err(),
                "overlapping roots must be rejected:\n{dirs}"
            );
        }

        // What the rule must NOT reject: siblings that merely share a prefix STRING, and the
        // period names, which are leaf segments joined under `personal` and `synthesis` and
        // are therefore expected to be equal to each other.
        for dirs in [
            "    wiki: wiki\n    daily: wiki-archive\n",
            // A period name may equal a ROOT: it is a leaf segment, so this asks for
            // `me/daily/2026-W21.md`, and comparing it as a root would refuse a legal vault.
            "    daily: daily\n    weekly: daily\n",
        ] {
            assert!(
                with(dirs).validate().is_ok(),
                "separate directories must be accepted:\n{dirs}"
            );
        }
    }

    #[test]
    fn graph_scope_dirs_shares_the_vault_dir_path_guard() {
        // `graph.scope.dirs` and `vault.dirs.*` go through the SAME component-based guard, so
        // a real `..` SEGMENT is rejected while a name merely CONTAINING `..` is allowed (the
        // old substring check wrongly rejected the latter).
        let base = |scope: &str| {
            format!(
                "vault:\n  root: /tmp/vault\nidentity:\n  name: t\n  email: t@t.com\n\
                 sources:\n  s1:\n    type: gmail\ngraph:\n  scope:\n    dirs: [{scope}]\n"
            )
        };
        let mut bad: Config = serde_yaml_ng::from_str(&base("\"../escape\"")).unwrap();
        assert!(bad.validate().is_err(), "'..' segment must be rejected");
        let mut ok: Config = serde_yaml_ng::from_str(&base("\"notes..old\"")).unwrap();
        assert!(
            ok.validate().is_ok(),
            "a name merely containing '..' (no parent segment) must be allowed"
        );
    }

    /// The guard normalizes as it checks, so every key that uses it yields ONE spelling of a
    /// directory. Two keys that agreed on the directory but not the spelling would disagree in
    /// every consumer: `Path` keeps a leading `CurDir`, so `./wiki` and `wiki` are unequal, and
    /// `lore graph lint` scoped to `./wiki` scanned zero pages of the vault it was pointed at
    /// and reported the vault clean.
    #[test]
    fn a_curdir_prefixed_scope_dir_names_the_same_directory_as_the_vault_dir() {
        let yaml = "vault:\n  root: /tmp/vault\n  dirs:\n    wiki: wiki\nidentity:\n  name: t\n  \
                    email: t@t.com\nsources:\n  s1:\n    type: gmail\ngraph:\n  scope:\n    \
                    dirs: [\"./wiki/\"]\n";
        let mut config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        config.validate().unwrap();
        assert_eq!(
            config.graph.scope.dirs,
            vec![PathBuf::from(&config.vault.dirs.wiki)]
        );
        assert!(
            Path::new("wiki/concepts/rag.md").starts_with(&config.graph.scope.dirs[0]),
            "a page under the vault's wiki dir must fall inside the scope dir"
        );
    }

    #[test]
    fn validate_rejects_curdir_only_vault_dir() {
        // "." survives the per-component check (CurDir is noise around real segments, e.g.
        // "./wiki") but leaves nothing behind once dropped, which would alias the directory to
        // the vault root. Reject an all-CurDir value so the normalized path can never be empty.
        let yaml = r#"
vault:
  root: /tmp/vault
  dirs:
    daily: "."
identity:
  name: test
  email: test@test.com
sources:
  s1:
    type: gmail
"#;
        let mut config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(
            config.validate().is_err(),
            "'.' must be rejected (it normalizes to an empty path)"
        );
    }

    #[test]
    fn validate_accepts_curdir_prefixed_vault_dir() {
        // "./wiki" is fine: the CurDir is noise, the guard yields "wiki", and a real segment
        // remains. Only an ALL-CurDir value is rejected.
        let yaml = r#"
vault:
  root: /tmp/vault
  dirs:
    wiki: "./wiki"
identity:
  name: test
  email: test@test.com
sources:
  s1:
    type: gmail
"#;
        let mut config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.validate().is_ok());
        assert_eq!(config.vault.dirs.wiki, "wiki");
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
        let mut config: Config = serde_yaml_ng::from_str(yaml).unwrap();
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
        let mut config: Config = serde_yaml_ng::from_str(yaml).unwrap();
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
        let mut ok: Config = serde_yaml_ng::from_str(&base("120")).unwrap();
        assert!(ok.validate().is_ok());
        let mut bad: Config = serde_yaml_ng::from_str(&base("0")).unwrap();
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
        let mut config: Config = serde_yaml_ng::from_str(yaml).unwrap();
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
        let mut config: Config = serde_yaml_ng::from_str(yaml).unwrap();
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
ingest:
  schedule: "0 7 *"
"#;
        let mut config: Config = serde_yaml_ng::from_str(yaml).unwrap();
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
ingest:
  schedule: "0 25 * * *"
"#;
        let mut config: Config = serde_yaml_ng::from_str(yaml).unwrap();
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
ingest:
  schedule: "*/0 * * * *"
"#;
        let mut config: Config = serde_yaml_ng::from_str(yaml).unwrap();
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
ingest:
  schedule: "0-29/foo * * * *"
"#;
        let mut config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn review_schedules_excludes_disabled() {
        let yaml = r#"
vault:
  root: /tmp/vault
identity:
  name: test
  email: test@test.com
sources:
  s1:
    type: gmail
personal:
  monthly:
    enabled: true
    schedule: "0 8 1 * *"
  quarterly:
    enabled: false
    schedule: "0 8 1 1 *"
"#;
        let mut config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        config.validate().unwrap();
        let personal = config.personal.as_ref().unwrap();
        let names: Vec<_> = personal.review_schedules().map(|(n, _)| n).collect();
        assert_eq!(
            names,
            vec!["monthly"],
            "disabled quarterly should be excluded"
        );
    }

    #[test]
    fn relative_vault_root_resolved_against_config_dir() {
        // A per-run directory, not a fixed name under `temp_dir()`: on macOS that is the
        // per-user `$TMPDIR`, so a fixed name looks private, and on Linux it is the shared
        // world-writable `/tmp`, where a concurrent run's cleanup deletes this config between
        // the write and the load and another uid's leftover makes the write fail outright.
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
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
            resolved.starts_with(dir),
            "vault root should be anchored to config dir, got: {}",
            resolved.display()
        );
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
ingest:
  schedule: "0 7 * * 1-5"
"#;
        let mut config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_accepts_stepped_star_in_a_comma_list() {
        // `0,*/15` is valid in the evaluator (`cron::parse_field` expands `*` over the
        // field range inside a comma list), so the validator must accept it too — the two
        // grammars are single-sourced by contract. Regression for a validator that parsed
        // the list member `*/15`'s base `*` as a number and rejected a runnable schedule.
        assert!(validate_cron_field("*/15", 0, 59).is_ok()); // whole-field step (single member)
        assert!(validate_cron_field("0,*/15", 0, 59).is_ok());
        assert!(validate_cron_field("*/15,30", 0, 59).is_ok()); // `*/n` as a list member
        assert!(validate_cron_field("*/0", 0, 59).is_err()); // zero step still rejected
        assert!(validate_cron_field("99", 0, 59).is_err()); // out-of-range still rejected
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
        let mut config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_rejects_performance_bridge_without_personal() {
        // A classify `performance_category` bridge targets the personal taxonomy, so it is
        // a contradiction with no `personal:` section — reject rather than silently no-op.
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
      - category: shipped
        keywords: ["deployed"]
        performance_category: project-delivery
"#;
        let mut config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(
            config.validate().is_err(),
            "a performance_category bridge with no `personal:` section must be rejected"
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
personal:
  source_category_map:
    s1: nonexistent-category
"#;
        let mut config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_rejects_highlight_for_unproduced_category() {
        // A highlight surfaces events a classify rule routed into `category`; a highlight
        // for a category no rule produces would render a permanently-empty section, so it
        // must fail at load rather than silently.
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
        keywords: ["deadline"]
    highlights:
      - { category: never_classified, label: "Oops" }
"#;
        let mut config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(
            config.validate().is_err(),
            "a highlight category with no classify rule must be rejected"
        );
    }

    #[test]
    fn validate_accepts_highlight_backed_by_classify_rule() {
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
        keywords: ["deadline"]
    highlights:
      - { category: action_required, label: "Action Required" }
"#;
        let mut config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn resolve_category_prefers_source_id() {
        let mut perf = PersonalConfig::default();
        perf.source_category_map
            .insert("my-tasks".into(), "innovation".into());
        let cat = perf.resolve_category("my-tasks", SourceType::Jira, None);
        assert_eq!(cat.as_deref(), Some("innovation"));
    }

    #[test]
    fn resolve_category_falls_back_to_source_type() {
        let mut perf = PersonalConfig::default();
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
        let mut perf = PersonalConfig::default();
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
        let mut config: Config = serde_yaml_ng::from_str(yaml).unwrap();
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
        let mut config_low: Config = serde_yaml_ng::from_str(yaml_low).unwrap();
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
        let mut config: Config = serde_yaml_ng::from_str(yaml).unwrap();
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
        let mut config: Config = serde_yaml_ng::from_str(yaml).unwrap();
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
        let mut config: Config = serde_yaml_ng::from_str(yaml).unwrap();
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
        let mut config: Config = serde_yaml_ng::from_str(yaml).unwrap();
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
        let mut config: Config = serde_yaml_ng::from_str(yaml).unwrap();
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
        let mut config: Config = serde_yaml_ng::from_str(yaml).unwrap();
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
        let mut config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(
            config.validate().is_err(),
            "category id with '/' must be rejected"
        );
    }

    /// `lore config categories` and `lore wiki concepts` publish the vocabulary as one
    /// tab-separated record per line, so a label carrying a newline invents a category id no
    /// config declares — and an agent that writes it fails `lint` on the page it just wrote.
    #[test]
    fn validate_rejects_a_concept_category_label_that_is_not_one_line() {
        for label in [r#""AI\nfake""#, r#""AI\tfake""#] {
            let yaml = format!(
                r#"
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
    - id: technology
      label: {label}
"#
            );
            let mut config: Config = serde_yaml_ng::from_str(&yaml).unwrap();
            assert!(
                config.validate().is_err(),
                "a label spanning more than one record must be rejected: {label}"
            );
        }
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
        let mut config: Config = serde_yaml_ng::from_str(yaml).unwrap();
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
        dirs.validate().unwrap();
        assert_eq!(dirs.wiki, "wiki");
        assert_eq!(dirs.daily, "daily");
        assert_eq!(dirs.personal, "me");
        assert_eq!(dirs.synthesis, "synthesis");
        assert_eq!(dirs.weekly, "weekly");
    }
}
