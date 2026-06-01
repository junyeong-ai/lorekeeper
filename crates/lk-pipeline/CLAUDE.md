# lk-pipeline

Deterministic transform stages between `lk-source` and `lk-vault`. Shares an
`Arc<PipelineContext>` (engine, llm, dirs, perf, timezone, locale,
concept_categories) with the `Synthesizer`.

- **`Pipeline::plan` is per-source and takes `&mut self`**; one `Pipeline` owns one
  ingest run exclusively (no shared-ref concurrency), so the plan→commit dedup window
  needs no lock. It returns that source's daily pages and merges any extracted concepts
  into a **run-level** `ConceptDrafts` accumulator. Concept pages are a cross-source
  aggregate, rendered ONCE via `render_concept_pages()` after all sources are planned —
  never per source (that would let a later source's write clobber an earlier one).
  `commit()` records dedup and must run only after writes + flush succeed.
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
- **`new_dry_run`** opens the dedup cache read-only (no file creation).
- **DedupCache**: cascade is `event-id → content-hash → url` (first match =
  duplicate). Every strategy is an EXACT match, so dedup is lossless: it merges only
  provably-identical observations and never collapses two distinct records (a false
  merge would silently drop an observation, and a dropped event never becomes a page or
  gets cited — irrecoverable in an accumulate-and-cite vault). Records that merely share
  a title/headline both survive; downstream concept-merge, `backlinks-sync`, and
  `near-duplicate-concepts` reconcile genuine overlap losslessly.
  `content-hash` is `Some(blake3(date + title + body))` — scoped by `date` so a recurring or
  templated body (a daily digest, a newsletter with a constant subject) observed on a
  DIFFERENT day is a distinct observation, not a silent cross-day merge. It is `None` when
  the body has no substantive text: a shared title alone is not content-equivalence (two
  distinct posts can share a headline), so title-only events are excluded from the
  content-hash strategy and fall through to url/event-id rather than being falsely merged.
  `dedup` returns `{novel, duplicates}`; `commit` records novel AND re-records duplicates (upsert) to
  refresh `seen_at`, so a steady-state re-arrival never ages past retention and
  re-emits as new. Persisted-table lookups are gated on the cache being present, but
  the intra-batch (seen-id/url) checks ALWAYS run — so a dry-run with no
  cache still matches a real run. URLs are canonicalised before lookup and storage:
  http→https, host lowercase, trailing slash removal, auth stripping, tracking-param
  removal (built-in `utm_*`, `fbclid`, `gclid`, `igshid`, `ref_src`, … — single-letter
  ambiguous params like `si` are deliberately kept (host-specific resource selectors) PLUS
  `dedup.extra_tracking_params` from config, where a trailing `*` is a prefix match)
  with resource-identifying params preserved and sorted. Pure anchor fragments
  (`#section`, `#L42`) are dropped, but fragments that carry resource identity are
  PRESERVED — SPA hash routes (`#/issues/1`, `#!/path`) and selector fragments
  (`#gid=1` for a sheet tab, `#tab=2`) — since stripping them would merge distinct
  resources and silently drop one observation. The cache is recreated only on a recoverable mismatch — a schema-type
  change or an outdated on-disk format after a redb major upgrade
  (`DatabaseError::UpgradeRequired`) — never on I/O/corruption errors. On recreation
  the stale file is renamed to `*.redb.backup.{timestamp}-pid{pid}` (not deleted),
  preserving dedup history for manual recovery.
- **classify**: ownership is decided at the source. Each adapter sets `RawItem::is_self`
  by comparing its structured authorship field (`From` address, message author id,
  issue assignee account, calendar organizer/attendee) to `ExtractContext::identity` —
  exact match, no text heuristics. `normalize` carries it to `Event::is_self`, and
  `mark_personal(events, track_personal)` sets `is_personal` + the `personal` label
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
  exactly from the wikilink graph — so a crash or `--force` re-ingest can never inflate
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
