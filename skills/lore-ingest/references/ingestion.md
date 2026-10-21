# Ingestion, originals and retries

## Contents

- Publication and original bundles
- [Semantic completion](#semantic-completion)
- [Refetch and replay](#refetch-and-replay)
- [Missing-date recovery](#missing-date-recovery)

Each source's pages and new semantic requests share a recoverable mutation journal.
Individual file replacement is atomic; the full run contains separate publication batches.
After failure, use retained inputs and pending journals where available. A remote refetch
may return edited, deleted or expired events, so it does not establish exact historical
replay. Failed planning discards that source's buffered requests. Native jobs do not
commit vault edits automatically; review and commit them separately.

Ordinary full runs for today reserve their first source under a short mutation claim
before collection, recovering pending journals before recording the next turn.
Acquisition and planning follow that rotation so repeated capacity failures cannot
always exclude the same later source. The claim is released before source I/O.
Filtered and historical runs leave this turn unchanged; dry runs only preview it.
Rejected sources still fail the job and retain their inputs.

1. **Collect observations** without holding the vault writer lock. Acquisition includes
   fetch, retries and local preprocessing within each source's configured budget.
   Failed sources are reported; independent healthy sources can continue.
2. **Plan against current pages** after acquiring the writer lock and recovering
   journals. The selected configuration must remain unchanged. Normalize, deduplicate
   exact source identities and classify. Document-unit sources (`manual`, `nodex`,
   `google-drive`) retain stable ownership under `<wiki>/documents/`; day-unit sources
   write `<daily>/{source-id}/DATE.md`.
3. **Publish pages and requests** through the same journal. Immutable originals precede
   their references. Source pages and JSONL admission commit together; concept pages
   accumulate across sources and render once. A publication failure retains inputs.
4. **Publish the work-log** only for full ingest and configured personal tracking.
   Adapter-owned authorship decides personal events; manual/nodex/RSS/Drive do not
   infer authorship. A filtered run cannot replace a cross-source work-log.
5. **Retire consumed inputs** only after successful publication. Eligible unchanged
   manual entries move to `archived/{date}/` when enabled. Partial dependency coverage,
   changed bytes, shared entry dependencies or a failed entry in the same source prevent
   retirement. Archive collisions never overwrite different bytes. An empty inbox says
   nothing about semantic queue or review completion.

The manual adapter reads top-level UTF-8 Markdown, text and HTML. DOCX/XLSX/PDF
use optional configured preprocessing; images require configured OCR or vision.
See [document preprocessing](../../lore-setup/references/config.md#document-preprocessing)
for setup, format scope and limits. Unsupported formats and subdirectories are not
ingestion units; failed reads and empty binary projections are reported, and
failure of every eligible read fails the source. Original YAML is preserved under
`source_record.manual_frontmatter` as source evidence, never as page identity or
completion state. Before page publication, the adapter preserves exact entry bytes
and supported local dependencies under `.lorekeeper/evidence/manual/`. Identical
file bytes share storage across bundle revisions. `source_record.manual_original`
identifies the immutable manifest; `lore wiki original <document-path>` verifies it
and lists members. Assets remain in the inbox. Images become independent ingestion
units only when selected by OCR/vision configuration or an explicit extension list.

Manual document ownership follows the canonical input file location. Editing the same
file updates its existing owner; renaming or copying it is a different input, even when
its bytes match. Drive and repository documents instead use verified provider/document
identities. Equal titles, URLs or text never authorize an automatic merge.

Explicit relative Markdown links and supported HTML resources are captured within
the inbox boundary. Missing, escaping or symlink dependencies fail capture. CSS/SVG
and other unsupported resource constructs are reported as incomplete coverage and
keep the entry for retry; HTML `base`, implicit Obsidian basename lookup and local
raw-HTML links in Markdown are refused. Inspect `unsupported` before assuming a
self-contained export. Each entry is a separate observation, not an atomic inbox
snapshot. Default capture limits are 32 MiB per entry, 64 MiB per member, 2048 members,
32 dependency levels and 8192 member records per source. `ingest.originals` configures entry/member/source acquisition limits, with a default
256 MiB per source and a fixed 512 MiB ingest envelope. Captures use private temporary
files; manual derived text has a separate 256 MiB source envelope.
`ingest.inputs` independently bounds cumulative serialized observations and projected
planning input (default 64 MiB per source, 256 MiB per run), including retained streaming
events. Failed admission preserves that stage's remaining capacity for independent
sources and keeps the source's inbox entries. These limits do not cap process memory or native
decoder allocations. Split unlinked batches, provide smaller self-contained exports or
deliberately adjust the corresponding configured limit; input is never clipped to fit.

RSS originals retain immutable pre-classification adapter-collected text, not the
provider's HTTP/XML bytes. New bound extraction requires explicit article IDs and
payload revisions for each reported concept; omission is not citation retraction.
Original payload admission retains the 64 MiB source and 256 MiB run bounds. An
oversized source releases its planning reservation so later fitting sources can proceed.
Legacy event logs are retained observations, not proof of a page's historical input.
Use the [query reference](../../lore-wiki/references/query.md) to inspect exact originals.

## Semantic completion

In **queue mode**, collection publishes typed requests for unfinished inputs.
Raw or prior prose can remain; matching completion markers establish reuse.
Run `lore run process` or the `lore-process` skill to publish summaries, concepts
and work-log synthesis. Native `lore run daily` collects and processes in order.
Collection, semantic completion and unresolved reviews are separate outcomes.

Personal period reviews verify actually published work-log inputs against recorded
normalized personal contributions. Changed inputs after filtered collection and
known work without a log block synthesis. Full-source `lore ingest --date DATE`
refreshes this binding only if it actually publishes that day's log. Unknown or
empty historical input does not authorize adopting or deleting retained logs.

## Refetch and replay

- Re-running for TODAY re-fetches the source window. The output is reproducible only
  when the inputs are identical: upstream edits/deletions and rolling windows can
  change it within the same day. Use `--dry-run` to inspect returned event counts,
  and retain a copy before a replacement whose evidence cannot be replayed. Counts
  alone cannot detect changed content. RSS retained event logs offer replay that
  other adapters may not provide.
- `lore ingest --date <past>` re-fetches and re-materializes that day. Complete-refetch
  sources render from the returned window: a Gmail `newer_than:` filter, remote edits,
  or deletions can make it incomplete. RSS instead unions fresh events with that date's
  retained event log, so entries that left the feed remain available. A missing log cannot
  recover entries absent from the feed. Inspect `--dry-run` counts and source replayability
  before replacing historical pages; counts alone do not prove identical content.
- A matching `llm_inputs.<key>_done` marker permits reuse of a completed semantic
  body. Without valid completion, rendering can empty a summary and replace refined
  events with freshly rendered raw events. In queue mode, unfinished enabled work
  is queued or its exact pending request reused.
  Existing valid concept links accumulate independently of completion, including
  when extraction is disabled. Reingestion does not retract those citations.
  Hand-written semantic bodies without valid completion are not protected from
  replacement. `lore ingest` reports bodies it will actually empty; `lore doctor`
  identifies unfinished semantic work. A retained body or link is not renewed approval.

## Missing-date recovery

Configure `ingest.recovery` with an explicit `start_date` and a positive
`max_days_per_source_per_run` (default 3). `lore ingest --recover --dry-run` resolves
current provider identity and previews the oldest missing closed dates without
collecting content or changing pages, receipts, queues or cursors. Authentication
may persist a rotated OAuth successor in the credential store. `lore ingest --recover`
requeries them within each source's budget; it cannot combine a source or `--date`.
Native daily runs recovery before ordinary collection. Only matching accepted
publications establish closed query-day coverage; failed/partial attempts remain
pending, and changing source selection does not inherit another selection's proof.
Coverage also requires the same observed provider namespace and account/resource key;
token rotation alone does not change it. Legacy receipts without that binding remain
unverified. Identity failures block only that source's automatic replay and report the
required account/permission check. Limited-scope grants may still collect unbound
through ordinary or explicit dated ingest, but those observations cannot establish
automatic recovery coverage. A changed identity during collection refuses publication.
Retained published inputs remain period-synthesis evidence without asserting the
current provider account.

Gmail, Slack channels, Drive, Calendar, Confluence and Tasks support dated replay.
RSS, Slack search and arbitrary Jira JQL do not establish historical date coverage;
manual/nodex keep current-input semantics. Recovery cannot restore deleted or
intermediate provider revisions, and per-source replay does not rebuild the full
cross-source personal work-log. Collection recovery and missing-period requests
are separate settings; see [scheduling](scheduling.md).
