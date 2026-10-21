# Grounded wiki queries

Read this reference for the `lore-wiki query` operation.

## Contents

- [Start a retrieval session](#start-a-retrieval-session)
- [Search and read selected content](#search-and-read-selected-content)
- [Evidence, lifecycle and originals](#evidence-lifecycle-and-originals)
- [Exact body and relationship queries](#exact-body-and-relationship-queries)
- [Answer and retain useful findings](#answer-and-retain-useful-findings)

## Start a retrieval session

Run `lore wiki snapshot` to inspect an existing prepared capture
without rereading the corpus. Its `captured_at` and `verified_at` describe stored
evidence, not current external edits. Pass its `snapshot` to search/browse with
`--snapshot`. Run `lore wiki prepare` when no compatible capture exists or current
collection membership is required; full preparation reads and verifies the corpus.
Prepare again after writes that must enter this investigation. A single current
lookup can use live search without preparing a snapshot. An unchanged capture may
answer historical questions, but never present its membership as freshly verified.
For an unfamiliar topic, start with `lore wiki browse --snapshot <id> --json`
in a prepared session (omit `--snapshot` for a single live lookup).
Use its category/type/tag counts to select a scope with `--category`, `--type` and
repeated `--tag` (all required, case-insensitive). Tags include preserved manual-source
metadata; they do not create concept identity or evidence relationships.
Browse orders by title, without treating citation frequency as relevance.
When time matters, use `lore wiki browse --snapshot <id> --sort created
--from YYYY-MM-DD --to YYYY-MM-DD --json`. Bounds are inclusive; unknown or invalid
dates are excluded by date filters. `created` may describe source authorship, not ingestion.
Without date filters, newest-first order places unknown dates last.
Search and browse return `total` and `next_offset`; follow `--offset` when more
results are needed. `total` counts matches after metadata/date filters; category,
type and tag facets count query matches before those filters. An empty category
key denotes uncategorized pages. Do not call the first page a complete inventory.

## Search and read selected content

Run `lore wiki search "<terms>" --snapshot <id> --json` — it answers which pages the
question reaches and, in `matched`, whether it names the subject (`identity`/`name`), opens
by stating it (`summary`), or merely mentions it (`text`). A whole declared name with
different spacing is `name-variant`: a retrieval candidate, not exact registry ownership. Query the question's terms
separately when it spans several. A query naming one concept matches its declared
names and aliases as phrases; other queries require every term. Markdown formatting
and soft line breaks preserve a phrase; parsed HTML text is searchable too, with
table cells and blocks kept separate. Markup, destinations, scripts and styles
do not contribute text. Separate blocks and separate aliases do not
combine into one. Use `--phrase` when separate words are too broad (for example
`"API 토큰"`). Use `--spacing` to ignore whitespace within a visible block (for example
`"인증정보"`); combine both for a compound phrase. These are explicit lexical operators,
not semantic equivalence or spelling aliases. Exact matches rank ahead of broader
spacing matches, while named subjects retain priority. Ask for the full
registered name to reach its alternate spellings in either language. Open each
selected page with `lore wiki read <path> --revision <file_revision from the hit>`.
Inspect its outline, then read relevant `--section ID` content and follow `next_offset`.
Carry `--revision` through pagination; a mismatch requires fresh retrieval. A section
includes nested subsections. Read the sources behind the claim, not only its synthesis.
For a selected page, `lore wiki context <exact-path> --snapshot <id> --limit 10`
returns links, concept evidence, declared document relations and scoped reviews in
one captured transaction. Each group has its own total and authority; links do not
establish support, and source declarations are not current-truth judgments.
Its `summary` is a bounded opening preview; use revision-pinned reads for full content.
`not-captured` differs from an available empty group. Follow its `next_offset` with
`--offset` in the same snapshot; the offset applies independently to every group.
Use the individual relation commands below when only one group needs expansion.
## Evidence, lifecycle and originals

For a selected concept, use `lore wiki citations <exact-concept-path> --snapshot <id>`
and follow `next_offset` with the same snapshot to discover its complete evidence.
This graph-derived set differs from ordinary explicit link occurrences. RSS entries
retain exact event selectors and payload revisions; `total` counts entries while
`source_count` counts origin pages. A captured citation is not proof of current bytes;
verify selected live pages or immutable originals through their respective contracts.
Normal discovery shows active knowledge. `knowledge_status: retired` preserves a page
and its history but excludes it; use `--knowledge-status retired|all` explicitly to inspect
its `knowledge_review` and scoped `reviews` previews. A single source does not imply poor
knowledge; verify supported scope and future retrieval value. Open or changed assessments
require reading their reasoning, not merely counting them.
Search, browse and context expose concept `synthesis.status` separately from snapshot
currency. `current-input` means the completed synthesis names the captured graph evidence
digest; it does not establish semantic correctness or independent corroboration. `pending`
means the completed input differs or is missing: read the changed sources before using
the synthesis as a present answer. This detects source edits even before backlink
reconciliation updates the recorded request. `untracked` has no recorded synthesis input,
`empty-evidence` has no captured citations, and `invalid-metadata` requires inspecting its
reported issues. `not-captured` means live or lexical-only discovery cannot establish this
input relationship; prepare graph evidence when it matters. `not-applicable` covers other
page kinds. These states do not automatically retire knowledge or settle contradictions.
Hits carry a compact `source_record` lifecycle preview and optional `source_identity`,
`source_file`, `source_url`. Full declarations, relations and original manifests remain
in the page; inspect them only for selected evidence. `metadata_issues` reports malformed
declarations rather than inventing a lifecycle. Use the preview to distinguish
active, archived, superseded and unknown source state;
`--source-status` filters exact recorded states. A historical claim can remain valid.
Manual `source_record.manual_frontmatter` retains unpromoted source declarations;
inspect them directly, since `--source-status` does not classify that nested metadata.
For a preserved manual original, run `lore wiki original <document-path>`.
Follow its member roster with `--offset`; pin `reference.manifest_revision.value`
with `--revision` and select an exact `--member` for a bounded text preview.
`preview_truncated` means the preview is incomplete; `selected_file` addresses
the verified preserved bytes. Check unsupported coverage before claiming every
dependency was captured. `available: false` means no preserved-original reference;
a mutable `source_file` is not a substitute for the historical bytes.
For declared RSS concept evidence, use `lore wiki original <origin-path>
--evidence-concept <concept-path>` to discover its selected articles, then read
`--event EVENT_ID --revision PAYLOAD_REV`. The inventory revision pins pagination;
the payload revision selects the historical article even after the current manifest
changes. Mixed page/event evidence requires both the page and selected articles.
Otherwise, `lore wiki original <daily-path>` first returns a bounded event inventory.
Pin its `revision`, select `--event EVENT_ID`, and follow `--text-offset` until
`next_offset` is null. `page_bound_collection` verifies the referenced adapter-collected
text; it is not provider HTTP/XML. `legacy_unbound` is retained-log discovery only,
not proof of a daily's historical derivation. Acquisition time may be unknown;
`event_timestamp` is published-or-updated adapter time, not acquisition time. A summary omission or a live URL refetch
cannot establish what the original collection contained.
Follow declared document relations with `lore wiki provenance <exact-path> --snapshot <id>`
in the same prepared session (omit the flag for a live collection).
It resolves only exact repository/document identities; missing or ambiguous owners
remain explicit. Use `--direction incoming|both` or `--relation <name>` to narrow,
then paginate with `--offset` and its collection `--revision`. Target candidates
provide paths and file revisions for `wiki read`; neither a declared relation nor
recorded source status establishes support, contradiction or current deployment.
`lore wiki concepts` is the registry for convergence decisions, not a
lookup — reading all of it to find one page is what search exists to replace.
## Exact body and relationship queries

For exact evidence such as a number, code symbol or quoted phrase, use
`lore wiki search "<literal>" --body --snapshot <id> --json`. This searches raw Markdown body lines,
including code and link destinations, with case-sensitive literal matching.
Choose `--regex` or `--ignore-case` explicitly; `--context 0..5` bounds surrounding
lines. Results carry physical line/column, file revision and clipping flags.
Follow `next_offset` for further matching lines, then use revision-checked `wiki read`
to inspect the containing passage. Repeated searches can use `--snapshot <id>` from
`lore wiki prepare` to scan captured bodies without reopening every source file.
Snapshot results retain captured file revisions; a changed live page requires fresh
retrieval before reading. Without a snapshot the scan reads live files. Totals count
matching lines/pages, not claims or semantic-review coverage.
For claims missing from the concept layer, search the same terms with
`--include-sources`; daily observations and managed reviews are then searchable.
Follow relationships with `lore wiki links <page.md> --snapshot <id> --direction both`.
The JSON gives explicit occurrences with source lines, bounded context and captured
revisions. Follow `next_offset` with `--offset` in the same snapshot; inspect selected
sources with revision-checked `wiki read` before asserting meaning. A bare link does
not establish support, contradiction or causality.
For live graph diagnostics, `lore graph --json neighbors <vault-relative.md>` supports
`--type`, `--direction incoming|outgoing|mutual` and pagination; it scans the live graph.
A single live connection can use `lore wiki links <source.md> --target <target.md>`;
pin subsequent pages with its returned `--revision`.
Check `lore wiki reviews` for relevant open reviews or changed evidence before answering.
A full natural-language question may match nothing: search its subjects and claim terms,
then inspect the sources. Report the queries and scope checked when evidence remains absent;
absence in a bounded result set does not establish absence from the vault.

## Answer and retain useful findings

1. Synthesize an answer grounded in vault content. Link each material claim to its
   source using Markdown, for example `[Retry policy](wiki/documents/retry-policy.md)`
   with the actual vault-relative path (AGENTS.md § Links). Include the relevant
   section or source lines in the surrounding text when useful.
2. **Concept enrichment.** If the answer reveals connections between concepts
   that aren't currently linked in their Related sections (heading from
   AGENTS.md), note these as suggested edits (but do NOT auto-apply).
3. **Compounding judgment** — after answering, judge reusability:
   - **Reusable and new** (a decision-relevant synthesis, comparison or evidence gap)
     → first search for an existing exploration addressing the same question. Update
     that assessment when appropriate; create a new **exploration** only for a distinct
     question (path from AGENTS.md). Read AGENTS.md
     for the exploration page format (frontmatter keys, section headings, ownership).
     Ground the assessment in direct source links, with scope and effective dates.
     A generated answer and the documents it summarizes are one derivation chain,
     not independent corroboration. For a durable assessment, record the evidence
     revisions through the Knowledge reviews contract; preserve unresolved differences.
     Before publishing, repeat the original question against the proposed change and
     check whether the cited evidence supports it. Do not treat structural lint as
     evidence that the answer is correct.
     Then **finalize** so the new page leaves the graph clean:
     `lore graph backlinks-sync` (its Grounding links are real citations that
     raise each grounded concept's `source_count`), then `lore wiki refresh` (so SQLite discovery captures the exploration). Run `lore queue status --json` and report any
     synthesis queued by reconciliation as pending. Use `/lore-process` when semantic
     completion is part of the request. Tell the user where the assessment landed.
   - **Ephemeral** (single-fact, navigational lookup) → do not file.

   The judgment is per-answer by the model. No frequency rule, no
   file-everything default.
