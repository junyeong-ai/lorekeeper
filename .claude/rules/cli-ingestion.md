---
paths:
  - "crates/lk-cli/src/commands/ingest.rs"
  - "crates/lk-cli/src/commands/ingest/**"
  - "crates/lk-cli/src/commands/fetch.rs"
  - "crates/lk-cli/src/commands/wiki.rs"
---

## Ingest and originals (`commands/ingest.rs`)

- Full current-day ingest reserves a persistent first-source turn under a short
  mutation session before collection; recover journals and recheck pinned config before
  reading that cursor. Release the claim before source I/O. Acquisition and planning
  share the reserved order; accepted publication stays canonical. Filtered/historical
  runs leave the turn unchanged; previews neither reserve nor recover.
  Ingest captures observations without holding the writer lock. After acquisition,
  recover journals and derive page plans against current pages; never refresh a
  previously captured publication baseline. Pin the selected config path/revision
  across collection, lock waiting and planning. `ingest.source_timeout_seconds`
  applies independently per source; source `collection_timeout_seconds` overrides
  it. Deadlines include retries/pagination/conversion, cancel async waits and reject
  over-budget blocking returns; they are not OS preemption or source completeness.
  `--wait-for-vault` permits publication waiting; dry run never takes that lock.
  Optional `--publication-workflow refuse|wait` fences publication after acquisition,
  leaving source cursor/admission claims and network I/O outside that lease. Native
  daily selects the mode explicitly; ordinary ingest retains its short-writer contract.
  Do not nest the option inside an existing workflow. Dry run ignores both waits.
- Dry run publishes no pages, receipts, cursors, logs or queue writes; real source
  errors still return nonzero. Authentication may persist a required rotated OAuth
  successor. A noop provider makes no promise to queue work.
  Document projection caches are read-only during dry run and input inspection;
  real collection may checkpoint verified units under source-owned cache claims.
- Work-log generation belongs to full ingest only, not filtered runs. A full run
  with partial source failure still writes its work-log and reports failure.
  Startup tmp sweeping/pending diagnostics must preserve recoverable work.
- Collection receipts bind only actually published work-log input hashes and
  normalized per-source personal contributions. Period readiness compares those
  inputs, including ownership and resolved categories; semantic prose edits are
  not new collection evidence. Known contributions without a log block. Missing
  legacy bindings are not adopted, and unknown history is not observed empty.
- Historical date replay never replaces current candidate or appointment snapshots.
  Source recovery needs an explicit start date and per-source day budget. Only
  config-matching observed windows with the same source-observed collection identity,
  closed before acquisition and durably published,
  establish query-day coverage; current snapshots and configured queries remain
  distinct from historical replay. Coverage is not deleted/revision-history proof.
  Resolve current identities per source before replay selection; unavailable identities
  block only that source, while legacy unbound receipts remain unverified. Recovery
  previews may authenticate but never collect content or change canonical state;
  rotating OAuth successors still require durable credential persistence.
- Manual original publication is immutable bytes→complete manifest→page/request
  journal→optional retirement. Keep source ownership separate from original
  references; archive=false still preserves original bytes. Capture limits are
  source-local plus a 512 MiB CLI aggregate bound, not unbounded whole-inbox memory.
- Keep all capture dependency information, even for events removed by the pipeline.
  Referenced entries are not retired; any capture failure holds that source's
  retirement. Attachments are never removed. Retirement rechecks consumed bytes.
- Manual admission reserves fair pending turns under the writer claim, then releases
  it before collection. Journal exact receipts with actual document/request outputs;
  archives and reservations are not completion. Reuse checks the pipeline-owned recipe,
  full preserved bundle and current target ownership. Resolve owners through the
  pipeline at acquisition, publication and retirement;
  refresh per boundary and retain exact target-byte guards. Filesystem aliases share
  an owner; hard-linked addresses do not. Backlog remains explicitly partial; previews
  never reserve or reset state. Preserve admission state across backup/restore.
- `fetch` uses the same unique enabled manual-inbox resolver. Preserve title/source
  URL frontmatter and require body. Publish exclusive collision suffixes
  (8/16/full digest then counter), never overwrite an occupied/nonregular path;
  keep filenames within the byte bound.

## Original query authority

- `wiki original` is metadata-first with explicit member totals/limit/next_offset
  and bounded 64 KiB UTF-8 previews. It does not launch a browser or fall back to the
  mutable inbox. Verification scope must be clear even when only entry bytes are
  checked by the default view.
- RSS selection pins collection revision and EventId; paginate in Unicode-safe
  units. Published/updated is not acquisition time (`captured_at` may be null).
  Root/explicit-root commands need no unrelated config discovery.
- `legacy_unbound` is current retained evidence, not proof of historical page/input
  binding. Never promote it to exact provider-wire/acquisition provenance.
- Typed event tasks validate selected membership and payload: removed pair is stale,
  corrupt selection unreadable. Normalize/merge/repoint typed declarations together with
  links.
  Source counts count origin pages, not selected events.
- GeneratedOnly pair retraction can remove generated occurrences/declarations while
  authored prose still supplies page evidence. Normal whole-link removal remains
  strict; do not use partial declarations to erase authored text.
