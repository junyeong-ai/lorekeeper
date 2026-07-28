# lk-pipeline

Deterministic transform stages between `lk-source` and `lk-vault`. Shares an
`Arc<PipelineContext>` (engine, llm, dirs, `personal: Option<PersonalConfig>`, timezone,
locale, concept_categories) with the `Synthesizer`. `personal` is `None` for a
domain-neutral engine — then no work-log, reviews, or `is_personal` are produced.

- **Streaming sources project from the event log** (root invariant) — the crate mechanics:
  for a streaming source (`SourceType::descriptor().streaming`, RSS), `plan` UNIONs the fetch with that date's
  `.lorekeeper/events/{source}/{date}.jsonl` by `EventId` (`event_log::merge_by_id`, fresh
  wins so a still-in-feed item picks up an in-place edit), persists it (unless dry-run), and
  renders from the merged set. The log holds RAW (pre-LLM) bodies, so a re-render always feeds
  refine raw text — no per-block refine state. `event_log::read` is STRICT on a corrupt line
  (errors, leaves the file intact) — silently dropping it would rewrite the reduced set and
  lose the event. `manual` is exempt (document pages, one per inbox file).
- **`Pipeline::plan` is per-source and takes `&mut self`**; one `Pipeline` owns one
  ingest run exclusively (no shared-ref concurrency). It returns that source's daily
  pages and merges any extracted concepts into a **run-level** `ConceptDrafts`
  accumulator. Concept pages are a cross-source aggregate, rendered ONCE via
  `render_concept_pages()` after all sources are planned — never per source (that would
  let a later source's write clobber an earlier one).
- **Materialized-view render**: a daily page is two layers. The **structural** layer
  (frontmatter, raw event list, all `## ` headings) is re-rendered every ingest from
  the template. The **semantic** layer (summary body, refined event bodies, concept
  links) is owned by the LLM via the queue and **preserved across re-renders**.
  `Pipeline::plan`, `work_log`, and `synthesis` all implement this with:
  1. Compute `Request::cache_hash()` for every LLM task that would fire.
  2. Look up the previous render via the `VaultStore` seam (`FsVault`; `InMemoryVault` for
     tests) and ask `llm_cache::lookup` whether the page's `*_done` completion marker
     already equals that hash (the body is never consulted). If yes, skip the LLM enqueue
     and stash the existing body as `preserved_body`.
  3. Render the fresh template (heading is always emitted; the body is empty when the
     LLM hasn't produced it).
  4. `render::splice_preserved_sections` replaces each fresh empty body with its
     `preserved_body` so the on-disk page round-trips unchanged when nothing changed.
     It returns `Option`: if a cached body's heading is absent from the fresh render
     (a custom template renamed the section), it returns `None` and the caller skips
     the write — keeping the previous page rather than dropping the preserved body.
  5. The fresh frontmatter records the hash in `llm_inputs.<key>` (the stale-task
     reference) regardless of whether the task was enqueued, and re-emits the `*_done`
     marker on a cache hit, so the cache is self-perpetuating.
  The pattern applies uniformly to daily, document, work-log, AND synthesis pages —
  every page with an LLM-owned section is a materialized view. `TargetKind::llm_inputs_key`
  is the single source of truth mapping a task kind to its frontmatter key.
  Manual override: a vault editor deleting the `llm_inputs.<key>_done` marker line
  invalidates the cache for the next run — wiping the body alone does NOT (the body is
  data, the marker is completion state). No flag, no out-of-band invalidation API.
  **Completion is uniformly marker-signalled (`TargetKind::completion_key`).** Every
  LLM-owned section has an input key (`summary`/`refine_events`/`concepts`/`themes`/
  `topic_summary`/`narrative`) pre-stamped with the current-input hash (the stale-task
  reference — `llm_inputs.<key>` always equals the current input), and a companion
  `llm_inputs.<key>_done` marker `/lore-process` writes when it finishes, EVEN with an
  empty result. There is NO body-emptiness completion signal: a non-empty body never
  means "done", because too many sections can be legitimately empty (a focus-filtered
  summary that matched nothing, an extraction that found nothing, a trivial-only
  work-log, an empty-period review) — a per-kind "can this be empty?" judgment proved
  intractable, so no kind makes it. `llm_cache::lookup` returns cached only when the
  marker equals the current hash, never consulting the body, so an empty-but-done result
  stays cached instead of re-enqueueing forever. The render re-emits each `*_done` marker
  ONLY on a cache hit (where `lookup` proved it equals the current input hash), so a stale
  marker is dropped on a miss instead of riding a changed-input render forward. Every
  template emits its section's `*_done` key conditionally (mirroring the input key), so
  the marker reaches the page — the `empty_*_is_cached_not_re_enqueued` tests guard that
  the template doesn't silently drop it. A queued task whose `cache_hash`
  differs from the input key is unambiguously stale and dropped; a first run, a crash
  before flush, an unprocessed page, OR a changed-input re-ingest all converge. Force a
  re-run by deleting the `*_done` line — wiping the body alone does NOT (it is data, not
  completion state).
- **Cache identity vs queue payload**: `Request::cache_identity()` in `lk-queue` is the
  hashable subset that shapes the LLM's output; `Request::task_input()` is the queue
  payload (identity PLUS `source_type`). `cache_hash()` BLAKE3-128s the identity;
  `categories`
  is sorted before hashing so config field order can't perturb the cache.
- **`dedup::deduplicate(events)`** is the ONLY deduplication — pure, stateless,
  in-memory, scoped to ONE source's single fetch. It collapses by EXACT identity ONLY:
  a shared `EventId` (`source:date:hash(external_id)`), keeping the first occurrence. Two
  events merge iff one fetch surfaced the literal same item twice (paginated history
  overlap); since `EventId` IS the identity, this is provably lossless — single-key dedup
  with first-wins can never drop a distinct observation, and has no transitivity gap.
  Nothing else is a merge signal: a shared url, title, or body does NOT merge. The same
  article carried by two RSS feeds keeps both (each feed namespaces its `external_id`); a
  recurring same-title event (a daily "Standup") always survives. url/title are deliberately
  NOT dedup keys — a url is not a reliable identity (shared meeting/landing links), so keying
  on it would false-merge. Cross-run (event log) and cross-source (concept/graph layer)
  convergence is owned elsewhere: the same item on two sources stays on both timelines
  (provenance) and converges at the page layer via `concept-merge` / `backlinks-sync` /
  `duplicate-concepts`.
- **classify**: ownership (`is_self`, root invariant; set by the adapter) is carried by
  `normalize` to `Event::is_self`; `assign_personal(events, tracked)` sets `is_personal` +
  the `personal` label only when the source is in `personal.tracked_sources` (so an absent
  personal module means no event is ever personal).
  `classify_by_keywords` reads `SourceConfig.classify` (ordered `Vec<ClassifyRule>`, first
  match wins) and uses `contains_bounded` (standard `\w` token boundary — ASCII-alphanumeric
  + `_`, so a keyword matches inside hyphen/dot compounds like `AI`→`AI-powered`,
  `GPT`→`GPT-4`; CJK matches across attached particles so `검토`→`검토를`/`재검토`). Keywords
  lowercased once per call. Deterministic; an unmatched event stays uncategorized (general
  section / `uncategorized` work-log) — a safe default, no LLM step.
- **Canonical event order is single-sourced.** Every page that materializes events sorts
  them through `Event::canonical_cmp` (newest first, ties broken by `id` for a total order)
  BEFORE any bucketing, hashing, or rendering — both the streaming event-log union
  (`merge_by_id`) and the complete-refetch daily path (`plan`). So a page's bytes and its
  LLM-input hash never depend on the adapter/API return order: re-ingesting the same set in
  a different order is byte-identical and enqueues zero tasks. Don't sort events ad hoc
  anywhere else — route through the one comparator.
- **Document slugs are collision-free, identity-aware, across runs.** A document's page slug
  is its title slug, but a same-titled *different* document never overwrites another's page.
  A candidate slug is claimed only when it is free in this batch AND its on-disk page (if any)
  is the SAME document — compared by the page's `source_file`/`source_url` identity. Otherwise
  a suffix from the document's own `EventId` trailing hash is appended and lengthened until the
  candidate is genuinely free (the full hash is unique per document, guaranteeing termination
  and collision-freedom — not a positional counter, and not merely improbable). This catches
  both intra-batch collisions and the cross-run case (a prior run's archived note's page still
  on disk). Re-ingesting the same document reuses its slug (identity match) → idempotent.
- **Concept merge** reads existing `created`/`updated` frontmatter (the keys actually
  written), preserves the original title, category, AND `aliases` (established identity =
  first writer wins) — a synonym a human or `/lore-wiki audit` registered survives the
  re-render instead of being reset to `[title]` (the title is always re-emitted as the
  first alias). A re-extraction whose category DISAGREES with the established one is a
  genuine conflict — kept established, but `tracing::warn`ed so a possibly-wrong day-1
  assignment doesn't silently calcify. `merge` only widens the `first_seen`/`last_seen`
  window (`observe`); it does NOT count citations. `source_count` is written as `0` by
  the template and owned solely by `lore graph backlinks-sync`, which re-derives it
  exactly from the link graph — so a crash or an idempotent re-ingest can never inflate
  it. Duplicate concept creation is prevented skill-side: `/lore-process` loads the
  on-disk concept registry (`lore wiki concepts`) and reuses an established name instead
  of forking a variant — the pipeline embeds no registry in the task.
- **`build_alias_index` seeds every page's own slug before any name.** The index maps
  `slugify(name) → ConceptIdentity` so an extraction naming a synonym lands on the
  established page; a page owning its OWN address is what stops a stale alias elsewhere
  from redirecting a concept away from its page. Deriving that claim from title/aliases
  alone would only protect pages whose names happen to reproduce the stem — and a page
  titled more descriptively than its file (`access-ingress-2axis-model` ←
  "Access × Ingress 2-Axis Deployment Model") has NO such name, leaving its address free
  for another page's alias to take and sending every citation of its own name elsewhere.
  Seeding is unconditional, so the guarantee does not depend on directory read order.
  Two pages claiming the same non-address name stays a `tracing::warn` (first wins,
  deterministic) and is reported deterministically by `lore graph lint`.
- **`theme` vs `topic` are deliberately distinct, not drift.** A weekly-synthesis
  `Theme` (`identify_themes`) is a cross-source cluster spanning a whole week; a work-log
  `topic_summary`/`topic_heading` is a single day's per-source activity grouping. They sit
  at different altitudes (like `category` vs `performance_category`), so the vocabulary is
  kept separate on purpose — do NOT unify them into one term (it would also collide with
  graph "topic communities").
- **Synthesizer** methods are `try_weekly_synthesis` (cross-source themes) +
  `try_{weekly,monthly,quarterly,annual}_review` (personal performance reviews). Each is a
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
  than omitting it. An ISO week straddling a month/quarter boundary is read by both
  adjacent fallback periods: for a narrative rollup that mild redundancy is harmless (the
  parent summary folds it) and dropping it would lose the later period's real activity,
  while the numeric category table is computed separately from raw work-log over the exact
  date range so counts are never double-tallied. Raw daily work-log is never fed to a
  higher-level synthesis (each level reads only pre-summarized pages).
- Fallback renderers must emit the same `##` anchors the templates use (so `/lore-process`
  can find the section) and the same frontmatter keys synthesis reads (e.g. work-log
  `categories`).
