use std::path::{Path, PathBuf};

use super::{queue_load, resolve_vault_root};

/// Shared read-only update/check boundary. Inspecting never recovers a journal or
/// removes a result, including when a caller explicitly permits outstanding work.
#[derive(serde::Serialize)]
pub(crate) struct QueueSettlement {
    pub current: usize,
    pub unreadable: usize,
    pub unparseable_lines: usize,
    pub staged_results: Vec<String>,
    pub pending_mutation: bool,
}

impl QueueSettlement {
    pub fn is_settled(&self) -> bool {
        self.current == 0
            && self.unreadable == 0
            && self.unparseable_lines == 0
            && self.staged_results.is_empty()
            && !self.pending_mutation
    }
}

pub(crate) fn inspect(root: &Path) -> miette::Result<QueueSettlement> {
    let load = queue_load(root)?;
    let results = root
        .join(".lorekeeper/queue")
        .join(lk_queue::RESULTS_SUBDIR);
    let mut staged = Vec::new();
    match std::fs::read_dir(&results) {
        Ok(entries) => {
            for entry in entries {
                let path = entry
                    .map_err(|e| miette::miette!("read results: {e}"))?
                    .path();
                if path.extension().is_some_and(|ext| ext == "json") {
                    staged.push(path.file_name().unwrap().to_string_lossy().into_owned());
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(miette::miette!("read results: {error}")),
    }
    staged.sort();
    let pending_mutation =
        match std::fs::symlink_metadata(root.join(".lorekeeper/mutation/pending.json")) {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(miette::miette!("inspect mutation journal: {error}")),
        };
    Ok(QueueSettlement {
        current: load.work,
        unreadable: load.unreadable,
        unparseable_lines: load.unparseable,
        staged_results: staged,
        pending_mutation,
    })
}

pub(super) fn run(
    opts: &crate::commands::GlobalOptions,
    root: Option<PathBuf>,
) -> miette::Result<()> {
    let root = resolve_vault_root(opts, root)?;
    let state = inspect(&root)?;
    let ok = state.is_settled();
    let report = serde_json::json!({
        "ok": ok,
        "data": state,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&report).map_err(|e| miette::miette!("{e}"))?
    );
    if !ok {
        return Err(miette::miette!(
            "queue is not settled; inspect the reported tasks, results or mutation journal"
        ));
    }
    Ok(())
}
