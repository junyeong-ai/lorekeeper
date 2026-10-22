#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;

use lk_dist::Installation;
#[cfg(unix)]
use lk_dist::SkillLevel;

use super::AgentTarget;
use super::{SelfCommand, SkillScope};
use crate::commands::GlobalOptions;

#[cfg(unix)]
use super::lease::{Lease, Mode, Request};

pub(super) const HANDOFF: &str = "LORE_INSTALLATION_HANDOFF";
#[cfg(unix)]
const SKIP_SCHEMA: &str = "LORE_INSTALLATION_SKIP_SCHEMA";

pub(crate) struct Protection {
    installation: Installation,
    config: Option<Configuration>,
    schema_ready: bool,
    #[cfg(unix)]
    lease: Option<Lease>,
}

struct Configuration {
    path: PathBuf,
    revision: String,
    root: PathBuf,
}

impl Configuration {
    fn capture(opts: &GlobalOptions) -> Option<Self> {
        let path = crate::commands::find_optional_config(opts).ok()??;
        let (config, revision) = crate::commands::load_config_with_revision(&path).ok()?;
        Some(Self {
            path,
            revision,
            root: config.vault.root_path(),
        })
    }

    fn validate(&self) -> miette::Result<()> {
        let selected = self.path.canonicalize().map_err(|error| {
            miette::miette!("selected installation configuration is unavailable: {error}")
        })?;
        let body = std::fs::read_to_string(&selected).map_err(|error| {
            miette::miette!("selected installation configuration is unavailable: {error}")
        })?;
        if selected != self.path || lk_core::concept::file_revision(&body) != self.revision {
            return Err(miette::miette!(
                "configuration changed while preparing installation access; retry the command"
            ));
        }
        Ok(())
    }
}

impl Protection {
    pub(crate) fn prepare(opts: &GlobalOptions, command: &crate::Command) -> miette::Result<Self> {
        let override_dir = match command {
            crate::Command::Installation { cmd } => match cmd {
                SelfCommand::Status { data_dir, .. }
                | SelfCommand::Deploy { data_dir, .. }
                | SelfCommand::Update { data_dir, .. }
                | SelfCommand::Uninstall { data_dir, .. } => data_dir.clone(),
            },
            _ => None,
        };
        let installation = super::installation(override_dir.clone())?;
        let config = Configuration::capture(opts);
        let protection = Self {
            installation,
            config,
            schema_ready: false,
            #[cfg(unix)]
            lease: None,
        };
        #[cfg(unix)]
        let mut protection = protection;
        let mutation = matches!(
            command,
            crate::Command::Installation {
                cmd: SelfCommand::Deploy { .. }
                    | SelfCommand::Update { .. }
                    | SelfCommand::Uninstall { .. }
            }
        );
        #[cfg(unix)]
        if !mutation {
            if std::env::var_os(HANDOFF).is_some() {
                return Err(miette::miette!(
                    "installation handoff is only valid for delegated deployment"
                ));
            }
            let requests =
                protection.requests(opts, Mode::Shared, AgentTarget::Both, None, false, false)?;
            let lease = acquire(&requests)?;
            protection.validate_selection(override_dir)?;
            protection.validate_binary()?;
            if matches!(
                command,
                crate::Command::Run { .. } | crate::Command::Workflow { .. }
            ) {
                lease
                    .inherit()
                    .map_err(|error| miette::miette!("inherit installation protection: {error}"))?;
            }
            protection.lease = Some(lease);
        }
        #[cfg(not(unix))]
        let _ = mutation;
        Ok(protection)
    }

    pub(crate) fn installation(&self) -> &Installation {
        &self.installation
    }

    pub(super) fn mutation(
        opts: &GlobalOptions,
        installation: Installation,
        override_dir: Option<PathBuf>,
        agent: AgentTarget,
        skills: Option<SkillScope>,
        replace_binary: bool,
    ) -> miette::Result<Self> {
        let inherited = std::env::var_os(HANDOFF).is_some();
        let config = Configuration::capture(opts);
        let protection = Self {
            installation,
            schema_ready: config.is_some(),
            config,
            #[cfg(unix)]
            lease: None,
        };
        #[cfg(unix)]
        let mut protection = protection;
        #[cfg(unix)]
        {
            if inherited && std::env::var_os(HANDOFF).as_deref() != Some(std::ffi::OsStr::new("1"))
            {
                return Err(miette::miette!("invalid installation handoff marker"));
            }
            let skip_schema = inherited && std::env::var_os(SKIP_SCHEMA).is_some();
            let requests = protection.requests(
                opts,
                Mode::Exclusive,
                agent,
                skills,
                replace_binary || inherited,
                !skip_schema,
            )?;
            protection.lease = Some(if inherited {
                Lease::receive(&requests)
                    .map_err(|error| miette::miette!("invalid installation handoff: {error}"))?
            } else {
                acquire(&requests)?
            });
            protection.validate_selection(override_dir)?;
            protection.validate_binary()?;
        }
        #[cfg(not(unix))]
        let _ = (opts, override_dir, agent, skills, replace_binary, inherited);
        Ok(protection)
    }

    #[cfg(unix)]
    fn validate_selection(&self, override_dir: Option<PathBuf>) -> miette::Result<()> {
        let selected = super::installation(override_dir)?;
        if selected.binary() != self.installation.binary()
            || selected.data_dir() != self.installation.data_dir()
            || selected.config_dir() != self.installation.config_dir()
        {
            return Err(miette::miette!(
                "installation selection changed while acquiring protection; retry the command"
            ));
        }
        if let Some(config) = &self.config {
            config.validate()?;
        }
        Ok(())
    }

    #[cfg(unix)]
    fn validate_binary(&self) -> miette::Result<()> {
        let version = lk_dist::running_version(self.installation.binary())
            .map_err(|error| miette::miette!("{error}"))?;
        if version != lk_dist::current_version() {
            return Err(miette::miette!(
                "installed binary changed before the command acquired protection; rerun the selected executable"
            ));
        }
        Ok(())
    }

    pub(super) fn schema_ready(&self) -> bool {
        self.schema_ready
    }

    pub(super) fn claim_publication(
        &self,
    ) -> miette::Result<Option<(lk_vault::MutationClaim, Option<std::fs::File>)>> {
        let Some(config) = &self.config else {
            return Ok(None);
        };
        config.validate()?;
        let claim = lk_vault::MutationClaim::acquire(&config.root)
            .map_err(|error| miette::miette!("claim installation publication boundary: {error}"))?;
        let queue = claim.root().join(".lorekeeper/queue");
        if !lk_core::fs::canonical_prefix(&queue).starts_with(claim.root()) {
            return Err(miette::miette!("installation queue state escapes vault"));
        }
        let queue_claim = crate::commands::queue::claim_writer(&queue, false)?;
        Ok(Some((claim, queue_claim)))
    }

    pub(super) fn recheck_configuration(&self) -> miette::Result<()> {
        if let Some(config) = &self.config {
            config.validate()?;
        }
        Ok(())
    }

    pub(super) fn release(&mut self) {
        #[cfg(unix)]
        {
            self.lease = None;
        }
    }

    pub(super) fn validate_staged(&self, current: &Self) -> miette::Result<()> {
        let before = self
            .config
            .as_ref()
            .map(|config| (&config.path, &config.revision));
        let after = current
            .config
            .as_ref()
            .map(|config| (&config.path, &config.revision));
        if before != after
            || self.installation.binary() != current.installation.binary()
            || self.installation.data_dir() != current.installation.data_dir()
            || self.installation.config_dir() != current.installation.config_dir()
        {
            return Err(miette::miette!(
                "installation or configuration changed during download; retry the update"
            ));
        }
        Ok(())
    }

    pub(super) fn configure(&self, command: &mut std::process::Command) -> miette::Result<()> {
        command.env("LORE_INSTALL_DATA_DIR", self.installation.data_dir());
        if let Some(config) = &self.config {
            command
                .env("LORE_CONFIG", &config.path)
                .env(crate::commands::CONFIG_REVISION_ENV, &config.revision);
        }
        #[cfg(unix)]
        {
            let lease = self.lease.as_ref().ok_or_else(|| {
                miette::miette!("installation deployment has no exclusive protection")
            })?;
            command
                .stdin(
                    lease.handoff().map_err(|error| {
                        miette::miette!("handoff installation protection: {error}")
                    })?,
                )
                .env(HANDOFF, "1");
            if !self.schema_ready {
                command.env(SKIP_SCHEMA, "1");
            } else {
                command.env_remove(SKIP_SCHEMA);
            }
        }
        Ok(())
    }
}

#[cfg(unix)]
impl Protection {
    fn requests(
        &mut self,
        opts: &GlobalOptions,
        mode: Mode,
        agent: AgentTarget,
        skills: Option<SkillScope>,
        replace_binary: bool,
        schema: bool,
    ) -> miette::Result<Vec<Request>> {
        let installation = &self.installation;
        let mut resources = vec![
            (
                installation.binary().to_path_buf(),
                if replace_binary { mode } else { Mode::Shared },
            ),
            (installation.data_dir().to_path_buf(), mode),
            (installation.templates_dir(), mode),
            (installation.config_dir().to_path_buf(), mode),
        ];
        let mut requests = Vec::new();
        for engine in agent.engines() {
            let selected = installation.for_engine(*engine);
            let levels = if mode == Mode::Shared {
                vec![SkillLevel::User, SkillLevel::Project]
            } else {
                match skills {
                    Some(scope) => scope.level().into_iter().collect(),
                    None => selected.deployed_skill_levels(),
                }
            };
            for level in levels {
                if let Some(path) = selected.skills_dir(level) {
                    if let Some(lock) = skill_lock(&path, mode)? {
                        requests.push(Request { path: lock, mode });
                    }
                    if std::fs::symlink_metadata(&path).is_ok() {
                        resources.push((path, mode));
                    }
                }
            }
        }
        if let Some(path) = &opts.template_dir {
            resources.push((path.clone(), mode));
        }
        let resources = resources
            .into_iter()
            .filter(|(path, _)| mode == Mode::Exclusive || path.exists())
            .map(|(path, mode)| destination_lock(&path).map(|path| Request { path, mode }))
            .collect::<miette::Result<Vec<_>>>()?;
        requests.extend(resources);
        if mode == Mode::Exclusive && schema {
            if let Some(config) = &self.config {
                match schema_state(&config.root) {
                    Ok(state) => requests.push(Request {
                        path: state.join("workflow.lock"),
                        mode: Mode::Exclusive,
                    }),
                    Err(error) => {
                        self.schema_ready = false;
                        eprintln!(
                            "warning: schema synchronization unavailable ({error}); run `lore schema` when the vault is available"
                        );
                    }
                }
            }
        } else if mode == Mode::Exclusive {
            self.schema_ready = false;
        }
        Ok(requests)
    }
}

#[cfg(unix)]
fn schema_state(root: &Path) -> std::io::Result<PathBuf> {
    let root = root.canonicalize()?;
    let state = root.join(".lorekeeper");
    if !lk_core::fs::canonical_prefix(&state).starts_with(&root) {
        return Err(std::io::Error::other(
            "installation schema state escapes vault",
        ));
    }
    std::fs::create_dir_all(&state)?;
    let state = state.canonicalize()?;
    if !state.starts_with(&root) {
        return Err(std::io::Error::other(
            "installation schema state escapes vault",
        ));
    }
    Ok(state)
}

#[cfg(unix)]
fn acquire(requests: &[Request]) -> miette::Result<Lease> {
    Lease::acquire(requests).map_err(|error| {
        if error.kind() == std::io::ErrorKind::WouldBlock {
            miette::miette!(
                "installation is busy or in use; no installation files changed: {error}"
            )
        } else {
            miette::miette!("installation protection unavailable: {error}")
        }
    })
}

#[cfg(unix)]
fn destination_lock(path: &Path) -> miette::Result<PathBuf> {
    let selected = std::path::absolute(path).map_err(|error| miette::miette!("{error}"))?;
    let selected = match selected.canonicalize() {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if std::fs::symlink_metadata(&selected).is_ok() {
                return Err(miette::miette!(
                    "installation resource is a broken symlink: {}",
                    selected.display()
                ));
            }
            selected
        }
        Err(error) => {
            return Err(miette::miette!(
                "resolve installation resource {}: {error}",
                selected.display()
            ));
        }
    };
    let parent = selected.parent().ok_or_else(|| {
        miette::miette!(
            "installation resource has no parent: {}",
            selected.display()
        )
    })?;
    let name = selected.file_name().ok_or_else(|| {
        miette::miette!("installation resource has no name: {}", selected.display())
    })?;
    std::fs::create_dir_all(parent).map_err(|error| {
        miette::miette!(
            "prepare installation coordination {}: {error}",
            parent.display()
        )
    })?;
    let parent = parent
        .canonicalize()
        .map_err(|error| miette::miette!("{error}"))?;
    Ok(parent.join(format!(
        ".lorekeeper-{}.lock",
        lk_dist::sha256_hex(name.as_encoded_bytes())
    )))
}

#[cfg(unix)]
fn skill_lock(path: &Path, mode: Mode) -> miette::Result<Option<PathBuf>> {
    let base = path
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| miette::miette!("skill resource has no host directory"))?;
    let relative = path
        .strip_prefix(base)
        .expect("skill root is under its host");
    if mode == Mode::Exclusive {
        std::fs::create_dir_all(base)
            .map_err(|error| miette::miette!("prepare skill host {}: {error}", base.display()))?;
    } else if !base
        .try_exists()
        .map_err(|error| miette::miette!("inspect skill host {}: {error}", base.display()))?
    {
        return Ok(None);
    }
    let base = base
        .canonicalize()
        .map_err(|error| miette::miette!("resolve skill host {}: {error}", base.display()))?;
    Ok(Some(base.join(format!(
        ".lorekeeper-{}.lock",
        lk_dist::sha256_hex(relative.as_os_str().as_encoded_bytes())
    ))))
}
