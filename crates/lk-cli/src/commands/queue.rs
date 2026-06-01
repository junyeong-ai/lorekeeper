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
    }
}

async fn status(
    opts: &super::GlobalOptions,
    root: Option<PathBuf>,
    json: bool,
) -> miette::Result<()> {
    let vault_root = match root {
        Some(r) => r,
        None => load_config(&find_config(opts)?)?.vault.root_path(),
    };
    let queue_dir = vault_root.join(".lorekeeper").join("queue");

    let mut reports = Vec::new();
    if queue_dir.is_dir() {
        let mut files: Vec<PathBuf> = std::fs::read_dir(&queue_dir)
            .map_err(|e| miette::miette!("read {}: {e}", queue_dir.display()))?
            .filter_map(Result::ok)
            .map(|e| e.path())
            // `.jsonl` only — skips the `processed/` subdir and any `.jsonl.tmp`.
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl"))
            .collect();
        files.sort();

        for file in files {
            let content = std::fs::read_to_string(&file)
                .map_err(|e| miette::miette!("read {}: {e}", file.display()))?;
            for line in content.lines().filter(|l| !l.trim().is_empty()) {
                let task: QueueTask = serde_json::from_str(line)
                    .map_err(|e| miette::miette!("parse task in {}: {e}", file.display()))?;
                let status = classify_task(&vault_root, &task)?;
                reports.push(TaskReport {
                    task_id: task.task_id,
                    kind: task.kind.as_str().to_string(),
                    vault_path: task.target.vault_path,
                    status,
                });
            }
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
}
