# Extraction discovery

Use before concept extraction needs related-knowledge context. Discovery finds complete
lexical candidates; screening chooses which bodies to inspect. Neither establishes
identity, support, contradiction or a completed source read.

## Choose and record recipes

Read the exact selected task-ID array, then `lore queue status --json`. Process only
its current `extract-concepts` tasks. Capture each target's `target_revision` before
reading its outline and bounded relevant sections with `wiki read --revision`.
Inspect source-written subjects, definitions and scope. Use those to choose up to
16 lexical queries, including declared names or distinguishing claim terms. These
retrieve complete candidates; a match does not establish identity or contradiction.
Do not load the full concept registry or copy a large corpus into the prompt.

Record one strict JSON file per selected task:

```json
{
  "target_revision": "<queue status revision captured before reading>",
  "queries": [{"query":"source-written subject", "phrase":true, "spacing":false}]
}
```

Run `lore work discover --task-id ID --file FILE`. A changed target requires rereading
and choosing the recipes again. Empty recipes are valid for an eventual empty extraction;
publishing any concepts requires at least one query. Never invent generic queries merely
to satisfy that count.

The native `discover <task-ids.json>` operation returns after recording recipes.
Ordinary selected/manual processing continues with `lore work context --task-id ID`.
An explicit `--discovery-file FILE` accepts `{"queries":[...]}` for context capture;
in progressive mode these must equal recipes already recorded with `work discover`.
File-only recipes remain available in `full-context` mode. Stored current recipes
are used when the flag is omitted. Keep the same queries through selection, planning
and the final result.

## Progressive examination

`execution.work.extraction_context: progressive` is the default. Follow the context
status rather than assuming that a context revision or resource roster is ready:

| Status | Next action |
|---|---|
| `needs-discovery` | Record meaningful recipes against the current target. |
| `needs-source-observations` | Run `work source-plan --task-id ID`; complete its bounded reads and reductions, then return to `work context`. |
| `needs-admission` | Read `work cost --task-id ID`; explicitly reconsider the discovery scope or capacity. Keep the task incomplete and all candidates/paid decisions retained. |
| `needs-screening` | Run `work select --task-id ID`; examine and record each returned packet. |
| `ready` | Retain `context_revision`, `selection` and the exact resource roster; read that evidence before drafting. |

The source phase uses the normal `work status`, `work read`, `work record` and
`work result` protocol. It observes the complete retained input, including required
target dependencies. **Do not run `work finish` for this phase:** its observations
prepare screening; they are not an extraction result. Preserve attributed source
questions and processing blockers through later examination and finalization.
`work cost --task-id ID` is read-only: it reports exact candidate and packet counts,
query-specific matches, compact JSON bytes, recorded packets and deferred/unknown
decisions. Remaining request bytes cover unrecorded packets; they exclude future
reassessment of deferred decisions, prompts, host envelopes, tokens and billed cost.
Packet cost remains unknown until oversized source observations are complete.
An optional `execution.work.max_screening_batches` bounds the complete selection's
packet count; omission adds no limit. A completed ledger remains usable. Exceeding
the limit defers the scope without dropping candidates or rewriting recorded work.
Refine a recipe only when its expressed source question is wrong or incomplete;
large lexical scope alone does not prove irrelevance. An unaffordable valid scope
remains deferred, never silently clipped. Recipe changes create a new scope and
preserve prior decisions rather than relabeling them.
If complete source observations reveal missing relevant subjects, revise the recipes
and recapture the inventory before choosing evidence for those subjects.

`work select --task-id ID --limit 20 --offset 0` creates or resumes a guarded ledger.
It returns one bounded `request`, progress and paged receipt metadata. Receipt pagination
does not remove candidates from coverage. Read the request's complete source context
and every assigned card. Cards preserve declared identity, bounded openings/matches,
available section locators, truncation and review counts; they are not full page reads.
Never load the whole manifest or all candidate bodies to perform screening.

Return one strict output covering every candidate in that request exactly once:

```json
{
  "selection_id": "<request.selection_id>",
  "batch_id": "<request.batch_id>",
  "input_revision": "<request.input_revision>",
  "decisions": [{
    "candidate_key": "<assigned exact owner>",
    "decision": "inspect",
    "reason": "The card may describe the same scoped subject; read the body."
  }]
}
```

Choose `inspect` for a body needed to judge identity, scope or a relevant claim;
`not-selected` is a reasoned **card-only** assessment for this extraction, never a
whole-body negative finding. Use `defer` when applicability remains unresolved.
Insufficient previews do not establish irrelevance. Each reason’s serialized JSON string, including quotes and escapes, must fit
`max_reason_json_bytes`; the complete output must fit `max_result_bytes`. Do not infer equivalence from names or turn search scores into
admission rules.

For interactive processing, stage the output under the vault's `.lorekeeper/` and run
`work select-record --task-id ID --file FILE`, then request the next packet. A native
structured screening worker returns only `{decisions: [...]}`. Rust supplies the exact
selection, batch and input identities from the supervised request and records the result;
the identity-bearing example above remains the standalone record format.
Unknown or deferred candidates block extraction completion. To reassess a recorded
batch, obtain its exact current packet with `work select --task-id ID --batch-id BATCH_ID`.
This read-only lookup also returns its `receipt.revision`. Submit the reconsidered
whole batch using `select-record --previous-revision RECEIPT_REV`; ordinary retries
cannot overwrite earlier decisions. Do not reconstruct packets from the manifest or
copy new guards onto old decisions.

An empty candidate inventory needs no screening model response. Let `work select`
record the empty selection, then continue to the full retained-source read. No matches
do not establish that the source contains nothing to extract.

## Draft from admitted evidence

After screening, call `work context` again and require `status: ready`. Read the retained
source, selected bodies and their required scoped reviews/current or verified archived
evidence under the captured revisions. A reused owner must be selected and fully read.
Excluded candidate bytes and all scoped review/evidence guards remain pinned even though
their bodies are not semantic inputs. New matches, owner/alias changes, edits or changed
dependencies invalidate the selection; recapture and reconsider rather than retagging it.

Ready context reports `resource_count` and `part_count`. When `requires_bounded_work`
is true, `resources` is null: execute `plan_command` and complete the bounded-work
protocol. Native selected workers retain that verified handoff, finish other selected
tasks and return; the controller schedules remaining source/screening/evidence units.
Do not replace native resumption with a forced manual whole-queue drain.

The final extraction carries the exact ready `context_revision`, `discovery` recipes
and returned typed `selection`. Coverage distinguishes card screening from selected
full-body inspection; it never certifies unselected knowledge or semantic truth.

`full-context` explicitly retains the complete retained-input plus every retrieved
candidate-body contract. It can be selected globally or through a profile's `work`
override. Results without `selection` use that contract; never omit a progressive
selection to bypass screening or reuse a hash from the other read scope.
