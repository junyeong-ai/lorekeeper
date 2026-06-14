use std::path::{Path, PathBuf};
use std::sync::Arc;

pub mod doctor;
pub mod graph;
pub mod health;
pub mod ingest;
pub mod init;
pub mod maintenance;
pub mod performance;
pub mod queue;
pub mod schedule;
pub mod schema;
pub mod status;
pub mod synthesis;
pub mod validate;
pub mod wiki;

pub struct GlobalOptions {
    pub config: Option<PathBuf>,
    pub template_dir: Option<PathBuf>,
}

pub fn find_config(opts: &GlobalOptions) -> miette::Result<PathBuf> {
    let path = locate_config(opts)?;
    // Canonicalize at this single I/O boundary so the returned path is always absolute
    // (and symlink-resolved). `Config::load` resolves a relative `vault.root` against the
    // config file's parent dir; an absolute parent makes the vault root — and every path
    // derived from it, in every crate — independent of the process CWD by construction,
    // so no downstream consumer has to reason about CWD. lk-core stays pure (no `current_dir`).
    path.canonicalize()
        .map_err(|e| miette::miette!("resolve config path {}: {e}", path.display()))
}

/// Locate the config file (possibly as a relative path); `find_config` absolutizes it.
fn locate_config(opts: &GlobalOptions) -> miette::Result<PathBuf> {
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

/// Atomically write `contents` to an absolute `path` from an async command handler.
/// Routes through the single `lk_core::fs::write_atomic` (temp + fsync + rename) on the
/// blocking pool — so command-level full-file rewrites (the ingest log, AGENTS.md) get the
/// same durability/atomicity as every other writer, never a torn `tokio::fs::write`.
pub(crate) async fn write_atomic(path: PathBuf, contents: Vec<u8>) -> std::io::Result<()> {
    tokio::task::spawn_blocking(move || lk_core::fs::write_atomic(&path, &contents, None))
        .await
        .map_err(std::io::Error::other)?
}

pub fn build_llm_client(
    config: &lk_core::config::Config,
    vault_root: &Path,
) -> miette::Result<Arc<dyn lk_queue::LlmClient>> {
    match config.llm.provider {
        lk_core::config::LlmProvider::Queue => {
            let queue_dir = vault_root.join(".lorekeeper").join("queue");
            Ok(Arc::new(lk_queue::QueueLlmClient::new(queue_dir)))
        }
        lk_core::config::LlmProvider::Noop => Ok(Arc::new(lk_queue::NoopLlmClient)),
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
