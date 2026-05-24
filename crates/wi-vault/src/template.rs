use std::path::Path;

use minijinja::Environment;

use crate::VaultError;

/// Default templates, compiled into the binary. A user template directory overrides any
/// of these by name; otherwise these are used — so a binary-only install (no repo, no
/// templates dir) always has every template and never falls back to ad-hoc rendering.
const EMBEDDED: &[(&str, &str)] = &[
    ("annual-review.md.jinja", include_str!("../../../templates/annual-review.md.jinja")),
    ("concept.md.jinja", include_str!("../../../templates/concept.md.jinja")),
    ("gmail.md.jinja", include_str!("../../../templates/gmail.md.jinja")),
    ("google-calendar.md.jinja", include_str!("../../../templates/google-calendar.md.jinja")),
    ("google-drive.md.jinja", include_str!("../../../templates/google-drive.md.jinja")),
    ("jira.md.jinja", include_str!("../../../templates/jira.md.jinja")),
    ("monthly-summary.md.jinja", include_str!("../../../templates/monthly-summary.md.jinja")),
    ("quarterly-review.md.jinja", include_str!("../../../templates/quarterly-review.md.jinja")),
    ("slack-channel.md.jinja", include_str!("../../../templates/slack-channel.md.jinja")),
    ("slack-search.md.jinja", include_str!("../../../templates/slack-search.md.jinja")),
    ("weekly-personal.md.jinja", include_str!("../../../templates/weekly-personal.md.jinja")),
    ("weekly-synthesis.md.jinja", include_str!("../../../templates/weekly-synthesis.md.jinja")),
    ("work-log.md.jinja", include_str!("../../../templates/work-log.md.jinja")),
];

fn embedded(name: &str) -> Option<String> {
    EMBEDDED
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, src)| (*src).to_string())
}

pub struct TemplateEngine {
    env: Environment<'static>,
}

impl TemplateEngine {
    pub fn new(template_dir: &Path) -> Result<Self, VaultError> {
        let mut env = Environment::new();
        let dir = template_dir.to_path_buf();
        // User dir wins (per-deployment override); the embedded copy is the default.
        env.set_loader(move |name| {
            let path = dir.join(name);
            match std::fs::read_to_string(&path) {
                Ok(src) => Ok(Some(src)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(embedded(name)),
                Err(e) => Err(minijinja::Error::new(
                    minijinja::ErrorKind::InvalidOperation,
                    format!("read template {name}: {e}"),
                )),
            }
        });
        Ok(Self { env })
    }

    pub fn render(
        &self,
        template_name: &str,
        context: &serde_json::Value,
    ) -> Result<String, VaultError> {
        let tmpl = self.env.get_template(template_name)?;
        let rendered = tmpl.render(context)?;
        Ok(rendered)
    }

    /// `Ok(true)` if the template exists (user dir or embedded) and parses, `Ok(false)`
    /// if simply not present, `Err` if it exists but fails to load/parse. Used to check
    /// for an optional per-source override; the per-type template is always embedded.
    pub fn available(&self, name: &str) -> Result<bool, VaultError> {
        match self.env.get_template(name) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == minijinja::ErrorKind::TemplateNotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
}
