use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use lk_queue::QueueTask;

use super::{find_config, load_config};

#[derive(clap::Subcommand)]
pub enum QueueCommand {
    /// Classify every pending queue task against its target page. `/lore-process`
    /// consults this instead of comparing hashes itself: it processes `current` tasks,
    /// skips `done` ones (this exact input is already answered on the page), drops
    /// `stale` ones (the page was re-rendered by a newer ingest), and reports
    /// `missing-target`. The guard is computed here in tested Rust, not in prose.
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
    /// Print the number of tasks that still need an LLM session — the count a drain would
    /// actually do work for — as a bare integer on stdout, and nothing else.
    ///
    /// `status` is written for a human and `--json` needs a JSON parser; a scheduled script
    /// deciding whether to spend an LLM session on the queue should not have to grep prose
    /// or take a dependency on `jq` to learn one number. A task already answered on its page
    /// is not work, so it is not counted — otherwise the script spends a session on a queue
    /// with nothing to do.
    Count {
        /// Vault root override (default: vault.root from config)
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Leave the pending queue holding only work that still needs an LLM session, using the
    /// same classification as `status`.
    ///
    /// `stale` and `missing-target` tasks are exactly what `/lore-process` would drop
    /// without editing anything, so removing them is that drop performed deterministically,
    /// without spending a session. A `done` task is kept — its answer is on the page and the
    /// run's record stays whole — but a run whose every remaining task is `done` needs no
    /// session, so nothing would ever archive it; it is retired to `processed/` exactly as a
    /// drain retires a finished run. A file left holding nothing never edited a page, so it
    /// is deleted rather than archived. Rewrites are atomic; a file needing no change is
    /// left byte-identical.
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

/// Where a queued task stands against its target page. Two independent questions decide
/// it: is the page still the one this task was made for (input hash, existence), and has
/// the work already been done (completion marker). Only a task that passes the first and
/// fails the second is work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskStatus {
    /// `llm_inputs.<key>` on the page equals the task's `cache_hash` and no completion
    /// marker matches it — process it.
    Current,
    /// `llm_inputs.<key>_done` already equals the task's `cache_hash`: this exact input
    /// has been answered. Nothing is left to do, and nothing was lost.
    Done,
    /// The page was re-rendered with a different input after this task was queued —
    /// drop it; a newer task carries the current hash.
    Stale,
    /// There is nowhere to land: the target page is gone, OR it no longer carries the
    /// section this task names.
    ///
    /// A page's headings come from the render, and the anchor was recorded by the render
    /// that created the task — so an anchor the page does not carry means the heading
    /// vocabulary changed since (a `vault.locale` switch, a custom `--template-dir`), and
    /// every later render uses the new one. Waiting cannot fix it. Nothing is lost by
    /// dropping: the page carries no completion marker for this input, so the next ingest
    /// that re-renders it re-enqueues the work under the heading the page now has — the
    /// same self-healing an unparseable result file relies on.
    MissingTarget,
}

/// Which queued artifact is being classified. They share every fact about the target page
/// and differ on one question.
///
/// A TASK is a REQUEST for work, so a section already answered for its input makes it
/// redundant. A RESULT is the ANSWER, in flight: it exists and has not been written to the
/// page yet. For concepts the completion marker is stamped by the very edit that writes
/// them, so asking a result whether the page is already answered would discard the value
/// that is about to answer it — permanently and silently, since the marker then keeps the
/// empty section looking cached and nothing re-enqueues.
///
/// Ignoring the marker is safe rather than merely necessary: re-applying a result
/// reproduces the page, because `accumulate_concepts` dedups citations by id and the
/// concept merge preserves everything an earlier application wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Artifact {
    Task,
    Result,
}

impl TaskStatus {
    fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Current => "current",
            TaskStatus::Done => "done",
            TaskStatus::Stale => "stale",
            TaskStatus::MissingTarget => "missing-target",
        }
    }

    /// Whether this task still needs an LLM session. The one question `lore queue count`
    /// answers for a scheduled script deciding whether to spend one.
    fn is_work(self) -> bool {
        matches!(self, TaskStatus::Current)
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
    // Keyed by page, because ONE page can receive several results in a batch: two ingests
    // before a single drain enqueue the same target twice under the same input hash, and
    // both classify current. Every write lands after this loop, so a second fold that read
    // the page from disk would see a version predating the first — and its write would then
    // drop the citations the first added, the exact loss the accumulating render exists to
    // prevent. Within a batch, the pending version IS the page.
    let mut origin_pages: BTreeMap<PathBuf, String> = BTreeMap::new();
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
            &result.target.anchor,
            Artifact::Result,
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
        let content = match origin_pages.get(&rel_path) {
            Some(pending) => pending.clone(),
            None => match std::fs::read_to_string(vault_root.join(&rel_path)) {
                Ok(c) => c,
                Err(e) => {
                    fail(format!("read {}: {e}", rel_path.display()));
                    failed += 1;
                    continue;
                }
            },
        };
        match pipeline.apply_concept_result(result, &content).await {
            Ok(rewritten) => {
                origin_pages.insert(rel_path, rewritten);
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

/// A path in `dir` for `source`'s file name that is not already taken, keeping its
/// extension. Every mover in this file preserves a file rather than replacing one — a
/// quarantined result is evidence, an archived run is a record — so neither may overwrite
/// an earlier file that happens to share a name.
fn free_path(dir: &Path, source: &Path) -> PathBuf {
    let name = source.file_name().unwrap_or(source.as_os_str());
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let stem = source.file_stem().unwrap_or(name).to_string_lossy();
    let ext = source
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    (1u32..)
        .map(|n| dir.join(format!("{stem}.{n}{ext}")))
        .find(|p| !p.exists())
        .expect("an unused suffix always exists")
}

async fn count(opts: &super::GlobalOptions, root: Option<PathBuf>) -> miette::Result<()> {
    let vault_root = resolve_vault_root(opts, root)?;
    let queue_dir = vault_root.join(".lorekeeper").join("queue");
    let mut current = 0usize;
    for file in pending_queue_files(&queue_dir)? {
        for task in read_tasks(&file)? {
            if classify_task(&vault_root, &task)?.is_work() {
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

    let tally = |want: TaskStatus| reports.iter().filter(|r| r.status == want).count();
    let current = tally(TaskStatus::Current);
    let done = tally(TaskStatus::Done);
    let stale = tally(TaskStatus::Stale);
    let missing = tally(TaskStatus::MissingTarget);

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
                "done": done,
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
            "queue: {current} current, {done} done, {stale} stale, {missing} missing-target \
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
    retired_done: usize,
    files_rewritten: usize,
    files_deleted: usize,
    files_archived: usize,
}

/// Leave the pending queue holding only work that still needs an LLM session.
///
/// A dead task — `stale` or `missing-target` — is dropped: `/lore-process` would discard
/// it on contact without editing anything, so removing it here is that discard performed
/// without spending a session. Safe by construction, since a pending file's tasks all
/// targeted pages that existed when it was renamed into place (the flush invariant), so
/// `missing-target` means the page was deleted afterwards.
///
/// A `done` task is not dead — its answer is already on the page — so it is retained,
/// keeping the run's record whole for whoever archives it. What it must not do is keep the
/// run alive: a file whose every remaining task is `done` needs no session, so nothing
/// would ever archive it, and it would sit in the queue making `lore ingest` warn about
/// pending work forever. This retires it exactly as a drain does — the file moved to
/// `processed/` as it stands — so the two agree on what a finished run looks like.
///
/// A file with nothing retained held only dead tasks, so it never edited a page and there
/// is nothing to archive; it is deleted. Rewrites go through `lk_queue::write_tasks_atomic`
/// (temp + fsync + rename), and a file needing no change stays byte-identical.
fn prune_queue(vault_root: &Path, queue_dir: &Path, dry_run: bool) -> miette::Result<PruneSummary> {
    let mut summary = PruneSummary::default();
    for file in pending_queue_files(queue_dir)? {
        let tasks = read_tasks(&file)?;
        let mut retained = Vec::with_capacity(tasks.len());
        let (mut dropped, mut work_left) = (0usize, 0usize);
        for task in tasks {
            match classify_task(vault_root, &task)? {
                TaskStatus::Current => {
                    work_left += 1;
                    summary.kept_current += 1;
                    retained.push(task);
                }
                TaskStatus::Done => {
                    summary.retired_done += 1;
                    retained.push(task);
                }
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

        if retained.is_empty() {
            // Only dead tasks, so the run never edited a page: nothing to archive.
            if dropped > 0 {
                summary.files_deleted += 1;
                if !dry_run {
                    std::fs::remove_file(&file)
                        .map_err(|e| miette::miette!("remove {}: {e}", file.display()))?;
                }
            }
        } else if work_left == 0 {
            // Retained but no work left, so every retained task is `done`: the run is
            // finished and nothing else will ever retire it. Drop its dead tasks BEFORE
            // archiving — the summary counts them as removed, and an archive still holding
            // them would make that count a lie.
            summary.files_archived += 1;
            if !dry_run {
                if dropped > 0 {
                    lk_queue::write_tasks_atomic(&file, &retained)
                        .map_err(|e| miette::miette!("rewrite {}: {e}", file.display()))?;
                }
                archive_queue_file(queue_dir, &file)?;
            }
        } else if dropped > 0 {
            summary.files_rewritten += 1;
            if !dry_run {
                lk_queue::write_tasks_atomic(&file, &retained)
                    .map_err(|e| miette::miette!("rewrite {}: {e}", file.display()))?;
            }
        }
    }
    Ok(summary)
}

/// Move a settled run into `processed/`, the same retirement `/lore-process` performs when
/// every task in a file has succeeded.
///
/// A drain runs on its own schedule and may have retired this very file between the scan
/// and here. That is the outcome this wanted, so a vanished source is success — failing
/// would take down the janitor over a race whose result is already correct. The archive
/// is a record, so it never overwrites an earlier file of the same name.
fn archive_queue_file(queue_dir: &Path, file: &Path) -> miette::Result<()> {
    let dir = queue_dir.join(lk_queue::PROCESSED_SUBDIR);
    std::fs::create_dir_all(&dir).map_err(|e| miette::miette!("create {}: {e}", dir.display()))?;
    match std::fs::rename(file, free_path(&dir, file)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(miette::miette!("archive {}: {e}", file.display())),
    }
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
                "retired_done": summary.retired_done,
                "files_rewritten": summary.files_rewritten,
                "files_deleted": summary.files_deleted,
                "files_archived": summary.files_archived,
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
            "{label}: dropped {} stale + {} missing-target, kept {} current, \
             {} already done; {} file(s) rewritten, {} archived, {} deleted",
            summary.pruned_stale,
            summary.pruned_missing_target,
            summary.kept_current,
            summary.retired_done,
            summary.files_rewritten,
            summary.files_archived,
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
    classify_against_page(
        vault_root,
        &rel_path,
        task.target.kind,
        &task.cache_hash,
        &task.target.anchor,
        Artifact::Task,
    )
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

/// The queue's one classification rule. Pending tasks and drained results go through it
/// together, so they can never disagree about what any status means.
fn classify_against_page(
    vault_root: &Path,
    rel_path: &Path,
    kind: lk_queue::TargetKind,
    cache_hash: &str,
    anchor: &str,
    artifact: Artifact,
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
    let llm_inputs = page
        .frontmatter
        .get(lk_core::frontmatter::field::LLM_INPUTS);
    let field = |key: &str| {
        llm_inputs
            .and_then(|v| v.get(key))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    if field(kind.llm_inputs_key()).as_deref() != Some(cache_hash) {
        return Ok(TaskStatus::Stale);
    }
    if artifact == Artifact::Task && field(&kind.completion_key()).as_deref() == Some(cache_hash) {
        return Ok(TaskStatus::Done);
    }
    // Asked last, because it only decides the fate of work still to be written: the
    // section named here must exist on the page to receive it. An anchor the page does
    // not carry cannot come back — see `MissingTarget`.
    let Some(heading) = anchor.strip_prefix("## ") else {
        return Ok(TaskStatus::MissingTarget);
    };
    Ok(if lk_vault::section_body(&page.body, heading).is_some() {
        TaskStatus::Current
    } else {
        TaskStatus::MissingTarget
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
                retired_done: 0,
                files_rewritten: 1,
                files_deleted: 0,
                files_archived: 0,
            }
        );
        let content = std::fs::read_to_string(&file).unwrap();
        assert_eq!(content.lines().count(), 1);
        let survivor: QueueTask = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(survivor.cache_hash, "live");
        assert_eq!(survivor.target.vault_path, "daily/s/2026-05-23.md");
    }

    /// A task whose page already carries the completion marker for its exact input is
    /// answered, not pending. Counting it as work is what makes a scheduled pipeline spend
    /// an LLM session on a queue with nothing to do.
    /// A `vault.locale` switch re-renders every page's headings but leaves the concept
    /// input hash untouched (locale is not part of that cache identity), so a task queued
    /// before the switch stays hash-current while naming a section that no longer exists.
    /// Treating it as work makes `lore queue apply` — and the whole scheduled pipeline —
    /// fail on it every day, with no janitor able to clear it. Nothing is lost by dropping:
    /// the page carries no completion marker, so the next ingest re-enqueues the work under
    /// the heading the page now has.
    #[test]
    fn a_task_whose_section_no_longer_exists_has_nowhere_to_land() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("daily/s/2026-05-23.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Same input hash, headings re-rendered in another locale.
        std::fs::write(
            &path,
            "---\nid: x\nllm_inputs:\n  summary: abc123\n---\n\n## Summary\n\nbody\n",
        )
        .unwrap();
        assert_eq!(
            classify_task(dir.path(), &task("daily/s/2026-05-23.md", "abc123")).unwrap(),
            TaskStatus::MissingTarget,
            "the task names `## 요약`, which this page no longer has"
        );
    }

    /// The landing check must not fire before the ones that already decide the task's fate,
    /// or a stale task would be reported as un-landable and an answered one re-examined.
    #[test]
    fn a_stale_or_answered_task_keeps_its_status_whatever_the_headings_say() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("daily/s/2026-05-23.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "---\nid: x\nllm_inputs:\n  summary: newhash\n  summary_done: newhash\n---\n\n\
             ## Summary\n\nbody\n",
        )
        .unwrap();
        assert_eq!(
            classify_task(dir.path(), &task("daily/s/2026-05-23.md", "oldhash")).unwrap(),
            TaskStatus::Stale
        );
        assert_eq!(
            classify_task(dir.path(), &task("daily/s/2026-05-23.md", "newhash")).unwrap(),
            TaskStatus::Done
        );
    }

    #[test]
    fn a_task_already_answered_on_its_page_is_done_not_current() {
        let dir = TempDir::new().unwrap();
        write_page(
            dir.path(),
            "daily/s/2026-05-23.md",
            "id: x\nllm_inputs:\n  summary: abc123\n  summary_done: abc123\n",
        );
        assert_eq!(
            classify_task(dir.path(), &task("daily/s/2026-05-23.md", "abc123")).unwrap(),
            TaskStatus::Done
        );
        assert!(!TaskStatus::Done.is_work());
    }

    /// A completion marker left over from an EARLIER input does not answer this task —
    /// the input key decides staleness first, and it wins.
    #[test]
    fn a_marker_from_an_older_input_does_not_make_a_task_done() {
        let dir = TempDir::new().unwrap();
        write_page(
            dir.path(),
            "daily/s/2026-05-23.md",
            "id: x\nllm_inputs:\n  summary: newhash\n  summary_done: oldhash\n",
        );
        assert_eq!(
            classify_task(dir.path(), &task("daily/s/2026-05-23.md", "newhash")).unwrap(),
            TaskStatus::Current,
            "the page moved to a new input and has not been answered for it"
        );
        assert_eq!(
            classify_task(dir.path(), &task("daily/s/2026-05-23.md", "oldhash")).unwrap(),
            TaskStatus::Stale,
            "a task for the old input is stale regardless of the marker"
        );
    }

    /// A run with nothing left to do would otherwise sit in the queue forever: no session
    /// is spent on it, so nothing ever archives it, and `lore ingest` warns about pending
    /// work on every run. Prune retires it exactly as a drain retires a finished run.
    #[test]
    fn prune_archives_a_run_whose_every_task_is_answered() {
        let dir = TempDir::new().unwrap();
        let queue_dir = dir.path().join(".lorekeeper").join("queue");
        write_page(
            dir.path(),
            "daily/s/2026-05-23.md",
            "id: x\nllm_inputs:\n  summary: live\n  summary_done: live\n",
        );
        let file = write_queue_file(
            &queue_dir,
            "run-1.jsonl",
            &[task("daily/s/2026-05-23.md", "live")],
        );

        let dry = prune_queue(dir.path(), &queue_dir, true).unwrap();
        assert_eq!(dry.files_archived, 1, "dry-run reports the same retirement");
        assert!(file.exists(), "dry-run archives nothing");

        let summary = prune_queue(dir.path(), &queue_dir, false).unwrap();
        assert_eq!(summary.retired_done, 1);
        assert_eq!(summary.files_archived, 1);
        assert_eq!(summary.kept_current, 0);
        assert!(!file.exists(), "the settled run leaves the pending queue");
        assert!(
            queue_dir
                .join(lk_queue::PROCESSED_SUBDIR)
                .join("run-1.jsonl")
                .exists(),
            "and lands in processed/, where a drain would have put it"
        );
    }

    /// A run that still has work keeps its answered tasks, so the record a drain archives
    /// stays whole. Only dead tasks are removed.
    /// The summary counts a dead task as pruned; the archive must not still contain it, or
    /// the count asserts a removal that did not happen.
    #[test]
    fn an_archived_run_no_longer_holds_the_tasks_prune_reported_dropping() {
        let dir = TempDir::new().unwrap();
        let queue_dir = dir.path().join(".lorekeeper").join("queue");
        write_page(
            dir.path(),
            "daily/s/2026-05-23.md",
            "id: x\nllm_inputs:\n  summary: live\n  summary_done: live\n",
        );
        write_queue_file(
            &queue_dir,
            "run-1.jsonl",
            &[
                task("daily/s/2026-05-23.md", "live"),    // done
                task("daily/s/2026-05-23.md", "oldhash"), // stale
                task("daily/s/2026-05-99.md", "live"),    // missing-target
            ],
        );

        let summary = prune_queue(dir.path(), &queue_dir, false).unwrap();
        assert_eq!(summary.files_archived, 1);
        assert_eq!(summary.pruned_stale, 1);
        assert_eq!(summary.pruned_missing_target, 1);

        let archived = queue_dir
            .join(lk_queue::PROCESSED_SUBDIR)
            .join("run-1.jsonl");
        let content = std::fs::read_to_string(&archived).unwrap();
        assert_eq!(
            content.lines().filter(|l| !l.trim().is_empty()).count(),
            1,
            "only the answered task belongs in the archive:\n{content}"
        );
    }

    /// The archive is a record. A drain retiring a run of the same name must not have its
    /// file replaced, and a drain that already moved THIS file is the outcome prune wanted.
    #[test]
    fn archiving_never_overwrites_and_never_fails_on_an_already_retired_run() {
        let dir = TempDir::new().unwrap();
        let queue_dir = dir.path().join(".lorekeeper").join("queue");
        let processed = queue_dir.join(lk_queue::PROCESSED_SUBDIR);
        std::fs::create_dir_all(&processed).unwrap();
        std::fs::write(processed.join("run-1.jsonl"), "earlier run\n").unwrap();

        let file = queue_dir.join("run-1.jsonl");
        std::fs::write(&file, "later run\n").unwrap();
        archive_queue_file(&queue_dir, &file).unwrap();
        assert_eq!(
            std::fs::read_to_string(processed.join("run-1.jsonl")).unwrap(),
            "earlier run\n",
            "the earlier record must survive"
        );
        assert_eq!(
            std::fs::read_to_string(processed.join("run-1.1.jsonl")).unwrap(),
            "later run\n",
            "and the later one keeps its extension"
        );

        // Already gone: a drain retired it between the scan and here.
        archive_queue_file(&queue_dir, &queue_dir.join("never-existed.jsonl"))
            .expect("a vanished source is the outcome this wanted");
    }

    #[test]
    fn prune_keeps_answered_tasks_in_a_run_that_still_has_work() {
        let dir = TempDir::new().unwrap();
        let queue_dir = dir.path().join(".lorekeeper").join("queue");
        write_page(
            dir.path(),
            "daily/s/2026-05-23.md",
            "id: x\nllm_inputs:\n  summary: live\n  summary_done: live\n",
        );
        write_page(
            dir.path(),
            "daily/s/2026-05-24.md",
            "id: y\nllm_inputs:\n  summary: live\n",
        );
        let file = write_queue_file(
            &queue_dir,
            "run-1.jsonl",
            &[
                task("daily/s/2026-05-23.md", "live"), // done
                task("daily/s/2026-05-24.md", "live"), // current
                task("daily/s/2026-05-99.md", "live"), // missing-target
            ],
        );

        let summary = prune_queue(dir.path(), &queue_dir, false).unwrap();
        assert_eq!(summary.retired_done, 1);
        assert_eq!(summary.kept_current, 1);
        assert_eq!(summary.pruned_missing_target, 1);
        assert_eq!(summary.files_rewritten, 1);
        assert_eq!(summary.files_archived, 0);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap().lines().count(),
            2,
            "the answered task stays; only the dead one goes"
        );
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
