# Retry, retained input and citation correction

## Contents

- [Retry and queue retirement](#retry-and-queue-retirement)
- [Publication and coordination](#publication-and-coordination)
- [Citation correction](#citation-correction)
- [Current-evidence resynthesis](#current-evidence-resynthesis)
- [Exact retained-input regeneration](#exact-retained-input-regeneration)

## Retry and queue retirement

Only `lore queue prune` retires queue files after classification. It removes stale
and missing-target tasks, archives a run whose remaining tasks are done, and
preserves unfinished or blocked work.

On failure, retain the queue file and report the task ID and error. Apply may have
published other valid results before returning nonzero: inspect status, remaining
results and quarantine before retrying. Reclassify the retained file and skip
completed tasks. For current tasks, capture fresh target and context guards before
reading and generating again. Never attach new revisions to an old draft.

Retry safety comes from classification and guarded publication, not an assumption
that repeated model calls produce identical text. Workers do not edit pages or
request lines. Apply publishes bodies and completion markers together; backlink
reconciliation owns the sources-section body and `source_count`.

## Publication and coordination

Ingest publishes each page/request batch through a recoverable journal and replaces
queue files atomically; the entire ingest is not one transaction. Readers do not
consume an append-in-progress queue file, but an interrupted journal can still
require recovery before processing. Atomic replacement does not freeze target
pages: preserve workflow coordination and exact revision guards during a worker wave.

## Citation correction

For a new bound RSS task, explicitly attribute each selected article using the typed
`rss_evidence` pairs described in `queue-format.md`; never infer an event from its title
or position. Page-only legacy tasks keep their existing guards. Event-aware synthesis
uses the packet's exact selected-original commands and treats the daily as interpretation.
Normal omission is not retraction: a reviewed event pair can be removed with
`graph retract-citation ORIGIN CONCEPT --revision ORIGIN_FILE_REVISION
--concept-revision CONCEPT_FILE_REVISION --event EVENT_ID
--event-revision PAYLOAD_REVISION` after the same pending-work and evidence checks.
Pair-only retraction preserves authored prose links: a partial retraction changes only
the declaration, and the last pair removes only the generated list item. Any surviving
prose link remains page-level evidence. Whole-citation retraction keeps its strict
refusal of prose or other occurrences.

For an established mistaken citation, finish work touching its origin first. Read
the source and the concept's complete synthesis, including its conditions and
boundary cases. A source can establish when a method does not apply; absence of
a positive example does not make that citation mistaken. Judge the evidence
against that scope, not just the concept's title or opening definition. Then preview `lore graph retract-citation
<origin.md> <concept.md> --revision <origin file_revision>
--concept-revision <concept file_revision> --dry-run`, using both revisions from
the reads that informed the decision.
Omit `--dry-run` to publish the reviewed correction. This archives prior evidence
and removes only standalone links in the existing related-concepts list. It never
rewrites concept bodies. Record consequential reasoning in a knowledge review and
run Finalize; an empty extraction cannot retract accumulated citations.

## Current-evidence resynthesis

For a deliberate concept rewrite from its **current citations**, including when its
original task has expired, preview `lore graph resynthesize <exact-concept-path.md> --dry-run`
and run it without `--dry-run`. This preserves the authored bodies and queues one typed
synthesis request; an already-current matching request is reused. Drain and Finalize
normally. Empty or unreadable evidence is refused. This is a new current-evidence
request, not replay of an unavailable historical input.


## Exact retained-input regeneration

Use `lore queue regenerate --task-id <retained-task-id> --dry-run` to preview, then
omit `--dry-run` to request that one task again. The task must exist exactly once in
pending or processed queue files. Its complete original typed input, input hash,
target kind and headings must still agree; synthesis source revisions must remain
current. Missing retained input is a refusal, not permission to remotely re-fetch an
expired source or reconstruct raw events from refined prose.

The command preserves authored bodies, clears only completion/provenance for that
section, and publishes a replacement through the mutation journal. A selected pending
request moves to processed history with its exact bytes; sibling pending lines remain
unchanged. Retirement, completion invalidation and replacement share that journal.
Malformed retained queue lines require repair before regeneration. Process the replacement
normally; never manually clear a marker and re-ingest as a substitute for this operation.
Equivalent pending work must finish and be pruned before another historical request;
selecting that pending task itself remains an explicit replacement. Processed history
permits later regeneration.

New tasks carry `input.generation_policy`, a per-kind semantic policy identifier.
Only apply writes `llm_inputs.<key>_policy_done` from the matched task. Missing policy
is legacy/unknown; it does not make existing answers stale or trigger a global rewrite.
Unknown requested policy versions require a compatible processor. Policy provenance
is separate from the unchanged data cache hash. Explicit regeneration records a
policy request for this input so an old legacy task cannot satisfy the new request.

A staged extraction without its retained request cannot establish the original input
or read scope and is refused, regardless of source size. Restore the exact request from
retained history or backup, or collect the preserved original into a new valid task.
Never fabricate a request/context revision from the current interpretation to publish
an old result. Rejected bytes and their diagnostic remain available for investigation.
