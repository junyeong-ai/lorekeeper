pub(super) mod check;
pub(super) mod readiness;
mod packet;
mod regenerate;
mod synthesis;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use lk_queue::{QueueFile, QueueTask};

use super::{find_config, load_config, load_config_with_revision};

#[derive(clap::Subcommand)]
pub enum QueueCommand {
    /// Verify no current tasks, blocked inputs, staged results or interrupted mutations remain.
    Check {
        /// Vault root override (default: vault.root from config)
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Classify every pending queue task against its target page. `/lore-process`
    /// consults this instead of comparing hashes itself: it processes `current` tasks,
    /// skips `done` ones (this exact input is already answered on the page), drops
    /// `stale` ones (the page was re-rendered by a newer ingest), and reports
    /// `missing-target`. The guard is computed here in tested Rust, not in prose.
    Status {
        /// Vault root override (default: vault.root from config)
        #[arg(long)]
        root: Option<PathBuf>,
        /// Output in JSON format. `ok` is always true: classifying a queue is not a verdict
        /// on it, and this exits 0 over any queue it can describe
        #[arg(long)]
        json: bool,
    },
    /// Apply staged semantic sections, concept extractions and syntheses.
    ///
    /// Validate pending task identity and the exact target revision before publishing
    /// sections and completion markers. Synthesis also validates cited source revisions.
    /// Superseded task inputs are dropped; changed target files require regenerated results.
    /// Publication requires the original retained task and captured read scope.
    Apply {
        /// Vault root override (default: vault.root from config)
        #[arg(long)]
        root: Option<PathBuf>,
        /// Report what would be written, and write nothing
        #[arg(long)]
        dry_run: bool,
    },
    /// Regenerate one retained task from its original input, without remote fetching.
    Regenerate {
        /// Exact task id retained once in the pending or processed queue
        #[arg(long)]
        task_id: String,
        /// Vault root override (default: vault.root from config)
        #[arg(long)]
        root: Option<PathBuf>,
        /// Validate retained inputs and report the request without writing or recovering
        #[arg(long)]
        dry_run: bool,
    },
    /// Export a read-only evidence packet for selected current synthesis tasks.
    Packet {
        /// Current synthesis task id to include; repeat to share a deduplicated source table
        #[arg(long = "task-id", required = true)]
        task_ids: Vec<String>,
        /// Vault root override (default: vault.root from config)
        #[arg(long)]
        root: Option<PathBuf>,
        /// Include exact source and target text in stdout (default: metadata only)
        #[arg(long)]
        include_content: bool,
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
        /// Output in JSON format. `ok` is the verdict the exit code carries: false when work
        /// is left that no drain can do
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
    /// The target page cannot be read as a page — unreadable bytes, or frontmatter that will
    /// not parse. Nothing can be decided about the task, so it is neither work nor dead.
    ///
    /// Reported and RETAINED. A repair of the page brings it back, which is what separates
    /// this from `MissingTarget`; dropping it would discard work a one-line fix restores.
    /// Per task rather than per run, because the likeliest author of unparseable frontmatter is
    /// the drain itself stamping a marker — and one mangled page must not make `count`,
    /// `status` and `prune` all fail, which leaves the scheduled drain skipping every night
    /// with no janitor able to clear it.
    Unreadable,
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
            TaskStatus::Unreadable => "unreadable",
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
    target_revision: Option<String>,
    generation_policy: Option<String>,
}

pub async fn run(opts: &super::GlobalOptions, cmd: QueueCommand) -> miette::Result<()> {
    match cmd {
        QueueCommand::Check { root } => check::run(opts, root),
        QueueCommand::Regenerate {
            task_id,
            root,
            dry_run,
        } => regenerate::run(opts, root, task_id, dry_run),
        QueueCommand::Packet {
            task_ids,
            root,
            include_content,
        } => packet::run(opts, root, task_ids, include_content),
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
        Some(r) => super::resolve_root_override(r),
        None => Ok(load_config(&find_config(opts)?)?.vault.root_path()),
    }
}

pub(super) fn current_task(root: &Path, task_id: &str) -> miette::Result<QueueTask> {
    select_task(root, task_id, TaskSelection::Current)
}

#[derive(Clone, Copy)]
pub(super) enum TaskSelection {
    Current,
    /// Publication keeps per-result isolation: matching typed work remains usable
    /// beside unrelated unparseable lines. Extraction results may also be reapplied
    /// after their completion marker was published.
    CurrentOrDoneExtraction,
}

pub(super) fn current_tasks(root: &Path) -> miette::Result<Vec<QueueTask>> {
    let pending = read_pending_tasks(&root.join(".lorekeeper/queue"))?;
    if pending.unparseable != 0 {
        return Err(miette::miette!(
            "unparseable pending tasks prevent selecting semantic work"
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut revisions = BTreeMap::new();
    let mut selected = Vec::new();
    for task in pending.tasks.into_values() {
        if !seen.insert(task.task_id.clone()) {
            return Err(miette::miette!(
                "ambiguous pending task id: {}",
                task.task_id
            ));
        }
        match classify_task(root, &task, &mut revisions)? {
            TaskStatus::Current => selected.push(task),
            TaskStatus::Unreadable => {
                return Err(miette::miette!(
                    "unreadable pending task prevents selecting semantic work: {}",
                    task.task_id
                ));
            }
            TaskStatus::Done | TaskStatus::Stale | TaskStatus::MissingTarget => {}
        }
    }
    Ok(selected)
}

pub(super) fn tasks_completed(root: &Path, tasks: &[QueueTask]) -> miette::Result<bool> {
    let mut revisions = BTreeMap::new();
    for task in tasks {
        if classify_task(root, task, &mut revisions)? != TaskStatus::Done {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(super) fn select_task(
    root: &Path,
    task_id: &str,
    selection: TaskSelection,
) -> miette::Result<QueueTask> {
    let pending = read_pending_tasks(&root.join(".lorekeeper/queue"))?;
    if pending.unparseable != 0 && matches!(selection, TaskSelection::Current) {
        return Err(miette::miette!(
            "unparseable pending tasks prevent selecting semantic work"
        ));
    }
    let mut selected = pending
        .tasks
        .into_values()
        .filter(|task| task.task_id == task_id);
    let task = selected
        .next()
        .ok_or_else(|| miette::miette!("current task not found: {task_id}"))?;
    if selected.next().is_some() {
        return Err(miette::miette!("ambiguous pending task id: {task_id}"));
    }
    let status = classify_task(root, &task, &mut BTreeMap::new())?;
    let mut allowed = status == TaskStatus::Current
        || (matches!(selection, TaskSelection::CurrentOrDoneExtraction)
            && status == TaskStatus::Done
            && task.kind == lk_queue::TaskKind::ExtractConcepts);
    if allowed && status == TaskStatus::Done {
        let target = resolve_target_path(&task.target.vault_path)
            .ok_or_else(|| miette::miette!("invalid extraction target"))?;
        allowed = classify_against_page(
            root,
            &target,
            task.target.kind,
            &task.cache_hash,
            &task.anchors(),
            Artifact::Result,
        )? == TaskStatus::Current;
    }
    if !allowed {
        return Err(miette::miette!(
            "task {task_id} is {}, not current",
            status.as_str()
        ));
    }
    task.validate_retained_input()
        .map_err(|e| miette::miette!("{e}"))?;
    task.generation_policy()
        .map_err(|e| miette::miette!("{e}"))?;
    Ok(task)
}

/// Pending queue files, sorted by name (run-id order). `.jsonl` only — skips the
/// `processed/` subdir and any `.jsonl.tmp`.
fn pending_queue_files(queue_dir: &Path) -> miette::Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(queue_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(miette::miette!("read {}: {error}", queue_dir.display())),
    };
    let mut files: Vec<PathBuf> = entries
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| miette::miette!("read {}: {error}", queue_dir.display()))?
        .into_iter()
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl"))
        .collect();
    files.sort();
    Ok(files)
}

fn read_tasks(file: &Path) -> miette::Result<QueueFile> {
    lk_queue::read_tasks(file).map_err(|e| miette::miette!("read {}: {e}", file.display()))
}

struct PendingTasks {
    tasks: BTreeMap<(String, String), QueueTask>,
    unparseable: usize,
}

fn read_pending_tasks(queue: &Path) -> miette::Result<PendingTasks> {
    let mut pending = PendingTasks {
        tasks: BTreeMap::new(),
        unparseable: 0,
    };
    for file in pending_queue_files(queue)? {
        let read = match read_tasks(&file) {
            Ok(read) => read,
            Err(error) => match std::fs::metadata(&file) {
                Err(io_error) if io_error.kind() == std::io::ErrorKind::NotFound => continue,
                _ => return Err(error),
            },
        };
        pending.unparseable += read.unparseable;
        for task in read.tasks {
            let key = (task.task_id.clone(), task.cache_hash.clone());
            if pending.tasks.insert(key, task).is_some() {
                return Err(miette::miette!("duplicate task identity in pending queue"));
            }
        }
    }
    Ok(pending)
}

/// A deliberate evidence edit must not strand work or race an unpublished draft.
/// Called under the vault mutation claim followed by the queue writer claim.
pub(super) fn require_citation_origin_quiescent(root: &Path, origin: &Path) -> miette::Result<()> {
    let queue = root.join(".lorekeeper/queue");
    let pending = read_pending_tasks(&queue)?;
    if pending.unparseable != 0 {
        return Err(miette::miette!(
            "unparseable pending work prevents citation retraction"
        ));
    }
    let addresses_origin = |address: &str| -> miette::Result<bool> {
        let Some(path) = resolve_target_path(address) else {
            return Ok(false);
        };
        if path == origin {
            return Ok(true);
        }
        if lk_vault::page_path(root, &path).is_err() {
            return Ok(false);
        }
        lk_vault::same_existing_file(root, &path, origin)
            .map_err(|error| miette::miette!("compare queued origin address: {error}"))
    };
    let touches = |task: &QueueTask| -> miette::Result<bool> {
        if addresses_origin(&task.target.vault_path)? {
            return Ok(true);
        }
        if let Some(citations) = task.input.get("citations").and_then(|v| v.as_array()) {
            for path in citations
                .iter()
                .filter_map(|citation| citation.get("path").and_then(|v| v.as_str()))
            {
                if addresses_origin(path)? {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    };
    let mut revisions = BTreeMap::new();
    for task in pending.tasks.values() {
        if !touches(task)? {
            continue;
        }
        if matches!(
            classify_task(root, task, &mut revisions)?,
            TaskStatus::Current | TaskStatus::Unreadable | TaskStatus::MissingTarget
        ) {
            return Err(miette::miette!(
                "pending task {} touches citation origin; finish or repair it first",
                task.task_id
            ));
        }
    }
    let batch =
        lk_queue::read_results(&queue).map_err(|e| miette::miette!("read queue results: {e}"))?;
    if !batch.unreadable.is_empty() {
        return Err(miette::miette!(
            "unreadable staged results prevent citation retraction"
        ));
    }
    let mut staged: Vec<(String, String)> = batch
        .ready
        .into_iter()
        .map(|(_, result)| {
            if addresses_origin(&result.target.vault_path)? {
                return Err(miette::miette!(
                    "staged extraction {} targets citation origin",
                    result.task_id
                ));
            }
            Ok((result.task_id, result.cache_hash))
        })
        .collect::<miette::Result<_>>()?;
    let entries = match std::fs::read_dir(queue.join(lk_queue::RESULTS_SUBDIR)) {
        Ok(entries) => Some(entries),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(miette::miette!("read staged results: {e}")),
    };
    for entry in entries.into_iter().flatten() {
        let path = entry
            .map_err(|e| miette::miette!("read staged result: {e}"))?
            .path();
        let name = path.to_string_lossy();
        let section = name.ends_with(lk_queue::SECTION_RESULT_SUFFIX);
        if !section && !name.ends_with(lk_queue::SYNTHESIS_RESULT_SUFFIX) {
            continue;
        }
        let raw =
            std::fs::read(&path).map_err(|e| miette::miette!("read {}: {e}", path.display()))?;
        let identity = if section {
            serde_json::from_slice::<lk_queue::SectionResult>(&raw)
                .map(|r| (r.task_id, r.cache_hash))
        } else {
            serde_json::from_slice::<lk_queue::SynthesisResult>(&raw)
                .map(|r| (r.task_id, r.cache_hash))
        }
        .map_err(|e| miette::miette!("unreadable staged result {}: {e}", path.display()))?;
        staged.push(identity);
    }
    for identity in staged {
        let task = pending.tasks.get(&identity).ok_or_else(|| {
            miette::miette!(
                "unmatched staged result {} prevents citation retraction",
                identity.0
            )
        })?;
        if touches(task)? {
            return Err(miette::miette!(
                "staged result {} touches citation origin; finish or repair it first",
                task.task_id
            ));
        }
    }
    Ok(())
}

pub(super) fn claim_writer(
    queue_dir: &Path,
    dry_run: bool,
) -> miette::Result<Option<std::fs::File>> {
    if dry_run {
        return Ok(None);
    }
    match lk_queue::claim_queue(queue_dir)
        .map_err(|error| miette::miette!("claim queue: {error}"))?
    {
        lk_queue::QueueClaim::Held(file) => Ok(Some(file)),
        lk_queue::QueueClaim::Busy => Err(miette::miette!(
            "another queue writer is materializing or pruning this queue; retry after it finishes"
        )),
        lk_queue::QueueClaim::Unavailable(error) => Err(miette::miette!(
            "cannot safely claim the queue writer lock: {error}"
        )),
    }
}

async fn apply(
    opts: &super::GlobalOptions,
    root: Option<PathBuf>,
    dry_run: bool,
) -> miette::Result<()> {
    let (config, config_revision) = load_config_with_revision(&find_config(opts)?)?;
    let vault_root = root.unwrap_or_else(|| config.vault.root_path());
    let queue_dir = vault_root.join(".lorekeeper").join("queue");

    // Global vault claim precedes narrower queue claims everywhere. Recovery runs
    // before deriving any write from current pages; both claims fail fast if busy.
    let mutation = (!dry_run)
        .then(|| lk_vault::MutationSession::acquire(&vault_root))
        .transpose()
        .map_err(|error| miette::miette!("claim vault mutation: {error}"))?;
    let _claim = claim_writer(&queue_dir, dry_run)?;

    let batch = lk_queue::read_results(&queue_dir)
        .map_err(|e| miette::miette!("read queue results: {e}"))?;

    let synthesis_result = synthesis::apply(
        &vault_root,
        &queue_dir,
        &config,
        &config_revision,
        dry_run,
        mutation.as_ref(),
    )?;

    let unusable = batch.unreadable;
    let results = batch.ready;
    // A file that will never parse is moved out of the apply path, once and loudly. Leaving
    // it would fail this command — and so the scheduled pipeline — on every run forever,
    // while the concepts it held are re-derived anyway: its page carries no completion
    // marker, so the next ingest that re-renders that page re-enqueues the task.
    // Quarantining is housekeeping, so its own failures must not do what it exists to
    // prevent: a `?` here would strand every ready result behind an unwritable directory,
    // which is the same "one bad file blocks the batch" shape, reached through I/O instead
    // of through parsing. A file that cannot be moved simply stays put and is retried.
    let unreadable = unusable.len() + synthesis_result.unreadable;
    let mut moved = synthesis_result.quarantined;
    if !unusable.is_empty() {
        let corrupt_dir = queue_dir
            .join(lk_queue::RESULTS_SUBDIR)
            .join(lk_queue::CORRUPT_SUBDIR);
        moved += quarantine(&corrupt_dir, &unusable, dry_run);
    }

    if results.is_empty() {
        eprintln!(
            "queue apply: {} staged sections applied, {} dropped, {} failed{}",
            synthesis_result.applied,
            synthesis_result.dropped,
            synthesis_result.failed,
            if dry_run { " (dry-run)" } else { "" }
        );
        return finish_apply(synthesis_result.failed, unreadable, moved, dry_run);
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
    let (mut applied, mut dropped, mut failed) = (
        synthesis_result.applied,
        synthesis_result.dropped,
        synthesis_result.failed,
    );
    // Keyed by page, because ONE page can receive several results in a batch: two ingests
    // before a single drain enqueue the same target twice under the same input hash, and
    // both classify current. Every write lands after this loop, so a second fold that read
    // the page from disk would see a version predating the first — and its write would then
    // drop the citations the first added, the exact loss the accumulating render exists to
    // prevent. Within a batch, the pending version IS the page.
    // Each page carries the results that produced it, because whether they may be deleted is
    // decided by whether that page WROTE, and one page can hold several.
    let pending = read_pending_tasks(&queue_dir)?;
    let mut origin_revisions: BTreeMap<PathBuf, String> = BTreeMap::new();
    let mut origin_pages: BTreeMap<PathBuf, (String, Vec<PathBuf>)> = BTreeMap::new();
    let mut consumed: Vec<PathBuf> = Vec::new();
    let mut completed_tasks = std::collections::BTreeSet::new();
    let mut work_guards = Vec::new();
    let mut proposals: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for (path, result) in results {
        proposals
            .entry((result.task_id.clone(), result.cache_hash.clone()))
            .or_default()
            .push((path, result));
    }
    let mut agreed = Vec::new();
    for group in proposals.into_values() {
        let expected =
            serde_json::to_value(&group[0].1).map_err(|error| miette::miette!("{error}"))?;
        let mut ambiguous = false;
        for (_, result) in group.iter().skip(1) {
            if serde_json::to_value(result).map_err(|error| miette::miette!("{error}"))? != expected
            {
                ambiguous = true;
                break;
            }
        }
        if ambiguous {
            eprintln!(
                "  ✗ {}: conflicting extraction results for the same retained task and input; review and reconcile all retained proposals to one agreed value before retrying",
                group[0].1.task_id
            );
            failed += 1;
            continue;
        }
        let mut entries = group.into_iter();
        let (path, result) = entries.next().expect("proposal groups are nonempty");
        let paths = std::iter::once(path)
            .chain(entries.map(|(path, _)| path))
            .collect::<Vec<_>>();
        agreed.push((paths, result));
    }
    agreed.sort_by(|left, right| left.0[0].cmp(&right.0[0]));

    // Everything a result can get wrong is that result's problem, not the batch's: aborting
    // would strand every other valid extraction in the run. A RETRYABLE failure leaves the
    // file in place so a fixed page picks it up next time, and the run exits non-zero.
    for (paths, result) in &agreed {
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
            consumed.extend(paths.iter().cloned());
            continue;
        };
        if !matches!(
            result.target.kind,
            lk_queue::TargetKind::DailyConcepts | lk_queue::TargetKind::DocumentConcepts
        ) {
            fail("extraction result requires a daily or document concept target".into());
            failed += 1;
            continue;
        }
        let task = pending
            .tasks
            .get(&(result.task_id.clone(), result.cache_hash.clone()));
        if task.is_none() {
            let reason = if pending.unparseable > 0 {
                "no matching readable task; repair unparseable queue lines before deciding this result's fate"
            } else {
                "no matching pending task; its run was retired or the task identity is invalid"
            };
            if pending.unparseable == 0 && !dry_run {
                for path in paths {
                    synthesis::quarantine_orphan(&queue_dir, path, reason)?;
                }
            }
            fail(reason.into());
            failed += 1;
            continue;
        };
        if let Some(task) = task
            && let Err(error) = task
                .validate_retained_input()
                .and_then(|()| task.generation_policy().map(|_| ()))
        {
            fail(error);
            failed += 1;
            continue;
        }
        if let Some(task) = task
            && (task.kind != lk_queue::TaskKind::ExtractConcepts
                || task.target.vault_path != result.target.vault_path
                || task.target.kind != result.target.kind
                || task.target.anchor != result.target.anchor
                || task.input.get("date").and_then(|value| value.as_str())
                    != Some(result.date.to_string().as_str())
                || task.input.get("concept_policy")
                    != Some(
                        &serde_json::to_value(result.concept_policy)
                            .expect("concept policy serializes"),
                    ))
        {
            fail("extraction result does not match its pending task".into());
            failed += 1;
            continue;
        }
        if let Some(task) = task
            && policy_request_status(&vault_root, task) == Some(TaskStatus::Stale)
        {
            drop_dead("superseded generation policy");
            dropped += 1;
            consumed.extend(paths.iter().cloned());
            continue;
        }
        match classify_against_page(
            &vault_root,
            &rel_path,
            result.target.kind,
            &result.cache_hash,
            // One section, because `TaskResult` carries `concepts` and nothing else: a result
            // from any other kind would arrive with no payload to apply, so the single anchor
            // is what the type can express rather than what the protocol asks for.
            &[result.target.anchor.as_str()],
            Artifact::Result,
        ) {
            Ok(TaskStatus::Current) => {}
            // A page nobody can read is the absence of a verdict, not a dead result. The
            // extraction it carries is the answer in flight, and a repair of the page is a
            // one-line fix that brings it back — consuming it here would spend that answer on
            // a page that could not receive it, with nothing left to say what was lost.
            Ok(TaskStatus::Unreadable) => {
                fail("target page will not parse; repair it and this result applies".into());
                failed += 1;
                continue;
            }
            Ok(status @ (TaskStatus::Done | TaskStatus::Stale | TaskStatus::MissingTarget)) => {
                drop_dead(status.as_str());
                dropped += 1;
                consumed.extend(paths.iter().cloned());
                continue;
            }
            Err(e) => {
                fail(e.to_string());
                failed += 1;
                continue;
            }
        }
        let content = match origin_pages.get(&rel_path) {
            Some((pending, _)) => pending.clone(),
            None => match read_page(&vault_root, &rel_path) {
                Ok(c) => c,
                Err(e) => {
                    fail(format!("read {}: {e}", rel_path.display()));
                    failed += 1;
                    continue;
                }
            },
        };
        let original_revision = origin_revisions
            .get(&rel_path)
            .cloned()
            .unwrap_or_else(|| synthesis::revision(&content));
        if original_revision != result.target_revision {
            if let Some(task) = task
                && classify_task(&vault_root, task, &mut BTreeMap::new())? == TaskStatus::Done
            {
                drop_dead("already materialized");
                dropped += 1;
                consumed.extend(paths.iter().cloned());
                continue;
            }
            fail("target changed; read the current page and regenerate this result".into());
            failed += 1;
            continue;
        }
        let guarded = super::work::prepare_queue_publication(
            &vault_root,
            &config,
            &config_revision,
            &result.task_id,
            serde_json::to_value(result).map_err(|e| miette::miette!("{e}"))?,
        );
        let result_guards = match guarded {
            Ok(guards) => guards,
            Err(error) => {
                fail(error.to_string());
                failed += 1;
                continue;
            }
        };
        let rss_manifest = task
            .and_then(|t| t.input.get("rss_original"))
            .map(|v| {
                serde_json::from_value::<lk_core::rss_original::RssOriginalManifest>(v.clone())
            })
            .transpose()
            .map_err(|e| miette::miette!("RSS manifest: {e}"))?;
        match pipeline
            .apply_concept_result_with_manifest(result, &content, rss_manifest.as_ref())
            .await
        {
            Ok(rewritten) => {
                let rewritten = stamp_policy(&rewritten, task, result.target.kind)?;
                origin_revisions.insert(rel_path.clone(), original_revision);
                let page = origin_pages
                    .entry(rel_path)
                    .or_insert_with(|| (String::new(), Vec::new()));
                page.0 = rewritten;
                page.1.extend(paths.iter().cloned());
                completed_tasks.insert((result.task_id.clone(), result.cache_hash.clone()));
                work_guards.push(result_guards);
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

    for (path, expected) in &origin_revisions {
        let current = read_page(&vault_root, path)
            .map_err(|error| miette::miette!("read {}: {error}", path.display()))?;
        if synthesis::revision(&current) != *expected {
            return Err(miette::miette!(
                "target changed before publication: {}; regenerate its extraction result",
                path.display()
            ));
        }
    }

    // One durable write set: concepts precede their origins, and recovery resumes it
    // before the next cooperating writer reads the vault. Results remain until commit.
    let mut publications = lk_vault::MutationPlan::default();
    let mut owned = BTreeMap::new();
    for page in &concept_pages {
        page.plan_publication(&mut publications, &vault_root)
            .map_err(|e| miette::miette!("prepare {}: {e}", page.path))?;
        if let Some(Some(original)) = &page.expected_original {
            register_publication_owner(&vault_root, &mut owned, page.path.as_ref(), original)?;
        }
        if page.expected_original.as_ref().and_then(Option::as_ref) == Some(&page.content) {
            publications.require_unchanged(page.path.as_ref(), page.content.clone());
        }
    }
    let mut committed_results = Vec::new();
    for (rel_path, (content, results)) in &origin_pages {
        let current = read_page(&vault_root, rel_path);
        match current {
            Ok(current) if synthesis::revision(&current) == origin_revisions[rel_path] => {
                // This content is derived from this exact page and parsed before staging;
                // keep its expected original bytes rather than capturing a newer version.
                lk_vault::guard_rss_evidence(&mut publications, &vault_root, rel_path, content)
                    .map_err(|e| miette::miette!("RSS evidence: {e}"))?;
                register_publication_owner(&vault_root, &mut owned, rel_path, &current)?;
                if current == *content {
                    publications.require_unchanged(rel_path.clone(), current);
                } else {
                    publications.edit(rel_path.clone(), current, content.clone());
                }
                committed_results.extend(results.iter().cloned());
            }
            other => {
                let reason = match other {
                    Err(error) => error.to_string(),
                    _ => {
                        "target changed before publication; regenerate its extraction result".into()
                    }
                };
                return Err(miette::miette!(
                    "{}: {reason}; extraction publication refused",
                    rel_path.display()
                ));
            }
        }
    }
    let mut plan = lk_vault::MutationPlan::default();
    for guard in work_guards {
        guard.append_to(&mut plan, &owned)?;
    }
    plan.extend(publications);
    if dry_run {
        for page in &concept_pages {
            super::report_refusals(page, true);
        }
        eprintln!(
            "[dry-run] queue apply: {applied} applied, {dropped} dropped, {failed} failed, \
             {} concept page(s)",
            concept_pages.len()
        );
        return finish_apply(failed, unreadable, moved, dry_run);
    }
    for identity in completed_tasks {
        super::work::retire_discovery(&vault_root, &pending.tasks[&identity], &mut plan)?;
    }
    mutation
        .as_ref()
        .expect("non-dry-run owns the mutation session")
        .apply(plan)
        .map_err(|e| miette::miette!("publish extraction: {e}"))?;
    for page in &concept_pages {
        super::report_refusals(page, false);
    }
    consumed.extend(committed_results);

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

fn register_publication_owner(
    root: &Path,
    owned: &mut BTreeMap<PathBuf, String>,
    path: &Path,
    original: &str,
) -> miette::Result<()> {
    let root = root
        .canonicalize()
        .map_err(|error| miette::miette!("{error}"))?;
    let absolute = lk_vault::page_path(&root, path)
        .map_err(|error| miette::miette!("{error}"))?
        .canonicalize()
        .map_err(|error| miette::miette!("{error}"))?;
    let relative = absolute
        .strip_prefix(&root)
        .map_err(|error| miette::miette!("{error}"))?
        .to_path_buf();
    if owned.contains_key(&relative) {
        return Err(miette::miette!(
            "multiple publication addresses resolve to the same page: {}",
            relative.display()
        ));
    }
    owned.insert(relative, original.into());
    Ok(())
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

fn policy_request_status(root: &Path, task: &QueueTask) -> Option<TaskStatus> {
    let path = resolve_target_path(&task.target.vault_path)?;
    let content = read_page(root, &path).ok()?;
    let page = lk_core::frontmatter::parse_page(&content).ok()?;
    let inputs = page.frontmatter.get("llm_inputs")?;
    let key = task.target.kind.llm_inputs_key();
    if inputs
        .get(format!("{key}_policy_requested_input"))
        .and_then(|v| v.as_str())
        != Some(&task.cache_hash)
    {
        return None;
    }
    let requested = inputs
        .get(format!("{key}_policy_requested"))
        .and_then(|v| v.as_str())?;
    if task.generation_policy().ok().flatten() != Some(requested) {
        Some(TaskStatus::Stale)
    } else {
        None
    }
}

fn stamp_policy(
    content: &str,
    task: Option<&QueueTask>,
    kind: lk_queue::TargetKind,
) -> miette::Result<String> {
    let policy = task
        .map(QueueTask::generation_policy)
        .transpose()
        .map_err(|e| miette::miette!("{e}"))?
        .flatten()
        .unwrap_or("legacy");
    let content = lk_vault::set_llm_input(
        content,
        &format!("{}_policy_done", kind.llm_inputs_key()),
        &serde_json::to_string(policy).expect("policy serializes"),
    )
    .ok_or_else(|| miette::miette!("cannot stamp generation policy"))?;
    let content = lk_vault::set_llm_input(
        &content,
        &format!("{}_policy_requested", kind.llm_inputs_key()),
        "null",
    )
    .ok_or_else(|| miette::miette!("cannot clear policy request"))?;
    lk_vault::set_llm_input(
        &content,
        &format!("{}_policy_requested_input", kind.llm_inputs_key()),
        "null",
    )
    .ok_or_else(|| miette::miette!("cannot clear policy request input"))
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
        let reason_path = dest.with_file_name(format!(
            "{}.reason.txt",
            dest.file_name().unwrap_or_default().to_string_lossy()
        ));
        if let Err(error) =
            lk_core::fs::write_atomic(&reason_path, format!("{reason}\n").as_bytes(), None)
        {
            eprintln!(
                "  ✗ cannot preserve quarantine reason for {}: {error}",
                path.display()
            );
            continue;
        }
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

/// What a drain would find pending, in the three kinds that call for different repairs.
///
/// Read by `queue count`, by the front-door summary, and by `self update` — which refuses to
/// swap the binary while work is outstanding, because a task written by one build and answered
/// under another build's skills is the one genuinely lossy moment in an upgrade.
pub(crate) struct QueueLoad {
    pub work: usize,
    /// The target is unreadable or the synthesis input is inconsistent, so work is blocked.
    pub unreadable: usize,
    /// The queue LINE decoded no target at all, so the work it names is unknown.
    pub unparseable: usize,
}

pub(crate) fn queue_load(vault_root: &Path) -> miette::Result<QueueLoad> {
    let mut revisions = BTreeMap::new();
    let queue_dir = vault_root.join(".lorekeeper").join("queue");
    let mut load = QueueLoad {
        work: 0,
        unreadable: 0,
        unparseable: 0,
    };
    for file in pending_queue_files(&queue_dir)? {
        let read = read_tasks(&file)?;
        load.unparseable += read.unparseable;
        for task in read.tasks {
            match classify_task(vault_root, &task, &mut revisions)? {
                s if s.is_work() => load.work += 1,
                TaskStatus::Unreadable => load.unreadable += 1,
                _ => {}
            }
        }
    }
    Ok(load)
}

async fn count(opts: &super::GlobalOptions, root: Option<PathBuf>) -> miette::Result<()> {
    let vault_root = resolve_vault_root(opts, root)?;
    let QueueLoad {
        work: current,
        unreadable,
        unparseable,
    } = queue_load(&vault_root)?;
    // stdout stays the machine contract: how many tasks a drain can act on. A task nothing can
    // classify is not one of them — spending a session on it would help nothing — but reporting
    // only the number leaves a queue that is BLOCKED indistinguishable from one that is empty,
    // and the scheduled drain reads exactly this number to decide whether to run.
    // Named separately because the repairs differ: an unreadable task points at a page whose
    // frontmatter needs fixing, while an unparseable LINE decoded no target at all — there is
    // nothing to look the page up by, and only the queue file itself can be repaired.
    if unreadable > 0 {
        eprintln!(
            "warning: {unreadable} task(s) have unreadable targets or inconsistent synthesis inputs — \
             `lore queue status` names them; repair the page or task before retrying."
        );
    }
    if unparseable > 0 {
        eprintln!(
            "warning: {unparseable} queue line(s) could not be parsed, so the work they name is \
             unknown — their file is left whole for repair by hand."
        );
    }
    println!("{current}");
    Ok(())
}

async fn status(
    opts: &super::GlobalOptions,
    root: Option<PathBuf>,
    json: bool,
) -> miette::Result<()> {
    let mut revisions = BTreeMap::new();
    let vault_root = resolve_vault_root(opts, root)?;
    let queue_dir = vault_root.join(".lorekeeper").join("queue");

    let mut reports = Vec::new();
    let mut unparseable = 0usize;
    for file in pending_queue_files(&queue_dir)? {
        let read = read_tasks(&file)?;
        unparseable += read.unparseable;
        for task in read.tasks {
            let status = classify_task(&vault_root, &task, &mut revisions)?;
            let target_revision = resolve_target_path(&task.target.vault_path)
                .and_then(|path| read_page(&vault_root, &path).ok())
                .map(|content| synthesis::revision(&content));
            reports.push(TaskReport {
                generation_policy: task
                    .input
                    .get("generation_policy")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
                target_revision,
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
    let unreadable = tally(TaskStatus::Unreadable);

    if json {
        let tasks: Vec<serde_json::Value> = reports
            .iter()
            .map(|r| {
                serde_json::json!({
                    "task_id": r.task_id,
                    "kind": r.kind,
                    "vault_path": r.vault_path,
                    "status": r.status.as_str(),
                    "target_revision": r.target_revision,
                    "generation_policy": r.generation_policy,
                })
            })
            .collect();
        let envelope = serde_json::json!({
            // `status` is a classification, not a verdict — it exits 0 over any queue it can
            // describe, so `ok` is true whenever it got that far.
            "ok": true,
            "data": {
                "current": current,
                "done": done,
                "stale": stale,
                "missing_target": missing,
                "unreadable": unreadable,
                "unparseable_lines": unparseable,
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
            "queue: {current} current, {done} done, {stale} stale, {missing} missing-target, \
             {unreadable} unreadable across {} task(s){}",
            reports.len(),
            if unparseable > 0 {
                format!("; {unparseable} line(s) could not be parsed")
            } else {
                String::new()
            }
        );
    }
    Ok(())
}

#[derive(Debug, Default, PartialEq, Eq)]
struct PruneSummary {
    pruned_stale: usize,
    pruned_missing_target: usize,
    kept_current: usize,
    /// Tasks whose target page could not be read. Retained: a repair of the page restores them.
    kept_unreadable: usize,
    /// Lines nobody could parse. Their file is left whole — see [`QueueFile::is_rewritable`].
    kept_unparseable: usize,
    /// Tasks held only because they share a file with such a line. Not classified: nothing here
    /// acted on them, and counting one under `kept_current` or `pruned_stale` would say prune
    /// decided something it did not.
    kept_blocked: usize,
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
    let session = (!dry_run)
        .then(|| lk_vault::MutationSession::acquire(vault_root))
        .transpose()
        .map_err(|error| miette::miette!("claim vault mutation: {error}"))?;
    prune_queue_with_session(vault_root, queue_dir, session.as_ref())
}

fn prune_queue_with_session(
    vault_root: &Path,
    queue_dir: &Path,
    session: Option<&lk_vault::MutationSession>,
) -> miette::Result<PruneSummary> {
    let dry_run = session.is_none();
    let _claim = claim_writer(queue_dir, dry_run)?;
    let mut revisions = BTreeMap::new();
    let mut summary = PruneSummary::default();
    for file in pending_queue_files(queue_dir)? {
        let read = match read_tasks(&file) {
            Ok(read) => read,
            Err(error) => match std::fs::metadata(&file) {
                Err(io_error) if io_error.kind() == std::io::ErrorKind::NotFound => continue,
                _ => return Err(error),
            },
        };
        // A file holding a line nobody can parse keeps every line: rewriting it would drop
        // work nothing could classify. Its own GC waits for a human; the rest proceeds.
        //
        // The tasks held with it are counted, because what is stuck is what a reader needs —
        // and counted as blocked rather than under their classification, because nothing here
        // acted on it.
        if !read.is_rewritable() {
            summary.kept_unparseable += read.unparseable;
            summary.kept_blocked += read.tasks.len();
            continue;
        }
        let tasks = read.tasks;
        let mut retained = Vec::with_capacity(tasks.len());
        let (mut dropped, mut work_left) = (0usize, 0usize);
        for task in tasks {
            match classify_task(vault_root, &task, &mut revisions)? {
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
                TaskStatus::Unreadable => {
                    // Counted as work LEFT, not merely retained. Retiring a run is for one whose
                    // every remaining task is ANSWERED; an unreadable task is not answered, and
                    // archiving it puts it where nothing will ever reclassify it — so the repair
                    // this status exists to wait for could never bring it back.
                    work_left += 1;
                    summary.kept_unreadable += 1;
                    retained.push(task);
                }
            }
        }

        if retained.is_empty() {
            // Nothing worth keeping: only dead tasks, or none at all. Either way the run
            // never edited a page, so there is no record to archive — and leaving it would
            // keep `lore ingest` warning about pending work no session will ever do.
            summary.files_deleted += 1;
            if !dry_run {
                std::fs::remove_file(&file)
                    .map_err(|e| miette::miette!("remove {}: {e}", file.display()))?;
            }
        } else if work_left == 0 {
            // Archive before rewriting so a failed rewrite retains the complete run history.
            summary.files_archived += 1;
            if !dry_run
                && let Some(archived) = archive_queue_file(queue_dir, &file)?
                && dropped > 0
            {
                lk_queue::write_tasks_atomic(&archived, &retained)
                    .map_err(|e| miette::miette!("rewrite {}: {e}", archived.display()))?;
            }
        } else if dropped > 0 {
            summary.files_rewritten += 1;
            if !dry_run {
                lk_queue::write_tasks_atomic(&file, &retained)
                    .map_err(|e| miette::miette!("rewrite {}: {e}", file.display()))?;
            }
        }
    }
    if let Some(session) = session {
        let mut identities = std::collections::BTreeSet::new();
        let mut complete = true;
        for path in pending_queue_files(queue_dir)? {
            let pending = match read_tasks(&path) {
                Ok(pending) => pending,
                Err(error) => match std::fs::metadata(&path) {
                    Err(io_error) if io_error.kind() == std::io::ErrorKind::NotFound => continue,
                    _ => return Err(error),
                },
            };
            if !pending.is_rewritable() {
                complete = false;
                break;
            }
            identities.extend(
                pending
                    .tasks
                    .into_iter()
                    .map(|task| (task.task_id, task.cache_hash)),
            );
        }
        if complete {
            let removed = super::work::prune_discovery(session, &identities)?;
            if removed != 0 {
                eprintln!("pruned {removed} unreferenced extraction discovery recipe(s)");
            }
        }
    }
    Ok(summary)
}

/// Archive a settled run under the queue writer lock, preserving earlier archive names.
/// A missing source has already been retired; other filesystem failures are reported.
fn archive_queue_file(queue_dir: &Path, file: &Path) -> miette::Result<Option<PathBuf>> {
    let dir = queue_dir.join(lk_queue::PROCESSED_SUBDIR);
    std::fs::create_dir_all(&dir).map_err(|e| miette::miette!("create {}: {e}", dir.display()))?;
    let dest = free_path(&dir, file);
    match std::fs::rename(file, &dest) {
        Ok(()) => Ok(Some(dest)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(miette::miette!("archive {}: {e}", file.display())),
    }
}

pub(super) async fn prune(
    opts: &super::GlobalOptions,
    root: Option<PathBuf>,
    json: bool,
    dry_run: bool,
) -> miette::Result<()> {
    let vault_root = resolve_vault_root(opts, root)?;
    let queue_dir = vault_root.join(".lorekeeper").join("queue");
    let summary = prune_queue(&vault_root, &queue_dir, dry_run)?;
    report_prune(summary, json, dry_run)
}

pub(super) fn prune_with_session(
    session: &lk_vault::MutationSession,
    json: bool,
) -> miette::Result<()> {
    let root = session.root();
    let summary = prune_queue_with_session(root, &root.join(".lorekeeper/queue"), Some(session))?;
    report_prune(summary, json, false)
}

fn report_prune(summary: PruneSummary, json: bool, dry_run: bool) -> miette::Result<()> {
    // A retained-because-unclassifiable task is a defect with a named repair — the target page's
    // frontmatter, or the queue line itself — and nothing else in a scheduled run reports it:
    // `queue count` deliberately omits it from the drain decision so a session is never spent on
    // work no session can do, which leaves the janitor as the only place it can surface.
    // Reporting one kind and not the other left the janitor green over a queue it had just
    // described as blocked.
    let blocked = summary.kept_unreadable > 0 || summary.kept_unparseable > 0;

    if json {
        let envelope = serde_json::json!({
            // The verdict the exit code carries, not a constant: a consumer reading `ok` out of
            // a process that exited 1 was told the opposite of what the process said.
            "ok": !blocked,
            "data": {
                "dry_run": dry_run,
                "pruned_stale": summary.pruned_stale,
                "pruned_missing_target": summary.pruned_missing_target,
                "kept_current": summary.kept_current,
                "kept_unreadable": summary.kept_unreadable,
                "kept_unparseable": summary.kept_unparseable,
                "kept_blocked": summary.kept_blocked,
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
            "{label}: dropped {} stale + {} missing-target, kept {} current + {} unreadable \
             + {} unparseable + {} blocked, {} already done; {} file(s) rewritten, {} archived, \
             {} deleted",
            summary.pruned_stale,
            summary.pruned_missing_target,
            summary.kept_current,
            summary.kept_unreadable,
            summary.kept_unparseable,
            summary.kept_blocked,
            summary.retired_done,
            summary.files_rewritten,
            summary.files_archived,
            summary.files_deleted
        );
    }

    if blocked {
        if summary.kept_unreadable > 0 {
            eprintln!(
                "{} task(s) kept because their target or synthesis input cannot be validated — \
                 repair the page or task; `lore queue status` names them.",
                summary.kept_unreadable
            );
        }
        if summary.kept_unparseable > 0 {
            eprintln!(
                "{} queue line(s) could not be parsed, so their file is kept whole, holding {} \
                 task(s) with them — repair it by hand; nothing can tell what work they named.",
                summary.kept_unparseable, summary.kept_blocked
            );
        }
        std::process::exit(1);
    }
    Ok(())
}

/// The (page, `llm_inputs` key) pairs a pending task can still fill.
///
/// The question a report about an unanswered section has to ask before calling the work lost.
/// `Current` is the only status that answers yes: a stale task was superseded, a done one has
/// already answered, a missing-target one has nowhere to land, and an unreadable target cannot
/// be judged at all. Classified by the same rule `queue status` uses, so the two can never
/// disagree about what is pending.
pub(super) struct WorkInFlight {
    pub keys: std::collections::HashSet<(PathBuf, &'static str)>,
    /// Why the answer may be incomplete: a queue file that could not be read, or a line nobody
    /// could parse. A caller that reports unanswered sections must say so rather than conclude
    /// nothing is pending — "no task can fill this" is the claim that licenses a destructive
    /// repair, and it cannot be made about a queue that was not fully read.
    pub unread: Vec<String>,
}
/// Find an already-current exact request while refusing an undecidable pending queue.
pub(super) fn current_synthesis_request(
    root: &Path,
    target: &str,
    hash: &str,
) -> miette::Result<Option<QueueTask>> {
    let mut revisions = BTreeMap::new();
    let mut found = None;
    for file in pending_queue_files(&root.join(".lorekeeper/queue"))? {
        let read = read_tasks(&file)?;
        if read.unparseable > 0 {
            return Err(miette::miette!(
                "pending queue has unparseable lines: {}",
                file.display()
            ));
        }
        for task in read.tasks {
            if task.target.kind == lk_queue::TargetKind::ConceptSynthesis
                && task.target.vault_path == target
                && task.cache_hash == hash
            {
                let state = classify_task(root, &task, &mut revisions)?;
                if state == TaskStatus::Unreadable {
                    return Err(miette::miette!(
                        "matching synthesis request is unreadable: {}",
                        task.task_id
                    ));
                }
                if state == TaskStatus::Current && task.generation_policy().ok().flatten().is_some()
                {
                    found.get_or_insert(task);
                }
            }
        }
    }
    Ok(found)
}

/// The `(vault_path, cache_hash)` of every pending task of `kind` that still classifies as
/// WORK — one a drain will act on.
///
/// The alive question is asked HERE, through the same `classify_task` `queue status` uses,
/// because a caller deciding whether to queue work again has to know whether what is on disk
/// can still be answered. A task can die with its input unchanged: `vault.locale` renames a
/// page's headings, the task's anchor is no longer on the page, and it classifies
/// `missing-target` while its `cache_hash` still matches. Reading the files directly would
/// see that task, suppress the re-queue, and leave the page's recorded input with nothing
/// able to answer it — and `queue count` counts only work, so the drain that would archive
/// the file is skipped too.
///
/// An unreadable file or unparseable line contributes nothing, which re-queues rather than
/// suppresses — the safe direction.
pub(super) fn pending_work_inputs(
    vault_root: &Path,
    kind: lk_queue::TargetKind,
) -> std::collections::HashSet<(String, String)> {
    let mut revisions = BTreeMap::new();
    let queue_dir = vault_root.join(".lorekeeper").join("queue");
    let Ok(files) = pending_queue_files(&queue_dir) else {
        return std::collections::HashSet::new();
    };
    files
        .into_iter()
        .filter_map(|file| read_tasks(&file).ok())
        .flat_map(|read| read.tasks)
        .filter(|task| task.target.kind == kind)
        .filter(|task| {
            classify_task(vault_root, task, &mut revisions).is_ok_and(|status| status.is_work())
        })
        .map(|task| (task.target.vault_path.clone(), task.cache_hash.clone()))
        .collect()
}

pub(super) fn work_in_flight(vault_root: &Path) -> WorkInFlight {
    let mut revisions = BTreeMap::new();
    let queue_dir = vault_root.join(".lorekeeper").join("queue");
    let mut flight = WorkInFlight {
        keys: std::collections::HashSet::new(),
        unread: Vec::new(),
    };
    let files = match pending_queue_files(&queue_dir) {
        Ok(files) => files,
        Err(e) => {
            flight.unread.push(format!("{e}"));
            return flight;
        }
    };
    for file in files {
        let read = match read_tasks(&file) {
            Ok(read) => read,
            Err(e) => {
                flight.unread.push(format!("{e}"));
                continue;
            }
        };
        if read.unparseable > 0 {
            flight.unread.push(format!(
                "{}: {} unparseable line(s)",
                file.display(),
                read.unparseable
            ));
        }
        for task in read.tasks {
            let status = match classify_task(vault_root, &task, &mut revisions) {
                Ok(status) => status,
                Err(e) => {
                    flight.unread.push(format!("{e}"));
                    continue;
                }
            };
            if !status.is_work() {
                continue;
            }
            if let Some(rel) = resolve_target_path(&task.target.vault_path) {
                flight.keys.insert((rel, task.target.kind.llm_inputs_key()));
            }
        }
    }
    flight
}

/// Classify one task by comparing its `cache_hash` to the current input hash the
/// pipeline stamped into the target page's `llm_inputs.<key>` frontmatter. This is
/// the deterministic form of the stale-task guard `/lore-process` must honor.
fn classify_task(
    vault_root: &Path,
    task: &QueueTask,
    revisions: &mut BTreeMap<PathBuf, Result<String, TaskStatus>>,
) -> miette::Result<TaskStatus> {
    // A target outside the vault root is a task that can never land, which is exactly what
    // `MissingTarget` means — and the answer has to be a status rather than an error, or one
    // malformed task takes down every command that classifies the queue.
    let Some(rel_path) = resolve_target_path(&task.target.vault_path) else {
        return Ok(TaskStatus::MissingTarget);
    };
    if task.generation_policy().is_err() || task.validate_retained_input().is_err() {
        return Ok(TaskStatus::Unreadable);
    }
    if let Some(status) = policy_request_status(vault_root, task) {
        return Ok(status);
    }
    let status = classify_against_page(
        vault_root,
        &rel_path,
        task.target.kind,
        &task.cache_hash,
        &task.anchors(),
        Artifact::Task,
    )?;
    if status != TaskStatus::Current || task.target.kind != lk_queue::TargetKind::ConceptSynthesis {
        return Ok(status);
    }
    let Some(value) = task.input.get("citations") else {
        return Ok(TaskStatus::Unreadable);
    };
    let Ok(citations) = serde_json::from_value::<Vec<lk_core::concept::Citation>>(value.clone())
    else {
        return Ok(TaskStatus::Unreadable);
    };
    // Bind the semantic payload to the marker before trusting its source guards.
    // A shortened list must not silently skip evidence while keeping the old hash.
    if lk_core::concept::citation_digest(&citations) != task.cache_hash {
        tracing::warn!(task_id = %task.task_id, "synthesis citations do not match cache identity");
        return Ok(TaskStatus::Unreadable);
    }
    for citation in citations {
        if citation.rss_event.is_some() {
            match lk_vault::has_rss_citation(vault_root, &rel_path, &citation) {
                Ok(true) => {}
                Ok(false) => return Ok(TaskStatus::Stale),
                Err(_) => return Ok(TaskStatus::Unreadable),
            }
            continue;
        }
        let Some(path) = resolve_target_path(&citation.path) else {
            return Ok(TaskStatus::Stale);
        };
        let revision = revisions
            .entry(path.clone())
            .or_insert_with(|| read_source_revision(vault_root, &path));
        match revision {
            Ok(revision) if *revision == citation.revision => {}
            Ok(_) => return Ok(TaskStatus::Stale),
            Err(status) => return Ok(*status),
        }
    }
    Ok(status)
}

fn read_page(root: &Path, relative: &Path) -> std::io::Result<String> {
    std::fs::read_to_string(lk_vault::page_path(root, relative)?)
}

fn read_source_revision(vault_root: &Path, path: &Path) -> Result<String, TaskStatus> {
    let content = read_page(vault_root, path).map_err(|e| {
        tracing::warn!(path = %path.display(), error = %e, "cannot read synthesis source");
        if e.kind() == std::io::ErrorKind::NotFound {
            TaskStatus::Stale
        } else {
            TaskStatus::Unreadable
        }
    })?;
    let page = lk_core::frontmatter::parse_page(&content).map_err(|e| {
        tracing::warn!(path = %path.display(), error = %e, "cannot parse synthesis source");
        TaskStatus::Unreadable
    })?;
    Ok(lk_core::concept::source_revision(&page))
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
    anchors: &[&str],
    artifact: Artifact,
) -> miette::Result<TaskStatus> {
    if lk_vault::writable_page_path(vault_root, rel_path).is_err() {
        return Ok(TaskStatus::Unreadable);
    }
    let content = match read_page(vault_root, rel_path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(TaskStatus::MissingTarget);
        }
        Err(_) => return Ok(TaskStatus::Unreadable),
    };
    let Ok(page) = lk_core::frontmatter::parse_page(&content) else {
        return Ok(TaskStatus::Unreadable);
    };
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
    // Asked last, because it only decides the fate of work still to be written: EVERY
    // section the task names must exist on the page to receive it. An anchor the page does
    // not carry cannot come back — see `MissingTarget`. A `synthesize-concept` task names
    // two, and one marker answers for both, so a task classified `current` on the strength
    // of the heading it happens to check first would have the drain fill one section and
    // stamp the pair answered.
    Ok(
        if anchors.iter().all(|anchor| {
            anchor
                .strip_prefix("## ")
                .is_some_and(|heading| lk_vault::section_body(&page.body, heading).is_some())
        }) {
            TaskStatus::Current
        } else {
            TaskStatus::MissingTarget
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use lk_queue::{TargetKind, TaskKind, TaskTarget};
    use tempfile::TempDir;

    fn classify_task(vault_root: &Path, task: &QueueTask) -> miette::Result<TaskStatus> {
        super::classify_task(vault_root, task, &mut BTreeMap::new())
    }

    #[test]
    fn pruning_respects_the_materialization_writer_lock() {
        let dir = TempDir::new().unwrap();
        let queue = dir.path().join(".lorekeeper/queue");
        let claim = lk_queue::claim_queue(&queue).unwrap();
        assert!(matches!(claim, lk_queue::QueueClaim::Held(_)));
        let pending = queue.join("empty.jsonl");
        std::fs::write(&pending, "").unwrap();
        assert!(prune_queue(dir.path(), &queue, false).is_err());
        assert!(pending.exists());
        assert_eq!(
            prune_queue(dir.path(), &queue, true).unwrap().files_deleted,
            1
        );
        assert!(pending.exists());
        drop(claim);
        assert_eq!(
            prune_queue(dir.path(), &queue, false)
                .unwrap()
                .files_deleted,
            1
        );
        assert!(!pending.exists());
    }

    #[test]
    fn pruning_reuses_the_held_vault_session_and_preserves_queue_protection() {
        let directory = TempDir::new().unwrap();
        let root = directory.path();
        let session = lk_vault::MutationSession::acquire(root).unwrap();
        let queue = root.join(".lorekeeper/queue");
        let recipes = queue.join("discovery");
        std::fs::create_dir_all(&recipes).unwrap();
        let recipe = recipes.join(format!(
            "{}.json",
            lk_core::rss_original::digest(b"completed-extraction")
        ));
        let original = serde_json::json!({
            "task_id": "completed-extraction",
            "cache_hash": "input",
            "target_revision": "target",
            "queries": [{"query": "subject", "phrase": false, "spacing": false}]
        })
        .to_string();
        std::fs::write(&recipe, &original).unwrap();

        assert!(prune_queue(root, &queue, false).is_err());
        let claim = lk_queue::claim_queue(&queue).unwrap();
        assert!(matches!(claim, lk_queue::QueueClaim::Held(_)));
        let error = super::prune_with_session(&session, false).unwrap_err();
        assert!(error.to_string().contains("another queue writer"));
        assert_eq!(prune_queue(root, &queue, true).unwrap(), Default::default());
        assert_eq!(std::fs::read_to_string(&recipe).unwrap(), original);
        drop(claim);

        super::prune_with_session(&session, false).unwrap();
        assert!(!recipe.exists());
        assert!(lk_vault::MutationSession::acquire(root).is_err());
        assert!(!root.join(".lorekeeper/mutation/pending.json").exists());
    }

    #[test]
    fn queue_discovery_distinguishes_missing_storage_from_invalid_storage() {
        let dir = TempDir::new().unwrap();
        let queue = dir.path().join(".lorekeeper/queue");
        let config = serde_json::from_value::<lk_core::config::Config>(serde_json::json!({
            "vault":{"root":dir.path()}, "identity":{"name":"Test","email":"test@example.com"}, "sources":{}
        })).unwrap();
        assert!(pending_queue_files(&queue).unwrap().is_empty());
        assert!(
            synthesis::apply(dir.path(), &queue, &config, "config-revision", true, None).is_ok()
        );

        std::fs::create_dir_all(queue.parent().unwrap()).unwrap();
        std::fs::write(&queue, "not a directory").unwrap();
        assert!(pending_queue_files(&queue).is_err());
        assert!(queue_load(dir.path()).is_err());
        assert!(prune_queue(dir.path(), &queue, true).is_err());
        assert!(
            synthesis::apply(dir.path(), &queue, &config, "config-revision", true, None).is_err()
        );
        assert!(!work_in_flight(dir.path()).unread.is_empty());
        assert_eq!(std::fs::read_to_string(&queue).unwrap(), "not a directory");
    }

    #[test]
    fn a_target_page_that_will_not_parse_strands_only_its_own_task() {
        // The likeliest author of unparseable frontmatter is the drain, editing a marker line
        // into it. Failing the classification instead makes `count`, `status` and `prune` all
        // error, so the scheduled drain skips every night and no janitor can clear it.
        let dir = TempDir::new().unwrap();
        let queue_dir = dir.path().join(".lorekeeper/queue");
        std::fs::create_dir_all(dir.path().join("daily/s")).unwrap();
        std::fs::write(
            dir.path().join("daily/s/2026-05-23.md"),
            format!(
                "---\nllm_inputs:\n  summary: {}\ntitle: Notes: today\n---\n\n## 요약\n\nbody\n",
                summary_hash("live")
            ),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("daily/s/2026-05-24.md"),
            format!(
                "---\nllm_inputs:\n  summary: {}\n---\n\n## 요약\n\nbody\n",
                summary_hash("live")
            ),
        )
        .unwrap();
        write_queue_file(
            &queue_dir,
            "run-1.jsonl",
            &[
                task("daily/s/2026-05-23.md", "live"),
                task("daily/s/2026-05-24.md", "live"),
            ],
        );

        let mut statuses: Vec<&str> = read_tasks(&queue_dir.join("run-1.jsonl"))
            .unwrap()
            .tasks
            .iter()
            .map(|t| classify_task(dir.path(), t).unwrap().as_str())
            .collect();
        statuses.sort_unstable();
        assert_eq!(statuses, vec!["current", "unreadable"]);

        // Retained, not pruned: repairing the page brings the task back.
        let summary = prune_queue(dir.path(), &queue_dir, false).unwrap();
        assert_eq!(summary.kept_unreadable, 1);
        assert_eq!(summary.kept_current, 1);
        assert_eq!(
            read_tasks(&queue_dir.join("run-1.jsonl"))
                .unwrap()
                .tasks
                .len(),
            2,
            "neither task is dropped"
        );
    }

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

    fn summary_input(text: &str) -> lk_queue::Identity {
        serde_json::json!({"text": text, "max_sentences": 3, "locale": "ko"})
            .as_object()
            .unwrap()
            .clone()
    }

    fn summary_hash(text: &str) -> String {
        lk_queue::cache_hash(&summary_input(text))
    }

    fn task(vault_path: &str, hash: &str) -> QueueTask {
        QueueTask {
            task_id: "sum-1".into(),
            kind: TaskKind::Summarize,
            created_at: "2026-05-23T10:00:00Z".parse().unwrap(),
            cache_hash: summary_hash(hash),
            input: summary_input(hash),
            target: TaskTarget {
                vault_path: vault_path.into(),
                kind: TargetKind::DailySummary,
                anchor: "## 요약".into(),
            },
        }
    }

    /// One marker answers for both sections a `synthesize-concept` task fills, so the task
    /// is work only while the page can receive BOTH. Checking the synthesis heading alone
    /// had the drain write it, find no relations heading, and stamp the pair answered —
    /// after which `backlinks-sync` re-queues nothing until the citation set moves.
    #[test]
    fn a_synthesis_task_is_not_work_while_the_relations_heading_is_missing() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("wiki/concepts")).unwrap();
        let hash = lk_core::concept::citation_digest(&[]);
        let page = |body: &str| format!("---\nllm_inputs:\n  synthesis: {hash}\n---\n{body}");
        let synthesis_task = |vault_path: &str| QueueTask {
            task_id: "syn-1".into(),
            kind: TaskKind::SynthesizeConcept,
            created_at: "2026-05-23T10:00:00Z".parse().unwrap(),
            cache_hash: hash.clone(),
            input: [
                ("related_anchor".to_string(), "## 관련".into()),
                ("citations".to_string(), serde_json::json!([])),
                ("locale".to_string(), "ko".into()),
            ]
            .into_iter()
            .collect(),
            target: TaskTarget {
                vault_path: vault_path.into(),
                kind: TargetKind::ConceptSynthesis,
                anchor: "## 핵심".into(),
            },
        };

        // Both headings present: work.
        std::fs::write(
            dir.path().join("wiki/concepts/whole.md"),
            page("\n## 핵심\n\nbody\n\n## 관련\n\n"),
        )
        .unwrap();
        assert_eq!(
            classify_task(dir.path(), &synthesis_task("wiki/concepts/whole.md"))
                .unwrap()
                .as_str(),
            "current"
        );

        // The relations heading deleted between the sweep and the drain: not work.
        std::fs::write(
            dir.path().join("wiki/concepts/half.md"),
            page("\n## 핵심\n\nbody\n"),
        )
        .unwrap();
        assert_eq!(
            classify_task(dir.path(), &synthesis_task("wiki/concepts/half.md"))
                .unwrap()
                .as_str(),
            "missing-target"
        );
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
            &format!(
                "id: x\nllm_inputs:\n  summary: {}\n",
                summary_hash("abc123")
            ),
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
            &format!(
                "id: x\nllm_inputs:\n  summary: {}\n",
                summary_hash("NEWHASH")
            ),
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
            &format!("id: x\nllm_inputs:\n  summary: {}\n", summary_hash("live")),
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
                kept_unreadable: 0,
                kept_unparseable: 0,
                kept_blocked: 0,
                retired_done: 0,
                files_rewritten: 1,
                files_deleted: 0,
                files_archived: 0,
            }
        );
        let content = std::fs::read_to_string(&file).unwrap();
        assert_eq!(content.lines().count(), 1);
        let survivor: QueueTask = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(survivor.cache_hash, summary_hash("live"));
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
            format!(
                "---\nid: x\nllm_inputs:\n  summary: {}\n---\n\n## Summary\n\nbody\n",
                summary_hash("abc123")
            ),
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
            format!("---\nid: x\nllm_inputs:\n  summary: {hash}\n  summary_done: {hash}\n---\n\n## Summary\n\nbody\n", hash = summary_hash("newhash")),
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
            &format!(
                "id: x\nllm_inputs:\n  summary: {}\n  summary_done: {}\n",
                summary_hash("abc123"),
                summary_hash("abc123")
            ),
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
            &format!(
                "id: x\nllm_inputs:\n  summary: {}\n  summary_done: {}\n",
                summary_hash("newhash"),
                summary_hash("oldhash")
            ),
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
            &format!(
                "id: x\nllm_inputs:\n  summary: {}\n  summary_done: {}\n",
                summary_hash("live"),
                summary_hash("live")
            ),
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
            &format!(
                "id: x\nllm_inputs:\n  summary: {}\n  summary_done: {}\n",
                summary_hash("live"),
                summary_hash("live")
            ),
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

    /// The rename is the EXCLUSIVE step, so it must come first. Rewriting the pending path
    /// before acquiring the run RECREATES a file a concurrent drain may already have
    /// retired — an atomic write is temp+rename, which creates regardless of what was
    /// there — and the archive would then hold two records for one run.
    ///
    /// Observable without staging a race: when the archive cannot happen at all, the
    /// pending run must be exactly as it was. That also pins what the janitor's idempotence
    /// rests on — a failed archive leaves the run replayable.
    #[test]
    fn a_settled_run_is_acquired_before_any_of_it_is_pruned() {
        let dir = TempDir::new().unwrap();
        let queue_dir = dir.path().join(".lorekeeper").join("queue");
        write_page(
            dir.path(),
            "daily/s/2026-05-23.md",
            &format!(
                "id: x\nllm_inputs:\n  summary: {}\n  summary_done: {}\n",
                summary_hash("live"),
                summary_hash("live")
            ),
        );
        let file = write_queue_file(
            &queue_dir,
            "run-1.jsonl",
            &[
                task("daily/s/2026-05-23.md", "live"), // done
                task("daily/s/2026-05-23.md", "old"),  // stale, so a rewrite is wanted
            ],
        );
        let before = std::fs::read_to_string(&file).unwrap();
        // `processed` as a FILE: the archive cannot even create its directory, so the
        // acquire fails before the rename — the same idiom the quarantine tests use.
        std::fs::write(
            queue_dir.join(lk_queue::PROCESSED_SUBDIR),
            "not a directory",
        )
        .unwrap();

        assert!(prune_queue(dir.path(), &queue_dir, false).is_err());
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            before,
            "a run that was never acquired must not have been pruned"
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
        assert_eq!(
            archive_queue_file(&queue_dir, &file).unwrap(),
            Some(processed.join("run-1.1.jsonl")),
            "it must report where the run landed"
        );
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

        // Already gone: a drain retired it between the scan and here. Reporting no
        // destination is what stops the caller writing back to the pending path, which an
        // atomic write would RECREATE — leaving two archive records for one run.
        assert_eq!(
            archive_queue_file(&queue_dir, &queue_dir.join("never-existed.jsonl"))
                .expect("a vanished source is the outcome this wanted"),
            None
        );
    }

    #[test]
    fn prune_keeps_answered_tasks_in_a_run_that_still_has_work() {
        let dir = TempDir::new().unwrap();
        let queue_dir = dir.path().join(".lorekeeper").join("queue");
        write_page(
            dir.path(),
            "daily/s/2026-05-23.md",
            &format!(
                "id: x\nllm_inputs:\n  summary: {}\n  summary_done: {}\n",
                summary_hash("live"),
                summary_hash("live")
            ),
        );
        write_page(
            dir.path(),
            "daily/s/2026-05-24.md",
            &format!("id: y\nllm_inputs:\n  summary: {}\n", summary_hash("live")),
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

    /// A drain retiring a file between the listing and the read must not abandon every
    /// other file in the directory. `prune` and the drain race by design — 1206709 taught
    /// the drain that side; this is the same race seen from here.
    ///
    /// A dangling symlink reproduces the exact state deterministically: `read_dir` lists it,
    /// the read fails `NotFound`, and the existence probe follows it to nothing — which is
    /// what a file moved out from under the scan looks like.
    #[cfg(unix)]
    #[test]
    fn a_run_retired_mid_scan_does_not_abort_the_whole_prune() {
        let dir = TempDir::new().unwrap();
        let queue_dir = dir.path().join(".lorekeeper").join("queue");
        write_page(
            dir.path(),
            "daily/s/2026-05-23.md",
            &format!("id: x\nllm_inputs:\n  summary: {}\n", summary_hash("live")),
        );
        // `pending_queue_files` sorts by name, so the vanished one is visited first.
        write_queue_file(
            &queue_dir,
            "b-real.jsonl",
            &[
                task("daily/s/2026-05-23.md", "live"),
                task("daily/s/2026-05-99.md", "live"), // missing-target
            ],
        );
        std::os::unix::fs::symlink(
            queue_dir.join("already-archived.jsonl"),
            queue_dir.join("a-gone.jsonl"),
        )
        .unwrap();

        let summary = prune_queue(dir.path(), &queue_dir, false)
            .expect("a file that raced away is not a failure");
        assert_eq!(
            summary.pruned_missing_target, 1,
            "the surviving file must still be pruned"
        );
        assert_eq!(summary.files_rewritten, 1);
    }

    /// A read that fails for any OTHER reason is a real failure and must still surface —
    /// the guard asks whether the file is gone, not whether reading was inconvenient.
    #[test]
    fn a_run_holding_only_unclassifiable_work_is_not_retired() {
        // Retiring is for a run whose every remaining task is ANSWERED. An unreadable task is
        // not answered — it is waiting for a page repair — and `processed/` is where nothing
        // reclassifies it, so archiving would make the repair this status exists for impossible.
        let dir = TempDir::new().unwrap();
        let queue_dir = dir.path().join(".lorekeeper").join("queue");
        std::fs::create_dir_all(dir.path().join("daily/s")).unwrap();
        std::fs::write(
            dir.path().join("daily/s/2026-05-23.md"),
            format!(
                "---\nllm_inputs:\n  summary: {}\ntitle: Notes: today\n---\n\n## 요약\n\nbody\n",
                summary_hash("live")
            ),
        )
        .unwrap();
        write_queue_file(
            &queue_dir,
            "run-1.jsonl",
            &[task("daily/s/2026-05-23.md", "live")],
        );

        let summary = prune_queue(dir.path(), &queue_dir, false).unwrap();
        assert_eq!(summary.kept_unreadable, 1);
        assert_eq!(summary.files_archived, 0, "not retired");
        assert!(
            queue_dir.join("run-1.jsonl").exists(),
            "the file stays pending so a page repair can bring the task back"
        );

        // And the repair does bring it back.
        std::fs::write(
            dir.path().join("daily/s/2026-05-23.md"),
            format!("---\nllm_inputs:\n  summary: {}\ntitle: \"Notes: today\"\n---\n\n## 요약\n\nbody\n", summary_hash("live")),
        )
        .unwrap();
        let tasks = read_tasks(&queue_dir.join("run-1.jsonl")).unwrap().tasks;
        assert_eq!(
            classify_task(dir.path(), &tasks[0]).unwrap(),
            TaskStatus::Current
        );
    }

    #[test]
    fn a_line_nobody_can_parse_keeps_its_own_file_whole_and_blocks_nothing_else() {
        // The old answer was to fail the prune, which took the only janitor down with it: one
        // malformed line and no command could classify the queue again. The line still must not
        // be dropped — nothing can tell what work it named — so its file keeps every line and
        // only its own GC waits, while every other file is pruned as usual.
        let dir = TempDir::new().unwrap();
        let queue_dir = dir.path().join(".lorekeeper").join("queue");
        std::fs::create_dir_all(&queue_dir).unwrap();
        std::fs::write(queue_dir.join("run-1.jsonl"), "{not json\n").unwrap();
        write_page(
            dir.path(),
            "daily/s/2026-05-23.md",
            &format!("llm_inputs:\n  summary: {}\n", summary_hash("gone")),
        );
        write_queue_file(
            &queue_dir,
            "run-2.jsonl",
            &[task("daily/s/2026-05-23.md", "stale-hash")],
        );

        let summary = prune_queue(dir.path(), &queue_dir, false).unwrap();
        assert_eq!(summary.kept_unparseable, 1);
        assert_eq!(summary.pruned_stale, 1, "the healthy file is still pruned");
        assert_eq!(
            std::fs::read_to_string(queue_dir.join("run-1.jsonl")).unwrap(),
            "{not json\n",
            "the unparseable line is never dropped"
        );
    }

    /// A file holding no tasks at all edited no page, so there is nothing to archive — and
    /// leaving it keeps `lore ingest` warning about pending work no session will ever do.
    #[test]
    fn a_task_less_file_is_deleted_rather_than_left_warning_forever() {
        let dir = TempDir::new().unwrap();
        let queue_dir = dir.path().join(".lorekeeper").join("queue");
        let file = write_queue_file(&queue_dir, "run-1.jsonl", &[]);

        let dry = prune_queue(dir.path(), &queue_dir, true).unwrap();
        assert_eq!(dry.files_deleted, 1, "dry-run reports the same retirement");
        assert!(file.exists(), "and deletes nothing");

        let summary = prune_queue(dir.path(), &queue_dir, false).unwrap();
        assert_eq!(summary.files_deleted, 1);
        assert!(!file.exists());
    }

    #[test]
    fn prune_leaves_all_current_file_byte_identical() {
        let dir = TempDir::new().unwrap();
        let queue_dir = dir.path().join(".lorekeeper").join("queue");
        write_page(
            dir.path(),
            "daily/s/2026-05-23.md",
            &format!("id: x\nllm_inputs:\n  summary: {}\n", summary_hash("live")),
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
