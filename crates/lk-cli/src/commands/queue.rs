use std::path::{Path, PathBuf};

use lk_queue::QueueTask;

use super::{find_config, load_config};

#[derive(clap::Subcommand)]
pub enum QueueCommand {
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
    /// Materialize the concept extractions a drain produced.
    ///
    /// The drain's job ends at judgement: which concepts a page names. Where each one lands
    /// — created or merged, with its synthesis, aliases, category and citation count intact,
    /// and the origin page's related-concepts section rebuilt from the same link builder the
    /// synchronous path uses — is decided here, by the code that already owns those rules.
    /// Results whose target page moved on since the drain read it are dropped, on the same
    /// hash the queue uses everywhere else.
    Apply {
        /// Vault root override (default: vault.root from config)
        #[arg(long)]
        root: Option<PathBuf>,
        /// Report what would be written, and write nothing
        #[arg(long)]
        dry_run: bool,
    },
    /// Print the number of `current` tasks — the count a drain would actually process —
    /// as a bare integer on stdout, and nothing else.
    ///
    /// `status` is written for a human and `--json` needs a JSON parser; a scheduled script
    /// deciding whether to spend an LLM session on the queue should not have to grep prose
    /// or take a dependency on `jq` to learn one number.
    Count {
        /// Vault root override (default: vault.root from config)
        #[arg(long)]
        root: Option<PathBuf>,
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

pub async fn run(opts: &super::GlobalOptions, cmd: QueueCommand) -> miette::Result<()> {
    match cmd {
        QueueCommand::Status { root, json } => status(opts, root, json).await,
        QueueCommand::Count { root } => count(opts, root).await,
        QueueCommand::Apply { root, dry_run } => apply(opts, root, dry_run).await,
        QueueCommand::Prune {
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

async fn apply(
    opts: &super::GlobalOptions,
    root: Option<PathBuf>,
    dry_run: bool,
) -> miette::Result<()> {
    let config = load_config(&find_config(opts)?)?;
    let vault_root = root.unwrap_or_else(|| config.vault.root_path());
    let queue_dir = vault_root.join(".lorekeeper").join("queue");

    let batch = lk_queue::read_results(&queue_dir)
        .map_err(|e| miette::miette!("read queue results: {e}"))?;
    let results = batch.ready;

    // A file that will never parse is moved out of the apply path, once and loudly. Leaving
    // it would fail this command — and so the scheduled pipeline — on every run forever,
    // while the concepts it held are re-derived anyway: its page carries no completion
    // marker, so the next ingest that re-renders that page re-enqueues the task.
    // Quarantining is housekeeping, so its own failures must not do what it exists to
    // prevent: a `?` here would strand every ready result behind an unwritable directory,
    // which is the same "one bad file blocks the batch" shape, reached through I/O instead
    // of through parsing. A file that cannot be moved simply stays put and is retried.
    let unreadable = batch.unreadable.len();
    let mut moved = 0usize;
    if unreadable > 0 {
        let corrupt_dir = queue_dir
            .join(lk_queue::RESULTS_SUBDIR)
            .join(lk_queue::CORRUPT_SUBDIR);
        moved = quarantine(&corrupt_dir, &batch.unreadable, dry_run);
    }

    if results.is_empty() {
        eprintln!("queue apply: no results pending");
        return finish_apply(0, unreadable, moved, dry_run);
    }

    let ctx = std::sync::Arc::new(
        lk_pipeline::PipelineContext::build(
            opts.template_dir.as_deref(),
            std::sync::Arc::new(lk_queue::NoopLlmClient),
            &config,
        )
        .map_err(|e| miette::miette!("{e}"))?,
    );
    let mut pipeline = lk_pipeline::Pipeline::new(&vault_root, ctx);
    let writer = lk_vault::VaultWriter::new(&vault_root);

    let (mut applied, mut dropped, mut failed) = (0usize, 0usize, 0usize);
    let mut origin_pages: Vec<(PathBuf, String)> = Vec::new();
    let mut consumed: Vec<PathBuf> = Vec::new();

    // Everything a result can get wrong is that result's problem, not the batch's: aborting
    // would strand every other valid extraction in the run. A RETRYABLE failure leaves the
    // file in place so a fixed page picks it up next time, and the run exits non-zero.
    for (path, result) in &results {
        let fail = |reason: String| {
            eprintln!(
                "  ✗ {} ({}): {reason}",
                result.task_id, result.target.vault_path
            );
        };
        let drop_dead = |reason: &str| {
            eprintln!(
                "  dropped {} ({}): {reason}",
                result.task_id, result.target.vault_path
            );
        };

        // An address outside the vault is not a page that might be fixed — it is a result
        // that can never be applied. Keeping it would fail `queue apply`, and so the whole
        // scheduled pipeline, on every run forever: `queue prune` classifies TASKS, so no
        // janitor would ever clear it. It is dead in the same sense a stale result is, and
        // is consumed on the same terms.
        let Some(rel_path) = resolve_target_path(&result.target.vault_path) else {
            drop_dead("target path escapes the vault root");
            dropped += 1;
            consumed.push(path.clone());
            continue;
        };
        match classify_against_page(
            &vault_root,
            &rel_path,
            result.target.kind,
            &result.cache_hash,
        ) {
            Ok(TaskStatus::Current) => {}
            Ok(status) => {
                drop_dead(status.as_str());
                dropped += 1;
                consumed.push(path.clone());
                continue;
            }
            Err(e) => {
                fail(e.to_string());
                failed += 1;
                continue;
            }
        }
        let content = match std::fs::read_to_string(vault_root.join(&rel_path)) {
            Ok(c) => c,
            Err(e) => {
                fail(format!("read {}: {e}", rel_path.display()));
                failed += 1;
                continue;
            }
        };
        match pipeline.apply_concept_result(result, &content).await {
            Ok(rewritten) => {
                origin_pages.push((rel_path, rewritten));
                consumed.push(path.clone());
                applied += 1;
            }
            Err(e) => {
                fail(e.to_string());
                failed += 1;
            }
        }
    }

    let concept_pages = pipeline
        .render_concept_pages()
        .await
        .map_err(|e| miette::miette!("{e}"))?;

    if dry_run {
        eprintln!(
            "[dry-run] queue apply: {applied} applied, {dropped} dropped, {failed} failed, \
             {} concept page(s)",
            concept_pages.len()
        );
        return finish_apply(failed, unreadable, moved, dry_run);
    }

    // Concept pages before origin pages. A crash between the two then leaves concept pages
    // nothing yet cites — harmless, and the retained result files replay the citation — where
    // the reverse order would leave citations pointing at pages that do not exist.
    for page in &concept_pages {
        writer
            .write_page(page.path.as_ref(), &page.content)
            .await
            .map_err(|e| miette::miette!("write {}: {e}", page.path))?;
    }
    for (rel_path, content) in &origin_pages {
        writer
            .write_page(rel_path, content)
            .await
            .map_err(|e| miette::miette!("write {}: {e}", rel_path.display()))?;
    }
    for path in &consumed {
        std::fs::remove_file(path)
            .map_err(|e| miette::miette!("remove {}: {e}", path.display()))?;
    }

    eprintln!(
        "queue apply: {applied} applied, {dropped} dropped, {failed} failed, \
         {} concept page(s)",
        concept_pages.len()
    );
    finish_apply(failed, unreadable, moved, dry_run)
}

/// Exit non-zero when anything needs a human: a retryable failure, or a file that could not
/// be parsed. `unreadable` counts every such file whether or not it was successfully moved —
/// one left in place still needs attention — while the per-file lines above say what actually
/// happened to each, which under `--dry-run` is nothing.
fn finish_apply(
    failed: usize,
    unreadable: usize,
    moved: usize,
    dry_run: bool,
) -> miette::Result<()> {
    // The last line of the run is what a pipeline log ends on, so it states what actually
    // happened to the files rather than what was attempted: under `--dry-run` nothing moved,
    // and a move can also fail, in which case the file is still sitting in `results/`.
    let fate = match (dry_run, moved, unreadable) {
        (true, ..) => format!(
            "none moved yet; would go to results/{}/",
            lk_queue::CORRUPT_SUBDIR
        ),
        (false, m, u) if m == u => format!("moved to results/{}/", lk_queue::CORRUPT_SUBDIR),
        (false, 0, _) => "none could be moved; they stay in results/".to_string(),
        (false, m, u) => format!(
            "{m} of {u} moved to results/{}/, the rest stay in results/",
            lk_queue::CORRUPT_SUBDIR
        ),
    };
    match (failed, unreadable) {
        (0, 0) => Ok(()),
        (0, u) => Err(miette::miette!(
            "{u} unreadable result file(s) ({fate}); the concepts they held re-enqueue on the \
             next ingest that re-renders their page"
        )),
        (f, 0) => Err(miette::miette!(
            "{f} result(s) could not be applied; their files were kept for retry"
        )),
        (f, u) => Err(miette::miette!(
            "{f} result(s) could not be applied (kept for retry); {u} unreadable file(s) ({fate})"
        )),
    }
}

/// Move each unreadable result into `corrupt_dir`, returning how many actually moved.
///
/// Every failure is reported with the OS error that caused it — `EACCES` and `EXDEV` need
/// different fixes — and none of them is fatal: a `?` here would strand every ready result
/// behind an unwritable directory, which is the same "one bad file blocks the batch" shape
/// this quarantine exists to remove, reached through I/O instead of through parsing. A file
/// that cannot be moved stays put and is retried.
fn quarantine(corrupt_dir: &Path, unreadable: &[(PathBuf, String)], dry_run: bool) -> usize {
    if dry_run {
        for (path, reason) in unreadable {
            eprintln!("  [dry-run] would quarantine {}: {reason}", path.display());
        }
        return 0;
    }
    if let Err(e) = std::fs::create_dir_all(corrupt_dir) {
        eprintln!(
            "  ✗ cannot create {}: {e}; {} unreadable file(s) stay put and will be retried",
            corrupt_dir.display(),
            unreadable.len()
        );
        return 0;
    }
    let mut moved = 0;
    for (path, reason) in unreadable {
        let dest = free_path(corrupt_dir, path);
        match std::fs::rename(path, &dest) {
            Ok(()) => {
                eprintln!(
                    "  quarantined {} → {}: {reason}",
                    path.display(),
                    dest.display()
                );
                moved += 1;
            }
            Err(e) => eprintln!(
                "  ✗ {} is unreadable ({reason}) and could not be moved to {}: {e}; it stays \
                 put and will be retried",
                path.display(),
                dest.display()
            ),
        }
    }
    moved
}

/// A path in `dir` for `source`'s file name that is not already taken. Quarantine preserves
/// a file for inspection, so it must not overwrite an earlier one that happens to share a
/// name — that would destroy the very evidence it exists to keep.
fn free_path(dir: &Path, source: &Path) -> PathBuf {
    let name = source.file_name().unwrap_or(source.as_os_str());
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let stem = source.file_stem().unwrap_or(name).to_string_lossy();
    (1u32..)
        .map(|n| dir.join(format!("{stem}.{n}.json")))
        .find(|p| !p.exists())
        .expect("an unused suffix always exists")
}

async fn count(opts: &super::GlobalOptions, root: Option<PathBuf>) -> miette::Result<()> {
    let vault_root = resolve_vault_root(opts, root)?;
    let queue_dir = vault_root.join(".lorekeeper").join("queue");
    let mut current = 0usize;
    for file in pending_queue_files(&queue_dir)? {
        for task in read_tasks(&file)? {
            if classify_task(&vault_root, &task)? == TaskStatus::Current {
                current += 1;
            }
        }
    }
    println!("{current}");
    Ok(())
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
    let rel_path = resolve_target_path(&task.target.vault_path).ok_or_else(|| {
        miette::miette!(
            "task {}: target `{}` escapes the vault root",
            task.task_id,
            task.target.vault_path
        )
    })?;
    classify_against_page(vault_root, &rel_path, task.target.kind, &task.cache_hash)
}

/// Resolve a task or result `target.vault_path` to the vault-relative path it addresses,
/// or `None` when it escapes the root.
///
/// A result file is written by the drain session, so this is the boundary where untrusted
/// text becomes a filesystem path that is read and then WRITTEN. It goes through the same
/// lexical rule as every other vault address (`.`/`..` folded, absolute and escaping forms
/// refused) rather than being joined onto the root as-is.
fn resolve_target_path(vault_path: &str) -> Option<PathBuf> {
    lk_core::link::resolve_dest(Path::new(""), vault_path)
}

/// The queue's one staleness rule: a task or result is current exactly when the page still
/// carries the `llm_inputs` hash it was created against. Pending tasks and drained results
/// are classified by the same code so they can never disagree about what "stale" means.
fn classify_against_page(
    vault_root: &Path,
    rel_path: &Path,
    kind: lk_queue::TargetKind,
    cache_hash: &str,
) -> miette::Result<TaskStatus> {
    let page_path = vault_root.join(rel_path);
    let content = match std::fs::read_to_string(&page_path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(TaskStatus::MissingTarget);
        }
        Err(e) => return Err(miette::miette!("read {}: {e}", page_path.display())),
    };
    let page = lk_core::frontmatter::parse_page(&content)
        .map_err(|e| miette::miette!("parse {}: {e}", page_path.display()))?;
    let stored = page
        .frontmatter
        .get(lk_core::frontmatter::field::LLM_INPUTS)
        .and_then(|v| v.get(kind.llm_inputs_key()))
        .and_then(|v| v.as_str());
    Ok(if stored == Some(cache_hash) {
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

    #[test]
    fn a_quarantine_that_cannot_run_leaves_the_files_and_reports_none_moved() {
        // Housekeeping failing must not do what the housekeeping exists to prevent. A
        // `corrupt` path that is a FILE makes `create_dir_all` fail, and the caller has to
        // learn that nothing moved so the summary does not claim otherwise.
        let dir = TempDir::new().unwrap();
        let corrupt = dir.path().join("corrupt");
        std::fs::write(&corrupt, "not a directory").unwrap();
        let stuck = dir.path().join("ext-1.json");
        std::fs::write(&stuck, "{trunc").unwrap();

        let unreadable = vec![(stuck.clone(), "parse error".to_string())];
        assert_eq!(quarantine(&corrupt, &unreadable, false), 0);
        assert!(stuck.exists(), "the file must stay put for the next run");

        // The summary must not say they moved.
        let err = finish_apply(0, 1, 0, false).unwrap_err().to_string();
        assert!(err.contains("none could be moved"), "{err}");
        assert!(!err.contains("moved to results/corrupt/"), "{err}");
    }

    #[test]
    fn a_partly_successful_quarantine_says_how_many_moved() {
        // A rename can fail for one file and not another; the summary must not round that
        // to all-or-nothing in either direction.
        let dir = TempDir::new().unwrap();
        let corrupt = dir.path().join("corrupt");
        let real = dir.path().join("ext-1.json");
        std::fs::write(&real, "{trunc").unwrap();
        let vanished = dir.path().join("ext-2.json"); // never created — its rename fails

        let moved = quarantine(
            &corrupt,
            &[
                (real.clone(), "parse error".into()),
                (vanished, "parse error".into()),
            ],
            false,
        );
        assert_eq!(moved, 1);
        assert!(!real.exists(), "the one that could move did");

        let err = finish_apply(0, 2, moved, false).unwrap_err().to_string();
        assert!(err.contains("1 of 2 moved"), "{err}");
    }

    #[test]
    fn a_dry_run_quarantine_moves_nothing_and_says_so() {
        let dir = TempDir::new().unwrap();
        let corrupt = dir.path().join("corrupt");
        let file = dir.path().join("ext-1.json");
        std::fs::write(&file, "{trunc").unwrap();

        assert_eq!(
            quarantine(&corrupt, &[(file.clone(), "parse error".into())], true),
            0
        );
        assert!(file.exists());
        assert!(!corrupt.exists(), "dry-run must not create the directory");

        let err = finish_apply(0, 1, 0, true).unwrap_err().to_string();
        assert!(err.contains("none moved yet"), "{err}");
    }

    #[test]
    fn quarantine_never_overwrites_an_earlier_file() {
        // Moving rather than deleting exists to keep the bytes for a human; an overwrite
        // would destroy exactly the evidence being preserved.
        let dir = TempDir::new().unwrap();
        let corrupt = dir.path().join("corrupt");
        std::fs::create_dir_all(&corrupt).unwrap();
        let source = dir.path().join("ext-1.json");

        let first = free_path(&corrupt, &source);
        assert_eq!(first, corrupt.join("ext-1.json"));
        std::fs::write(&first, "one").unwrap();

        let second = free_path(&corrupt, &source);
        assert_ne!(second, first, "a taken name must not be reused");
        std::fs::write(&second, "two").unwrap();

        let third = free_path(&corrupt, &source);
        assert!(![first.clone(), second.clone()].contains(&third));
        // The earlier files are still there with their own contents.
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "one");
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "two");
    }

    #[test]
    fn a_target_path_never_addresses_anything_outside_the_vault() {
        // Result files are written by the drain session; this is the boundary where that
        // text becomes a path the command reads and then WRITES. The property is
        // containment: whatever a result asks for, the address either lands inside the
        // vault or is refused outright.
        for escaping in ["../outside.md", "daily/../../outside.md", "wiki/../../x.md"] {
            assert_eq!(resolve_target_path(escaping), None, "{escaping}");
        }
        // A leading `/` is the OKF vault-root-relative form, not a filesystem absolute —
        // it addresses a page inside the vault, so it resolves rather than escaping.
        assert_eq!(
            resolve_target_path("/etc/passwd"),
            Some(PathBuf::from("etc/passwd"))
        );
        // Ordinary addresses resolve, and `.`/`..` inside the vault fold normally.
        assert_eq!(
            resolve_target_path("daily/src/2026-05-23.md"),
            Some(PathBuf::from("daily/src/2026-05-23.md"))
        );
        assert_eq!(
            resolve_target_path("daily/src/../other/p.md"),
            Some(PathBuf::from("daily/other/p.md"))
        );
    }

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
                concepts_dir: "../../wiki/concepts".into(),
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
