# Native run report

## Contents

- [Outcome](#outcome)
- [Stages and host failures](#stages-and-host-failures)
- [Budget, progress and publication](#budget-progress-and-publication)
- [Work steps](#work-steps)
- [Finalization admissions and input capacity](#finalization-admissions-and-input-capacity)
- [Timing](#timing)
- [Progress events](#progress-events)
- [Batch campaigns](#batch-campaigns)

Read this to interpret the `--json` report of `lore run daily`, `lore run process` or
`lore run knowledge`. Stdout holds the
single structured outcome; stage output and progress events go to stderr. Byte counts
are not token counts or billed cost, and unobserved or inapplicable measurements stay
`null`.

## Outcome

`ok` is false for a halted controller and for known incomplete states; `halted`
distinguishes the two. A halted controller starts no further semantic or mutating
stage; read-only final checks still report preserved state when configuration is
current. An incomplete job can still publish independent work and verify selected
audits; neither field implies full-corpus semantic coverage. A recorded observation or
staged final result alone does not prove publication: inspect the job's stages,
publication result and final checks.

## Stages and host failures

`stages` names the typed invocation stages; each is completed, skipped, failed or
cancelled. `execution` records the selected profile, engine, observed CLI version and
requested model/effort; omitted native settings stay `null`. It does not establish the
actual provider model, effective effort, tokens or billed cost. The top-level
`execution` describes the first prepared host; its `policy_profile` and `work` retain
the job profile and aggregate budget. Stage routing is described in
[optional stage profiles](../../lore-setup/references/config.md#optional-stage-profiles).
A skipped `synthesis review refused` stage names the concept and why Rust refused its
synthesis's review proposal; the synthesis the reply carried was staged instead and
publishes with the next queue application like any staged result, so read the
publication result for whether it published. The reason is never written to a vault page.

A failed structured host includes `host_failure`. Its `phase` is `version-preflight`,
`invocation` or `response`; preflight never consumes a semantic call. It reports the
engine, exit code and the JSON shape of captured stdout. `stdout_capture` is
`complete`, `not-captured` or `unavailable`; only complete capture has a known byte
count and JSON format. Stdout diagnostics expose fixed status tokens, types, UTF-8 byte
lengths and member counts, never payloads or session data. `stderr_tail` is the host's
own account: the last 2 KiB of its stderr on one line, control characters removed and
credentials `lore doctor` recognizes masked by their kind, such
as `Error finding codex home`; a `host failure output` stage repeats it with the profile
for each answerless failure. It stays in the run report, never in hold records or vault
pages; attended runs still stream stderr unsanitized. A diagnostic never permits a retry, fallback or adoption: a
nonzero exit remains a failure even with a success-looking envelope. Clear the
resulting hold as [bounded work](bounded-work.md#native-holds-and-retries) describes.

## Budget, progress and publication

`budget_exhausted` records an actual call-budget refusal rather than an inference from
the call count. `retained_progress` counts newly recorded `evidence_nodes`,
`screening_packets`, `staged_requests`, `discovery_recipes` and `action_receipts`.
Staged requests can contain several task results; actions and discovery remain
incomplete, and cached or already-complete steps add no progress.
`queue_publication_observed` is a boolean from an applied report or an exact
selected-task completion check, not a publication count. `queue_application`
accumulates controller application `attempts`, `duration_ms`, `applied`, `dropped` and
`rejected` counts and retains the `latest` typed report; repeated rejection counts are
attempts, not distinct tasks. These counters do not enumerate writes an interactive
child CLI performed; use the exact native receipt or a guarded task-identity comparison
when complete publication accounting is needed.

## Work steps

`work_steps` lists evidence, screening, finalization and discovery steps. The first
1024 steps are retained per run; `work_steps_omitted` counts later ones. Cached
refusals and already-complete work can have no invocation, so the array is not a call
ledger.

| Fields | Meaning |
| --- | --- |
| `role`, `work_id`, `node_id`, `node_kind`, `screening_id`, `request_revision` | Exact selected work or screening identities; unavailable identities are `null` |
| `request_json_bytes`, `result_json_bytes` | Compact UTF-8 JSON of the prepared request and decoded response, excluding Rust-supplied identity, prompts and host envelopes |
| `preparation_duration_ms`, `preparation_from_queue_plan` | Additional local preparation of the selected request or completed root; a screening request carried from the same queue checkpoint has `null` duration and `true` reuse |
| `agent_duration_ms` | Supervised invocation attempt wall time, including a failed or cancelled attempt; never server inference time |
| `revalidation_record_duration_ms` | Fresh input validation and recording together |
| `verification_duration_ms` | Finalizer completion and sealed-result verification |
| `outcome` | `recorded`, `refined`, `staged`, `restaged`, `already-complete`, `rejected`, `failed`, `cancelled` or `yielded` |

## Finalization admissions and input capacity

`finalization_admissions` records native checks of prepared Root or Frontier
finalization: the Work, task, Work subject revision, mode and typed outcome. Up to 1024
checks are retained; `finalization_admissions_omitted` counts later ones. Entries are
checks, not distinct tasks or calls. `available` is readiness, not host execution or
publication, and a Root `needs-action` is the prepared outcome, not proof of a new
model call.

`native-input-capacity` reports `canonical_input_bytes` and `request_components` from
the same capture. Components count compact JSON bytes for observations, context, proof,
inspected resource roster, contracts, response schema and instructions;
`metadata_envelope` counts the rest, and their sum equals `request_bytes`. Use them to
see what fills an oversized request. They count no tokens and do not establish that a
faithful smaller response exists.

## Timing

`semantic_duration_ms` sums invocation attempt wall times, so parallel attempts can
exceed the job's elapsed `duration_ms`. `queue_planning` reports local planning
`attempts`, cumulative `duration_ms` and prepared targets; counts accumulate across
checkpoints, so one target can be prepared again. `mutation_access` reports in-process
writer acquisitions, including contention waiting, configuration checks and journal
recovery. These durations overlap and are not independent values to sum.
`queue_planning.discovery_io` and `screening_preparation_io` count file reads in
successful discovery captures; read time includes opening and decoding but does not
identify an OS latency cause, and the counters never authorize cache reuse.

## Progress events

During execution, stderr carries `lore.queue-planning` events at local checkpoint start
and end, `lore.work-step` events for retained steps as they finish, and
`lore.queue-read-wave` before an admitted queue wave with its request counts, configured
parallel limit and remaining calls. A wave event describes dispatched requests, not
observed concurrency. Events use existing measurements and establish no publication,
inference time or cost.

## Batch campaigns

From the coordinating shell, `lore run process --profile NAME --batches N --json`
permits at most N queue-only batches, each with the configured call budget and a fresh
workflow lease; the default is one batch. Configuration bytes and the first acquired
vault's directory identity remain pinned across the campaign. It continues only at a
typed exhausted-budget checkpoint with newly retained progress; fatal or unknown
failures, cancellation, ordinary deferral or no progress stop it. New action or
discovery receipts are preparation progress, never semantic completion. Every
checkpoint retains its own incomplete outcome; the campaign succeeds only after a
settled final batch passes its full checks, and reaching N with remaining work stays
incomplete and nonzero. Review reassessment keeps Process completion nonzero without
halting a checkpoint that retained new queue progress; once the queue settles,
outstanding reassessment stops the campaign as deferred, and Process does not resolve
or approve reviews. `lore run knowledge --batches N` follows the
[maintenance campaign contract](../../lore-wiki/references/maintenance.md#operating-budget-and-measurement).
