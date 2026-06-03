# lk-pipeline

Deterministic transform stages between `lk-source` and `lk-vault`. Shares an
`Arc<PipelineContext>` (engine, llm, dirs, perf, timezone, locale,
concept_categories) with the `Synthesizer`.

- **Streaming sources project their page from an event log.** A complete-refetch source
  renders directly from the fetch. A STREAMING source (`SourceType::is_streaming` — only RSS:
  a rolling, capped feed that can't reproduce a past day) instead projects from `event_log`,
  a durable per-date record of its raw (pre-LLM) events (`.lorekeeper/events/{source}/{date}.jsonl`).
  For those sources `plan` UNIONs the fetch with the stored log by `EventId`
  (`event_log::merge_by_id`, fresh wins so a still-in-feed item picks up an in-place edit),
  persists it (unless dry-run), and renders the page from the merged set — so an item that
  scrolled out of the feed is never lost. This is NOT a suppression cache: it never blocks
  regeneration, it enables it — a deleted page self-heals from the log and `--date` repairs
  any day. The log holds RAW bodies, so a re-render always feeds refine raw text — no per-block
  refine state, no skill change. `read` is STRICT on a corrupt line (errors, leaves the file
  intact) — silently dropping it would re-write the reduced set and lose the event. `manual` is
  exempt (document pages, one per inbox file). Duplication converges at the concept/graph layer
  and is preserved at the raw layer, where every observation is provenance.
- **`Pipeline::plan` is per-source and takes `&mut self`**; one `Pipeline` owns one
  ingest run exclusively (no shared-ref concurrency). It returns that source's daily
  pages and merges any extracted concepts into a **run-level** `ConceptDrafts`
  accumulator. Concept pages are a cross-source aggregate, rendered ONCE via
  `render_concept_pages()` after all sources are planned — never per source (that would
  let a later source's write clobber an earlier one).
- **Materialized-view render**: a daily page is two layers. The **structural** layer
  (frontmatter, raw event list, all `## ` headings) is re-rendered every ingest from
  the template. The **semantic** layer (summary body, refined event bodies, concept
  wiki-links) is owned by the LLM via the queue and **preserved across re-renders**.
  `Pipeline::plan`, `work_log`, and `synthesis` all implement this with:
  1. Compute `Request::cache_hash()` for every LLM task that would fire.
  2. Look up the previous render via the `VaultStore` (the pipeline's vault-read seam,
     backed by `FsVault`; `lk_vault::InMemoryVault` is the in-memory double for store-level
     tests and a future non-filesystem backend) and ask `llm_cache::lookup` whether
     the same hash + a filled section body already exist on disk. If yes, skip the
     LLM enqueue and stash the existing body as `preserved_body`.
  3. Render the fresh template (heading is always emitted; the body is empty when the
     LLM hasn't produced it).
  4. `render::splice_preserved_sections` replaces each fresh empty body with its
     `preserved_body` so the on-disk page round-trips unchanged when nothing changed.
     It returns `Option`: if a cached body's heading is absent from the fresh render
     (a custom template renamed the section), it returns `None` and the caller skips
     the write — keeping the previous page rather than dropping the preserved body.
  5. The fresh frontmatter records the hash in `llm_inputs.<key>` regardless of whether
     the task was enqueued, so the cache is self-perpetuating.
  The pattern applies uniformly to daily, document, work-log, AND synthesis pages —
  every page with an LLM-owned section is a materialized view. `TargetKind::llm_inputs_key`
  is the single source of truth mapping a task kind to its frontmatter key.
  Manual override: a vault editor deleting either the section body or the
  `llm_inputs.<key>` line invalidates the cache for the next run — no flag, no
  skill argument, no out-of-band invalidation API.
  **Two cache shapes (`TargetKind::cache_shape`).** `FillEmpty` kinds (summary,
  concepts, narratives) follow steps 1–5 exactly: the section starts empty, so a
  non-empty body is the completion signal and the pipeline pre-stamps the hash. The
  daily event list is the sole `InPlace` rewrite: its section is non-empty from the
  first render (the raw event list is structural), so completion is tracked by a
  SECOND frontmatter field. The pipeline still pre-stamps `llm_inputs.refine_events`
  with the current-input hash (the stale-task reference point — `llm_inputs.<key>`
  always equals the current input for every kind), and `/lore-process` writes
  `llm_inputs.refine_events_done` once it has rewritten the bodies.
  `llm_cache::lookup_in_place` returns cached only when `refine_events_done` equals
  that current hash. Because `refine_events` is always current, a queued refine task
  whose `cache_hash` differs is unambiguously stale and dropped; a first run, a crash
  before flush, an unprocessed page, OR a changed-input re-ingest all converge (the
  matching task processes, stale ones drop). Force a re-run by deleting the
  `refine_events_done` line.
- **Cache identity vs queue payload**: `Request::cache_identity()` in `lk-queue` is the
  hashable subset that shapes the LLM's output; `Request::task_input()` is the queue
  payload (identity PLUS registry hints like `existing_concepts`). `cache_hash()`
  BLAKE3-128s the identity. `existing_concepts` is excluded from the identity so adding
  a concept anywhere in the vault never invalidates unrelated cache entries; `categories`
  is sorted before hashing so config field order can't perturb the cache.
- **`dedup::deduplicate(events)`** is the ONLY deduplication — pure, stateless,
  in-memory, scoped to ONE source's single fetch. It collapses by EXACT identity ONLY:
  a shared `EventId` (`source:date:hash(external_id)`), keeping the first occurrence. Two
  events merge iff one fetch surfaced the literal same item twice (paginated history
  overlap); since `EventId` IS the identity, this is provably lossless — single-key dedup
  with first-wins can never drop a distinct observation, and has no transitivity gap.
  Nothing else is a merge signal: a shared url, title, or body does NOT merge. The same
  article carried by two RSS feeds keeps both (each feed namespaces its `external_id`, so
  the ids differ → distinct provenance); a recurring same-title event (a daily "Standup")
  always survives. url/title are deliberately NOT dedup keys — a url is not a reliable
  identity (shared meeting links, shared landing pages), so keying on it would false-merge.
  Cross-RUN behaviour is owned by the event log, not by this dedup: an event's date pins it
  to one `<daily>/{source}/{date}` page, the fetch is UNIONed with that date's log, and the
  page re-renders IN FULL from the merged set — so re-runs are byte-identical and late- or
  partially-arriving items accumulate (never deplete). Cross-SOURCE observations are
  deliberately NOT suppressed — the same article in RSS and Slack appears on both source
  timelines (provenance), and `concept-merge` / `backlinks-sync` / `near-duplicate-concepts`
  converge the knowledge losslessly at the page layer.
- **classify**: ownership is decided at the source. Each adapter sets `RawItem::is_self`
  by comparing its structured authorship field (`From` address, message author id,
  issue assignee account, calendar organizer/attendee) to `ExtractContext::identity` —
  exact match, no text heuristics. `normalize` carries it to `Event::is_self`, and
  `assign_personal(events, track_personal)` sets `is_personal` + the `personal` label
  only when the source opts into `track_personal`. A recipient/CC/mention is never the
  author, so it is never personal. `classify_by_keywords` reads `SourceConfig.classify`
  (ordered `Vec<ClassifyRule>`, first match wins) and uses `contains_bounded`
  (standard `\w` token boundary — ASCII-alphanumeric + `_`, so a keyword matches inside
  hyphen/dot compounds like `AI`→`AI-powered`, `GPT`→`GPT-4`; CJK matches across attached
  particles so `검토`→`검토를`/`재검토`) — keyword classification only. Keywords are
  lowercased once per `classify_by_keywords` call, not per event. Classification is purely
  deterministic; an event no rule matches stays uncategorized (general section /
  `uncategorized` work-log) — a safe default with no LLM step.
- **Concept merge** reads existing `created`/`updated` frontmatter (the keys actually
  written), preserves the original title and category (established identity = first
  writer wins). A re-extraction whose category DISAGREES with the established one is a
  genuine conflict — kept established, but `tracing::warn`ed so a possibly-wrong day-1
  assignment doesn't silently calcify. `merge` only widens the `first_seen`/`last_seen`
  window (`observe`); it does NOT count citations. `source_count` is written as `0` by
  the template and owned solely by `lore graph backlinks-sync`, which re-derives it
  exactly from the wikilink graph — so a crash or an idempotent re-ingest can never inflate
  it. Before extraction, `load_existing_concept_refs()` scans the vault's concept
  directory + in-memory drafts and passes them as `existing_concepts` in the LLM
  request, preventing duplicate concept creation.
- **Synthesizer** methods are `try_weekly_synthesis` + `try_*_personal`. Each is a
  materialized view like a daily page: `summarize_section` (or, for weekly themes,
  an inlined `identify_themes` path) computes `cache_hash`, looks up the existing
  page via `lookup`, gates the LLM call on `decision.enqueue()`, and `render_section`
  stamps `llm_inputs.<key>` + splices the preserved body on a cache hit. A transient
  LLM failure yields `SynthesisSection::failed()` (`narrative: None`) so the caller
  skips the page rather than stamping an empty hash. Every `TaskTarget` carries
  `anchor` — the exact `## …` heading resolved from i18n at construction time — so
  the skill never needs a hardcoded kind→heading table.
- **Synthesis fallback cascade** is **per-missing-child**, not all-or-nothing: each
  month of a quarter independently falls back to its weekly reviews when its monthly
  review is absent (`month_child_narrative`), and each quarter of a year falls back to
  its months — then weeks — when its quarterly review is absent (`quarter_child_narrative`).
  So a quarter whose latest month isn't summarized yet still includes that month rather
  than omitting it. A `used_weeks` set dedups a boundary week shared by two adjacent
  fallback months. Raw daily work-log is never fed to a higher-level synthesis (each
  level reads only pre-summarized pages).
- Fallback renderers must emit the same `##` anchors the templates use (so `/lore-process`
  can find the section) and the same frontmatter keys synthesis reads (e.g. work-log
  `categories`).
