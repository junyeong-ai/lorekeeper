use std::path::{Path, PathBuf};
use std::sync::Arc;

pub mod graph;
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
    // Project/dev checkout: config next to the working directory.
    let cwd = PathBuf::from("config.yaml");
    if cwd.exists() {
        return Ok(cwd);
    }
    // Binary-only install (no repo): the standard XDG config location. A vault-relative
    // path can't be auto-discovered — the vault path itself lives inside the config.
    if let Some(p) = xdg_config_path()
        && p.exists()
    {
        return Ok(p);
    }
    Err(miette::miette!(
        "No config found. Create ./config.yaml or ~/.config/lorekeeper/config.yaml \
         (copy config.example.yaml), or pass --config / set LORE_CONFIG."
    ))
}

/// `$XDG_CONFIG_HOME/lorekeeper/config.yaml`, falling back to `~/.config/lorekeeper/...`.
fn xdg_config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("lorekeeper").join("config.yaml"))
}

pub fn load_config(path: &Path) -> miette::Result<lk_core::config::Config> {
    lk_core::config::Config::load(path).map_err(|e| miette::miette!("{e}"))
}

pub fn build_llm_client(
    config: &lk_core::config::Config,
    vault_root: &Path,
) -> Arc<dyn lk_llm::LlmClient> {
    match config.llm.provider {
        lk_core::config::LlmProvider::Anthropic => {
            match lk_llm::AnthropicClient::new(&config.llm) {
                Ok(c) => Arc::new(c),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "Anthropic provider selected but ANTHROPIC_API_KEY missing; falling back to NoopLlmClient"
                    );
                    Arc::new(lk_llm::NoopLlmClient)
                }
            }
        }
        lk_core::config::LlmProvider::Queue => {
            let queue_dir = vault_root.join(".lorekeeper").join("queue");
            Arc::new(lk_llm::QueueLlmClient::new(queue_dir))
        }
        lk_core::config::LlmProvider::Noop => Arc::new(lk_llm::NoopLlmClient),
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
