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

/// A resolved vault root plus the config that governs it — the single source for every
/// command that takes a `--root` override (`wiki`, `schema`, `graph`).
pub(crate) struct RootConfig {
    /// The effective vault root (the `--root` override, or `vault.root` from config).
    pub root: PathBuf,
    /// `None` ONLY when an explicit `--root` was given AND no config file exists at all
    /// (binary-only use: run on the root with defaults). A config that EXISTS but fails to
    /// parse/validate propagates the error instead of silently degrading to defaults —
    /// otherwise a typo'd config would make the command read/write the WRONG dirs/locale.
    pub config: Option<lk_core::config::Config>,
}

/// Resolve `(root, config)` for a `--root`-capable command. The ONE place the override
/// semantics live, so every such command behaves identically: an explicit `--root` runs
/// even without a config (defaults fill in), a present config always drives dirs/locale/etc.
/// even under `--root` (only the root is overridden), and a present-but-broken config fails
/// loudly. Without `--root`, a config is mandatory and its `vault.root` is used.
pub(crate) fn resolve_root_config(
    opts: &GlobalOptions,
    root_override: Option<PathBuf>,
) -> miette::Result<RootConfig> {
    match root_override {
        Some(root) => match find_config(opts) {
            Ok(path) => Ok(RootConfig {
                root,
                config: Some(load_config(&path)?),
            }),
            Err(_) => Ok(RootConfig { root, config: None }),
        },
        None => {
            let path = find_config(opts)?;
            let config = load_config(&path)?;
            Ok(RootConfig {
                root: config.vault.root_path(),
                config: Some(config),
            })
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const VALID_CONFIG: &str = "\
vault:
  root: /tmp/v
  dirs:
    wiki: knowledge
identity:
  name: t
  email: t@t.com
sources:
  s1:
    type: gmail
";

    fn opts(config: Option<PathBuf>) -> GlobalOptions {
        GlobalOptions {
            config,
            template_dir: None,
        }
    }

    #[test]
    fn root_override_keeps_a_present_config_not_defaults() {
        // The bug this guards (was live in `graph --root`): an explicit `--root` must NOT
        // discard a present config. Only the root is overridden; dirs/locale/etc. still come
        // from the config — otherwise the command reads/writes the WRONG dirs in the WRONG locale.
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("config.yaml");
        std::fs::write(&cfg, VALID_CONFIG).unwrap();
        let rc = resolve_root_config(&opts(Some(cfg)), Some(PathBuf::from("/override"))).unwrap();
        assert_eq!(rc.root, PathBuf::from("/override"));
        let config = rc
            .config
            .expect("a present config must be loaded, never dropped");
        assert_eq!(
            config.vault.dirs.wiki, "knowledge",
            "config dirs must survive the --root override"
        );
    }

    #[test]
    fn root_override_with_a_broken_config_fails_loudly() {
        // A config that EXISTS but fails to validate must error, never silently fall back to
        // defaults (which would hide the user's mistake and act on the wrong dirs).
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("config.yaml");
        std::fs::write(
            &cfg,
            "vault:\n  root: /tmp/v\n  dirs:\n    wiki: \"../escape\"\n\
             identity:\n  name: t\n  email: t@t.com\nsources:\n  s1:\n    type: gmail\n",
        )
        .unwrap();
        assert!(
            resolve_root_config(&opts(Some(cfg)), Some(PathBuf::from("/override"))).is_err(),
            "a present-but-invalid config must propagate, not degrade to defaults"
        );
    }

    #[test]
    fn root_override_without_any_config_yields_none() {
        // No config file at all → None, so the caller fills in defaults (binary-only use).
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("nope.yaml");
        let rc =
            resolve_root_config(&opts(Some(missing)), Some(PathBuf::from("/override"))).unwrap();
        assert_eq!(rc.root, PathBuf::from("/override"));
        assert!(rc.config.is_none());
    }
}
