# Queue file lifecycle, recovery, and task schema

Read-once reference for `/lore-process`. The skill body carries the processing
protocol; this file carries the format details you consult only when needed.

## Queue file lifecycle

```
<run>.jsonl.tmp        ←  ingest is mid-flush (transient, sub-second; not for consumption)
<run>.jsonl            ←  pending, ready to be drained by this skill
processed/<run>.jsonl  ←  drained successfully, retained 90 days
(deleted)              ←  pruned by `lore maintenance` after retention expires
```

`lore ingest` sweeps `*.jsonl.tmp` files older than 1 hour at startup (crash debris
from previous runs); `lore maintenance` does NOT touch tmp files, so a concurrent
maintenance run cannot race an active flush. Only `.jsonl` files matter to this skill.

Cooperating ingest runs hold the vault mutation lock before sweeping temporary files;
a paused ingest therefore blocks another ingest from sweeping its active file. The sweep
uses mtime, so external processes and older binaries must not share an active queue writer.
After failure, inspect retained tasks and input before retrying. Refetching the same date
can observe changed remote content; it is not exact historical replay. Use
`lore queue regenerate --task-id ID --dry-run` when the original task is retained and its
target still matches. Missing input must be reported, not reconstructed from generated prose.

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

`input.locale` is on EVERY task, whatever its kind: it is `vault.locale`, the language the
vault is authored in, and it is what every word the drain adds gets written in. See
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

For `synthesize-concept`, `input.citations` is an array of
`{"path": "wiki/documents/spec.md", "revision": "<hash>"}` objects. Paths are relative to
the vault root. `queue status` checks revisions against current source pages; an edit makes
pending work `stale`, and an unreadable source leaves it `unreadable` until repaired.

## Recovering a lost extraction task

When the original extraction task is unavailable, recover from the stored source page;
re-fetching an expired source window may lose content. Run `lore wiki evidence <page>`
before reading it and use `file_revisions[<page>]` as the result's `target_revision`.
Read the current page and produce the complete extraction result described in
`processing-kinds.md`, retaining its concepts input hash, target section and source date.
Do not invent missing source metadata or reuse an old answer with a new revision.
Use a unique recovery task id and apply that result explicitly:
`lore queue apply --recover-extraction <task_id>`.
Other orphan results remain rejected. Unparseable queue lines must be repaired first.
If a rejected result is already in `results/orphaned/`, regenerate it from the current
page in `results/`; preserve the rejected copy and its diagnostic record.

## Synthesis result

Write `.lorekeeper/queue/results/<task_id>.synthesis.json` atomically (write a temporary
file, then rename). Copy `target_revision` from `lore queue status --json` before reading
and drafting. The task supplies the exact source revisions and section anchors.

```json
{
  "task_id": "<task.task_id>",
  "cache_hash": "<task.cache_hash>",
  "target_revision": "<status.tasks entry target_revision>",
  "synthesis": "Section body, without its heading",
  "related": "Section body, without its heading; empty is valid"
}
```

Omit `related` only when the task has no `input.related_anchor`. Do not add level-two
headings inside either body. `lore queue apply` validates the pending task, source revisions,
and target snapshot before publishing both sections and `synthesis_done` in one atomic
file replacement. If the target changed, read it again and regenerate the result; never
update only the revision on an old draft. Apply, prune, and processed-file maintenance share a writer lock;
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
headings. Section bodies cannot introduce sibling level-two headings or unclosed fences.
A changed target requires rereading and regenerating the answer, not copying a new
revision onto old prose. Apply each task before reading another task on the same page.

Malformed JSON and invalid UTF-8 results of every type are preserved byte-for-byte in
`results/corrupt/` with a reason sidecar. True filesystem read failures remain errors.
The first corrupt-file apply reports failure even when quarantine succeeds; retrying
then processes the remaining active results. A missing or inconsistent synthesis
citation payload remains blocked for repair rather than being retired as stale.


## Generation policy provenance and explicit regeneration

New task inputs include `generation_policy` (`summary-v1`, `event-refinement-v1`,
`themes-v1`, `concept-extraction-v1`, or `concept-synthesis-v1`). These identifiers
change only when generation semantics change, independently of binary release numbers.
They are not part of `cache_hash`; old tasks without them remain valid legacy inputs.
Result JSON stays unchanged: apply reads policy from its matching task and writes
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

When the user explicitly requests `gpt-6.1-sol` parallel work, retain that model for
the assigned workers. Give each target one owner, share source-group notes under the
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
