# Retry, retained input and citation correction

Contents: queue retirement; RSS attribution; citation retraction; current-evidence rewrite; exact regeneration.

## Idempotency contract

For a new bound RSS task, explicitly attribute each selected article using the typed
`rss_evidence` pairs described in `queue-format.md`; never infer an event from its title
or position. Page-only legacy tasks keep their existing guards. Event-aware synthesis
uses the packet's exact selected-original commands and treats the daily as interpretation.
Normal omission is not retraction: a reviewed event pair can be removed with
`graph retract-citation ORIGIN CONCEPT --revision FILE_REVISION --event EVENT_ID
--event-revision PAYLOAD_REVISION` after the same pending-work and evidence checks.
Pair-only retraction preserves authored prose links: a partial retraction changes only
the declaration, and the last pair removes only the generated list item. Any surviving
prose link remains page-level evidence. Whole-citation retraction keeps its strict
refusal of prose or other occurrences.

Only `lore queue prune` retires queue files after classification. It removes stale
and missing-target tasks, archives a run whose remaining tasks are done, and
preserves unfinished or blocked work. Failure rules:

- **Any task fails** → leave the queue file in place, report the failed
  `task_id` list to the user, and stop. The next `/lore-process` run
  reattempts the whole file from the start.
- **Re-running on a partially-processed file is safe** because:
  - Section edits replace the body — repeating the edit produces identical
    content. No drift.
  - Concept page merging preserves the original `created` and reuses the
    existing slug; the sources-section body is never written during processing
    (it stays empty for `backlinks-sync` to own), so a re-run touches
    nothing there.
  - `source_count` is never written by processing — new pages start at `0`,
    ingest preserves the existing value, and only `lore graph
    backlinks-sync` computes the authoritative count.
- **Never partially-commit progress** to the queue file itself (no
  `processed.jsonl` sidecar): the source-of-truth is the applied vault state, which
  are themselves idempotent.


## When NOT to invoke

`lore ingest` writes the queue file atomically (temp + rename) after all
vault pages are written, so a concurrent ingest cannot produce a partial
`.jsonl` you might consume mid-write. There is no append-while-reading
hazard. Atomic queue publication does not freeze target pages: serialize ingest
and other page writers with a processing wave so they do not invalidate drafts.

## Regenerating a section

For an established mistaken citation, finish work touching its origin first. Read
the source and the concept's complete synthesis, including its conditions and
boundary cases. A source can establish when a method does not apply; absence of
a positive example does not make that citation mistaken. Judge the evidence
against that scope, not just the concept's title or opening definition. Then preview `lore graph retract-citation
<origin.md> <concept.md> --revision <file_revision from wiki read> --dry-run`.
Omit `--dry-run` to publish the reviewed correction. This archives prior evidence
and removes only standalone links in the existing related-concepts list. It never
rewrites concept bodies. Record consequential reasoning in a knowledge review and
run Finalize; an empty extraction cannot retract accumulated citations.

For a deliberate concept rewrite from its **current citations**, including when its
original task has expired, preview `lore graph resynthesize <exact-concept-path.md> --dry-run`
and run it without `--dry-run`. This preserves the authored bodies and queues one typed
synthesis request; an already-current matching request is reused. Drain and Finalize
normally. Empty or unreadable evidence is refused. This is a new current-evidence
request, not replay of an unavailable historical input.


Use `lore queue regenerate --task-id <retained-task-id> --dry-run` to preview, then
omit `--dry-run` to request that one task again. The task must exist exactly once in
pending or processed queue files. Its complete original typed input, input hash,
target kind and headings must still agree; synthesis source revisions must remain
current. Missing retained input is a refusal, not permission to remotely re-fetch an
expired source or reconstruct raw events from refined prose.

The command preserves authored bodies, clears only completion/provenance for that
section, and publishes a new unique task through the mutation journal. Process it
normally. Never manually clear a marker and re-ingest as a substitute for this operation.

New tasks carry `input.generation_policy`, a per-kind semantic policy identifier.
Only apply writes `llm_inputs.<key>_policy_done` from the matched task. Missing policy
is legacy/unknown; it does not make existing answers stale or trigger a global rewrite.
Unknown requested policy versions require a compatible processor. Policy provenance
is separate from the unchanged data cache hash. Explicit regeneration records a
policy request for this input so an old legacy task cannot satisfy the new request.
