---
paths:
  - "crates/lk-cli/src/commands/queue.rs"
  - "crates/lk-cli/src/commands/queue/**"
  - "crates/lk-cli/src/commands/synthesis.rs"
  - "crates/lk-cli/src/commands/run.rs"
  - "crates/lk-cli/src/commands/run/**"
  - "crates/lk-cli/src/commands/work/publication.rs"
---

## Queue retirement

- Prepare queue retirement and related rejection/action/discovery/screening cleanup before
  publishing one guarded mutation plan. Preview validates the same plan without recovery
  or canonical writes. Skip guard-only journals; empty screening-directory cleanup follows
  successful publication and can resume separately.
- Retirement classifications retain their same-read target and cited-source dependencies,
  including absence and RSS membership snapshots. Guard only classifications that retire
  work, before destructive queue steps; unrelated retained current pages do not constrain
  cleanup. Share captures only within that preparation, never attach a later read baseline.
  Discard non-retiring raw snapshots. If a cached revision yields retirement without its
  read proof, reclassify the whole task from fresh snapshots before creating guards.
  Done rows leave for processed history whenever their captured proof agrees, under
  guards on those pages; a run with work left keeps done rows whose captures disagree,
  and a whole-run archive refuses them.
  Validate discovery storage containment before enumerating or reading its records.
- Result retirement uses that same captured classification and guards its target/source
  or absence in the deletion journal. Extraction result admission may ignore completion;
  an already-materialized retirement uses the task verdict from the same capture.
  Never replace the original decision's read proof with a later baseline.
- Pending input retains full current typed validation. Processed history protects exact task/cache references through a strict known JSON
  envelope; its unique-key input object is opaque historical data, never current semantic
  authority. Do not reconstruct it with today's request, supply a policy, rehash, rewrite or
  publish it. Unknown envelopes, duplicate fields and unreadable records refuse cleanup. Preserve original history bytes and live
  staged references; a missing historical field never grants current semantic authority.

## Native queue scheduling

- Native queue scheduling chooses one task per target so publication cannot invalidate
  another simultaneous task on the same page. Whole-input tasks share an
  evidence-bounded batch; oversized work reserves fair inspection/reduction/finalization
  turns.
- `execution.work.max_consecutive_calls` defaults to 4 and bounds host attempts for an
  admitted exact queue Work or screening phase. Reserve the durable fair turn once;
  continuation requires an in-process exact phase token and freshly prepared inputs.
  Charge every attempted host including parallel peers against the same total budget.
  A peer's completed screening refreshes preparation without resetting unchanged
  bindings or their remaining allowances. Reprepare admitted targets first and retain
  only exact current bindings; changed targets return to their durable fair position.
  Publication, changed phase bindings, deferral and writer-wait replanning discard
  admission; restart/cancellation retain the already reserved next target.
- `execution.work.max_parallel` defaults to 1 and bounds simultaneous dependency-ready
  evidence observers and distinct-target candidate screening within remaining calls.
  Compatible roles share a fair contiguous prefix; serial finalizers and discovery
  remain boundaries. Never filter past those boundaries or reserve skipped targets.
  A later exact evidence action ends the queue read prefix without discarding earlier
  eligible requests. Charge that prefix through its current admission before replanning;
  an empty held prefix invokes no host and reserves no turn.
- Discovery, finalizers and publication remain serial. Supervise every child
  through exit and cleanup. Carry prepared screening requests only within their local
  pre-host queue checkpoint; any host call or writer wait invalidates their reuse.
  Screening replies record against candidate keys without another capture. For evidence
  replies, reload what each Work reads after hosts return, its resources and scoped
  reviews, loading each Work once per checkpoint; screened extraction evidence consults
  no candidate roster.
  Reevaluate current node dependencies for every serial record; preserve each mutation's
  durable checkpoint and exact page/ledger guards. Never retain a capture across a host.
  Preserve an independent valid response when a peer fails. Never reuse pre-host
  captures for recording. Shared reload timings belong to the first recorded step, not
  every peer.
- Verify target owners before batching. Defer concept synthesis while its cited origin
  pages have unfinished inputs or publishable pending work; unrelated context is not a
  dependency barrier. Reconcile backlinks after a source publication wave and replan
  before selecting another task. A dependency-only queue remains explicitly incomplete.
  One queue-planning checkpoint captures all ready synthesis inputs against the complete
  graph universe for its selected window; discard that view before host execution.
- Enumerate and validate the complete queue and physical target owners before selecting
  a fair preparation window. Section/progressive windows obey remaining-call and
  parallel bounds, capped at 64; possible whole-input batches retain 64 targets even
  when one call remains. Continue bounded preparation across task-kind groups while the
  prepared jobs are compatible reads; stop at validated serial boundaries without
  skipping targets; actual prepared TaskWork remains authoritative, including saved
  plans. Small tasks also obey the input-byte bound. Share discovery traversal,
  projection and immutable raw bytes within that checkpoint while preserving task-local
  recipes, reviews, guards and identities. Later checkpoints read, classify and traverse
  afresh; only stamp-verified page texts and pending queue bytes carry over (lk-vault),
  never a capture, a classification or its guards. Selection classifies from them;
  completion receipts and publication read current bytes.
  Unprepared targets remain pending. Empty or deferred windows never prove settlement.
  Check synthesis input readiness only within those fair windows; remove checked
  blockers from local dispatch keys, retain unexamined keys and continue past blocked
  windows. Readiness deferral alone never reserves a cursor turn.
- Planning does not consume cursor turns; reserve actual invocation targets together
  immediately before execution, including every member of a whole-input batch.
  `.lorekeeper/execution-cursor.json` stores the next queue/audit target under a guarded
  short mutation, preserving turns across budget-limited runs. Reserve before external
  execution. Cursor turns are round-robin: never infer their priority or semantic
  quality from size, progress or citation counts. Which audit subjects enter a batch
  is decided earlier by `lk-graph` planning, which orders each class by typed risk.

## Queue processing (`commands/queue/`, `commands/synthesis.rs`)

- The shared typed classifier owns current/stale/done/unreadable decisions. Validate
  input kind/policy, target revisions, and synthesis citation guards. Missing or
  unreadable evidence is not a stale task that can safely disappear.
  Malformed target-owned input/completion/policy markers are unreadable, never evidence
  of supersession. Absent/null markers retain their existing absence meaning.
- Queue counts describe current work, not raw lines. Prune only eligible dead work,
  remove empty files, archive all-done files, preserve unreadable/unparseable data.
  A task a staged result still answers stays pending whatever its classification:
  apply publishes or retires that result first, so pruning never orphans it.
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
  still owns reapplication or retirement. Unreadable, ambiguous and corrupt state
  remains fatal. A readable result whose exact request is absent from a fully parsed
  queue is an `orphaned` group, usually because its request was retired (by prune or
  regeneration) while a standalone session drafted it, which no lock spans; an invalid
  identity takes the same path. Apply quarantines it
  under `results/orphaned/` with its reason before any format or publication check,
  reports the run not ok (`orphaned`), and continues with independent groups; it is
  never published or judged, and input readiness ignores it. Quarantine is one guarded
  plan per group: each move expects the bytes the group was judged from and carries its
  reason file; the preview validates that plan and the apply journals it. A quarantine
  store (`orphaned/`, `superseded/`, `corrupt/`) whose path below the vault root holds a link or file
  refuses in both modes, and no quarantine replaces an earlier file or reason.
  A synthesis result refused because its still-current request's read scope or target
  changed (`semantic-context-changed`, `target-changed`) is `superseded`: the same guarded
  plan keeps it under `results/superseded/` with its reason and removes its rejection
  receipt, the run reports not ok, and the request is drafted again from its current
  scope (a native run spends that target's one finalization replan, and a selected task
  superseded this way replans rather than failing completion). Holding it would block the
  request until the scope returned byte for byte. Citation changes stay held, because
  reconciliation retires their request; incomplete or unsealed Work
  (`evidence-scope-changed`) stays held for its Work to finish.
  A synthesis whose concept's own audit assessment or current-revision discovery recipe
  cannot be read is `held`: apply leaves its result staged without a rejection receipt,
  reports the run not ok and continues with independent groups, including when the hold
  surfaces while recording another rejection; planning defers that target, a retained
  rejection of it blocks host admission, native staging and a selected-task completion
  check defer it, and `work context`/`plan` refuse only that request. A repaired record
  makes the result eligible for validation again; a changed recipe still supersedes it.
  A malformed audit store as a whole stays fatal.
- Synthesis owns both sections and its completion marker atomically, with exact
  source/target/queue guards. Do not split core and related publication. Explicit
  year/date conflicts with `--previous` fail before flushing work.
- A synthesis reads its citations and direct links in full, and of its recorded audit
  discovery matches a ranked roster: per query, every identity match and the best
  `lk_graph::audit::SYNTHESIS_CANDIDATES_PER_QUERY` other owners in search order, from the
  same traversal that found the matches. The bound is a fixed semantic policy, not
  `max_query_candidates`, so the roster does not depend on the execution profile; byte
  limits and the other configuration still shape the request and its Work. A page matched
  through several spellings is read through its own address when a query matched it.
  It never waits for audit screening decisions.
- A synthesis request answers the citation set it was made for. Drafting, preparation
  and publication refuse it (`citations-changed`) once the concept's graph citations
  captured with its context differ, whatever order reconciliation and application run
  in: preparation defers it without a call, staging treats it as changed scope, and
  apply holds its result under a rejection whose scope carries those citations. The
  result publishes if they return, and retires with the request once reconciliation
  records the current set. Regeneration refuses to request that set again. The set is
  compared at the capture inside the writer session: a cited page edited, retracted or
  removed afterwards still refuses at the journal through its exact source guard, while
  a page that starts citing the concept afterwards lets the answered set publish, the
  same state as a citation added just after publication, which the next reconciliation
  supersedes. Do not rescan the vault at the journal to close that window.
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
- `queue packet` emits `version: 2` only when it carries RSS event evidence, each item
  with its exact `lore wiki original … --event … --revision …` argv in
  `selected_event_command`; packets without event evidence stay `version: 1`.
- Document identity adoption compares the production projection and guarded current
  observation, including full commit refs, blob/document identity, manifest, and
  remote ownership. A blocked mismatch is not adoption. Archival source identity
  does not itself decide lifecycle state.
  Current and historical Content proofs use the same Markdown fragment renderer;
  do not approximate prior lossy formatting or infer provenance from similar text.
