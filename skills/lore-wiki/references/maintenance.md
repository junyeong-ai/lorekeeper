# Bounded knowledge maintenance

This operation is a complete run of one batch, not a request to audit the entire vault.
The scheduled session already holds the workflow lease. Follow the shared page and
publication contracts in SKILL.md and read the curation reference before repairs.

1. Run `lore wiki audit-plan` once. When the caller supplies exact batch addresses,
   use repeated `--path` for those addresses instead of choosing a new batch.
   Keep its candidate addresses as this run's scope.
   `graph.audit.batch_size` bounds subject count (1..100). Selection preserves fair
   pending order, including oversized subjects. Do not expand the batch to reduce
   the whole backlog to zero. An empty candidate list reports current coverage;
   it is not a guarantee that all statements are true.
   Candidates identify `kind: concept` or `kind: document`; document subjects are
   declared source documents under the configured wiki documents collection, including
   nested addresses. Aggregate counts cover both kinds; `counts_by_kind` separates
   their coverage. A document needs no corresponding concept to be selectable.
   Check `input.status` before reading: `oversized` retains the exact candidates
   because their complete page, scoped review/evidence and selected RSS payload bytes exceed
   `graph.audit.max_input_bytes`. Report the measured bytes and configured bound;
   do not truncate evidence or replace selected subjects. Use `lore work plan --path
   SUBJECT` and the bounded-work protocol linked from SKILL.md. The native controller
   uses fresh configured agent sessions per node and verifies complete reduction
   before accepting an assessment. The whole-input bound remains visible; it is
   distinct from `execution.work` request/result bounds. An execution profile changes
   the agent, not these bounds.
2. Read each complete selected subject according to its `kind`.
   For a concept, the candidate's `evidence` is the complete graph-derived set.
   Read that actual evidence. For independent
   discovery, `wiki citations CONCEPT --snapshot ID` paginates that set; do not
   substitute ordinary outgoing Sources links or stop after its first page.
   Use `wiki read` outlines and revision-checked sections. For an RSS event citation,
   read the exact selected event through `wiki original ORIGIN --evidence-concept
   CONCEPT`, including its event ID and payload revision. An uncited concept has an
   evidence gap, not an invitation to infer a source from its filename or keywords.
   For a document, read the complete source page, including its declared ownership,
   status, scope and attribution. The document is the subject, not evidence for
   itself: an empty `evidence` list is expected. Read its explicit `related_pages`
   as context, never as automatically supporting evidence or proof of causation.
   Context links do not authorize a transitive audit or establish that provider
   originals, preserved attachments or external destinations were read.
   Read the candidate's `reviews` too: each dependency exposes the current review,
   recorded citations, current evidence and available verified archives. Use those
   exact paths and file revisions. A missing, invalid or stale scoped review must be
   repaired through a dated reassessment before recording completion; omitting it
   from the submitted `reviews` cannot bypass it. Scope includes declared subjects,
   the subject's `knowledge_review` and reviews retained in its prior receipt.
3. Examine subject, definitions, conditions, effective time and observation vs claim.
   For concepts, verify declared aliases denote the same subject. Read direct related
   context and
   check whether their stated relationship helps answer a concrete future question.
   Similar words, co-citation, one source or a high degree do not decide these issues.
4. Apply supported repairs through `wiki curate` for concept metadata and authored
   reviews, and the typed synthesis queue for concept synthesis/related prose. Use the curation and lore-process
   references; never overwrite owned sections or completion markers. Preserve prior
   judgments and evidence. Reuse an existing scoped review instead of adding a new
   report for each run. A useful subject with missing support deserves recovery or
   an open evidence-gap review, rather than automatic retirement.
   Do not rewrite an imported source to make competing claims agree or manufacture
   a concept to hold a document-only finding. A scoped review may assess document
   paths directly and explicitly pin the actual sources supporting its judgment.
5. Resolve tasks created by this batch through lore-process. Use `wiki audit-plan
   --path SUBJECT` (repeat `--path` for surviving original addresses) to obtain their
   final fingerprints without expanding the batch. For concept subjects after a
   verified merge, confirm
   the old address was retired by that exact merge and its evidence reached the
   survivor. Record the reviewed final survivor as the replacement subject for that
   original batch slot, using its current fingerprints. Reading this survivor does
   not authorize auditing unrelated additional subjects. The native merge records
   the retirement in the same journal; a direct deletion cannot complete a slot.
   A document slot requires a current assessed document at its same exact address;
   concept merge receipts cannot substitute for it.
   Do not substitute a new fingerprint
   without rereading changes. Record a completed assessment only
   when the exact final subject, evidence and related context have been read.

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
for a documented unresolved finding. Follow-up requires a current durable open review
listing the selected concept or document in `review.subjects`; being supporting
evidence alone does not make a page the assessed subject. Keep its evidence, resolution conditions and any
contrary observations concrete. Open is a valid judgment; unreadable evidence or a
failed step must be reported, never recorded as a successful read.

Receipts in `.lorekeeper/knowledge-audit.json` track the latest completed inputs and
pending order, not truth or confidence. Changed subjects, evidence, related context,
or scoped reviews and their evidence reopen work. Changed and first-time work rotate;
unchanged inputs are skipped without an age-based reread. Unchanged open findings
remain discoverable through their reviews without being rewritten every week.

Finish by reconciling backlinks and completing any queued repairs through the
lore-process publication protocol; read that skill only when repairs need queue
processing. Standalone sessions then run `lore wiki refresh`, `lore graph lint`,
`lore wiki reviews` and `lore queue check`. In `lore run knowledge`, the native
controller owns final global checks and reports review health after the semantic
session. Do not repeat these checks or inventory unrelated reviews; all selected
review dependencies still require complete reads and guarded assessment.
Unrelated `needs-reassessment` records remain backlog without expanding this batch.
Invalid records or a blocked repair are failures; preserve their tasks and report
them. The native audit verifier requires current scoped reviews; an unchanged open
finding with a current scoped review remains a valid follow-up assessment.
Report batch coverage, repairs, unresolved findings and remaining backlog separately.
No lint or receipt count certifies that the entire corpus is semantically consistent.
