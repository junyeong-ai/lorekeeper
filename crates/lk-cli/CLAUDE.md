# lk-cli

The `lore` binary. `main.rs` defines the clap surface; `commands/` has one module per
subcommand; `commands/mod.rs` holds shared helpers (`find_config`, `load_config`,
`build_llm_client`, `parse_date`).

- **Global flags** `--config`/`LORE_CONFIG` and `--template-dir`/`LORE_TEMPLATE_DIR` are
  injected into the cron lines `lore schedule` generates, so scheduled runs don't depend on
  CWD. Templates are embedded in the binary by default; `--template-dir` overrides them.
- **`find_config`** resolves in order: `--config`/`LORE_CONFIG` → `./config.yaml` →
  `~/.config/lorekeeper/config.yaml` (XDG). The XDG path is what makes a binary-only install
  (no repo) work. There is no `config.example.yaml` fallback — running with the example's
  placeholder values silently is a footgun. A vault-relative config can't be auto-found
  (the vault path lives *inside* the config). It `canonicalize`s the located path at this
  single I/O boundary, so the returned path is always absolute: `Config::load` then resolves
  a relative `vault.root` against an absolute config dir, making `vault.root` — and every
  path derived from it in every crate — CWD-independent by construction (lk-core needs no
  `current_dir`).
- **`build_llm_client`** maps `llm.provider` to a client: `queue` buffers pending tasks
  to `<vault>/.lorekeeper/queue/*.jsonl` for `/lore-process` to fill via the LLM; `noop`
  discards them (dry-run, CI, or ingest-only runs with no LLM work).
- **`lore ingest`** owns the ingest phase flow and the exit code: any source/extract/pipeline
  failure (`had_failure`) or write/flush failure returns non-zero — including under
  `--dry-run`. Dry-run plans normally but skips every vault write, the tmp sweep, and the
  ingest log, so it leaves the vault untouched.
- **Work-log gate**: the work-log renders only on a FULL ingest (`source.is_none()`) — a
  filtered `lore ingest <id>` sees a structural subset of personal events and must never
  rewrite the cross-source page (it prints why instead). A transient source failure inside
  a full run still writes: the failure is loud (non-zero exit) and the next full run
  re-renders the page complete, while blocking on `had_failure` would freeze the work-log
  on any persistently failing source, news feeds included.
- **`lore schedule`** emits ONE all-source entry from `ingest.schedule` — never per source —
  plus synthesis/maintenance/queue-prune entries from their own keys. `build_jobs` is the
  testable job list; `--pipeline-dir` swaps the ingest and weekly-synthesis entries for the
  installed `lore-daily.sh`/`lore-weekly.sh` (those subcommands are only the pipelines' first
  stage — the drain and `queue apply` live in the scripts), and `pipeline_env` attaches the
  `PATH`/`LORE_BIN`/`LORE_CONFIG`/`CLAUDE_BIN` a scheduler does not provide, inherited from
  the session this command runs in.
- **`lore ingest` startup** sweeps stale `*.jsonl.tmp` from crashed runs and
  warns if pending queue files exist (run `/lore-process` first to avoid duplicate
  LLM work).
- **`lore maintenance`** prunes the ingest log and drained `queue/processed/` files past
  `maintenance.retention_days` — operational history only, and `--dry-run` reports the same
  counts without deleting. **It keeps each source's newest COLLECTED entry whatever its age**:
  the log is both a history and the store `IngestLog::find_last_collection` reads, so
  pruning that line would prune STATE. It must be the newest *collected* one, not the newest
  overall — a source failing daily has a recent `failed` entry the reader skips and a much
  older success it actually returns, so protecting the newest line protects the wrong one
  and lets the success age out. Either way the source then stops reading `stale` (non-zero)
  to read "never ingested" (silent without `--strict`) — the health signal going quiet as
  the outage ages. The
  streaming
  `events/{source}/{date}.jsonl` logs are the permanent raw layer (`lore ingest --date
  <past>` re-projects any day from them) and are NEVER pruned. It also never touches
  live `*.jsonl.tmp` — the ingest startup sweep does. With `maintenance.schedule` set,
  `lore schedule` emits crontab lines for it and for `lore queue prune`, so both
  janitors run unattended.
- **`lore synthesis <period>`** rejects `--date`/`--year` together with `--previous`
  (clap `conflicts_with`), and flushes the LLM queue after writing its pages.
- **`lore queue status`** (`commands/queue.rs`) is the authoritative task guard, and it asks
  TWO independent questions of the target page: is it still the page this task was made for
  (`llm_inputs.<key>` vs `task.cache_hash`; `missing-target` when the page is gone, `stale`
  when the hash moved on), and has this exact input already been answered
  (`llm_inputs.<key>_done` → `done`), and — for work still to be written — does the page
  still carry the SECTION it names (`missing-target` when not) — and, when the page's
  frontmatter will not parse, neither question can be asked, which is `unreadable`: not a
  judgment but the absence of one, so it counts as work LEFT and waits for the page to be
  repaired. That third question closes a
  permanent-failure hole: a `vault.locale` switch re-renders every heading while leaving the
  concept input hash untouched (locale is not in that cache identity), so a task or result
  queued before the switch stayed hash-current while naming a section that no longer exists,
  failing `queue apply` — and the whole scheduled pipeline — every run, with `prune`
  classifying only TASKS and so unable to clear a result. Dropping loses nothing: the page
  carries no completion marker, so the next ingest re-enqueues the work under the heading it
  now has, the same self-healing an unparseable result file relies on. A task passing the
  first, failing the second and having somewhere to land is `current` — the only status that
  is WORK. **A RESULT never asks the completion question** (`Artifact::Result`): a task is a
  REQUEST, so an answered section makes it redundant, while a result IS the answer in
  flight, and for concepts the marker is stamped by the very edit that writes them. Asking
  it would discard the value about to answer the section — silently and forever, since the
  marker then keeps the empty section looking cached. Ignoring it is safe as well as
  necessary: re-applying reproduces the page (`accumulate_concepts` dedups by id, the
  concept merge preserves). `/lore-process` consumes `--json` and acts on
  those alone; the check lives here in tested Rust, never re-derived in skill prose.
  **`lore queue count` prints only that work count**, because it exists to tell a scheduled
  script whether to spend an LLM session, and a queue of already-answered tasks is not a
  reason to spend one.
- **`lore queue prune`** applies the same classification destructively, leaving the pending
  queue holding only work. Dead tasks (`stale`, `missing-target`) are dropped — exactly what
  `/lore-process` would discard without editing. A `done` task is KEPT, so the record a
  drain archives stays whole; but a run whose every remaining task is `done` needs no
  session, so nothing would ever archive it and `lore ingest` would warn about pending work
  forever — it is retired to `processed/` as it stands, the same retirement a drain performs
  on a finished run. An `unreadable` task counts as work LEFT, so its run is never retired:
  archiving it would put it where nothing reclassifies it, and the page repair this status
  waits for could never bring it back. A file left holding nothing was all dead tasks, so it
  never edited a page and is deleted rather than archived. Rewrites go through
  `lk_queue::write_tasks_atomic` (the one writer for queue files); a file needing no change
  stays byte-identical. `--dry-run` reports the same counts with zero writes.
  A file holding a LINE nobody can parse is never rewritten — that would drop work nothing
  could classify — so it is reported as `unparseable` with its tasks counted as `blocked`,
  deliberately not under a classification prune did not act on. Either that or an
  `unreadable` task makes prune exit 1 and its `--json` envelope's `ok` false: both leave
  work nothing can drain, both need a human, and the janitor is the only place either
  surfaces (`queue count` omits them so a session is never spent on work no session can do).
- **`lore queue apply`** materializes the concept extractions a drain wrote to
  `queue/results/*.json` through the same `ConceptDrafts` merge the ingest path uses, then
  deletes each result it consumed — so an empty `results/` after a pipeline run is evidence
  of success, not of a drain that produced nothing. A result file that fails to PARSE is not
  fatal and is not left to be retried forever: it moves to `results/{CORRUPT_SUBDIR}/` via
  `quarantine`, under a name `free_path` guarantees is unused so an earlier quarantined file
  is never overwritten, and `finish_apply` reports all/some/none distinctly. I/O errors
  still fail the command — only malformed CONTENT is quarantined, because a disk that
  cannot be read is not a corrupt payload.
- **`lore init credentials`** (in `init.rs`) is the interactive credential wizard. UX
  (dialoguer prompts, masked secrets, TTY guard) lives here; the JSON shape + atomic
  `0600` write live in `lk_source::credentials` (`load_file`/`load`/`save`). The Google branch
  can mint a refresh token via `lk_source::build_google_refresh_token` (browser OAuth
  loopback) or accept a pasted one. `Init` is a subcommand-of-subcommand so future
  scaffolders (`lore init config`) slot in cleanly.
