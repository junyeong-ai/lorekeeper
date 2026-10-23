# Bounded knowledge maintenance

## Contents

- [Structural preflight and repair](#structural-preflight-and-repair)
- [Related-document discovery](#related-document-discovery)
- [Assessment](#assessment)
- [Operating budget and measurement](#operating-budget-and-measurement)

This operation is a complete run of one batch, not a request to audit the entire vault.
The scheduled session already holds the workflow lease. Follow the shared page and
publication contracts in SKILL.md and read the curation reference before repairs.

## Structural preflight and repair

`lore run knowledge` reconciles the vault and stops before semantic execution when
structural lint fails. Inspect `lore graph --json lint` and its exact affected pages.
Use the audit and curation procedures for evidence-based name, alias or category
corrections; guarded merge and normalization commands handle their respective repairs.
A duplicate name does not establish that two subjects are equivalent. Standalone
audit and curation remain available while native maintenance is blocked. Preserve
revision guards, reconcile the supported repair, then rerun `lore run knowledge`.
Do not bypass the preflight or record an assessment to hide an unresolved violation.

## Related-document discovery

For `discover`, prepare search scope only for the exact selected subjects, then return.
For standalone maintenance, do this before reading the expanded audit inputs when
`candidate.discovery` is null. Native maintenance performs discovery before partitioning.

Read the selected subject's declared names, scope and outline with revision-checked
`wiki read`. Choose distinguishing queries for its claims and conditions, including
the source's names and declared aliases where useful. A query is a retrieval choice,
not a semantic verdict. Do not add synonyms or equivalence as facts merely to find pages.

Stage JSON under `.lorekeeper/`, then run `lore wiki audit-discover --file FILE`:

```json
{
  "path": "wiki/documents/retention-policy.md",
  "file_revision": "FROM_CURRENT_SUBJECT_READ",
  "queries": [{"query": "production order retention", "phrase": false, "spacing": false}]
}
```

Use 1..16 explicit nonblank queries. Native discovery captures every matching knowledge
page, including retired pages, without a top-result cutoff. `wiki audit-plan --path
SUBJECT` then reports the current query set and complete matched context. Read that
expanded input through ordinary or bounded work before assessing. If a query is too
broad, refine it explicitly before assessment; never treat a clipped result as complete.
Stored queries reopen an assessment when matching pages or their authored revisions
change. A changed subject needs fresh query selection. Unrelated corpus changes do not
alone invalidate an assessment. This records the searched scope, not exhaustive semantic
consistency: differently phrased claims outside the selected queries can remain undiscovered.

Discovery does not create citations or semantic graph links. Compare candidates under
their actual scope/time/attribution and record supported relationships or durable reviews
through the normal publication contracts. A `discover` session never records completed
assessments or performs repairs.

## Assessment

1. Run `lore wiki audit-plan --ready` once. When the caller supplies exact batch addresses,
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
   Native selection defers subjects with unfinished semantic work in their actual
   subject/evidence/context/review pages and can select later ready subjects without
   draining unrelated queue work. Newly discovered context can also block a selected
   assessment. `audit-record` rechecks these inputs under the publication locks.
   Complete authored pages need no synthetic completion markers. Do not clear an
   owned input or bypass a blocker to record a receipt; report it for later processing.
   `--ready` adds `readiness` to the ordinary plan. Automatic selection skips blocked
   subjects; explicit `--path` retains every requested candidate, and only
   `readiness.ready` identifies ready subjects. Deferred details include exact input
   revisions and reasons, at most 100 entries with an exact `deferred_count` for the
   checked scope. `scope: encountered` describes automatic selection, not the whole
   corpus; `scope: selection` describes explicit subjects. This read-only preview
   does not reserve turns, acquire a workflow, authenticate, run structural preflight
   or certify discovery/evidence reading. Native admission and publication recheck
   readiness. Omit `--ready` when only the original assessment-coverage plan is needed.
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
   For concepts, verify declared aliases denote the same subject using AGENTS.md's
   Concept convergence rules for existing mappings. Absence from the current citations
   is not a reason to remove an alias; preserve uncertainty unless read evidence
   establishes a wrong or ambiguous identity. Read direct related context and
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
5. Resolve tasks created by this batch through lore-process. Run `lore wiki refresh`
   once after those repairs, before the final audit plan or assessment recording.
   This explicitly prepares complete lexical discovery; recording checks corpus-wide
   pending order, while `--path` plans evaluate only selected discovery recipes and
   report `coverage_scope: selection`. Automatic plans report corpus coverage.
   Use `wiki audit-plan --path SUBJECT` (repeat `--path` for surviving original
   addresses) to obtain their
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
   If repairs changed the subject and `discovery` is null, select queries again against
   its final revision and read the expanded final scope before recording completion.

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
The controller verifies the selected receipts again after global reconciliation;
later queued publications can invalidate a draft or reopen an assessment. Reported
audit progress can coexist with unrelated unfinished queue work; only the controller
determines the overall job outcome.
Unrelated `needs-reassessment` records remain backlog without expanding this batch.
Invalid records or a blocked repair are failures; preserve their tasks and report
them. The native audit verifier requires current scoped reviews; an unchanged open
finding with a current scoped review remains a valid follow-up assessment.
Report batch coverage, repairs, unresolved findings and remaining backlog separately.
No lint or receipt count certifies that the entire corpus is semantically consistent.

## Operating budget and measurement

Use `lore run knowledge --profile NAME --batches N --json` for an explicitly bounded
initial campaign. Each batch releases its workflow lease. A selected audit that
actually completes can advance to the next explicitly budgeted batch despite unrelated
scoped queue rejections; the campaign remains incomplete/nonzero. Fatal or unknown
failures, cancellation, and a batch without verified audit progress stop the campaign
with unfinished work retained. `batch-limit` means the execution
budget was reached; `settled` means no subjects remain pending in the current scope.
Neither outcome certifies truth or a read of every external original.

Measure a small run before selecting a campaign size or schedule. `audit_progress`
reports before/after coverage counts; `semantic_calls` counts agent invocation attempts.
One invocation can make multiple model requests and tool calls, so `execution.work.max_calls`
does not cap provider requests, tokens or billing. `duration_ms` includes waiting and
cleanup; campaigns report total and per-batch time. `semantic_duration_ms` measures
the sum of the batch's agent invocation wall times, including failed or cancelled
attempts. Concurrent evidence observers can make that sum exceed elapsed job time;
it is not provider processing time or billed cost.

Compare actual completions, calls and elapsed time with first-time/changed backlog
and incoming work. `batch_size` bounds selected subjects, not successful completion
or cost: deferred inputs, bounded work and failures change throughput. Keep global
review health separate from whether the selected batch completed.
