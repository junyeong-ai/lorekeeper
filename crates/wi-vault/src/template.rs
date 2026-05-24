use std::path::Path;

use minijinja::Environment;

use crate::VaultError;

pub struct TemplateEngine {
    env: Environment<'static>,
}

impl TemplateEngine {
    pub fn new(template_dir: &Path) -> Result<Self, VaultError> {
        let mut env = Environment::new();
        env.set_loader(minijinja::path_loader(template_dir));
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

    /// `Ok(true)` if the template exists and parses, `Ok(false)` if it simply isn't
    /// present (caller should fall back), `Err` if it exists but failed to load/parse.
    /// Distinguishing these prevents a user's template with a syntax error from being
    /// silently treated as "absent" and falling back to the embedded renderer.
    pub fn available(&self, name: &str) -> Result<bool, VaultError> {
        match self.env.get_template(name) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == minijinja::ErrorKind::TemplateNotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
}
