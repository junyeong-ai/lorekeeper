---
paths:
  - "crates/lk-cli/src/commands/run.rs"
  - "crates/lk-cli/src/commands/run/**"
  - "crates/lk-cli/src/commands/execution.rs"
  - "crates/lk-cli/src/commands/execution/**"
---

## Native runs (`commands/run.rs`, `execution.rs`)

- Audit captures require current candidate decisions. Synthesis loads its inputs through
  `lk_graph::audit::load_synthesis_inputs_controlled`, which reads only each concept's own
  assessment and discovery recipe and never the selection store: a defect there holds that
  target with its reason, and any other audit record stays the audit's error; audit readiness cannot block its
  prerequisite queue tasks. Existing Work compares fresh subject/input
  identity before ready-only audit capture so valid scope drift remains a typed replan.
  Deferral counts track encountered subject paths independently of bounded details.
  Selected replans replace those subjects while retaining other observed holds;
  final coverage checks add current readiness observations without clearing earlier holds.
  Final completion validates canonical assessments and readiness from the same captured
  candidates, not prepublication Work identities or a second plan. Metadata-only edits
  preserve current assessments; changed semantic
  inputs or unfinished dependencies defer their subjects without pausing the operation.

- Bounded native finalizers receive RSS article selectors separately from semantic
  observations. Match the retained manifest revision and every event's authority,
  address and revision to the current Work before supplying resource-indexed
  bindings. Count these bytes in admission. Neither a binding nor roster membership
  establishes support; unbound legacy inputs never gain original authority.

- `commands/run.rs` owns daily/process/knowledge sequencing; `execution.rs` owns
  host-specific invocation and supervised Unix process groups. The session deadline and
  cancellation bound a child only until it exits; its answer is then complete, and cleanup
  is bounded on its own: its pipes get `CLEANUP_GRACE`, then the group is killed so a
  descendant releases them, then one more grace. Stdout still open after that fails only
  where stdout carries the answer. The shared WorkflowLease
  covers semantic reads through final publication. Native daily acquisition precedes a
  separate publication-only lease; `--wait` selects waiting rather than refusal at that
  boundary. Release it before the parent semantic lease. Never hold a writer claim
  across collection or workflow waiting.
- Independent recovery stages continue after source failure, while dependent period
  stages stop on failure. Cancellation stops further stages. Preserve observed
  cancellation through cleanup errors and report those errors; send group SIGKILL once
  before reaping a forced foreground child.
- Knowledge --batches is an explicit initial-campaign budget. Each batch uses a fresh
  workflow lease, while configuration revision and cancellation supervision remain
  pinned across the campaign, including the first acquired vault's physical identity.
  Process --batches is a separate queue-only campaign: continue only after typed
  call-budget exhaustion with newly retained observations, screening, staged requests,
  discovery/action receipts or verified publication. Cached refusals and
  already-complete work add no progress. Each checkpoint retains its incomplete outcome;
  a settled process campaign requires the final batch's full checks to pass. Defaults
  remain one batch and the configured per-batch calls for manual runs. Scheduled
  process/knowledge campaigns continue within their single cumulative automation budget.
- Scheduled process/knowledge runs may yield an owned workflow after new retained
  progress when admission is contended and the current turn has held it for
  `execution.automation.yield_after_seconds`; each acquisition starts a new turn, so
  handoff preparation cannot consume every turn. Settle every launched peer before checking
  contention; never yield from inside a paid recording wave. Probe admission only,
  leaving the owned workflow lock intact until the checkpoint returns. A yielded
  checkpoint is incomplete, not a failure or budget reset. Preserve genuine failed
  stages in the campaign outcome. Discard captures and revalidate pinned configuration,
  root and current inputs after reacquisition. Intermediate checkpoints skip global
  completion tails but verify any recorded selected audit receipts. Uncontended runs
  keep their existing checks. Cached holds cannot cause zero-progress handoff loops.
  Original-subject replan allowances also survive checkpoints and proven merges;
  inherited budget groups never resolve subjects or authorize captured-input reuse.
- A newly retained semantic audit action or other verified progress can advance the
  next knowledge batch while the overall result stays incomplete. Operational
  rejections and cached refusals earn no progress and cannot sustain continuation.
- Daily and process classify review health through the shared typed reports.
  Reassessment keeps strict completion nonzero without halting independent queue
  progress or pausing automation; invalid reviews and failed reads remain fatal.
  Backlink reconciliation and graph lint run inside the run through the shared graph
  owners and the run's read control; reconciliation also requests the resynthesis of a
  concept whose recorded open reviews changed since its synthesis (cli-native-results.md). A concept page reconciliation skips and a lint
  violation name their pages: they keep completion nonzero without halting or pausing.
  Failed reads, publication conflicts and checks that could not complete stay fatal.
  Navigation refresh stays a child process so cancellation can stop its capture; its
  capture-changed exit (`wiki refresh`, 75) records the stage failed and continues
  without halting or pausing, keeping the previous snapshot for a later run.
  Their stage records carry the counts and the first skipped pages; `lore graph lint`
  and `lore graph backlinks-sync --dry-run` report the rest.
  Queue retirement runs inside the run under its own writer claim, after the first
  reconciliation and before planning, through the shared prune owner, so planning reads
  only work still owed. It retires pending rows only, leaving sidecar retention to
  maintenance; a retirement that cannot complete keeps completion nonzero without
  halting or pausing, and unclassifiable rows stay for the final queue check.
  Queue settlement alone cannot satisfy review completion. Daily requires a passing
  review check before period resumption and checks again after publication.
- Stop on fatal or unknown state, cancellation, no verified progress, or an empty plan.
  An input pipe closed by the child may retain a natural numeric nonzero exit only
  after complete stdout draining, reaping and group cleanup. Closed input with exit zero
  or an unsettled deadline remains an I/O failure; it never becomes success or an automatic retry.
  Verified selected audits can advance subsequent budgeted batches despite unrelated
  queue rejections; the campaign remains incomplete/nonzero. Batch-limit success does
  not establish full-corpus coverage. Existing receipts resume verified work.
- Scheduled runs discard host invocations' stderr and uncaptured stdout
  (`ChildOutput::Discard`): a host transcript repeats its request, source text included,
  and the typed report is the unattended record. Lore stage commands always forward
  theirs, so a job log keeps their progress, errors and panics; attended runs forward
  host output too. Every spawn passes its `ChildOutput`; the supervisor holds no policy.
- Report elapsed job time and agent-invocation time separately, including failures and
  cancellation. Invocation counts and wall time are not tokens or billed cost. Native
  work telemetry retains at most 1024 evidence/screening/finalizer steps and counts
  omissions. Measure existing operations only; unknown/inapplicable fields are null.
  Queue-planning `finalization_admissions` records typed outcomes from that same
  captured preparation, separately bounded with an omitted count. These checks are
  neither host calls nor retained progress; keep `work_steps` and campaign decisions
  unchanged. Do not recapture inputs just to explain admission.
  For native frontier input capacity, partition the same compact request into
  disjoint value bytes and remaining metadata/envelope; count canonical completion
  bytes separately. This diagnostic never changes the native request or receipt key.
  Compact request/decoded-result bytes exclude prompts and host envelopes; tool-enabled
  native result request/response bytes are measured. Revalidation and recording share
  one measured operation.
  Parallel invocation durations are summed and can exceed elapsed job time. Failed
  structured hosts retain only fixed diagnostic fields, JSON types/sizes and recognized
  status tokens. Keep version preflight, invocation and response phases distinct;
  streamed or discarded stdout has unknown size/shape. Never infer quota, retry policy
  or semantic output from this diagnostic, or copy raw error payloads. Queue-planning
  metrics count local checkpoints and shared synthesis captures, separately from later
  revalidation and host work. `mutation_access` measures in-process writer acquisition,
  including waiting, config validation and recovery; queue-planning duration excludes
  it. Child-command waits are not in that metric.
- Scheduled runs share `execution.automation`'s UTC-day invocation allowance in the
  physical vault. Its required daily cap may be null for unlimited daily usage;
  per-run calls, duration, pauses and durable accounting still apply. Cap changes
  never reset usage or pauses. Under an acquired workflow lease, synchronously reserve
  complete waves in the atomic operational usage ledger
  (`.lorekeeper/automation-usage.json`) before spawning; a prepared but unlocked
  lease grants no ownership. Never refund ambiguous launches, reset usage on config
  changes, recover corrupt accounting as zero, or detach its writer. This ledger is
  independent of canonical mutation journals; page, Work and cursor writes still use
  `MutationSession`. Execution duration starts with the first owned workflow and
  excludes later controller workflow acquisition waits. Those waits have a separate
  configured bound; source collection, ingest publication and short writer waits do
  not use that allowance. Neither allowance renews across batches. Duration limits stop
  new calls, while returned paid results remain recordable. Fatal scheduled failures
  pause that operation: the `automation-usage-v2` ledger records the first halting stage
  and at most 1024 bytes of its detail, while the job report keeps the full detail. An
  `automation-usage-v1` ledger decodes with cause-less pauses and is written back as v2.
  Explicit `--resume` clears the pause without clearing usage or semantic holds.
  Before workflow waiting, a read-only ledger preflight may refuse a proven operation
  pause under a validated physical-root pin. It neither reserves calls nor clears pauses;
  absence is not admission. Resume and authoritative admission still require the lease.
  Confirmed session timeouts, numeric nonzero exits and unusable remote responses from
  tool-free Inspect/Reduce hold only that node's exact input across instruction-contract
  changes. Attempts persist separately from observations and semantic actions through the
  mutation journal; they are not retained semantic progress. Timeout counts written before
  a timeout became an unanswered failure read as pending holds whose host and time were
  not kept, so any recorded answer releases them once, a probe scan reaches them, and an
  unanswered failure after that release exhausts them by the same any-host rule.
  A confirmed timeout, numeric nonzero exit or answerless exit of a tool-enabled finalizer, after completed process-group cleanup, holds only
  its exact request: each queue task's input in its
  native generation receipt, each audit subject's context as an audit-assessment
  preparation attempt. Inputs the host changed before failing are not held; they are
  prepared afresh. Preserve valid wave peers. Every invocation that ends without an
  answer (a timeout, a failed exit, empty output, a missing terminal result or a successful
  Claude envelope without structured output, `execution::observation::is_answerless`, as
  distinct from a malformed or oversized answer), on any route and in serial or
  parallel waves, counts against its
  host configuration (role,
  observation or tool-result host, and execution profile) when the run records its step,
  and an answered call on that configuration, usable or not, clears its count. Below `HOST_FAILURE_LIMIT` (2) consecutive
  failures of one configuration the run replans without the held requests and continues
  its independent work; at the limit the run sends that configuration no further call,
  because a repeat points at a shared fault of that host: work routed to it is deferred
  for the rest of the run, unheld, while other configurations keep calling and returned
  results keep publishing. Only when no configuration that answered in the run remains
  untripped does its campaign stop (`host-fault`) instead of starting another batch,
  without pausing the scheduled operation or counting semantic progress. Signal exits,
  process cleanup failure, local I/O, unusable tool-enabled replies and unknown errors
  remain fatal.
  An unanswered failure cannot tell a host fault from a request the host cannot serve, so
  its hold (inspection attempt, generation receipt, screening operational hold or
  preparation attempt) records the host that served it, its execution profile, and when
  that host ended (`InvocationFailure.host`), and lasts only until that profile answers
  again (`work/host.rs`); another profile of the same engine, even with another executable,
  is another host. A step whose host returned a response, usable or not, stores its own
  completion time as its profile's and engine's latest answer in
  `.lorekeeper/queue/host-answers.json`, whichever operation ran it; a launch failure or an
  answerless exit is no answer, and a parallel wave's settlement order never reorders them.
  The ledger keys profiles by a fixed-length digest of their name and keeps the 256 most
  recently answering, so no configuration sharing the vault erases another's evidence and
  its size never depends on name length; a ledger beyond its read bound is refused before
  it is written, never left for later reads to reject.
  A hold recorded before its host was kept follows any answer of its engine. A pending hold
  whose host answered after its failure no longer holds: the request is admitted once more,
  the retry an operator makes after diagnosing the host. Failing again without an answer
  after that release exhausts the hold until the explicit retry command clears it; a
  changed input or request is a fresh judgment. Unusable answers and semantic holds are
  never released this way. When planning finds nothing admissible, a queue drain (or a
  knowledge audit, for its subjects) releases as a probe the oldest pending hold whose
  request is still current, whose host has not answered since and which this run has not
  invoked yet; if replanning still admits no call, that probe is withdrawn and the next
  oldest is tried, within the same run, until one admits a call or none remains. Probes
  also take turns beside ordinary work, so a recovered host is not starved by another
  profile's stream of new work: when a run of a drain gives its first turn to ordinary
  work while probe candidates wait, the ledger records that recovery is owed
  (`recovery_due`), and the next run of that drain releases a probe first, plans its
  target first and alone (never batched with another target) and sends it before other
  work; any release pays the debt. A released probe that planning leaves out, because its
  target is no longer current work, waits on an unfinished dependency or falls outside
  the drain's admission scope, is withdrawn and the next oldest released in the same run. Each side therefore waits at
  most one run, even at one call per run. While the
  probe's host has given no outcome in the run, waves send one call at a time; a probe an
  earlier run released and whose host has not answered since counts as this run's probe,
  since that run may have stopped before sending it. A probe that
  fails without an answer renews its failure time and stays pending, so an outage costs at
  most one unanswered call per host per run and exhausts nothing. The renewed failure counts
  its consecutive failed probes (`HostEnd.probe_failures`), and the hold is a probe
  candidate again only 5 minutes × 2^(n−1) after that failure, at most 24 hours
  (`work::host::probe_backoff`), so a request that always fails after its host served it
  stops spending a call every scheduled run; its hold reason names the next probe time. An
  answer of its host, or an explicit retry, still releases it at once. The backoff has no
  terminal cutoff by design: a run sends at most one probe and a hold at most one probe per
  24 hours, and a multi-day outage must not exhaust holds that only await their host. Hold reasons
  name the state: pending holds are retried automatically once the host answers; exhausted
  and unusable ones need diagnosis and the named retry command. The scheduled pause for
  fatal stages is unchanged; navigation refresh commits beside readers holding their
  snapshot because the search index uses write-ahead logging, and a refresh that still finds
  the index busy exits 75 like a changed capture, so the run defers instead of pausing.
  `work retry` clears only a current operational hold;
  `work hold` suspends an exact ready node without invoking a host or granting refinement.
  Both use fresh Work/input guards; dry runs publish/recover nothing and invoke no host.
- Native writer waiting is cancellable, retries only typed contention and validates the
  same-read config pin before recovery. A pre-host cursor wait discards prepared
  requests and replans before invocation; post-host recording reloads fresh inputs.
  The controller derives Work state for native preparation only under its own writer
  claim, so a cooperating writer's pending journal is awaited and recovered first; a wait
  there discards queue planning captures and replans, and the claim ends before any host
  call. Standalone read-only Work queries still refuse pending journals.
  Native discovery reads also check the supervised signal flag between enumeration,
  header lines and body chunks. Cancellation discards incomplete captures; it does not
  interrupt an active kernel I/O call or abandon a journal transition.
  Carry that same cancellation control through selected review/evidence reads and
  fresh publication guards. Ephemeral prepared captures may retain the control outside
  semantic identity; automation duration and call budgets are not cancellation flags.
- Mixed queue read waves record screening and evidence replies inside one post-host
  mutation session. Screening decisions bind candidate keys and record without reading
  candidate pages; evidence Work reloads only the resources and reviews it reads and
  consults no candidate roster. Evaluate ledgers and Work state after owned writes.
  Revalidate the configuration and physical root before each role records; drift
  refuses remaining replies. Any screening recording error releases that session before
  reacquiring/recovering and freshly recording remaining paid evidence peers. Charge
  the shared reload duration once.
- Finalization shares one fresh capture for status and preparation in the same local
  checkpoint. Preparation includes that capture; verification measures sealed-result
  checks separately. Never reuse this capture after a host call or writer wait.
  Screened extraction binds its current candidate roster in that checkpoint and again
  after the host returns. A roster that cannot be formed (screening capacity, a blocked or
  incomplete source), is not completely decided, or admits different evidence replans
  before the call and is a typed `semantic-context-changed` refusal after it; other
  roster churn rebinds (cli-extraction.md).
- Native consumers own selected semantic reads and guarded publication; the controller
  owns final global reconciliation/checks. Keep this boundary explicit in the host
  instruction and shared skills; standalone skills still perform their own final checks.
- Knowledge jobs give ready audit subjects a turn before global queue draining. Scope
  readiness to the actual subject/evidence/context/review pages, verified physical
  owners, typed unfinished work and staged results. Unknown task/result targets cannot
  be assumed unrelated. Reserve pending first-discovery order under vault→queue claims
  before external audit discovery or assessment, preserving it across failed or
  budget-limited runs without recording completion or consuming deferred class turns;
  it orders only equal risk priorities.
  During automatic admission, check exact current Work holds only along the available
  planned prefix needed to fill the configured batch. Preserve held order and paid
  bytes; all-held is deferred, while corrupt or unreadable state is fatal. No Work-ID
  shortcut, persistent hold index or capture reuse across a host/wait boundary. Recheck
  readiness under those claims before recording, and verify selected receipts after
  final reconciliation even when unrelated backlog leaves the overall job incomplete.
  Authored pages require no invented done markers. Recheck discovered dependencies
  within the originally selected batch. Known blockers defer only their subjects;
  recompute the ready input union and final host selection. Preserve pending order and
  report the latest checked scope without claiming completion. `wiki audit-plan --ready`
  uses the same readiness owner without reserving work; automatic plans skip blockers,
  while exact selections retain them with bounded diagnostics and a complete count for
  that checked scope.
  An empty audit settles only after fresh corpus readiness succeeds. Unfinished recorded
  inputs remain deferred without invalidating current semantic receipts.
- Grant each private temporary result directory (reply schema and result file) to the
  selected host with `--add-dir`; keep it alive through the foreground consumer and
  remove it afterward.
- A tool-enabled native audit finalizer may legitimately create reviews or repair
  its inputs. A typed semantic-context refusal still rejects the old submission;
  recapture the remaining selected subjects, refresh missing discovery and prepare
  current Work under the same run budget. Allow one such replan per subject per run;
  repeated drift defers that subject while independent subjects continue. Keep all
  predecessor Work and paid outputs unchanged. Configuration/root drift and corrupt
  state remain fatal. A tool-free observation whose Work's inputs changed while its host
  ran (context, citations or evidence scope, or a task no longer current) is discarded
  unrecorded and its Work replans as it would before a host call, spending its queue
  target's one replan per run: an external edit during an inspection never halts or
  pauses the run, and repeated drift defers that target.
  Queue finalizers follow the same rule per target: a fresh guard that finds changed
  inputs before or after the host discards that step without relabeling its answer,
  allows one replan per target per run and then defers the target. A publication guard
  that refuses a selected result whose captured input is still current holds that exact
  input in its generation receipt instead.
- Whole-input audit judgments carry unique complete subject references. Rust supplies
  assessment metadata and records current inputs through the shared audit owner.
  Replan original slots through this workflow's guarded concept merges; documents
  retain exact addresses. Isolate known blocked peers from publication readiness.
  Whole-input holds bind per-subject semantic context and assessment rules, excluding
  batch peers/order. Unfinished plan creation never clears a hold. Keep one bounded
  latest receipt per subject; canonical assessment removes it in the same journal.
  Retention under a workflow lease removes only expired receipts for proven-absent
  subjects, with absence and captured-byte guards; existing subjects stay held.
  Check fixed assessment identity/review-path capacity before discovery or inspection,
  and again before persisting a scope expanded by discovery/repair. A typed capacity
  refusal defers that subject without a paid call or a durable semantic action.
