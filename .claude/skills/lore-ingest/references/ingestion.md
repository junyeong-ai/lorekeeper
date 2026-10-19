# Ingestion, originals and retries

Contents: per-page publication; original bundles; semantic completion; refetch and replay.

Pages are replaced atomically per file; a run is not a multi-page transaction.
After a write or queue-flush failure, retry from retained inputs where available.
A remote re-fetch may return edited, deleted or expired events, even on the same day,
so byte-identical recovery requires identical retained inputs. Per-source queue
boundaries roll back buffered tasks when source planning fails; the queue is
published only after vault writes succeed. The scheduled pipeline logs an explicit
automatic-commit skip: it cannot prove which working-tree changes belong to its run.
Review and commit vault changes separately; existing staging remains untouched.

1. **Plan each source** — fetch, normalize, intra-batch dedup, classify. A source
   that fails is recorded and skipped; the run continues with the rest.
2. **Write pages** — atomic per file (tmp + rename). Document-unit sources (`manual`
   and `nodex`) write individual pages under `<wiki>/documents/`; day-unit sources
   write `<daily>/{source-id}/DATE.md`. Imported repository documents keep their
   identity and vault address across source renames. Concept pages are a cross-source aggregate, rendered once
   after all sources plan.
3. **Write work-log** — personal events only, and only when the optional `personal:`
   module is configured (a source must be in `personal.tracked_sources` AND match its
   adapter `is_self`; `manual`/nodex/RSS/Drive have no authorship, so never produce work-log
   entries even if listed). FULL ingest only: a source-filtered run skips this step —
   its event set is a structural subset and would overwrite the complete page.
4. **Flush LLM queue** — one atomic JSONL task file (queue mode).
5. **Archive** — eligible `manual` inbox entries move to `archived/{date}/` when
   enabled, only after original preservation, every vault write and the queue flush
   succeeded. Partial dependency coverage, changed bytes, shared entry dependencies
   or a failed entry in the same manual source prevent retirement.
   Existing archive entries are never replaced; identical bytes may reuse an entry.
   An empty inbox means collection finished, not that semantic tasks or reviews finished.

The manual adapter reads top-level UTF-8 Markdown, text and HTML files. Unsupported
formats and subdirectories are not ingestion units; failed reads are reported, and
failure of every eligible read fails the source. Original YAML is preserved under
`source_record.manual_frontmatter` as source evidence, never as page identity or
completion state. Before page publication, the adapter preserves exact entry bytes
and supported local dependencies under `.lorekeeper/evidence/manual/`. Identical
file bytes share storage across bundle revisions. `source_record.manual_original`
identifies the immutable manifest; `lore wiki original <document-path>` verifies it
and lists members. Assets remain in the inbox; they are not independent ingestion units.

Explicit relative Markdown links and supported HTML resources are captured within
the inbox boundary. Missing, escaping or symlink dependencies fail capture. CSS/SVG
and other unsupported resource constructs are reported as incomplete coverage and
keep the entry for retry; HTML `base`, implicit Obsidian basename lookup and local
raw-HTML links in Markdown are refused. Inspect `unsupported` before assuming a
self-contained export. Each entry is a separate observation, not an atomic inbox
snapshot. Capture is bounded to 32 MiB per text, 64 MiB per member, 2048 members,
32 dependency levels and 8192 member records per source. Each source allows
256 MiB of captured bytes and 256 MiB of derived text; captures across an ingest
are limited to 512 MiB. Split unlinked batches or provide smaller self-contained
exports when those limits fail.

In **queue mode** (the default), phases 1–5 leave summary/concept/work-log
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
