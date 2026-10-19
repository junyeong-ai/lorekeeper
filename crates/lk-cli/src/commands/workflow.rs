//! Cooperative Unix leases shared by manual agents and native job execution.

#[cfg(unix)]
pub(super) struct WorkflowLease {
    id: lk_core::execution::WorkflowId,
    lock: std::fs::File,
    config_path: std::path::PathBuf,
    executable: std::path::PathBuf,
    path: std::ffi::OsString,
    template: Option<std::path::PathBuf>,
}

#[cfg(unix)]
impl WorkflowLease {
    pub fn prepare(opts: &super::GlobalOptions) -> miette::Result<Self> {
        use rustix::fs::{Mode, OFlags};
        use rustix::io::{FdFlags, fcntl_getfd, fcntl_setfd};
        let config_path = super::find_config(opts)?;
        let config = super::load_config(&config_path)?;
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
        let root = config
            .vault
            .root_path()
            .canonicalize()
            .map_err(|e| miette::miette!("resolve workflow vault: {e}"))?;
        let state = root.join(".lorekeeper");
        if !lk_core::fs::canonical_prefix(&state).starts_with(&root) {
            return Err(miette::miette!("workflow state escapes vault"));
        }
        std::fs::create_dir_all(&state)
            .map_err(|e| miette::miette!("create workflow state: {e}"))?;
        let state = state
            .canonicalize()
            .map_err(|e| miette::miette!("resolve workflow state: {e}"))?;
        if !state.starts_with(&root) {
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
        Ok(Self {
            id: lk_core::execution::WorkflowId::from_bytes(identity),
            lock,
            config_path,
            executable,
            path,
            template,
        })
    }

    pub fn try_lock(&self) -> Result<(), std::fs::TryLockError> {
        self.lock.try_lock()
    }

    pub fn id(&self) -> &lk_core::execution::WorkflowId {
        &self.id
    }

    pub fn configure(&self, command: &mut std::process::Command) {
        command.env("LORE_WORKFLOW_ID", self.id.as_str());
        command
            .env("LORE_CONFIG", &self.config_path)
            .env("LORE_BIN", &self.executable)
            .env("PATH", &self.path);
        if let Some(template) = &self.template {
            command.env("LORE_TEMPLATE_DIR", template);
        }
    }
}

pub fn run(
    opts: &super::GlobalOptions,
    wait: bool,
    command: Vec<std::ffi::OsString>,
) -> miette::Result<()> {
    #[cfg(not(unix))]
    {
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
        let lease = WorkflowLease::prepare(opts)?;
        if wait {
            eprintln!("Waiting for the vault workflow lease...");
            lease
                .lock
                .lock()
                .map_err(|e| miette::miette!("lock workflow: {e}"))?;
        } else {
            lease.try_lock().map_err(|e| {
                miette::miette!("workflow busy or lock unavailable; command not started: {e}")
            })?;
        }
        let mut child = std::process::Command::new(program);
        child.args(arguments);
        lease.configure(&mut child);
        let error = child.exec();
        Err(miette::miette!(
            "exec workflow command {program:?}: {error}"
        ))
    }
}
