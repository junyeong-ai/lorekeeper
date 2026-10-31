# lk-cli

Clap front door and orchestration (`main.rs`, `commands/`). Follow the root
AGENTS.md for shared architecture, output, locks, and mutation rules. Delegate
meaning to the owning crate; do not recreate its parser/status model in a command.

## Configuration and workflow sessions

- `commands/mod.rs` resolves flag/environment config, working-directory config,
  then XDG config; never silently use config.example.yaml. Resolve the selected
  config absolutely; relative vault roots are relative to that config.
- A mutation session's canonical root owns every page and queue path derived for
  publication. Ingestion pins its physical root before building acquisition clients.
- Workflow configuration is loaded and revisioned from one read. Pass its selected
  path and `LORE_CONFIG_REVISION` to children; every CLI config load rejects changed
  bytes or another path. A workflow also pins its original root selector, working
  path and directory identity. Bind parent and child I/O to that working path;
  validate selection before and after waiting/recovery and before host/publication.
  Same-directory aliases remain valid; another directory or incomplete inherited
  context refuses. Root context validates scope, never lease ownership or reentrancy.
  Native jobs stop after drift and retain unfinished work.
  New semantic Work and selection identities exclude only execution call/parallel/consecutive-call/
  screening-packet limits and profile session timeouts. Input/result byte limits,
  extraction policy, model/effort and all other configuration remain semantic inputs.
  The live workflow still pins every configuration byte. Legacy byte-bound manifests
  resume only through an exact current-configuration reconstruction; never retag them
  after an operational change or guess historical settings.
  Optional typed stage overrides choose only host profiles. Keep the originating
  profile selected for Work policy, budgets and child LORE_EXECUTION_PROFILE.
  Resolve the actual node/task kind before each invocation; never route from prompts
  or paths. Independent read waves retain per-request host/timeout/decoder association;
  generation batches require the same resolved profile name. Reuse the shared engine
  adapters and both-engine installation claims. Interactive returned-host telemetry
  is not retained semantic progress or publication proof.
  Revalidate pinned configuration/root and cancellation after awaited host-version
  preflight, before reserving a turn or counting/invoking a semantic call.
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
  Claude grants exact quoted executable spellings and, only for plain ASCII shell
  path characters, the same unquoted path. Never use a wildcard executable prefix,
  wrapper permission or unrestricted Bash to accommodate another spelling.
- `commands/run.rs` owns daily/process/knowledge sequencing; `execution.rs` owns
  host-specific invocation and supervised Unix process groups. The shared
  WorkflowLease covers semantic reads through final publication. Native daily
  acquisition precedes a separate publication-only lease; `--wait` selects waiting
  rather than refusal at that boundary. Release it before the parent semantic lease.
  Never hold a writer claim across collection or workflow waiting. Independent
  recovery stages continue after source failure, while
  dependent period stages stop on failure. Cancellation stops further stages.
  Preserve observed cancellation through cleanup errors and report those errors;
  send group SIGKILL once before reaping a forced foreground child.
  Knowledge --batches is an explicit initial-campaign budget. Each batch uses a fresh
  workflow lease, while configuration revision and cancellation supervision remain pinned
  across the campaign, including the first acquired vault's physical identity.
  Process --batches is a separate queue-only campaign: continue only after typed call-budget
  exhaustion with newly retained observations, screening, staged requests, discovery/action
  receipts or verified publication. Cached refusals and already-complete work add no progress.
  Each checkpoint retains its incomplete outcome; a settled process campaign requires the
  final batch's full checks to pass. Defaults remain one batch and the configured per-batch calls.
  Process classifies final review health through the shared typed reports. Reassessment
  keeps strict completion nonzero without halting budgeted independent queue progress;
  invalid reviews and failed reads remain fatal. Queue settlement alone cannot satisfy
  review completion. Daily retains its strict review checkpoints before period resumption.
  Stop on fatal or unknown state, cancellation, no verified progress,
  or an empty plan. Verified selected audits can advance subsequent budgeted batches despite
  unrelated queue rejections; the campaign remains incomplete/nonzero. Batch-limit success
  does not establish full-corpus coverage. Existing receipts resume verified work.
  Report elapsed job time and agent-invocation time separately, including failures
  and cancellation. Invocation counts and wall time are not tokens or billed cost.
  Native work telemetry retains at most 1024 evidence/screening/finalizer steps and counts
  omissions. Measure existing operations only; unknown/inapplicable fields are null.
  Compact request/decoded-result bytes exclude prompts and host envelopes; tool-enabled native
  result request/response bytes are measured; ordinary interactive audit responses remain unobserved. Revalidation and recording share one measured operation.
  Parallel invocation durations are summed and can exceed elapsed job time.
  Failed structured hosts retain only fixed diagnostic fields, JSON types/sizes and
  recognized status tokens. Keep version preflight, invocation and response phases
  distinct; streamed or discarded stdout has unknown size/shape. Never infer quota,
  retry policy or semantic output from this diagnostic, or copy raw error payloads.
  Queue-planning metrics count local checkpoints and shared synthesis captures,
  separately from later revalidation and host work. `mutation_access` measures
  in-process writer acquisition, including waiting, config validation and recovery;
  queue-planning duration excludes it. Child-command waits are not in that metric.
  Native writer waiting is cancellable, retries only typed contention and validates
  the same-read config pin before recovery. A pre-host cursor wait discards prepared
  requests and replans before invocation; post-host recording reloads fresh inputs.
  Native discovery reads also check the supervised signal flag between enumeration,
  header lines and body chunks. Cancellation discards incomplete captures; it does
  not interrupt an active kernel I/O call or abandon a journal transition.
  Finalization shares one fresh capture for status and preparation in the same local
  checkpoint. Preparation includes that capture; verification measures sealed-result
  checks separately. Never reuse this capture after a host call or writer wait.
  Native consumers own selected semantic reads and guarded publication; the controller
  owns final global reconciliation/checks. Keep this boundary explicit in the host
  instruction and shared skills; standalone skills still perform their own final checks.
  Knowledge jobs give ready audit subjects a turn before global queue draining. Scope
  readiness to the actual subject/evidence/context/review pages, verified physical
  owners, typed unfinished work and staged results. Unknown task/result targets cannot
  be assumed unrelated. Reserve pending first-discovery order under vault→queue claims
  before external audit discovery or assessment, preserving it across failed or
  budget-limited runs without recording completion or consuming deferred class turns.
  During automatic admission, check exact current Work holds only along the available
  pending prefix needed to fill the configured batch. Preserve held order and paid
  bytes; all-held is deferred, while corrupt or unreadable state is fatal. No Work-ID
  shortcut, persistent hold index or capture reuse across a host/wait boundary.
  Recheck readiness under those claims before recording,
  and verify selected receipts after final reconciliation even when unrelated backlog
  leaves the overall job incomplete. Authored pages require no invented done markers.
  Recheck discovered dependencies within the originally selected batch. Known blockers
  defer only their subjects; recompute the ready input union and final host selection.
  Preserve pending order and report the latest checked scope without claiming completion.
  `wiki audit-plan --ready` uses the same readiness owner without reserving work;
  automatic plans skip blockers, while exact selections retain them with bounded
  diagnostics and a complete count for that checked scope.
  Grant each temporary selection manifest's directory explicitly to the selected host;
  keep it alive through the foreground consumer and remove it afterward.
  `commands/work.rs` binds retained input and audit dependencies to resumable derived
  work. Oversized requests use a fresh configured agent per inspection/reduction and
  a bounded finalizer. Unplanned whole-input summarize/refine/themes use one tool-free
  section request, including scoped reviews and generation rules; other small requests
  retain ordinary batching. Complete-envelope capacity defers only that target without
  clipping or rewriting paid Work. `execution.work` limits
  serialized request/result bytes and agent calls per native run, resetting per knowledge
  batch. Exhaustion retains unfinished work and fails explicitly, without clipping evidence.
  Resume current explicit Work plans even when the remaining batch fits whole input;
  validate their exact manifest before using recorded observations.
  Saved section finalizers capture fresh scoped reviews separately from source-only Work;
  guard the complete final request after the host and its context again at apply. Do not
  incorporate reviews into old Work identities or rewrite paid nodes to acquire that scope.
  New work fixes reduction topology from the exact envelope plus each child's authored result
  allowance; every child and its evidence mapping participates. A reduction can retain
  unchanged inherited questions in a self-contained versioned node snapshot. Its authored
  result and position metadata obey the result bound; the snapshot obeys the input bound.
  Full reconstructed questions participate in every parent and finalizer input. Actual
  complete-input overflow defers that Work before invocation, never clips or replans it.
  Persist the policy and
  preserve missing-policy binary plans, node IDs and seals without rewriting them on resume.
  Explicit audit inspection refinement requires a current exact processing action.
  Keep the canonical plan, prior positive outputs and original action bytes unchanged.
  Capture its bounded sidecar and original-action revision with the expanded tree;
  guard that snapshot or absence through recording, completion and historical retention.
  New records bind refinement provenance and compose with retained-question snapshots.
  Use the existing byte-budgeted reduction owner, preserve complete original coverage,
  and refuse refined cross-Work reuse without its complete closure. Never infer splits
  from action prose or retry unchanged holds automatically.
  Native queue extraction/synthesis may finalize a disjoint current frontier covering
  every planned inspection when all observations, evidence and rules fit the existing
  input limit and no processing blocker remains. Its distinct frontier seal never
  marks a missing root complete or rewrites paid nodes. Missing/stale/oversized/blocked
  coverage retains normal reduction; source-only work, sections and audits are excluded.
  Revalidate the exact full request and completion proof after host execution and at apply.
  Corrected observations require a newly generated result; preserve old proposals.
  Authority replacement refuses corruption/dual seals and published work; journal its
  prior final bytes and publication absence in the same mutation.
  Completion requires exact sealed typed results and source/work guards in the canonical
  publication journal. Keep the queue target's guard in its publication edit: a separate
  unchanged guard for the same target would reject the journal's own recovery.
  Prepare ready native nodes and their progress from one captured evaluation. Refresh
  the complete scope after the host returns before recording; added discovery matches
  invalidate work too. Never retain a candidate capture across host execution.
  Queue preparation checks the next exact evidence request's current processing action
  in that checkpoint, after reusable root/frontier completion. A later held node cannot
  suppress an earlier ready request. Keep the pre-host recheck for changed state; do not
  replace complete preparation with a Work-ID hold filter.
  Progressive extraction separates complete source observations, bounded candidate-card
  screening and admitted full-body evidence. All candidate and scoped-review guards remain
  pinned; unknown or deferred decisions block completion, and card-only rejection is not
  a full-body assessment. Continue unrecorded packets even when earlier cards defer.
  A fully recorded ledger with deferred cards holds only that target for explicit
  reassessment; fresh validated state must not become a missing-request failure or
  repeat paid screening. Corrupt ledgers remain fatal. Reused owners must be selected
  and read in full. New computed selections may reuse only an entire equal semantic
  packet under fresh complete inventory/ownership/review guards. Bind ordered cards,
  source context, queries, limits and resolved screening contract; opaque closure
  identities are not semantic input. Preserve saved legacy layout/receipts/contracts.
  Reserve full direct-origin provenance inside the result bound. Validate and guard
  direct same-owner origins at every current use; known reassessment holds dependent
  packets without calls, while missing/corrupt/indirect origins refuse. The latest
  computed-selection head is only a bounded lookup hint. Retention protects all proofs
  for pending/staged owners and removes completed owners together. Source work
  never finishes extraction independently: canonical extraction publication journals its
  exact supporting-consumption receipt with the knowledge edits. Retention validates that
  receipt and preserves live queue/reuse references before removing expired source work.
  `work cost` reports complete candidate/packet counts and compact JSON bytes without
  persisting plans or reading candidate bodies through a host. Optional positive
  `max_screening_batches` defers oversized incomplete ledgers before new screening calls;
  complete ledgers remain usable. No clipping, implicit recipe rewrite, token/price claim
  or completion from capacity admission. Unknown packet costs await source observations.
  An indivisible packet's typed byte-capacity failure defers only its target; malformed
  inputs and corrupt saved manifests remain integrity failures.
  Capture capacity and typed source-observation blockers with their complete inventory and
  source guards for staged proposal rejection too; known incomplete scope defers only its work.
- Native inspection/reduction and screening are structured inference roles
  (`execution/observation.rs`); bounded single-section finalization uses
  `execution/finalization.rs`. Rust selects applicable shared generation rules through
  typed task/target kinds. Extraction/synthesis use tool-enabled final-schema responses;
  audits retain interactive finalization. Native Rust assembles TaskResult/SynthesisResult
  metadata from the exact prepared scope, reserves envelope bytes and owns finish/staging.
  Models return semantic values, needs-action, or extraction-only needs-discovery
  with explicit DiscoveryQuery values. Discovery recovery journals the legacy recipe
  and native request-bound sidecar together under vault→queue claims. Preserve exact
  current Source Work; changed knowledge scope replans without retagging paid nodes.
  Recovery is incomplete; exact repeated requests/cycles defer before another host.
  Retention protects pending task identities and validates recovery sidecars before
  removing them. Never interpret old reason text as a recovery query.
  Do not couple output schema to
  tool prohibition, scrape events/prose or mutate saved Work to adopt a reply.
  Selected action receipts bind the actual batch request. Reconsider action-bearing
  tasks individually after request changes, then suppress unchanged singleton actions;
  never substitute a reconstructed singleton for the original batch's identity.
  Tool-free observation, screening and section roles carry the bounded request through
  stdin and decode bounded stdout. Rust records the selected observation or creates
  the guarded section result. In those roles Claude disables tools/customizations;
  Codex uses a read-only sandbox.
  Never fall back to parsing prose/Markdown when structured output is missing or invalid.
  Evidence observers return only a typed observation body or processing-action reason.
  Rust binds native work/node/input and screening selection/batch/input identities from
  the supervised invocation; reject model-supplied identity fields without adopting or
  repairing them. Fixed schemas and exact reply allowances reserve stored identity bytes.
  Native screening assigns a short `candidate_ref` to each exact card. Validate complete,
  unique reference coverage before mapping decisions to canonical paths; reply order is
  irrelevant. Canonical requests retain full owner/source revisions while the native
  presentation leaves those opaque guard hashes to Rust. Reserve path expansion in the
  reply allowance. Reference-coverage or blank/oversized decision-reason rejection journals
  an exact request/contract-bound receipt, records no decisions and defers only that task;
  unchanged requests spend no calls. Reasons obey compact UTF-8 JSON string bytes,
  including quotes/escapes, not character count. Read existing screening-rejections-v1
  without rewriting them; new receipts use strict typed v2 dispositions. Earlier readers
  refuse v2 rather than ignore it. Changed requests/contracts can be reconsidered. Malformed replies, corrupt
  receipts and failed fresh scope validation remain failures. Standalone full-path
  `select-record`, selection identities and existing paid decisions remain unchanged.
  Native new questions contain only semantic text/evidence; Rust supplies IDs and
  retains inherited active questions unchanged unless an exact local question key is
  explicitly resolved. Native child question tuples separate active keys from closed
  resolution history without expanding canonical inputs. Bind action receipts to the
  exact delivered presentation as well as schema/instructions. Keep canonical Work
  requests, node input revisions and standalone record formats unchanged. New node records
  use `work-output-retained-questions-v1` only when needed: preserve exact question kinds,
  IDs, payloads and ordering, and digest the reconstructed full output. Older readers refuse
  the new record; keep legacy bytes, seals and paid observations untouched. Retained-question
  instructions apply only to reductions with active inherited questions. Bind node-role
  instructions to the native contract without changing canonical input revisions. Reserve the
  native retained-position envelope before the host call; reject actual assembled overflow.
  Fresh owner/currentness validation remains authoritative. Native range/text mapping
  is already verified; workers can copy
  assigned ranges without inventing finer offsets. Every observation concern still needs
  evidence. Actions record no observation, discharge no concerns and defer only their
  Work. Bind the receipt to the complete request and response/instruction contract; skip
  unchanged calls, revalidate before recording and preserve independent parallel results.
  A valid decoded evidence output above its allowance records a typed exact capacity
  rejection through the same fresh Work/node guards. It supplies no observation,
  prevents unchanged paid retries and preserves independent progress. Transport errors,
  malformed responses and corrupt state remain failures; never truncate the reply.
  Deferred audit subjects remain unassessed; verify receipts only for the other selected
  subjects and retain an incomplete overall result. Corrupt or unknown-scope state stops.
  A section `needs-action` response records an exact request-bound action receipt;
  unchanged requests defer before invocation, while independent work continues. Changed
  requests can be reconsidered; valid sealed results restage before checking the receipt.
  Corrupt receipts refuse. Retention validates stored receipt identity without requiring
  historical generation rules to match current ones. Interactive finalizers retain their
  explicit repair and guarded submission capabilities.
  Whole-section action receipts bind exact task/input, captured scope and complete request;
  unchanged actions defer before invocation. Scoped SectionResult values retain an optional
  context revision, revalidated against current reviews at apply. Omission preserves the
  legacy section wire contract. Pending identities protect actions during queue pruning.
- Prepared native selections/finalizers supply exact metadata, admitted evidence/root and
  complete shared contracts through supervised stdin, within the complete serialized
  JSON input bound. Ordinary interactive roles retain their argument prompt. Legacy
  prepared requests and standalone consumers may use private files with bounded
  continuation reads; one tool display is not a complete-read requirement. No transport
  authorizes changed scope or certifies a read.
  Tool-enabled native results are decoded only from Claude terminal structured_output
  or Codex explicit final-message output; complete semantic and assembled-result bounds
  stay separate. Fresh request/root comparison precedes sealing/staging. Native results
  use the shared Markdown body validator before staging. Structurally invalid decoded
  bodies retain exact request-bound actions; malformed responses remain failures.
  Native workers do not run work finish/apply. Standalone record/result contracts,
  inherited prepared finish pins and saved Work remain unchanged. Exact request-bound action receipts
  suppress unchanged native calls; unknown/corrupt state refuses and pending identities
  protect receipts from pruning.
- Native queue scheduling chooses one task per target so publication cannot invalidate
  another simultaneous task on the same page. Whole-input tasks share an evidence-bounded
  batch; oversized work reserves fair inspection/reduction/finalization turns.
  `execution.work.max_consecutive_calls` defaults to 4 and bounds host attempts for an
  admitted exact queue Work or screening phase. Reserve the durable fair turn once;
  continuation requires an in-process exact phase token and freshly prepared inputs.
  Charge every attempted host including parallel peers against the same total budget.
  Publication, changed phase bindings, deferral and writer-wait replanning discard admission;
  restart/cancellation retain the already reserved next target.
  `execution.work.max_parallel` defaults to 1 and bounds simultaneous dependency-ready
  evidence observers and distinct-target candidate screening within remaining calls.
  Compatible roles share a fair contiguous prefix; serial finalizers and interactive
  work remain boundaries. Never filter past those boundaries or reserve skipped targets.
  Interactive roles, finalizers and publication remain serial. Supervise every child
  through exit and cleanup. Carry prepared screening requests only within their local
  pre-host queue checkpoint; any host call or writer wait invalidates their reuse.
  Capture the complete current recipe/review universe per typed view after hosts return,
  sharing discovery across extraction Work and loading each Work once per checkpoint.
  Reevaluate current node dependencies for every serial record; preserve each mutation's
  durable checkpoint and exact page/ledger guards. Never retain a capture across a host.
  Preserve an independent valid response when a peer fails. Never reuse pre-host captures
  for recording. Shared capture timings belong to the first recorded step, not every peer.
  Verify target owners before batching. Defer concept synthesis while its cited origin
  pages have unfinished inputs or publishable pending work; unrelated context is not
  a dependency barrier. Reconcile backlinks after a source publication wave and replan
  before selecting another task. A dependency-only queue remains explicitly incomplete.
  One queue-planning checkpoint captures all ready synthesis inputs against the
  complete graph universe for its selected window; discard that view before host execution.
  Enumerate and validate the complete queue and physical target owners before selecting
  a fair preparation window. Section/progressive windows obey remaining-call and
  parallel bounds, capped at 64; possible interactive batches retain 64 targets even
  when one call remains. Continue bounded preparation across task-kind groups while the
  prepared jobs are compatible reads; stop at validated serial boundaries without skipping targets;
  actual prepared TaskWork remains authoritative, including saved plans. Small tasks also obey the input-byte
  bound. Share discovery traversal, projection and immutable raw bytes within that
  checkpoint while preserving task-local recipes, reviews, guards and identities.
  Unprepared targets remain pending. Empty or deferred windows never prove settlement.
  Planning does not consume cursor turns; reserve actual invocation targets together
  immediately before execution, including every member of a whole-input batch.
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
  Optional `--publication-workflow refuse|wait` fences publication after acquisition,
  leaving source cursor/admission claims and network I/O outside that lease. Native
  daily selects the mode explicitly; ordinary ingest retains its short-writer contract.
  Do not nest the option inside an existing workflow. Dry run ignores both waits.
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
- Manual admission reserves fair pending turns under the writer claim, then releases
  it before collection. Journal exact receipts with actual document/request outputs;
  archives and reservations are not completion. Reuse checks the pipeline-owned recipe,
  full preserved bundle and current target ownership. Resolve owners through the
  pipeline at acquisition, publication and retirement;
  refresh per boundary and retain exact target-byte guards. Filesystem aliases share
  an owner; hard-linked addresses do not. Backlog remains explicitly partial; previews
  never reserve or reset state. Preserve admission state across backup/restore.
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
- `graph repair-link` repairs an explicitly mapped missing concept address in one
  exact concept page. Pin origin and destination file revisions, require canonical
  declared concepts and an absent old address, use shared CommonMark link rewriting,
  and journal target/absence guards with the source edit. Preserve prose, frontmatter
  and completion values; never regenerate semantics or infer mappings from names.
  Typed RSS evidence requires its provenance-aware path. The preview is inert and
  refuses pending journals; the local repair makes no global lint or audit claim.

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
  Authored section-body violations are typed local rejections; target replacement,
  corrupt state and I/O failures remain fatal. Never rewrite invalid prose to fit.
  `queue apply --json` retains typed group identities, exact proposal revisions and
  verified rejection scope. Native orchestration separates safe independent progress
  from aggregate success; retained rejections keep the final job nonzero. Queue-owned
  exact rejection receipts suppress host retries, never deterministic apply or input
  readiness. Changed scope/proposals require revalidation before host admission.
  Unknown/fatal state and abnormal child termination stop; never classify error text.
  Classify retained rejected tasks before capturing current semantic scope. Done,
  stale and missing-target tasks receive no new host admission; deterministic apply
  still owns reapplication or retirement. Unreadable, ambiguous, corrupt and orphaned
  state remains fatal.
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
  Validate the historical completion dependency closure from captured observations
  before retirement. Root, frontier and consumed-source proofs retain their distinct
  coverage; expired valid Work does not require current source-page freshness.

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
Standalone mutators also pin the physical vault. Schema synchronization carries the
original configuration revision and root identity through acquisition and publication;
delegation preserves that context. Selector or directory replacement invalidates it.
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
Create Unix private workspaces without group/other access at mkdir, before payloads;
restore owner permissions afterward when a restrictive umask removed them.
`input install` owns the optional native decoder environment. `input install-vision`
owns separate Paddle layout and MLX recognition environments plus verified models;
their dependencies must not share an environment. Installation is explicit, never
an ingest fallback. Engine platform/version checks belong to the source adapter.
Credential wizard requires a TTY/dialoguer flow; persist shared credentials through
atomic mode0600 writes. Avoid command-specific alternate credential stores.
