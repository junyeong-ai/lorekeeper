use std::path::{Path, PathBuf};

use lk_queue::QueueTask;

use super::{find_config, load_config};

#[derive(clap::Subcommand)]
pub enum QueueCmd {
    /// Classify every pending queue task against its target page. `/lore-process`
    /// consults this instead of comparing hashes itself: it processes `current`
    /// tasks, drops `stale` ones (the page was re-rendered by a newer ingest), and
    /// reports `missing-target`. The stale-task guard is computed here in tested
    /// Rust, not in prose.
    Status {
        /// Vault root override (default: vault.root from config)
        #[arg(long)]
        root: Option<PathBuf>,
        /// Output in JSON format (envelope: {"ok": true, "data": …})
        #[arg(long)]
        json: bool,
    },
    /// Remove dead tasks from pending queue files using the same classification as
    /// `status`: `stale` and `missing-target` tasks are exactly what `/lore-process`
    /// would drop without editing anything, so pruning them is that drop performed
    /// deterministically, without an LLM session. Files are rewritten atomically
    /// keeping only `current` tasks; a file left with none is deleted (it never
    /// produced page edits, so there is nothing to archive in `processed/`).
    Prune {
        /// Vault root override (default: vault.root from config)
        #[arg(long)]
        root: Option<PathBuf>,
        /// Output in JSON format (envelope: {"ok": true, "data": …})
        #[arg(long)]
        json: bool,
        /// Classify and report only; write and delete nothing
        #[arg(long)]
        dry_run: bool,
    },
}

/// Where a queued task stands relative to its target page's current input hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskStatus {
    /// `llm_inputs.<key>` on the page equals the task's `cache_hash` — process it.
    Current,
    /// The page was re-rendered with a different input after this task was queued —
    /// drop it; a newer task carries the current hash.
    Stale,
    /// The target page does not exist — the result has nowhere to land.
    MissingTarget,
}

impl TaskStatus {
    fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Current => "current",
            TaskStatus::Stale => "stale",
            TaskStatus::MissingTarget => "missing-target",
        }
    }
}

struct TaskReport {
    task_id: String,
    kind: String,
    vault_path: String,
    status: TaskStatus,
}

pub async fn run(opts: &super::GlobalOptions, cmd: QueueCmd) -> miette::Result<()> {
    match cmd {
        QueueCmd::Status { root, json } => status(opts, root, json).await,
        QueueCmd::Prune {
            root,
            json,
            dry_run,
        } => prune(opts, root, json, dry_run).await,
    }
}

fn resolve_vault_root(
    opts: &super::GlobalOptions,
    root: Option<PathBuf>,
) -> miette::Result<PathBuf> {
    match root {
        Some(r) => Ok(r),
        None => Ok(load_config(&find_config(opts)?)?.vault.root_path()),
    }
}

/// Pending queue files, sorted by name (run-id order). `.jsonl` only — skips the
/// `processed/` subdir and any `.jsonl.tmp`.
fn pending_queue_files(queue_dir: &Path) -> miette::Result<Vec<PathBuf>> {
    if !queue_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(queue_dir)
        .map_err(|e| miette::miette!("read {}: {e}", queue_dir.display()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl"))
        .collect();
    files.sort();
    Ok(files)
}

/// Read every task in one queue file. An unparseable line is a hard error — the
/// file is left intact for inspection rather than silently dropping work.
fn read_tasks(file: &Path) -> miette::Result<Vec<QueueTask>> {
    let content = std::fs::read_to_string(file)
        .map_err(|e| miette::miette!("read {}: {e}", file.display()))?;
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .map_err(|e| miette::miette!("parse task in {}: {e}", file.display()))
        })
        .collect()
}

async fn status(
    opts: &super::GlobalOptions,
    root: Option<PathBuf>,
    json: bool,
) -> miette::Result<()> {
    let vault_root = resolve_vault_root(opts, root)?;
    let queue_dir = vault_root.join(".lorekeeper").join("queue");

    let mut reports = Vec::new();
    for file in pending_queue_files(&queue_dir)? {
        for task in read_tasks(&file)? {
            let status = classify_task(&vault_root, &task)?;
            reports.push(TaskReport {
                task_id: task.task_id,
                kind: task.kind.as_str().to_string(),
                vault_path: task.target.vault_path,
                status,
            });
        }
    }

    let current = reports
        .iter()
        .filter(|r| r.status == TaskStatus::Current)
        .count();
    let stale = reports
        .iter()
        .filter(|r| r.status == TaskStatus::Stale)
        .count();
    let missing = reports
        .iter()
        .filter(|r| r.status == TaskStatus::MissingTarget)
        .count();

    if json {
        let tasks: Vec<serde_json::Value> = reports
            .iter()
            .map(|r| {
                serde_json::json!({
                    "task_id": r.task_id,
                    "kind": r.kind,
                    "vault_path": r.vault_path,
                    "status": r.status.as_str(),
                })
            })
            .collect();
        let envelope = serde_json::json!({
            "ok": true,
            "data": {
                "current": current,
                "stale": stale,
                "missing_target": missing,
                "tasks": tasks,
            }
        });
        println!("{}", serde_json::to_string_pretty(&envelope).unwrap());
    } else {
        for r in &reports {
            eprintln!(
                "  [{}] {} ({}) → {}",
                r.status.as_str(),
                r.task_id,
                r.kind,
                r.vault_path
            );
        }
        eprintln!(
            "queue: {current} current, {stale} stale, {missing} missing-target \
             across {} task(s)",
            reports.len()
        );
    }
    Ok(())
}

#[derive(Debug, Default, PartialEq, Eq)]
struct PruneSummary {
    pruned_stale: usize,
    pruned_missing_target: usize,
    kept_current: usize,
    files_rewritten: usize,
    files_deleted: usize,
}

/// Drop every `stale` and `missing-target` task from the pending queue, keeping
/// `current` ones. Safe by construction: a pending file's tasks targeted pages that
/// existed when the file was renamed into place (flush invariant), so missing-target
/// means the page was deleted afterwards — the result has nowhere to land — and
/// stale means `/lore-process` would drop the task on contact without editing.
/// Files all-current are left untouched; rewrites go through
/// `lk_queue::write_tasks_atomic` (temp + fsync + rename).
fn prune_queue(vault_root: &Path, queue_dir: &Path, dry_run: bool) -> miette::Result<PruneSummary> {
    let mut summary = PruneSummary::default();
    for file in pending_queue_files(queue_dir)? {
        let tasks = read_tasks(&file)?;
        let mut kept = Vec::with_capacity(tasks.len());
        let mut dropped = 0usize;
        for task in tasks {
            match classify_task(vault_root, &task)? {
                TaskStatus::Current => kept.push(task),
                TaskStatus::Stale => {
                    summary.pruned_stale += 1;
                    dropped += 1;
                }
                TaskStatus::MissingTarget => {
                    summary.pruned_missing_target += 1;
                    dropped += 1;
                }
            }
        }
        summary.kept_current += kept.len();
        if dropped == 0 {
            // All-current file: stays byte-identical on disk.
            continue;
        }
        if kept.is_empty() {
            summary.files_deleted += 1;
            if !dry_run {
                std::fs::remove_file(&file)
                    .map_err(|e| miette::miette!("remove {}: {e}", file.display()))?;
            }
        } else {
            summary.files_rewritten += 1;
            if !dry_run {
                lk_queue::write_tasks_atomic(&file, &kept)
                    .map_err(|e| miette::miette!("rewrite {}: {e}", file.display()))?;
            }
        }
    }
    Ok(summary)
}

async fn prune(
    opts: &super::GlobalOptions,
    root: Option<PathBuf>,
    json: bool,
    dry_run: bool,
) -> miette::Result<()> {
    let vault_root = resolve_vault_root(opts, root)?;
    let queue_dir = vault_root.join(".lorekeeper").join("queue");
    let summary = prune_queue(&vault_root, &queue_dir, dry_run)?;

    if json {
        let envelope = serde_json::json!({
            "ok": true,
            "data": {
                "dry_run": dry_run,
                "pruned_stale": summary.pruned_stale,
                "pruned_missing_target": summary.pruned_missing_target,
                "kept_current": summary.kept_current,
                "files_rewritten": summary.files_rewritten,
                "files_deleted": summary.files_deleted,
            }
        });
        println!("{}", serde_json::to_string_pretty(&envelope).unwrap());
    } else {
        let label = if dry_run {
            "queue prune (dry-run)"
        } else {
            "queue prune"
        };
        eprintln!(
            "{label}: dropped {} stale + {} missing-target, kept {} current; \
             {} file(s) rewritten, {} deleted",
            summary.pruned_stale,
            summary.pruned_missing_target,
            summary.kept_current,
            summary.files_rewritten,
            summary.files_deleted
        );
    }
    Ok(())
}

/// Classify one task by comparing its `cache_hash` to the current input hash the
/// pipeline stamped into the target page's `llm_inputs.<key>` frontmatter. This is
/// the deterministic form of the stale-task guard `/lore-process` must honor.
fn classify_task(vault_root: &Path, task: &QueueTask) -> miette::Result<TaskStatus> {
    let page_path = vault_root.join(&task.target.vault_path);
    let content = match std::fs::read_to_string(&page_path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(TaskStatus::MissingTarget);
        }
        Err(e) => return Err(miette::miette!("read {}: {e}", page_path.display())),
    };
    let page = lk_core::frontmatter::parse_page(&content)
        .map_err(|e| miette::miette!("parse {}: {e}", page_path.display()))?;
    let key = task.target.kind.llm_inputs_key();
    let stored = page
        .frontmatter
        .get(lk_core::frontmatter::field::LLM_INPUTS)
        .and_then(|v| v.get(key))
        .and_then(|v| v.as_str());
    Ok(if stored == Some(task.cache_hash.as_str()) {
        TaskStatus::Current
    } else {
        TaskStatus::Stale
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lk_queue::{TargetKind, TaskKind, TaskTarget};
    use tempfile::TempDir;

    fn task(vault_path: &str, hash: &str) -> QueueTask {
        QueueTask {
            task_id: "sum-1".into(),
            kind: TaskKind::Summarize,
            created_at: "2026-05-23T10:00:00Z".parse().unwrap(),
            cache_hash: hash.into(),
            input: serde_json::Value::Null,
            target: TaskTarget {
                vault_path: vault_path.into(),
                kind: TargetKind::DailySummary,
                anchor: "## 요약".into(),
            },
        }
    }

    fn write_page(root: &Path, rel: &str, frontmatter: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("---\n{frontmatter}---\n\n## 요약\n\nbody\n")).unwrap();
    }

    #[test]
    fn matching_hash_is_current() {
        let dir = TempDir::new().unwrap();
        write_page(
            dir.path(),
            "daily/s/2026-05-23.md",
            "id: x\nllm_inputs:\n  summary: abc123\n",
        );
        let t = task("daily/s/2026-05-23.md", "abc123");
        assert_eq!(classify_task(dir.path(), &t).unwrap(), TaskStatus::Current);
    }

    #[test]
    fn divergent_hash_is_stale() {
        let dir = TempDir::new().unwrap();
        write_page(
            dir.path(),
            "daily/s/2026-05-23.md",
            "id: x\nllm_inputs:\n  summary: NEWHASH\n",
        );
        // Task carries the old hash; the page was re-rendered with a new input.
        let t = task("daily/s/2026-05-23.md", "oldhash");
        assert_eq!(classify_task(dir.path(), &t).unwrap(), TaskStatus::Stale);
    }

    #[test]
    fn absent_page_is_missing_target() {
        let dir = TempDir::new().unwrap();
        let t = task("daily/s/2026-05-23.md", "abc123");
        assert_eq!(
            classify_task(dir.path(), &t).unwrap(),
            TaskStatus::MissingTarget
        );
    }

    #[test]
    fn page_without_llm_inputs_is_stale() {
        let dir = TempDir::new().unwrap();
        write_page(dir.path(), "daily/s/2026-05-23.md", "id: x\n");
        let t = task("daily/s/2026-05-23.md", "abc123");
        assert_eq!(classify_task(dir.path(), &t).unwrap(), TaskStatus::Stale);
    }

    fn write_queue_file(queue_dir: &Path, name: &str, tasks: &[QueueTask]) -> PathBuf {
        std::fs::create_dir_all(queue_dir).unwrap();
        let path = queue_dir.join(name);
        let lines: String = tasks
            .iter()
            .map(|t| format!("{}\n", serde_json::to_string(t).unwrap()))
            .collect();
        std::fs::write(&path, lines).unwrap();
        path
    }

    #[test]
    fn prune_rewrites_mixed_file_keeping_only_current() {
        let dir = TempDir::new().unwrap();
        let queue_dir = dir.path().join(".lorekeeper").join("queue");
        write_page(
            dir.path(),
            "daily/s/2026-05-23.md",
            "id: x\nllm_inputs:\n  summary: live\n",
        );
        let file = write_queue_file(
            &queue_dir,
            "run-1.jsonl",
            &[
                task("daily/s/2026-05-23.md", "live"),    // current
                task("daily/s/2026-05-23.md", "oldhash"), // stale
                task("daily/s/2026-05-24.md", "live"),    // missing-target
            ],
        );

        let summary = prune_queue(dir.path(), &queue_dir, false).unwrap();
        assert_eq!(
            summary,
            PruneSummary {
                pruned_stale: 1,
                pruned_missing_target: 1,
                kept_current: 1,
                files_rewritten: 1,
                files_deleted: 0,
            }
        );
        let content = std::fs::read_to_string(&file).unwrap();
        assert_eq!(content.lines().count(), 1);
        let survivor: QueueTask = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(survivor.cache_hash, "live");
        assert_eq!(survivor.target.vault_path, "daily/s/2026-05-23.md");
    }

    #[test]
    fn prune_deletes_file_with_no_current_tasks() {
        let dir = TempDir::new().unwrap();
        let queue_dir = dir.path().join(".lorekeeper").join("queue");
        let file = write_queue_file(
            &queue_dir,
            "run-1.jsonl",
            &[task("daily/s/2026-05-23.md", "x")], // missing-target
        );

        let summary = prune_queue(dir.path(), &queue_dir, false).unwrap();
        assert_eq!(summary.pruned_missing_target, 1);
        assert_eq!(summary.files_deleted, 1);
        assert!(!file.exists(), "an all-dead file must be deleted");
    }

    #[test]
    fn prune_leaves_all_current_file_byte_identical() {
        let dir = TempDir::new().unwrap();
        let queue_dir = dir.path().join(".lorekeeper").join("queue");
        write_page(
            dir.path(),
            "daily/s/2026-05-23.md",
            "id: x\nllm_inputs:\n  summary: live\n",
        );
        let file = write_queue_file(
            &queue_dir,
            "run-1.jsonl",
            &[task("daily/s/2026-05-23.md", "live")],
        );
        let before = std::fs::read_to_string(&file).unwrap();

        let summary = prune_queue(dir.path(), &queue_dir, false).unwrap();
        assert_eq!(summary.kept_current, 1);
        assert_eq!(summary.files_rewritten, 0);
        assert_eq!(summary.files_deleted, 0);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
    }

    #[test]
    fn prune_dry_run_reports_but_changes_nothing() {
        let dir = TempDir::new().unwrap();
        let queue_dir = dir.path().join(".lorekeeper").join("queue");
        let file = write_queue_file(
            &queue_dir,
            "run-1.jsonl",
            &[task("daily/s/2026-05-23.md", "x")], // missing-target
        );
        let before = std::fs::read_to_string(&file).unwrap();

        let summary = prune_queue(dir.path(), &queue_dir, true).unwrap();
        assert_eq!(summary.pruned_missing_target, 1);
        assert_eq!(
            summary.files_deleted, 1,
            "dry-run still reports the outcome"
        );
        assert!(file.exists(), "dry-run must not delete");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
    }

    #[test]
    fn prune_without_queue_dir_is_a_clean_noop() {
        let dir = TempDir::new().unwrap();
        let queue_dir = dir.path().join(".lorekeeper").join("queue");
        let summary = prune_queue(dir.path(), &queue_dir, false).unwrap();
        assert_eq!(summary, PruneSummary::default());
    }
}
