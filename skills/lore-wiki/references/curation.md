# Reviewed knowledge curation

Use this operation for authorized repairs identified by an audit. Read the relevant
concept, complete supporting context and any existing review under the workflow
lease before drafting changes. Counts and lexical similarity do not decide meaning.

- Keep a durable subject when it answers a useful future question. A single source
  can support a scoped definition; repeated pages do not prove independent confirmation.
- Recover primary evidence for a valuable subject with a missing definition. Otherwise
  narrow unsupported claims or retain an open evidence-gap review. Missing support
  does not establish falsehood.
- A source-only episode can remain in its source document with its concept retired
  from normal discovery. Preserve the concept address, prose and historical judgments.
- Alias only the same subject and scope. Product vs method, family vs version and a
  homonym need distinct names. Search spelling variants never authorize a merge.

`lore wiki curate --file PLAN.json --dry-run` validates a proposed plan without writing
or recovering journals. Omit `--dry-run` to publish the reviewed plan atomically.
The command archives prior authored revisions and guards all evidence and targets.
A changed revision requires rereading and reassessment, not replacement of the guard.

The JSON contract is:

```json
{
  "evidence": [{"path": "wiki/documents/source.md", "revision": "FULL_FILE_REVISION"}],
  "concepts": [{
    "path": "wiki/concepts/subject.md",
    "revision": "FULL_FILE_REVISION",
    "title": "Source-grounded qualified name",
    "aliases": ["Equivalent source form"],
    "category": "CONFIGURED_CATEGORY_ID",
    "tags": ["specific-theme"],
    "knowledge_status": "retired",
    "knowledge_review": "wiki/explorations/subject-review.md"
  }],
  "reviews": [{
    "path": "wiki/explorations/subject-review.md",
    "revision": null,
    "content": "COMPLETE_AUTHORED_EXPLORATION_WITH_REVIEW_METADATA"
  }]
}
```

`evidence` is nonempty and guards the exact files read (`wiki read.file_revision`).
Concept fields other than path/revision are optional; aliases replace the complete
list, so retain existing forms unless read evidence establishes a wrong or ambiguous
identity. Absence from selected citations is not such evidence; record the uncertainty
without deleting the lookup key. Omit `category` to keep it, use a configured
category ID to replace it, or `null` to clear it. `tags` replaces the complete list;
`[]` clears it. Tags must be unique nonblank single lines. Assign only themes the
concept itself supports; do not copy every source tag onto every extracted concept.
Tags support filtering, while aliases assert equivalent subject identity.
Status changes require a durable review listing the concept in `review.subjects`;
appearance in `review.evidence` alone does not mean the review assesses that concept. `reviews` can create (`revision: null`, must be absent) or replace
(`revision`: current full file hash) complete authored reviews without `llm_inputs`.
An existing generated page or unrelated authored exploration cannot be converted by
this operation; its original ownership is checked before replacing content.
Review metadata follows AGENTS.md: explicit `subjects` scope the assessment without
adding citations; `evidence` pins authored source revisions from `wiki evidence`.
Evidence must match the proposed final state when the same plan changes that page.
For such evidence, write the complete proposed Markdown to a temporary file and use
`lore wiki fingerprint --file DRAFT.md` to obtain its `source_revision`. This hashes
a draft without publishing it; it does not certify the draft or replace current-file
guards. Prefer unchanged primary source evidence and explicit subjects when that
accurately expresses the assessment.
Do not retag old reasoning to a new revision. Archive an existing review and reference
its prior archive in a dated reassessment; curation preserves its bytes automatically.

Curation changes metadata and an optional leading title, never synthesis/related/source
sections or queue completion markers. Use `graph resynthesize` and `/lore-process` for
semantic corrections: preview `lore graph resynthesize wiki/concepts/subject.md --dry-run`,
then omit `--dry-run` to queue current-evidence synthesis. Unlike retained-input
`queue regenerate`, this does not require the original extraction task.
Use revision-checked `graph merge --dry-run` only after comparing
both complete concepts and their sources; shared words or source count are insufficient.
Finish through lore-process's reconciliation and verification protocol, then refresh
retrieval snapshots. New citations do not automatically reactivate retired knowledge.
