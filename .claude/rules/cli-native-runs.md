---
paths:
  - "crates/lk-cli/src/commands/run.rs"
  - "crates/lk-cli/src/commands/run/**"
  - "crates/lk-cli/src/commands/execution.rs"
  - "crates/lk-cli/src/commands/execution/**"
---

## Native runs (`commands/run.rs`, `execution.rs`)

- Audit captures require current candidate decisions. Synthesis retains the complete
  discovery knowledge scope without audit selection metadata; audit readiness cannot
  block its prerequisite queue tasks. Existing Work compares fresh subject/input
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
  host-specific invocation and supervised Unix process groups. The shared WorkflowLease
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
  remain one batch and the configured per-batch calls.
- Process classifies final review health through the shared typed reports. Reassessment
  keeps strict completion nonzero without halting budgeted independent queue progress;
  invalid reviews and failed reads remain fatal. Queue settlement alone cannot satisfy
  review completion. Daily retains its strict review checkpoints before period
  resumption.
- Stop on fatal or unknown state, cancellation, no verified progress, or an empty plan.
  An input pipe closed by the child may retain a natural numeric nonzero exit only
  after complete stdout draining, reaping and group cleanup. Closed input with exit zero
  or an unsettled deadline remains an I/O failure; it never becomes success or an automatic retry.
  Verified selected audits can advance subsequent budgeted batches despite unrelated
  queue rejections; the campaign remains incomplete/nonzero. Batch-limit success does
  not establish full-corpus coverage. Existing receipts resume verified work.
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
  never reset usage or pauses. Under an acquired workflow lease, synchronously reserve complete
  waves in the atomic operational usage ledger before spawning; a prepared but unlocked
  lease grants no ownership. Never refund ambiguous launches, reset usage on config
  changes, recover corrupt accounting as zero, or detach its writer. This ledger is
  independent of canonical mutation journals; page, Work and cursor writes still use
  `MutationSession`. Execution duration starts with the first owned workflow and
  excludes later controller workflow acquisition waits. Those waits have a separate
  configured bound; source collection, ingest publication and short writer waits do
  not use that allowance. Neither allowance renews across batches. Duration limits stop
  new calls, while returned paid results remain recordable. Fatal scheduled failures pause that operation;
  explicit `--resume` clears its pause without clearing usage or semantic holds.
  Confirmed tool-free Inspect/Reduce session timeouts retain one automatic retry per
  exact request/contract, then hold only that node. Persist counts separately from
  observations and semantic actions through the mutation journal; they are not retained
  semantic progress. Confirmed numeric nonzero exits and unusable remote responses from
  tool-free Inspect/Reduce hold the exact input across instruction-contract changes.
  Preserve valid wave peers; a wave with only invocation failures ends the current run
  without pausing the scheduled operation or counting semantic progress. Signal exits,
  process cleanup failure, unknown errors and tool-enabled failures remain fatal.
  `work retry` clears only a current operational hold or exhausted timeout allowance;
  `work hold` suspends an exact ready node without invoking a host or granting refinement.
  Both use fresh Work/input guards; dry runs publish/recover nothing and invoke no host.
- Native writer waiting is cancellable, retries only typed contention and validates the
  same-read config pin before recovery. A pre-host cursor wait discards prepared
  requests and replans before invocation; post-host recording reloads fresh inputs.
  Native discovery reads also check the supervised signal flag between enumeration,
  header lines and body chunks. Cancellation discards incomplete captures; it does not
  interrupt an active kernel I/O call or abandon a journal transition.
  Carry that same cancellation control through selected review/evidence reads and
  fresh publication guards. Ephemeral prepared captures may retain the control outside
  semantic identity; automation duration and call budgets are not cancellation flags.
- Finalization shares one fresh capture for status and preparation in the same local
  checkpoint. Preparation includes that capture; verification measures sealed-result
  checks separately. Never reuse this capture after a host call or writer wait.
- Native consumers own selected semantic reads and guarded publication; the controller
  owns final global reconciliation/checks. Keep this boundary explicit in the host
  instruction and shared skills; standalone skills still perform their own final checks.
- Knowledge jobs give ready audit subjects a turn before global queue draining. Scope
  readiness to the actual subject/evidence/context/review pages, verified physical
  owners, typed unfinished work and staged results. Unknown task/result targets cannot
  be assumed unrelated. Reserve pending first-discovery order under vault→queue claims
  before external audit discovery or assessment, preserving it across failed or
  budget-limited runs without recording completion or consuming deferred class turns.
  During automatic admission, check exact current Work holds only along the available
  pending prefix needed to fill the configured batch. Preserve held order and paid
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
- Grant each temporary selection manifest's directory explicitly to the selected host;
  keep it alive through the foreground consumer and remove it afterward.
- A tool-enabled native audit finalizer may legitimately create reviews or repair
  its inputs. A typed semantic-context refusal still rejects the old submission;
  recapture the remaining selected subjects, refresh missing discovery and prepare
  current Work under the same run budget. Allow one such replan per subject per run;
  repeated drift defers that subject while independent subjects continue. Keep all
  predecessor Work and paid outputs unchanged. Configuration/root drift, corrupt
  state and unexpected changes during tool-free observation recording remain fatal.
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
