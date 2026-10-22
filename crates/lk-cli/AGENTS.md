# lk-cli

Clap front door and orchestration (`main.rs`, `commands/`). Follow the root
AGENTS.md for shared architecture, output, locks, and mutation rules. Delegate
meaning to the owning crate; do not recreate its parser/status model in a command.

## Configuration and workflow sessions

- `commands/mod.rs` resolves flag/environment config, working-directory config,
  then XDG config; never silently use config.example.yaml. Resolve the selected
  config absolutely; relative vault roots are relative to that config.
- Workflow configuration is loaded and revisioned from one read. Pass its selected
  path and `LORE_CONFIG_REVISION` to children; every CLI config load rejects changed
  bytes or another path. Native jobs stop after drift and retain unfinished work.
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
- `commands/run.rs` owns daily/process/knowledge sequencing; `execution.rs` owns
  host-specific invocation and supervised Unix process groups. The shared
  WorkflowLease covers semantic reads through final publication. Collection precedes
  that lease; independent recovery stages continue after source failure, while
  dependent period stages stop on failure. Cancellation stops further stages.
  Preserve observed cancellation through cleanup errors and report those errors;
  send group SIGKILL once before reaping a forced foreground child.
  Knowledge --batches is an explicit initial-campaign budget. Each batch uses a fresh
  workflow lease, while configuration revision and cancellation supervision remain pinned
  across the campaign. Stop on failure/cancellation or an empty plan; batch-limit success
  does not establish full-corpus coverage. Existing receipts resume verified work.
  Report elapsed job time and agent-invocation time separately, including failures
  and cancellation. Invocation counts and wall time are not tokens or billed cost.
  Native consumers own selected semantic reads and guarded publication; the controller
  owns final global reconciliation/checks. Keep this boundary explicit in the host
  instruction and shared skills; standalone skills still perform their own final checks.
  Knowledge jobs give ready audit subjects a turn before global queue draining. Scope
  readiness to the actual subject/evidence/context/review pages, verified physical
  owners, typed unfinished work and staged results. Unknown task/result targets cannot
  be assumed unrelated. Reserve pending first-discovery order under vault→queue claims
  before external audit discovery or assessment, preserving it across failed or
  budget-limited runs without recording completion or consuming deferred class turns.
  Recheck readiness under those claims before recording,
  and verify selected receipts after final reconciliation even when unrelated backlog
  leaves the overall job incomplete. Authored pages require no invented done markers.
  Grant each temporary selection manifest's directory explicitly to the selected host;
  keep it alive through the foreground consumer and remove it afterward.
  `commands/work.rs` binds retained input and audit dependencies to resumable derived
  work. Oversized requests use a fresh configured agent per inspection/reduction and
  a bounded finalizer; small requests retain ordinary batching. `execution.work` limits
  serialized request/result bytes and agent calls per native run, resetting per knowledge
  batch. Exhaustion retains unfinished work and fails explicitly, without clipping evidence.
  Completion requires exact sealed typed results and source/work guards in the canonical
  publication journal. Keep the queue target's guard in its publication edit: a separate
  unchanged guard for the same target would reject the journal's own recovery.
- Native inspection/reduction is a structured inference role (`execution/observation.rs`):
  stdin carries only the bounded node request and instructions, stdout is bounded and
  decoded by the host's explicit response contract. Rust owns recording the selected
  observation. Claude disables tools/customizations; Codex uses a read-only sandbox.
  Never fall back to parsing prose/Markdown when structured output is missing or invalid.
  This restriction does not describe the interactive finalizer/maintenance role.
- Native queue scheduling chooses one task per target so publication cannot invalidate
  another simultaneous task on the same page. Whole-input tasks share an evidence-bounded
  batch; oversized work gets one inspection/reduction/finalization turn at a time.
  `.lorekeeper/execution-cursor.json` stores the next queue/audit target under a guarded
  short mutation, preserving turns across budget-limited runs. Reserve before external
  execution, never infer priority or semantic quality from size/progress/citation counts.
- `commands/schedule.rs` renders native jobs with stable scheduling-slot labels.
  Queue-mode semantic jobs need explicit execution profiles. Capture selected
  absolute config/data paths, anchored PATH and relevant existing host directories;
  never capture credentials. Rendering never authenticates or registers jobs.
- Launchd `--namespace` constructs labels and plist names (default `com.lorekeeper`);
  custom namespaces use separate default logs. `--log-dir` is an explicit absolute
  path. These options only render definitions: never register jobs or create log
  directories here. Reject unsafe namespaces before any output.
- Preserve explicitly set standard proxy variables in both formats and direct jobs,
  including casing and empty values. Refuse credential-bearing proxy endpoints
  without printing their values; do not capture arbitrary environment variables.

## Ingest and originals (`commands/ingest.rs`)

- Full current-day ingest reserves a persistent first-source turn under a short
  mutation session before collection; recover journals and recheck pinned config before
  reading that cursor. Release the claim before source I/O. Acquisition and planning
  share the reserved order; accepted publication stays canonical. Filtered/historical
  runs leave the turn unchanged; previews neither reserve nor recover.
  Ingest captures observations without holding the writer lock. After acquisition,
  recover journals and derive page plans against current pages; never refresh a
  previously captured publication baseline. Pin the selected config path/revision
  across collection, lock waiting and planning. `ingest.source_timeout_seconds`
  applies independently per source; source `collection_timeout_seconds` overrides
  it. Deadlines include retries/pagination/conversion, cancel async waits and reject
  over-budget blocking returns; they are not OS preemption or source completeness.
  `--wait-for-vault` permits publication waiting; dry run never takes that lock.
- Dry run publishes no pages, receipts, cursors, logs or queue writes; real source
  errors still return nonzero. Authentication may persist a required rotated OAuth
  successor. A noop provider makes no promise to queue work.
  Document projection caches are read-only during dry run and input inspection;
  real collection may checkpoint verified units under source-owned cache claims.
- Work-log generation belongs to full ingest only, not filtered runs. A full run
  with partial source failure still writes its work-log and reports failure.
  Startup tmp sweeping/pending diagnostics must preserve recoverable work.
- Collection receipts bind only actually published work-log input hashes and
  normalized per-source personal contributions. Period readiness compares those
  inputs, including ownership and resolved categories; semantic prose edits are
  not new collection evidence. Known contributions without a log block. Missing
  legacy bindings are not adopted, and unknown history is not observed empty.
- Historical date replay never replaces current candidate or appointment snapshots.
  Source recovery needs an explicit start date and per-source day budget. Only
  config-matching observed windows with the same source-observed collection identity,
  closed before acquisition and durably published,
  establish query-day coverage; current snapshots and configured queries remain
  distinct from historical replay. Coverage is not deleted/revision-history proof.
  Resolve current identities per source before replay selection; unavailable identities
  block only that source, while legacy unbound receipts remain unverified. Recovery
  previews may authenticate but never collect content or change canonical state;
  rotating OAuth successors still require durable credential persistence.
- Manual original publication is immutable bytes→complete manifest→page/request
  journal→optional retirement. Keep source ownership separate from original
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

- Markdown is canonical; SQLite is the prepared derived query snapshot. Refresh
  validates the small owned entry guide, prepares SQLite, then publishes that guide.
  There are no Markdown catalog/log/map exporters, archive-replace migration, or
  graph index-currency checks. Existing authored files are not retired by refresh.
- Use captured config to refuse canonical manual-inbox overlap before capture or
  navigation mutation. Guide ownership, symlink, nonregular-file and authored
  collision guards remain strict; never infer ownership from an old export name.
- Browse metadata with query=None; reject query-only/date-sort combinations in the
  library too. Declared-created bounds compose with queries; created sorting is browse-only.
  Exact repository/provider scopes match declared ownership, not concept evidence membership.
  Created means declared authored date, not ingestion time. Preserve
  missing/invalid visibility, stable path ties, scope/filter/facet/pagination meaning.
  Search rejects an empty query and a zero limit. Do not reintroduce full
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
  Stage matching task/result pairs; missing retained input refuses application.
  Extraction and synthesis pin the worker's full read scope. Extraction also
  carries explicit discovery recipes and new/reuse admission. Native oversized
  extraction records discovery before planning bounded evidence work; discovery
  never marks extraction complete. Retire successful recipes in the publication
  journal; prune only owned valid recipes with no retained pending identity.
  Compose batch read guards against the actual publication set and matching original
  bytes; owned edits guard themselves during recovery. Distinct publication addresses
  resolving to one entry refuse before page writes or journaling, including in dry runs.
  Use `MutationPlan::validate_addresses` for publication previews; it removes private
  directory-only probes after checking missing names on the destination filesystem.
- Hold the vault claim for each apply, not an entire LLM session; vault→queue is the
  lock order. Distinguish a page-local failure, quarantined malformed result, and
  fatal IO. Aggregate output honestly reports all/some/none applied.
- Synthesis owns both sections and its completion marker atomically, with exact
  source/target/queue guards. Do not split core and related publication. Explicit
  year/date conflicts with `--previous` fail before flushing work.
- Optional synthesis recovery registers only closed periods starting on/after its
  explicit activation date. Persist per-kind cursors and fair turns in the existing
  intent store; bound examined slots, including existing requests. Disabled modes
  pause without losing their cursor. Registration never supplies missing collection
  coverage, semantic completion or a cross-source personal work log.
- Backlinks queues synthesis only with an answering provider; noop neither changes
  that promise nor fabricates input. Reuse exact pending requests through lk-queue.
- `queue check` is read-only: report current/unreadable/unparseable tasks,
  unpublished results, and pending journals without recovery. Final checks follow
  backlink reconciliation, not precede it.
- Document identity adoption compares the production projection and guarded current
  observation, including full commit refs, blob/document identity, manifest, and
  remote ownership. A blocked mismatch is not adoption. Archival source identity
  does not itself decide lifecycle state.
  Current and historical Content proofs use the same Markdown fragment renderer;
  do not approximate prior lossy formatting or infer provenance from similar text.

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
  Bounded-work retention captures pending requests, staged results and transitive
  reuse references under workflow→mutation→queue claims. Unrelated valid pending
  work does not block expired published work; unknown references refuse cleanup.

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
Unix dispatch retains shared installation claims on absolute selected destinations;
schedule rendering creates no files and does not acquire installation claims.
Mutators exclusively claim changed resources and the selected vault workflow before
writing. Native runs retain claims across collection and batch gaps. Pin the same
Installation and data directory in children rather than rediscovering mutable records.
Native project-skill claims use the vault CWD, not the invoking directory. Reacquire
that scope before the first host invocation so a vault created by Daily collection
is protected too; retain it through the rest of that run.
Downloads occur without exclusive claims; recheck selection, binary bytes and queue
under protection before replacement. The final mutation claim excludes writers but
never recovers a pending journal. Drop it and the queue claim before delegated schema
publication. New-binary deployment receives actual exclusive descriptors through a
Unix socket, verifies their lock-file identities and never unlocks inherited claims.
An environment marker alone grants no ownership. `--force` never bypasses active
claims. Bootstrap/manual replacement requires all shared users to be quiescent.
`input install` owns the optional native decoder environment. `input install-vision`
owns separate Paddle layout and MLX recognition environments plus verified models;
their dependencies must not share an environment. Installation is explicit, never
an ingest fallback. Engine platform/version checks belong to the source adapter.
Credential wizard requires a TTY/dialoguer flow; persist shared credentials through
atomic mode0600 writes. Avoid command-specific alternate credential stores.
