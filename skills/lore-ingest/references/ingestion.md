# Ingestion, originals and retries

Contents: per-page publication; original bundles; semantic completion; refetch and replay.

Each source's pages and new semantic requests share a recoverable mutation journal.
Individual file replacement is atomic; the full run contains separate publication batches.
After failure, use retained inputs and pending journals where available. A remote refetch
may return edited, deleted or expired events, so it does not establish exact historical
replay. Failed planning discards that source's buffered requests. Native jobs do not
commit vault edits automatically; review and commit them separately.

1. **Collect observations** before taking the vault writer lock. Acquisition includes
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
use optional configured preprocessing; images use explicitly configured OCR.
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
units only when selected by OCR configuration or an explicit extension list.

Explicit relative Markdown links and supported HTML resources are captured within
the inbox boundary. Missing, escaping or symlink dependencies fail capture. CSS/SVG
and other unsupported resource constructs are reported as incomplete coverage and
keep the entry for retry; HTML `base`, implicit Obsidian basename lookup and local
raw-HTML links in Markdown are refused. Inspect `unsupported` before assuming a
self-contained export. Each entry is a separate observation, not an atomic inbox
snapshot. Default capture limits are 32 MiB per entry, 64 MiB per member, 2048 members,
32 dependency levels and 8192 member records per source. `ingest.originals` configures entry/member/source acquisition limits, with a default
256 MiB per source and a fixed 512 MiB ingest envelope. Captures use private temporary
files; decoded text retains its separate bounds. Each source permits 256 MiB of derived text. Split unlinked batches or provide smaller self-contained
exports when those limits fail.

In **queue mode** (the default), the run leaves summary/concept/work-log
sections empty and emit JSONL tasks. They are NOT knowledge yet — run
**`/lore-process`** afterward to drain the queue (fill summaries, create/merge
concept pages, work-log topic synthesis). Its own Finalize step reconciles the
graph and SQLite discovery. Follow that skill's complete finalization protocol
after publication. A daily run is `lore ingest` → `/lore-process`.

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
- An LLM-owned section (a summary, refined events, concept links) survives a re-render only
  while its `llm_inputs.<key>_done` marker matches the input the render computes. That is the
  ordinary case, and the section is spliced through from the page on disk. Where the marker is
  absent or stale the section is ANSWERED AGAIN, which means it is written empty and re-queued
  — so a body nobody recorded (written by hand, or by a drain that did not stamp) is lost.
  `lore ingest` names each section it is about to empty before it writes; `lore doctor` lists
  the pages in that state.
