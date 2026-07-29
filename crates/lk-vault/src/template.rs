use std::path::{Path, PathBuf};

use minijinja::Environment;

use crate::VaultError;

/// Default templates, compiled into the binary. A user template directory overrides any
/// of these by name; otherwise these are used — so a binary-only install (no repo, no
/// templates dir) always has every template and never falls back to ad-hoc rendering.
const EMBEDDED: &[(&str, &str)] = &[
    (
        "_daily_base.md.jinja",
        include_str!("../../../templates/_daily_base.md.jinja"),
    ),
    (
        "annual-review.md.jinja",
        include_str!("../../../templates/annual-review.md.jinja"),
    ),
    (
        "concept.md.jinja",
        include_str!("../../../templates/concept.md.jinja"),
    ),
    (
        "confluence.md.jinja",
        include_str!("../../../templates/confluence.md.jinja"),
    ),
    (
        "document.md.jinja",
        include_str!("../../../templates/document.md.jinja"),
    ),
    (
        "gmail.md.jinja",
        include_str!("../../../templates/gmail.md.jinja"),
    ),
    (
        "google-calendar.md.jinja",
        include_str!("../../../templates/google-calendar.md.jinja"),
    ),
    (
        "google-drive.md.jinja",
        include_str!("../../../templates/google-drive.md.jinja"),
    ),
    (
        "jira.md.jinja",
        include_str!("../../../templates/jira.md.jinja"),
    ),
    (
        "monthly-review.md.jinja",
        include_str!("../../../templates/monthly-review.md.jinja"),
    ),
    (
        "quarterly-review.md.jinja",
        include_str!("../../../templates/quarterly-review.md.jinja"),
    ),
    (
        "rss.md.jinja",
        include_str!("../../../templates/rss.md.jinja"),
    ),
    (
        "slack-channel.md.jinja",
        include_str!("../../../templates/slack-channel.md.jinja"),
    ),
    (
        "slack-search.md.jinja",
        include_str!("../../../templates/slack-search.md.jinja"),
    ),
    (
        "weekly-review.md.jinja",
        include_str!("../../../templates/weekly-review.md.jinja"),
    ),
    (
        "weekly-synthesis.md.jinja",
        include_str!("../../../templates/weekly-synthesis.md.jinja"),
    ),
    (
        "work-log.md.jinja",
        include_str!("../../../templates/work-log.md.jinja"),
    ),
];

fn embedded(name: &str) -> Option<String> {
    EMBEDDED
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, src)| (*src).to_string())
}

pub struct TemplateEngine {
    env: Environment<'static>,
    user_dir: Option<PathBuf>,
}

impl TemplateEngine {
    /// `user_dir: Some` overrides embedded templates per-file from that directory;
    /// `None` uses only the embedded defaults. There is no implicit directory search —
    /// the embedded copies are the source of truth.
    pub fn build(user_dir: Option<&Path>) -> Result<Self, VaultError> {
        let mut env = Environment::new();
        let user_dir = user_dir.map(Path::to_path_buf);
        let dir = user_dir.clone();
        env.set_loader(move |name| {
            if let Some(d) = &dir {
                match std::fs::read_to_string(d.join(name)) {
                    Ok(src) => return Ok(Some(src)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        return Err(minijinja::Error::new(
                            minijinja::ErrorKind::InvalidOperation,
                            format!("read template {name}: {e}"),
                        ));
                    }
                }
            }
            Ok(embedded(name))
        });
        Ok(Self { env, user_dir })
    }

    pub fn render(
        &self,
        template_name: &str,
        context: &serde_json::Value,
    ) -> Result<String, VaultError> {
        let tmpl = self.env.get_template(template_name)?;
        let rendered = tmpl.render(context)?;
        Ok(lk_core::text::collapse_blank_lines(&rendered))
    }

    /// `Ok(true)` only if `name` exists as a USER-provided override file (not an
    /// embedded default) and parses; `Ok(false)` if there is no such user file; `Err`
    /// if the user file exists but fails to parse. A per-source daily override must
    /// come from the user dir — an embedded template basename (e.g. `manual.md.jinja`)
    /// is selected explicitly by type, never matched as a `{source_id}.md.jinja`
    /// override, so a source id colliding with a built-in name can't hijack rendering.
    pub fn has_user_override(&self, name: &str) -> Result<bool, VaultError> {
        let Some(dir) = &self.user_dir else {
            return Ok(false);
        };
        if !dir.join(name).is_file() {
            return Ok(false);
        }
        // Present as a user file — surface a parse error rather than silently ignoring it.
        match self.env.get_template(name) {
            Ok(_) => Ok(true),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Page templates the pipeline renders by name, as opposed to the per-source ones a
    /// `SourceType` descriptor selects.
    const RENDERED_BY_NAME: &[&str] = &[
        "concept.md.jinja",
        "document.md.jinja",
        "work-log.md.jinja",
        "weekly-synthesis.md.jinja",
        "weekly-review.md.jinja",
        "monthly-review.md.jinja",
        "quarterly-review.md.jinja",
        "annual-review.md.jinja",
    ];

    /// Every embedded template must be reachable by some renderer: a `SourceType` default, a
    /// `RENDERED_BY_NAME` entry, or a `_`-prefixed partial that daily templates extend. One
    /// that is reachable by none cannot be rendered at all, yet still reads as a machine
    /// writer to anyone documenting the page format — which is exactly how the exploration
    /// format came to advertise a machine-owned Grounding section that no command fills.
    #[test]
    fn every_embedded_template_has_a_renderer() {
        use strum::IntoEnumIterator;
        let mut reachable: std::collections::HashSet<&str> =
            RENDERED_BY_NAME.iter().copied().collect();
        for st in lk_core::config::SourceType::iter() {
            reachable.insert(st.descriptor().default_template);
        }
        for (name, _) in EMBEDDED {
            assert!(
                name.starts_with('_') || reachable.contains(name),
                "{name} is embedded but no renderer names it"
            );
        }
        for name in RENDERED_BY_NAME {
            assert!(
                EMBEDDED.iter().any(|(n, _)| n == name),
                "{name} is rendered by name but not embedded"
            );
        }
    }

    /// Every `SourceType`'s `descriptor().default_template` must be an embedded template.
    /// Adding a source type is compiler-forced everywhere EXCEPT here — the template file
    /// and its `EMBEDDED` entry — so a missing one would otherwise only surface as a render
    /// failure at runtime. Iterating `SourceType` (not a hand-list) keeps this drift-proof.
    #[test]
    fn every_source_type_default_template_is_embedded() {
        use strum::IntoEnumIterator;
        let engine = TemplateEngine::build(None).unwrap();
        for st in lk_core::config::SourceType::iter() {
            let name = st.descriptor().default_template;
            assert!(
                EMBEDDED.iter().any(|(n, _)| *n == name),
                "{st:?}: default_template {name:?} is not in EMBEDDED"
            );
            engine
                .env
                .get_template(name)
                .unwrap_or_else(|e| panic!("{st:?}: default_template {name:?} unresolved: {e}"));
        }
    }

    /// Every daily template extends `_daily_base.md.jinja` via blocks, so a typo in any
    /// child's `{% block %}` (or the base's `self.title()` wiring) only surfaces at render
    /// time. Render each with a representative context to catch inheritance breakage —
    /// the integration tests otherwise only exercise the Gmail and RSS templates.
    #[test]
    fn every_daily_template_renders_with_expected_frontmatter() {
        let engine = TemplateEngine::build(None).unwrap();
        let i18n = serde_json::to_value(lk_core::i18n::Locale::En.strings()).unwrap();
        let context = serde_json::json!({
            "source_id": "s",
            "date": "2026-06-14",
            "labels": ["x"],
            "event_count": 1,
            "events": [{"title": "T", "body": "B", "author": "a", "url": "https://x"}],
            "summary": "sum",
            "concepts": ["- [C1](../../wiki/concepts/c1.md)", "- [C2](../../wiki/concepts/c2.md)"],
            "extract_concepts": true,
            "highlights": [{"label": "HL", "items": [{"subject": "Subj", "sender": "me"}]}],
            "i18n": i18n,
            "llm_inputs": {"summary": "h1", "refine_events": "h2"},
        });

        // The daily-page frontmatter contract — kept in lockstep with lk-cli schema.rs's
        // "daily" page type and lk-pipeline render.rs. A child that drifts the base
        // frontmatter (adds/drops a key) fails here.
        let expected_keys = [
            "id:",
            "type: daily",
            "title:",
            "created:",
            "labels:",
            "source:",
            "event_count:",
        ];

        for tmpl in [
            "gmail",
            "jira",
            "slack-channel",
            "slack-search",
            "google-calendar",
            "google-drive",
            "rss",
        ] {
            let name = format!("{tmpl}.md.jinja");
            let out = engine
                .render(&name, &context)
                .unwrap_or_else(|e| panic!("{name} failed to render: {e}"));

            assert!(
                out.starts_with("---\n"),
                "{name}: missing frontmatter:\n{out}"
            );
            for key in expected_keys {
                assert!(
                    out.contains(key),
                    "{name}: frontmatter missing `{key}`:\n{out}"
                );
            }
            assert!(
                out.contains(&format!(
                    "## {}",
                    lk_core::i18n::Locale::En.strings().summary
                )),
                "{name}: missing summary heading:\n{out}"
            );
            // The concept list is a TIGHT markdown list (no blank line between entries) —
            // uniform across all daily templates via the shared base loop. Entries arrive
            // FULLY prerendered including their bullet (`render::concept_links`), because the
            // section has a second writer — `lore queue apply` — and a bullet stated here as
            // well would be a second answer to what a citation looks like.
            assert!(
                out.contains(
                    "- [C1](../../wiki/concepts/c1.md)\n- [C2](../../wiki/concepts/c2.md)"
                ),
                "{name}: concept links must render as a tight list:\n{out}"
            );
            assert_eq!(
                out.matches("### T").count(),
                1,
                "{name}: event rendered the wrong number of times:\n{out}"
            );
        }
    }

    /// A document page carries the same second-writer section as a daily page, through its own
    /// template rather than the shared base — so the daily loop above proves nothing about it,
    /// and a bullet restated here would double every citation on every document page with no
    /// test to notice.
    #[test]
    fn a_document_renders_its_concept_links_exactly_as_a_daily_page_does() {
        let engine = TemplateEngine::build(None).unwrap();
        let i18n = serde_json::to_value(lk_core::i18n::Locale::En.strings()).unwrap();
        let out = engine
            .render(
                "document.md.jinja",
                &serde_json::json!({
                    "slug": "d",
                    "title": "D",
                    "created": "2026-06-14",
                    "updated": "2026-06-14",
                    "document_type": "note",
                    "tags": ["document"],
                    "summary": "sum",
                    "content": "body",
                    "concepts": ["- [C1](../concepts/c1.md)", "- [C2](../concepts/c2.md)"],
                    "extract_concepts": true,
                    "i18n": i18n,
                    "llm_inputs": {"summary": "h1", "concepts": "h2"},
                }),
            )
            .expect("document.md.jinja renders");
        assert!(
            out.contains("- [C1](../concepts/c1.md)\n- [C2](../concepts/c2.md)"),
            "concept links must render as a tight list of the prerendered entries:\n{out}"
        );
    }

    /// A configured highlight renders ABOVE the event list on any daily source — the loop
    /// lives in the shared base, no longer a Gmail special-case.
    #[test]
    fn highlights_render_on_any_daily_source_from_the_base() {
        let engine = TemplateEngine::build(None).unwrap();
        let i18n = serde_json::to_value(lk_core::i18n::Locale::En.strings()).unwrap();
        let context = serde_json::json!({
            "source_id": "s",
            "date": "2026-06-14",
            "labels": [],
            "event_count": 1,
            "events": [{"title": "T", "body": "B", "author": null, "url": null}],
            "summary": "sum",
            "concepts": [],
            "extract_concepts": false,
            "highlights": [{"label": "Action Required", "items": [{"subject": "Ship it", "sender": "a@b"}]}],
            "i18n": i18n,
            "llm_inputs": {"summary": "h1", "refine_events": "h2"},
        });
        // jira (a non-Gmail source) proves highlights are a base feature.
        let out = engine.render("jira.md.jinja", &context).unwrap();
        assert!(
            out.contains("## Action Required"),
            "highlight section missing:\n{out}"
        );
        assert!(
            out.contains("- **Ship it** — a@b"),
            "highlight item missing:\n{out}"
        );
    }
}
