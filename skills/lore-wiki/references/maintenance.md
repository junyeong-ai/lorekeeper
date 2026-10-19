# Bounded knowledge maintenance

This operation is a complete run of one batch, not a request to audit the entire vault.
The scheduled session already holds the workflow lease. Follow the shared page and
publication contracts in SKILL.md and read the curation reference before repairs.

1. Run `lore wiki audit-plan` once. Keep its candidate addresses as this run's scope.
   `graph.audit.batch_size` bounds semantic work (1..100). If no candidate exists,
   report the returned counts and stop successfully. Do not expand the batch to
   reduce the whole backlog to zero.
2. For each candidate, read the complete concept and the actual evidence it cites.
   Use `wiki read` outlines and revision-checked sections. For an RSS event citation,
   read the exact selected event through `wiki original ORIGIN --evidence-concept
   CONCEPT`, including its event ID and payload revision. An uncited concept has an
   evidence gap, not an invitation to infer a source from its filename or keywords.
3. Examine subject, definitions, conditions, effective time and observation vs claim.
   Verify declared aliases denote the same subject. Read direct related concepts and
   check whether their stated relationship helps answer a concrete future question.
   Similar words, co-citation, one source or a high degree do not decide these issues.
4. Apply supported repairs through `wiki curate` for metadata/reviews and the typed
   synthesis queue for synthesis/related prose. Use the curation and lore-process
   references; never overwrite owned sections or completion markers. Preserve prior
   judgments and evidence. Reuse an existing scoped review instead of adding a new
   report for each run. A useful subject with missing support deserves recovery or
   an open evidence-gap review, rather than automatic retirement.
5. Resolve tasks created by this batch through lore-process. Use `wiki audit-plan
   --path CONCEPT` (repeat `--path` for surviving original addresses) to obtain their
   final fingerprints without expanding the batch. After a verified merge, confirm
   the old address was retired by that exact merge and its evidence reached the
   survivor; omit the removed address from receipt submission. Reading a survivor
   outside the original batch is required merge verification, not permission to
   enlarge the remaining audit batch. Do not substitute a new fingerprint
   without rereading changes. Record a completed assessment only
   when the exact final concept, evidence and related context have been read.

Submit JSON via `lore wiki audit-record --file RESULTS.json`:

```json
{
  "assessments": [{
    "path": "wiki/concepts/subject.md",
    "file_revision": "FROM_CURRENT_AUDIT_CANDIDATE",
    "input_revision": "FROM_CURRENT_AUDIT_CANDIDATE",
    "disposition": "follow-up",
    "reason": "The source states a proposal; the concept describes an observed outcome.",
    "reviews": ["wiki/explorations/subject-review.md"]
  }]
}
```

Use `reviewed` after checking supported scope and completing repairs, or `follow-up`
for a documented unresolved finding. Follow-up requires a current durable review
listing the concept in `review.subjects`; being supporting evidence alone does
not make a concept the assessed subject. Keep its evidence, resolution conditions and any
contrary observations concrete. Open is a valid judgment; unreadable evidence or a
failed step must be reported, never recorded as a successful read.

Receipts in `.lorekeeper/knowledge-audit.json` track the latest completed inputs and
pending order, not truth or confidence. Changed concepts, evidence, related concepts,
or missing/stale finding reviews reopen work. Changed and first-time work rotate;
unchanged inputs are skipped without an age-based reread. Unchanged open findings
remain discoverable through their reviews without being rewritten every week.

Finish by reconciling backlinks and completing any queued repairs through the
lore-process finalization protocol, then run `lore wiki refresh`, `lore graph lint`,
`lore wiki reviews --check` and `lore queue check`. A blocked or failed repair is
not successful completion; preserve its tasks and report the failure. An unchanged
open finding with a current scoped review remains a valid follow-up assessment.
Report batch coverage, repairs, unresolved findings and remaining backlog separately.
No lint or receipt count certifies that the entire corpus is semantically consistent.
