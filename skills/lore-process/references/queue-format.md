# Queue file lifecycle, recovery, and task schema

## Contents

- [Queue lifecycle](#queue-file-lifecycle)
- [Task schema](#queue-task-schema)
- [Completion keys](#completion-keys)
- [Lost extraction recovery](#recovering-a-lost-extraction-task)
- [Synthesis results](#synthesis-result)
- [Section results](#single-section-result)
- [Policy and regeneration](#generation-policy-provenance-and-explicit-regeneration)
- [Evidence packets](#read-only-evidence-packets)

Read-once reference for `/lore-process`. The skill body carries the processing
protocol; this file carries the format details you consult only when needed.

## Queue file lifecycle

```
<batch>.jsonl            ← pending requests, published with their target pages
processed/<batch>.jsonl  ← fully answered batch, retained for configured history
(deleted)                ← maintenance expires processed history after retention
```

Page edits and newly admitted requests share one recoverable mutation journal.
Only root-level `.jsonl` files are pending work; names are generated batch addresses,
not task identity. Default processed-history retention is 90 days.
Older standalone queue flushes may leave `.jsonl.tmp` files. Ingest sweeps those older
than 1 hour under the vault writer lock; maintenance preserves temporary files.
Cooperating queue producers hold the vault lock first. External writers and older
binaries must not share an active writer directory.
After failure, inspect retained tasks and input before retrying. Refetching the same date
can observe changed remote content; it is not exact historical replay. Use
`lore queue regenerate --task-id ID --dry-run` when the original task is retained and its
target still matches. Missing input must be reported, not reconstructed from generated prose.

If several extraction result files carry the same task/input identity, apply folds
identical typed values once and retires every matching file only after publication.
Different values remain unpublished for review. Reconcile all retained proposals to
one agreed value before retrying; adding a third draft does not resolve the conflict.

## Queue task schema

Each line in a queue file is one task. All `vault_path` values are vault-relative paths
resolved by the pipeline from `vault.dirs.*` — never construct them manually; use the
path patterns from AGENTS.md.

```json
{
  "task_id": "sum-2026-05-23T07-00-00Z-pid4211-0-000",
  "kind": "summarize",
  "created_at": "2026-05-23T07:00:00Z",
  "cache_hash": "0123456789abcdef0123456789abcdef",
  "input": {
    "text": "<events concatenated for summarization>",
    "max_sentences": 5,
    "locale": "ko",
    "source_type": "rss"
  },
  "target": {
    "vault_path": "<daily>/ai-news/2026-05-23.md",
    "kind": "daily-summary",
    "anchor": "## Summary"
  }
}
```

`kind` values: `summarize` | `extract-concepts` | `identify-themes` | `refine-events` |
`synthesize-concept`

`target.kind` values: `daily-summary` | `daily-refine-events` | `daily-concepts` |
`document-summary` | `document-concepts` | `weekly-synthesis-themes` |
`weekly-review-narrative` | `monthly-review-narrative` |
`quarterly-review-narrative` | `annual-review-narrative` | `work-log-synthesis` |
`concept-synthesis`

`input.locale` is on every task and supplies the configured language for generated prose.
Established concept bodies retain their existing language under the per-kind policy. See
[processing-kinds.md](processing-kinds.md) § Output language.

`target.anchor`: the exact section heading (e.g. `"## Summary"`, or its localized form per
AGENTS.md) the pipeline wrote, resolved from i18n at queue time. Always use this as the
locate key — never hardcode headings per `target.kind`.

`input.related_anchor` is on a `synthesize-concept` task only, and holds the concept page's
related-concepts heading as THAT page spells it (absent where the page carries no such
section). One set of source revisions is what both sections answer to, so one task fills both and one
`synthesis_done` answers for the pair. It is payload rather than cache identity: a heading
says where an answer lands, not what the answer would be.

`cache_hash` is BLAKE3-128 (32 hex chars) of the cache-identity subset of `input` — the
fields that decide whether the output would differ, which is narrower than the payload. It
excludes the routing field `source_type`, and it includes
`locale` for the kinds writing a section that is re-derived from its input (a summary, a
week's themes) while excluding it for the two that write onto a concept page, whose authored
body a locale switch deliberately leaves standing. It equals the value the pipeline wrote
into `target.vault_path`'s `llm_inputs.<key>` frontmatter at queue time. The skill MUST
consult `queue status` before generating a result. Normal status/apply reconstruct the typed
request and recompute its identity; invalid or mismatched inputs are `unreadable` and remain
pending even when page markers say done. Never repair a hash merely to make an old answer
applicable. See the stale-task guard in the skill body.

Extraction carries `input.concept_policy` (`existing-only` or `supported-new`) in
its cache identity and result. A policy change requires a fresh extraction. Missing
or disabled policies are invalid; never assume permission to create concepts.

For `synthesize-concept`, `input.citations` is an array of
`{"path": "wiki/documents/spec.md", "revision": "<hash>"}` objects. Paths are relative to
the vault root. `queue status` checks revisions against current source pages; an edit makes
pending work `stale`, and an unreadable source leaves it `unreadable` until repaired.

New bound RSS extraction uses `concept-extraction-rss-v3` with a typed
`input.rss_original` manifest. Read selected originals from that inventory; return a
nonempty `rss_evidence: [{"event_id":"...","revision":"<payload revision>"}]` on every
reported concept after verifying that the selected articles support it. A result with
no concepts is valid; omitting a concept never retracts its prior selections. Legacy/page-only tasks keep
their original schema and policy; never invent an RSS binding from a title, URL or date.

`concept-synthesis-rss-v2` may mix old page citations with event citations:
`{"path":"<origin daily>","revision":"<64-hex payload revision>","rss_event":{"schema":"rss-event-evidence-v1","event_id":"..."}}`.
The optional selector is absent from legacy JSON and its hash remains unchanged.
Event revisions identify immutable collected observations, not the daily interpretation.
Use packet `event_evidence[].selected_event_command` and follow text pagination.
`wiki original ORIGIN --evidence-concept CONCEPT` discovers declared pairs with bounded
pagination; selecting `--event ID --revision PAYLOAD_REV` reads that exact historical
selection even when the current collection manifest has moved. The inventory revision
pins inventory pagination; it is not the selected payload revision. Validate current task
status and target bytes normally. Do not replace a stale answer's guards.

## Completion keys

Only `lore queue apply` publishes these markers with their section bodies.
Workers never edit them. Matching input and completion values establish publication
for that input; a filled body alone does not.

| target.kind | input key | completion marker |
|---|---|---|
| `daily-summary`, `document-summary` | `summary` | `summary_done` |
| `daily-refine-events` | `refine_events` | `refine_events_done` |
| `daily-concepts`, `document-concepts` | `concepts` | `concepts_done` |
| `concept-synthesis` | `synthesis` | `synthesis_done` |
| `work-log-synthesis` | `topic_summary` | `topic_summary_done` |
| `weekly-synthesis-themes` | `themes` | `themes_done` |
| `weekly-review-narrative`, `monthly-review-narrative` | `narrative` | `narrative_done` |
| `quarterly-review-narrative`, `annual-review-narrative` | `narrative` | `narrative_done` |

## Recovering a lost extraction task

Publication requires the original retained, validated task and the read scope captured
before drafting. A current source page cannot establish a lost request's original input
or candidate/review context. Restore the exact retained request from pending/processed
history or a backup and validate it before generating a new result. If that input is no
longer available, collect the preserved original as a new valid task; do not reconstruct
raw evidence from generated prose or relabel the old answer. Rejected orphan results and
their reasons are preserved in `results/orphaned/`. Unparseable pending lines require repair
before absence of a matching task can be established.

## Synthesis result

Write `.lorekeeper/queue/results/<task_id>.synthesis.json` atomically (write a temporary
file, then rename). Before reading and drafting, run `lore work context --task-id ID`.
Copy its `subject.target_revision` and `context_revision`, then read the selected
resource roster and scoped review dependencies. This read-only command does not
create a work plan or certify a read. The task supplies exact source revisions and
section anchors; the context also binds related pages, review membership and the
selected configuration. Bounded finalizers use the same `context_revision` from
`work plan` or `work status`.
Context reports resource and part counts before disclosing bodies. When
`requires_bounded_work` is true, `resources` is null; execute `plan_command` and
complete its exact bounded inspections instead of loading a growing resource roster.

```json
{
  "task_id": "<task.task_id>",
  "cache_hash": "<task.cache_hash>",
  "target_revision": "<work context subject.target_revision captured before reading>",
  "context_revision": "<work context revision captured before drafting>",
  "synthesis": "Section body, without its heading",
  "related": "Section body, without its heading; empty is valid"
}
```

Omit `related` only when the task has no `input.related_anchor`. Both bodies follow
the [section-containment rules](#single-section-result). `lore queue apply` validates the pending task, source revisions,
and target/read-scope snapshot before publishing both sections and `synthesis_done` in
one atomic file replacement. New, changed or removed scoped reviews and changed context
invalidate the draft. Read the changed scope and regenerate; never replace only a guard
on old prose. Apply, prune, and processed-file maintenance share a writer lock;
external editors do not participate, so avoid editing the target during publication.


## Single-section result

Existing queued tasks need no migration. For `summarize`, `identify-themes`, and
`refine-events`, write `.lorekeeper/queue/results/<task_id>.section.json` atomically:

```json
{
  "task_id": "<task.task_id>",
  "cache_hash": "<task.cache_hash>",
  "target_revision": "<queue status target_revision captured before reading>",
  "body": "Replacement section body without the target heading; empty is valid"
}
```

The pending task supplies the destination, target kind, anchor and completion key;
never add these fields to this strict result contract. `queue status --json` reports
`target_revision` for every readable target. Only `queue apply` publishes the body
and marker together after exact revision validation. No worker writes target pages.
Refinement may change bodies but must preserve the original sequence of `###` event
headings. Section bodies must remain beneath their H2: no root H1/H2 headings
(including Setext) or HTML/code blocks consuming the next section. Valid HTML and
headings inside quoted/code blocks are allowed. The publisher rejects invalid bodies
without rewriting their prose; staged results remain available for correction.
A changed target requires rereading and regenerating the answer, not copying a new
revision onto old prose. Apply each task before reading another task on the same page.

Use the canonical result filename above. If copied or renamed result files name the
same current task and input, equivalent typed values publish once; conflicting
values remain unpublished and every proposal is kept for review. Reconcile them to
one agreed value before retrying. Unrelated valid tasks can still publish.

Malformed JSON and invalid UTF-8 results of every type are preserved byte-for-byte in
`results/corrupt/` with a reason sidecar. True filesystem read failures remain errors.
The first corrupt-file apply reports failure even when quarantine succeeds; retrying
then processes the remaining active results. A missing or inconsistent synthesis
citation payload remains blocked for repair rather than being retired as stale.


## Generation policy provenance and explicit regeneration

New task inputs include `generation_policy` (`summary-v1`, `event-refinement-v1`,
`themes-v1`, `concept-extraction-v2`, or `concept-synthesis-v1`). These identifiers
change only when generation semantics change, independently of binary release numbers.
They are not part of `cache_hash`; old tasks without them remain valid legacy inputs.
Apply reads generation policy from the matching retained task and writes
`llm_inputs.<key>_policy_done` (or `legacy` when no policy was recorded).

`queue regenerate --task-id <exact-retained-id>` selects one pending/processed task,
validates its complete original payload through the request's existing cache identity,
and requires the current page input marker and all task headings to match. Concept
synthesis also revalidates every source revision. Ambiguous IDs, missing raw inputs,
and stale sources are refused without remote fetching. Regeneration selects the current
policy; a historical retained policy does not change the verified data input. Processing
an unknown requested policy is refused.

Regeneration journals completion invalidation before fresh task publication while
holding vault then queue claims. Bodies are preserved. The new task records
`regenerated_from`. An explicit `<key>_policy_requested` plus
`<key>_policy_requested_input` excludes old-policy work only for that selected input;
apply clears these request fields when it publishes the answer. Absent provenance
never schedules a whole-vault regeneration.

## Read-only evidence packets

For parallel synthesis, export selected **current** tasks with
`lore queue packet --task-id <id> [--task-id <another-id>]`. Group work using each
entry’s `source_indices`: the shared `sources` table lists each exact
`(path, authored_revision, file_revision)` once. Each task keeps its original guards,
`target_revision` and generation policy; absent policy remains legacy/unknown.
Default output is metadata. Add `--include-content` only when source and target
bytes are needed in a user-designated local artifact or worker context; it includes
those bytes in stdout. Export creates no permanent blob store or read ledger.

A packet is input evidence, **not a read receipt or completed target judgment**.
An existing complete-source read can be reused for the same exact `file_revision`,
with its reader/run and notes attributed accurately. Another worker’s accepted
assessment is a reviewed baseline, not your own full read. Authored revision equality
alone does not prove byte equality: bookkeeping is excluded from that revision.
For changed bytes, review the actual complete diff and relevant original context;
new claims or previously unread sources require their full supporting context.
Every worker still reads its current target and judges its claims and relations
individually. No command certifies this semantic work from hashes or receipt counts.
Capture `lore work context --task-id ID` before each target's evidence reading and
carry its `context_revision` into the synthesis result; packet export does not bind
the scoped review/context membership for publication.

Preserve an explicitly requested worker model. Give each target one owner, share
source-group notes under the
conditions above, and have one coordinator run apply and Finalize. Workers stage only
the existing typed result envelopes and send review proposals separately; packet
metadata or reading notes never become extra result fields.

Export rereads sources, targets and selected tasks before emitting JSON. A pending
mutation journal or changed input is refused; export neither recovers the journal nor
locks an immutable snapshot. Validate revisions again before reuse. If evidence changes,
the coordinator runs backlinks-sync and status, reissues affected tasks, and exports a
new packet; workers reread the changed context and regenerate affected drafts instead
of retagging old prose. For an explicit policy rewrite of **unchanged retained input**,
preview `lore queue regenerate --task-id <retained-id> --dry-run`, then omit
`--dry-run`; process its new task normally. Neither path remotely re-fetches sources.
