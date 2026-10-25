//! Cooperative Unix leases shared by manual agents and native job execution.

#[cfg(unix)]
mod root;
#[cfg(unix)]
pub(super) use root::VaultRootPin;

const ROOT_ENV: &str = "LORE_VAULT_ROOT";
const SELECTOR_ENV: &str = "LORE_VAULT_ROOT_SELECTOR";
const IDENTITY_ENV: &str = "LORE_VAULT_ROOT_IDENTITY";

pub(super) fn validate_context() -> miette::Result<()> {
    #[cfg(unix)]
    if let Some(pin) = VaultRootPin::inherited()? {
        pin.validate()?;
    }
    #[cfg(not(unix))]
    if [ROOT_ENV, SELECTOR_ENV, IDENTITY_ENV]
        .iter()
        .any(|key| std::env::var_os(key).is_some())
    {
        return Err(miette::miette!("workflow vault context requires Unix"));
    }
    Ok(())
}

pub(super) fn validate_root(root: &std::path::Path) -> miette::Result<()> {
    #[cfg(unix)]
    if let Some(pin) = VaultRootPin::inherited()? {
        pin.validate_target(root)?;
    }
    #[cfg(not(unix))]
    {
        let _ = root;
        validate_context()?;
    }
    Ok(())
}

pub(super) fn bind_root(root: std::path::PathBuf) -> miette::Result<std::path::PathBuf> {
    #[cfg(unix)]
    if let Some(pin) = VaultRootPin::inherited()? {
        pin.validate_target(&root)?;
        return Ok(pin.root().to_path_buf());
    }
    #[cfg(not(unix))]
    validate_context()?;
    Ok(root)
}

#[derive(Debug)]
pub(super) struct WorkflowBusy;

impl std::fmt::Display for WorkflowBusy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("workflow busy; command not started")
    }
}

impl std::error::Error for WorkflowBusy {}
impl miette::Diagnostic for WorkflowBusy {}

#[cfg(unix)]
pub(super) struct WorkflowLease {
    id: lk_core::execution::WorkflowId,
    lock: std::fs::File,
    root: VaultRootPin,
    config_path: std::path::PathBuf,
    config_revision: String,
    executable: std::path::PathBuf,
    path: std::ffi::OsString,
    template: Option<std::path::PathBuf>,
}

#[cfg(unix)]
impl WorkflowLease {
    pub fn prepare_selected(
        opts: &super::GlobalOptions,
        config: &lk_core::config::Config,
        expected_root: &std::path::Path,
        config_path: std::path::PathBuf,
        config_revision: String,
    ) -> miette::Result<Self> {
        use rustix::fs::{Mode, OFlags};
        use rustix::io::{FdFlags, fcntl_getfd, fcntl_setfd};
        let executable = std::env::current_exe()
            .map_err(|e| miette::miette!("resolve workflow executable: {e}"))?;
        let binary_dir = executable
            .parent()
            .ok_or_else(|| miette::miette!("workflow executable has no parent directory"))?;
        if !same_file::is_same_file(&executable, binary_dir.join("lore")).unwrap_or(false) {
            return Err(miette::miette!(
                "workflow requires this executable to be available as `lore` in its directory; \
                 install it with the canonical name so child commands use the selected version"
            ));
        }
        let path = std::env::join_paths(std::iter::once(binary_dir.to_path_buf()).chain(
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
        ))
        .map_err(|e| miette::miette!("prepare workflow PATH: {e}"))?;
        let root = VaultRootPin::capture(&config.vault.root_path(), expected_root)?;
        let state = root.root().join(".lorekeeper");
        if !lk_core::fs::canonical_prefix(&state).starts_with(root.root()) {
            return Err(miette::miette!("workflow state escapes vault"));
        }
        std::fs::create_dir_all(&state)
            .map_err(|e| miette::miette!("create workflow state: {e}"))?;
        let state = state
            .canonicalize()
            .map_err(|e| miette::miette!("resolve workflow state: {e}"))?;
        if !state.starts_with(root.root()) {
            return Err(miette::miette!("workflow state escapes vault"));
        }
        let directory = rustix::fs::open(
            &state,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|e| miette::miette!("open workflow state: {e}"))?;
        let lock = rustix::fs::openat(
            &directory,
            "workflow.lock",
            OFlags::CREATE | OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::RUSR | Mode::WUSR,
        )
        .map(std::fs::File::from)
        .map_err(|e| miette::miette!("open workflow lock: {e}"))?;
        if !lock
            .metadata()
            .map_err(|e| miette::miette!("inspect workflow lock: {e}"))?
            .is_file()
        {
            return Err(miette::miette!("workflow lock is not a regular file"));
        }
        let flags = fcntl_getfd(&lock)
            .map_err(|e| miette::miette!("read workflow descriptor flags: {e}"))?;
        fcntl_setfd(&lock, flags & !FdFlags::CLOEXEC)
            .map_err(|e| miette::miette!("inherit workflow descriptor: {e}"))?;
        let template = opts
            .template_dir
            .as_ref()
            .map(|path| path.canonicalize())
            .transpose()
            .map_err(|e| miette::miette!("resolve workflow template directory: {e}"))?;
        let mut identity = [0; 16];
        rand::fill(&mut identity);
        root.validate()?;
        Ok(Self {
            id: lk_core::execution::WorkflowId::from_bytes(identity),
            lock,
            root,
            config_path,
            config_revision,
            executable,
            path,
            template,
        })
    }

    pub async fn acquire_selected(
        opts: &super::GlobalOptions,
        config: &lk_core::config::Config,
        expected_root: &std::path::Path,
        config_path: std::path::PathBuf,
        config_revision: String,
        wait: bool,
    ) -> miette::Result<Self> {
        let pin =
            super::ConfigurationPin::from_loaded(config_path.clone(), config_revision.clone());
        pin.validate()?;
        let lease =
            Self::prepare_selected(opts, config, expected_root, config_path, config_revision)?;
        let mut waiting = false;
        loop {
            pin.validate()?;
            lease.root.validate()?;
            match lease.try_lock() {
                Ok(()) => {
                    pin.validate()?;
                    lease.root.validate()?;
                    return Ok(lease);
                }
                Err(std::fs::TryLockError::WouldBlock) if wait => {
                    if !waiting {
                        eprintln!("Waiting for the vault workflow lease...");
                        waiting = true;
                    }
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(miette::Report::new(WorkflowBusy));
                }
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(miette::miette!("workflow lock unavailable: {error}"));
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    pub async fn acquire_publication(
        opts: &super::GlobalOptions,
        config: &lk_core::config::Config,
        expected_root: &std::path::Path,
        config_path: std::path::PathBuf,
        config_revision: String,
        wait: bool,
    ) -> miette::Result<Self> {
        use rustix::io::{FdFlags, fcntl_getfd, fcntl_setfd};
        let lease = Self::acquire_selected(
            opts,
            config,
            expected_root,
            config_path,
            config_revision,
            wait,
        )
        .await?;
        let flags = fcntl_getfd(&lease.lock)
            .map_err(|error| miette::miette!("read publication workflow descriptor: {error}"))?;
        fcntl_setfd(&lease.lock, flags | FdFlags::CLOEXEC)
            .map_err(|error| miette::miette!("isolate publication workflow descriptor: {error}"))?;
        Ok(lease)
    }

    pub fn try_lock(&self) -> Result<(), std::fs::TryLockError> {
        self.lock.try_lock()
    }

    pub fn id(&self) -> &lk_core::execution::WorkflowId {
        &self.id
    }

    pub fn root_pin(&self) -> &VaultRootPin {
        &self.root
    }

    pub fn configure(&self, command: &mut std::process::Command) {
        self.root.configure(command);
        command.env("LORE_WORKFLOW_ID", self.id.as_str());
        command.env(super::CONFIG_REVISION_ENV, &self.config_revision);
        command
            .env("LORE_CONFIG", &self.config_path)
            .env("LORE_BIN", &self.executable)
            .env("PATH", &self.path);
        if let Some(template) = &self.template {
            command.env("LORE_TEMPLATE_DIR", template);
        }
    }
}

pub async fn run(
    opts: &super::GlobalOptions,
    installation: &lk_dist::Installation,
    wait: bool,
    command: Vec<std::ffi::OsString>,
) -> miette::Result<()> {
    #[cfg(not(unix))]
    {
        let _ = installation;
        let _ = (opts, wait, command);
        Err(miette::miette!(
            "workflow leases require Unix; Windows is unsupported"
        ))
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let (program, arguments) = command
            .split_first()
            .ok_or_else(|| miette::miette!("workflow requires a command after --"))?;
        let config_path = super::find_config(opts)?;
        let (config, revision) = super::load_config_with_revision(&config_path)?;
        let lease = WorkflowLease::acquire_selected(
            opts,
            &config,
            &config.vault.root_path(),
            config_path,
            revision,
            wait,
        )
        .await?;
        let mut child = std::process::Command::new(program);
        child.args(arguments);
        child.env("LORE_INSTALL_DATA_DIR", installation.data_dir());
        lease.configure(&mut child);
        let error = child.exec();
        Err(miette::miette!(
            "exec workflow command {program:?}: {error}"
        ))
    }
}
