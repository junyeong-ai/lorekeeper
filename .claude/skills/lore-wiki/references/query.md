# Grounded wiki queries

Read this reference for `/lore-wiki query`.

## Contents

- [Gather and answer](#gather-and-answer)
- Source metadata, original evidence and bounded relation traversal
- Existing assessments and reusable follow-up knowledge

## Gather and answer

Answer a question grounded in vault content, with compounding.

1. **Gather context.** For several searches in one investigation, run `lore wiki prepare`
   once and pass its `snapshot` to search/browse with `--snapshot`. This is a captured
   corpus, not a claim of current state. Prepare again after writes or before a new
   investigation. A single lookup can use live search without preparing a snapshot.
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
   results are needed. Counts describe the query before metadata filters. An empty
   category key denotes uncategorized pages. Do not call the first page a complete inventory.
   Run `lore wiki search "<terms>" --snapshot <id> --json` — it answers which pages the
   question reaches and, in `matched`, whether each ANSWERS to it (`identity`/`name`), opens
   by stating it (`summary`), or merely mentions it (`text`). A whole declared name with
   different spacing is `name-variant`: a retrieval candidate, not exact registry ownership. Query the question's terms
   separately when it spans several. A query naming one concept matches its declared
   names and aliases as phrases; other queries require every term. Markdown formatting
   and soft line breaks preserve a phrase; separate blocks and separate aliases do not
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
   Normal discovery shows active knowledge. `knowledge_status: retired` preserves a page
   and its history but excludes it; use `--knowledge-status retired|all` explicitly to inspect
   its `knowledge_review` and scoped `reviews` previews. A single source does not imply poor
   knowledge; verify supported scope and future retrieval value. Open or changed assessments
   require reading their reasoning, not merely counting them.
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
   Snapshot link pagination uses the same snapshot ID and offset; do not combine `--snapshot` with the live `--revision` flag. A full natural-language question may match nothing: search its subjects and claim terms,
   then inspect the sources. Report the queries and scope checked when evidence remains absent;
   absence in a bounded result set does not establish absence from the vault.
2. Synthesize an answer grounded in vault content. Cite sources using
   link format (AGENTS.md § Links).
3. **Concept enrichment.** If the answer reveals connections between concepts
   that aren't currently linked in their Related sections (heading from
   AGENTS.md), note these as suggested edits (but do NOT auto-apply).
4. **Compounding judgment** — after answering, judge reusability:
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
     raise each grounded concept's `source_count`), then `lore wiki refresh` (so SQLite discovery captures the exploration and configured exports are refreshed). Run `lore queue status --json` and report any
     synthesis queued by reconciliation as pending. Use `/lore-process` when semantic
     completion is part of the request. Tell the user where the assessment landed.
   - **Ephemeral** (single-fact, navigational lookup) → do not file.

   The judgment is per-answer by the model. No frequency rule, no
   file-everything default.
