# Extraction discovery

## Contents

- [Choose and record recipes](#choose-and-record-recipes)
- [Progressive examination](#progressive-examination)
- [Draft from admitted evidence](#draft-from-admitted-evidence)

Use before concept extraction needs related-knowledge context. Discovery gathers a
ranked roster of lexical candidates; screening chooses which bodies to inspect. Neither
establishes identity, support, contradiction or a completed source read.

In a native selection, discovery precedes `work context`. Ordinary small-source screening
can finish within the selected session. When a phase needs bounded native execution,
retain its verified handoff, stage no extraction result, finish the other selected tasks
and return. The controller resumes the work; an ordinary unfinished task is still an error.

## Choose and record recipes

Read the exact selected task-ID array, then `lore queue status --json`. Process only
its current `extract-concepts` tasks. Capture each target's `target_revision` before
reading its outline and bounded relevant sections with `wiki read --revision`.
Inspect source-written subjects, definitions and scope. Use those to choose up to
16 lexical queries, including declared names or distinguishing claim terms. Each
query rosters the pages it names by identity and its
`execution.work.max_query_candidates` (default 50) best other matches in search order;
a match does not establish identity or contradiction, and an unrostered match was not
examined. Prefer specific queries over broad ones.
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

Native `extraction-discovery` requests are read-only. Return one subject_ref entry
with `status: complete`, `queries` and `reason: null`, or `status: needs-action`,
`queries: null` and a nonblank reason within 256 compact JSON bytes. Return semantic
values only; Rust revalidates the current task and records its recipes. A recipe
registered during that read remains authoritative. Unchanged failed inputs remain held
across instruction changes; diagnose the failure before using `work retry-preparation
--stage extraction-discovery --task-id ID --input-revision HASH`, optionally with
`--dry-run`. Use the retained input hash reported by the run. This clears only its
preparation hold and invokes no agent.
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
| `needs-screening` | Run `work select --task-id ID`; record a decision for every card of each returned packet until it reports `ready`. A fully decided roster becomes ready once `work select` has sealed it. |
| `ready` | Retain `context_revision`, `selection` and the exact resource roster; read that evidence before drafting. |

The source phase uses the normal `work status`, `work read`, `work record` and
`work result` protocol. It observes the complete retained input, including required
target dependencies. **Do not run `work finish` for this phase:** its observations
prepare screening; they are not an extraction result. Preserve attributed source
questions and processing blockers through later examination and finalization.
`work cost --task-id ID` is read-only: it reports the roster's candidates, per-query
matched and rostered pages, the packets the undecided candidates still need, compact
JSON bytes and deferred/unknown counts. Remaining request bytes cover undecided
candidates; they exclude future reassessment of deferred decisions, prompts, host
envelopes, tokens and billed cost.
Packet cost remains unknown until oversized source observations are complete.
An optional `execution.work.max_screening_batches` bounds the packets the undecided
candidates need; omission adds no limit. A completely decided roster remains usable.
Exceeding the limit defers the scope without dropping candidates or rewriting recorded
decisions.
Refine a recipe only when its expressed source question is wrong or incomplete;
large lexical scope alone does not prove irrelevance. An unaffordable valid scope
remains deferred, never silently clipped. A recipe change alters the roster and the
rows of the candidates it matches; a row that stays the same keeps its decision and
nothing is relabeled.
If complete source observations reveal missing relevant subjects, revise the recipes
before choosing evidence for those subjects.

`work select --task-id ID` returns the next packet of undecided candidates as one
bounded `request`, with progress and any `hold`. It writes nothing while candidates
remain undecided. Read the request's complete `source` and every assigned card. Cards
preserve declared identity, bounded openings/matches, outline, truncation and review
counts; they are not full page reads. Judge each card on its own: a decision depends
only on that card and the source, never on the other cards, their order or how many
are assigned. Never load all candidate bodies to perform screening.

For standalone `select-record`, return one strict output covering every candidate in
that request exactly once:

```json
{
  "task_id": "<request.task_id>",
  "request_revision": "<request.request_revision>",
  "decisions": [{
    "candidate_key": "<that candidate's request.candidates[].candidate_key>",
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
`work select-record --task-id ID --file FILE`, then run `work select` again.
Native structured workers instead follow the [native screening response](bounded-work.md#native-screening-responses)
contract: each decision uses its assigned `candidate_ref`, never `candidate_key` or
response order. Rust binds the candidate keys and the task/request identities;
the identity-bearing example above is not the native response format.
Unknown or deferred candidates block extraction completion. A recorded decision other
than `defer` is final for that card under that source and screening model. To
reassess deferred cards, run `work select --task-id ID --reassess`: it issues only the
deferred rows, each with its `previous` decision reference, and `select-record` answers
that request. After diagnosing a failed native invocation,
`work select-retry --task-id ID`, optionally with `--dry-run`, clears only the current
packet's operational hold. It never changes paid decisions or semantic rejections.
Do not reconstruct requests from stored packets or copy keys between requests.

Once every candidate is decided and none is deferred, `work select` seals that exact
roster and reports `status: ready` with the typed `selection`. An empty candidate roster
needs no screening model response: `work select` seals it at once. No matches do not
establish that the source contains nothing to extract.

## Draft from admitted evidence

After `work select` reports `ready`, call `work context` and require `status: ready`.
Read the retained source, selected bodies and their required scoped reviews/current or
verified archived evidence under the captured revisions. A reused owner must be selected
and fully read. The source, the selected owners and their scoped reviews stay pinned
through publication; a change to any of them requires rereading and a regenerated
result, never a retagged one. A candidate that was not selected is bound by the card
that was judged: its later edits, new matches and owner or alias changes do not
invalidate a result drafted against the sealed roster. They return as undecided cards
before the next draft, and `work context` reports `needs-screening` again until the
current roster is decided and sealed.

Ready context reports `resource_count` and `part_count`. When `requires_bounded_work`
is true, `resources` is null: execute `plan_command` and complete the bounded-work
protocol. Native selected workers retain that verified handoff, finish other selected
tasks and return; the controller schedules remaining source/screening/evidence units.
Do not replace native resumption with a forced manual whole-queue drain.

The final extraction carries the exact ready `context_revision`, `discovery` recipes
and returned typed `selection`, which names the sealed roster and its decisions.
Coverage distinguishes card screening from selected full-body inspection; it never
certifies unselected knowledge or semantic truth.

`full-context` explicitly retains the complete retained-input plus every retrieved
candidate-body contract. It can be selected globally or through a profile's `work`
override. Results without `selection` use that contract; never omit a progressive
selection to bypass screening or reuse a hash from the other read scope.
