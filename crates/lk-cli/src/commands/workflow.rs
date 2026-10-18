//! A cooperative workflow lease, separate from short page/queue writer locks.
//!
//! Unix exec replaces this process with the command. The locked descriptor has only
//! CLOEXEC cleared, so it follows that exec and ordinary shell children; no waiter,
//! PID file, expiry or environment-variable ownership claim is involved.

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
        use rustix::fs::{Mode, OFlags};
        use rustix::io::{FdFlags, fcntl_getfd, fcntl_setfd};
        use std::os::unix::process::CommandExt;

        let (program, arguments) = command
            .split_first()
            .ok_or_else(|| miette::miette!("workflow requires a command after --"))?;
        let config_path = super::find_config(opts)?;
        let config = super::load_config(&config_path)?;
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
        if wait {
            eprintln!("Waiting for the vault workflow lease...");
            lock.lock()
                .map_err(|e| miette::miette!("lock workflow: {e}"))?;
        } else {
            lock.try_lock().map_err(|e| {
                miette::miette!("workflow busy or lock unavailable; command not started: {e}")
            })?;
        }
        let flags = fcntl_getfd(&lock)
            .map_err(|e| miette::miette!("read workflow descriptor flags: {e}"))?;
        fcntl_setfd(&lock, flags & !FdFlags::CLOEXEC)
            .map_err(|e| miette::miette!("inherit workflow descriptor: {e}"))?;
        let mut child = std::process::Command::new(program);
        child.args(arguments).env("LORE_CONFIG", &config_path);
        if let Some(template) = &opts.template_dir {
            let template = template
                .canonicalize()
                .map_err(|e| miette::miette!("resolve workflow template directory: {e}"))?;
            child.env("LORE_TEMPLATE_DIR", template);
        }
        let error = child.exec();
        // exec failure returns here; File drop releases the lease. Successful exec
        // retains the descriptor, without a supervising process that could die first.
        Err(miette::miette!(
            "exec workflow command {program:?}: {error}"
        ))
    }
}
