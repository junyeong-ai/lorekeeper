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

    pub fn available(&self, name: &str) -> bool {
        self.env.get_template(name).is_ok()
    }
}
