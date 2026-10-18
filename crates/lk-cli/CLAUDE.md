# lk-cli

Clap front door and orchestration (`main.rs`, `commands/`). Follow the root
CLAUDE.md for shared architecture, output, locks, and mutation rules. Delegate
meaning to the owning crate; do not recreate its parser/status model in a command.

## Configuration and workflow sessions

- `commands/mod.rs` resolves flag/environment config, working-directory config,
  then XDG config; never silently use config.example.yaml. Resolve the selected
  config absolutely; relative vault roots are relative to that config.
- `commands/workflow.rs` is a Unix foreground coordinator for long LLM consumers,
  not a vault mutation lock. It locks canonical-vault `.lorekeeper/workflow.lock`,
  clears only FD_CLOEXEC, and `exec`s literal argv. Busy defaults to nonzero with no
  child; `--wait` explicitly waits. Pass selected absolute LORE_CONFIG and optional
  LORE_TEMPLATE_DIR to the child via Command.env, not global environment mutation.
- The lease lasts only while a participating process retains the descriptor.
  Detached programs or programs closing inherited descriptors are outside that
  guarantee. No PID/TTL cleanup, env-bool bypass, or reentrancy guess. Windows is
  explicitly unsupported. Ordinary short CLI writers do not reacquire the lease;
  they retain their own locks/revision guards. Workflow does not recover journals.
- Child PATH starts with the selected executable's directory and LORE_BIN names
  that executable. Its canonical `lore` sibling must identify the same file; a
  renamed binary cannot silently delegate skill commands to another installation.
- Scheduled coordination wraps only `claude_skill` in scripts/lore-pipeline.sh,
  including its capture→LLM→apply lifetime. Do not wrap every scheduled writer:
  period intent, reminders, and maintenance retain their own short-lock contracts.
  Manual agents acquire once before reading inputs, not again inside a skill.
- `commands/schedule.rs` emits one all-source ingest plus independent period and
  maintenance jobs. Use absolute installed Bash scripts/executable mode and captured
  PATH/LORE_BIN/LORE_CONFIG/CLAUDE_BIN and explicit LORE_SKILL_DIR. Preserve cron
  versus launchd calendar/interval meaning; reject unsupported translation instead of approximating it. Reminder
  scheduling may use a separate board/pipeline directory.
- Launchd `--namespace` constructs labels and plist names (default `com.lorekeeper`);
  custom namespaces use separate default logs. `--log-dir` is an explicit absolute
  path. These options only render definitions: never register jobs or create log
  directories here. Reject unsafe namespaces before any output.
- Preserve explicitly set standard proxy variables in both formats and direct jobs,
  including casing and empty values. Refuse credential-bearing proxy endpoints
  without printing their values; do not capture arbitrary environment variables.

## Ingest and originals (`commands/ingest.rs`)

- Ingest waits for a cooperating writer only with `--wait-for-vault`; scheduled
  daily collection uses it. Dry run never takes the writer lock.
- Dry run creates no vault files, tmp results, logs, or queue writes; real source
  errors still return nonzero. A noop provider makes no promise to queue work.
- Work-log generation belongs to full ingest only, not filtered runs. A full run
  with partial source failure still writes its work-log and reports failure.
  Startup tmp sweeping/pending diagnostics must preserve recoverable work.
- Manual original publication is immutable bytes→complete manifest→page journal→
  queue flush→optional retirement. Keep source ownership separate from original
  references; archive=false still preserves original bytes. Capture limits are
  source-local plus a 512 MiB CLI aggregate bound, not unbounded whole-inbox memory.
- Keep all capture dependency information, even for events removed by the pipeline.
  Referenced entries are not retired; any capture failure holds that source's
  retirement. Attachments are never removed. Retirement rechecks consumed bytes.
- `fetch` uses the same unique enabled manual-inbox resolver. Preserve title/source
  URL frontmatter and require body. Publish exclusive collision suffixes
  (8/16/full digest then counter), never overwrite an occupied/nonregular path;
  keep filenames within the byte bound.

## Canonical pages and derived discovery (`commands/wiki.rs`, `graph.rs`)

- Markdown is canonical; SQLite is a prepared derived query snapshot. Refresh
  prepares it first. Default navigation is a small marked guide; giant index/log/map
  are opt-in exports. Explicit export/index-sync requests remain explicit even
  when automatic exports are disabled.
- Use captured config to refuse canonical manual-inbox overlap before capture or
  navigation mutation at EVERY export entrypoint. Typed root, symlink, and marked
  child guards remain in force. Never infer legacy ownership from filename/content.
- Refresh's explicit repeated `--archive-replace PATH=FILE_REVISION` accepts exact
  root paths only. Validate full revisions first; one MutationPlan preserves exact
  original archives+receipt and replaces/retires roots plus marked children. Reused
  archive/receipt bytes are guarded too. Authored children remain. Default is strict;
  reject this migration with automatic exports enabled and explain the two steps.
- Browse metadata with query=None; reject query-only/date-sort combinations in the
  library too. Created means declared authored date, not ingestion time. Preserve
  missing/invalid visibility, stable path ties, scope/filter/facet/pagination meaning.
  Search rejects an empty query; limit0 returns no hits. Do not reintroduce full
  corpus metadata decode/sort for SQL browse or load concept vocabulary there.
- Concept filenames are addresses, not necessarily IDs or implicit aliases. Use
  exact public resolution and return the resolved canonical title.
- Evidence/review discovery defaults to read-only JSON. Evidence archive is a writer.
  Open valid reviews and drift needing attention are distinct, not all “resolved.”
  Provenance is metadata-first, bounded and revision-pinned; relation/direction are
  explicit. Discovery must not incidentally prepare a new snapshot.

## Queue processing (`commands/queue/`, `commands/synthesis.rs`)

- The shared typed classifier owns current/stale/done/unreadable decisions. Validate
  input kind/policy, target revisions, and synthesis citation guards. Missing or
  unreadable evidence is not a stale task that can safely disappear.
- Queue counts describe current work, not raw lines. Prune only eligible dead work,
  remove empty files, archive all-done files, preserve unreadable/unparseable data.
  A file with unparseable lines is not rewritable. Use the shared queue reader and
  atomic writer; do not add another UTF-8/JSONL interpretation.
- Extraction result application may ignore a prior done marker while retaining exact
  input/target guards. Already-done section/synthesis tasks can retire normally.
  Stage matching task/result pairs; no orphan application except explicit recovery.
- Hold the vault claim for each apply, not an entire LLM session; vault→queue is the
  lock order. Distinguish a page-local failure, quarantined malformed result, and
  fatal IO. Aggregate output honestly reports all/some/none applied.
- Synthesis owns both sections and its completion marker atomically, with exact
  source/target/queue guards. Do not split core and related publication. Explicit
  year/date conflicts with `--previous` fail before flushing work.
- Backlinks queues synthesis only with an answering provider; noop neither changes
  that promise nor fabricates input. Reuse exact pending requests through lk-queue.
- `queue check` is read-only: report current/unreadable/unparseable tasks,
  unpublished results, and pending journals without recovery. Final checks follow
  backlink reconciliation, not precede it.
- Document identity adoption compares the production projection and guarded current
  observation, including full commit refs, blob/document identity, manifest, and
  remote ownership. A blocked mismatch is not adoption. Archival source identity
  does not itself decide lifecycle state.

## Task intent (`commands/task.rs`, delegated to lk-task)

- Reconcile→requested change→commit; recheck parsed board bytes before history,
  history before board, reminder retirement after successful publication best-effort.
  Read-only agenda/brief must not mutate. Hold PlaneLock for plane writes only.
- Parse yesterday/today/tomorrow in the vault timezone with command-specific bounds;
  a close targets an ended day. Board state, completion history, carried_too_long,
  schedules, and reminder status have separate meanings, not one generic “done.”
- JSON preserves unplaced tasks, unwritable diagnostics, timezone and completion
  information. Unavailable stores are null, not empty; incomplete parsing cannot
  prove a task absent. `status` delegates each subsystem's state to its owner.
- Maintenance reads store lifetime declarations. Keep newest collected source
  currency even when old, and preserve raw stream logs. Never prune another live
  worker's tmp directory or treat a disabled source as removed.

## Original query authority

- `wiki original` is metadata-first with explicit member totals/limit/next_offset
  and bounded 64 KiB UTF-8 previews. It does not launch a browser or fall back to the
  mutable inbox. Verification scope must be clear even when only entry bytes are
  checked by the default view.
- RSS selection pins collection revision and EventId; paginate in Unicode-safe
  units. Published/updated is not acquisition time (`captured_at` may be null).
  Root/explicit-root commands need no unrelated config discovery.
- `legacy_unbound` is current retained evidence, not proof of historical page/input
  binding. Never promote it to exact provider-wire/acquisition provenance.
- Typed event tasks validate selected membership and payload: removed pair is stale,
  corrupt selection unreadable. Packetv2 carries selection argv; preserve oldv1
  semantics. Normalize/merge/repoint typed declarations together with links.
  Source counts count origin pages, not selected events.
- GeneratedOnly pair retraction can remove generated occurrences/declarations while
  authored prose still supplies page evidence. Normal whole-link removal remains
  strict; do not use partial declarations to erase authored text.

## Installation and credentials

`commands/installation.rs` performs artifact/schema synchronization using the NEW
binary and refuses unsafe update over pending queue work. Uninstall never removes
user vaults. Delegate byte coherence/target/release validation to lk-dist.
Credential wizard requires a TTY/dialoguer flow; persist shared credentials through
atomic mode0600 writes. Avoid command-specific alternate credential stores.
