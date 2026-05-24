use std::path::{Path, PathBuf};
use std::sync::Arc;

pub mod health;
pub mod ingest;
pub mod init;
pub mod maintenance;
pub mod performance;
pub mod schedule;
pub mod status;
pub mod synthesis;
pub mod validate;

pub struct GlobalOpts {
    pub config: Option<PathBuf>,
    pub template_dir: Option<PathBuf>,
}

pub fn find_config(opts: &GlobalOpts) -> miette::Result<PathBuf> {
    if let Some(p) = opts.config.as_ref() {
        if !p.exists() {
            return Err(miette::miette!("Config file not found: {}", p.display()));
        }
        return Ok(p.clone());
    }
    for name in ["config.yaml", "config.example.yaml"] {
        let p = PathBuf::from(name);
        if p.exists() {
            return Ok(p);
        }
    }
    Err(miette::miette!(
        "No config file found. Set --config, WI_CONFIG, or copy config.example.yaml to config.yaml."
    ))
}

pub fn load_config(path: &Path) -> miette::Result<wi_core::config::Config> {
    wi_core::config::Config::load(path).map_err(|e| miette::miette!("{e}"))
}

pub fn build_llm_client(
    config: &wi_core::config::Config,
    vault_root: &Path,
) -> Arc<dyn wi_llm::LlmClient> {
    match config.llm.provider {
        wi_core::config::LlmProvider::Anthropic => {
            match wi_llm::AnthropicClient::new(&config.llm) {
                Ok(c) => Arc::new(c),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "Anthropic provider selected but ANTHROPIC_API_KEY missing; falling back to NoopLlmClient"
                    );
                    Arc::new(wi_llm::NoopLlmClient)
                }
            }
        }
        wi_core::config::LlmProvider::Queue => {
            let queue_dir = vault_root.join(".wiki-ingest").join("queue");
            Arc::new(wi_llm::QueueLlmClient::new(queue_dir))
        }
        wi_core::config::LlmProvider::Noop => Arc::new(wi_llm::NoopLlmClient),
    }
}

pub fn parse_date(
    s: Option<&str>,
    fallback: jiff::civil::Date,
) -> miette::Result<jiff::civil::Date> {
    match s {
        Some(s) => s
            .parse::<jiff::civil::Date>()
            .map_err(|e| miette::miette!("invalid date '{s}': {e}")),
        None => Ok(fallback),
    }
}

/// Resolve template directory lookup order:
///   1. `--template-dir` / `WI_TEMPLATE_DIR` (explicit override)
///   2. `<vault>/.wiki-ingest/templates/`     (per-vault user override)
///   3. `$XDG_DATA_HOME/wi-ingest/templates/` (installed by `scripts/install.sh`)
///   4. `./templates/`                        (development fallback)
pub fn resolve_template_dir(opts: &GlobalOpts, vault_root: &Path) -> PathBuf {
    if let Some(p) = opts.template_dir.as_ref() {
        return p.clone();
    }
    let vault_templates = vault_root.join(".wiki-ingest").join("templates");
    if vault_templates.exists() {
        return vault_templates;
    }
    let xdg = xdg_data_template_dir();
    if xdg.exists() {
        return xdg;
    }
    PathBuf::from("templates")
}

fn xdg_data_template_dir() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
            PathBuf::from(home).join(".local").join("share")
        });
    base.join("wi-ingest").join("templates")
}
