use std::path::{Path, PathBuf};

use minijinja::Environment;

use crate::VaultError;

/// Default templates, compiled into the binary. A user template directory overrides any
/// of these by name; otherwise these are used — so a binary-only install (no repo, no
/// templates dir) always has every template and never falls back to ad-hoc rendering.
const EMBEDDED: &[(&str, &str)] = &[
    (
        "annual-review.md.jinja",
        include_str!("../../../templates/annual-review.md.jinja"),
    ),
    (
        "concept.md.jinja",
        include_str!("../../../templates/concept.md.jinja"),
    ),
    (
        "document.md.jinja",
        include_str!("../../../templates/document.md.jinja"),
    ),
    (
        "exploration.md.jinja",
        include_str!("../../../templates/exploration.md.jinja"),
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
