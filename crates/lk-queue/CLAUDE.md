# lk-queue

Semantic task queue abstraction. The `LlmClient` trait is the only seam the pipeline
knows about; provider choice is config-driven (`build_llm_client` in lk-cli).

- **Trait surface**: `summarize`, `extract_concepts`, `identify_themes`, `flush`, and the
  per-source transaction pair `begin_source`/`rollback_source`. The CLI opens a boundary
  before planning each source and rolls back if that source's plan fails partway, so a
  half-produced source's buffered tasks never reach the flushed file — preserving the
  invariant that a queued task always targets a written page. Both default to no-ops, so
  non-buffering providers (noop/mock) are unaffected.
  `identify_themes` returns structured `Vec<Theme>` (JSON parsed) and has a default
  no-op implementation so noop/mock clients work without overriding it. `flush` is the
  transactional commit point for buffered side-effects. (Classification is deterministic
  in the pipeline — keyword rules, no LLM task — so the trait has no `classify`.)
- **Concept dedup is skill-side, not per-task**: the queue task carries no concept
  registry. `/lore-process` loads the on-disk registry once per run (`lore wiki concepts`
  — slugs, names, aliases) and reuses an established name instead of forking a variant, so
  the per-task payload stays O(1) as the vault grows. `ExtractConceptsRequest` carries only
  `categories: Vec<CategoryRef>` (config-driven category list), serialized into the task
  `input` for `/lore-process`.
- **`focus`** (`Option<String>` on both requests, from `SourceConfig.focus`) is a
  source's natural-language relevance criterion. The queue serializes it into the
  task `input` so `/lore-process` applies the filter. `None` = no filtering.
  This is how a broad source (a news aggregator carrying human-interest or politics)
  contributes focused tech knowledge without polluting the concept graph — the filter
  runs at LLM summarization/concept-extraction time in the skill.
- **`LlmError::is_fatal()`** is true for `QueueIo` (a persistence failure that must
  abort the run).
- **`TaskTarget { vault_path, kind, anchor }`** rides through the trait so queue mode
  records where each result should land. `anchor` is the exact `## …` heading the
  pipeline wrote, resolved from i18n at construction time — the skill locates by
  `target.anchor` instead of a hardcoded kind→heading table.
- **Each request type exposes two JSON projections.**
  - `task_input()` — payload serialized into the queue file. Carries every field
    `/lore-process` needs to do its work (`source_type` plus the cache-identity fields).
  - `cache_identity()` — the subset hashed for caching. Restricted to fields that
    actually shape the LLM's output: `summarize` hashes `text` + `max_sentences` +
    `locale` + `focus`; `extract-concepts` hashes `text` + `source_id` + `date` + `focus`
    + `categories` (`source_id`/`date` scope a concept extraction to one source+day).
    `source_type` is in `task_input` but NOT the identity — it scopes extraction without
    shaping the prompt's semantic output. `categories` is sorted by `id` so configuration field
    order can't perturb the hash. `target` is excluded — it describes where the result
    lands, not the prompt.
- **`Request::cache_hash()`** (BLAKE3-128, 32 hex of `cache_identity()`) is
  stamped into `QueueTask.cache_hash` at enqueue time and pre-recorded by the
  pipeline in the page's `llm_inputs.<key>` frontmatter — for EVERY kind, so
  `llm_inputs.<key>` always holds the current input's hash. The skill MUST verify
  `page.llm_inputs.<key> == task.cache_hash` before writing; a mismatch means
  the task is stale (page was re-rendered between enqueue and processing) and
  must be dropped. Without that guard a stale task overwrites the section with
  content keyed to an older input and the next ingest's cache lookup freezes
  the mismatch forever. The comparison is computed in tested Rust by
  **`lore queue status`** (`commands::queue::classify_task`), which reads each
  pending task's target page and classifies it `current` / `stale` / `missing-target`;
  `/lore-process` consults that classification and processes only `current` tasks
  rather than re-deriving the hash check in prose.
- **`TargetKind::llm_inputs_key`** is the single source of truth mapping each task
  kind to its `llm_inputs.<key>` input field; **`TargetKind::completion_key`** is the
  companion source for its `llm_inputs.<key>_done` completion marker. Completion is
  UNIFORMLY marker-signalled — every kind has a `*_done` marker, always `llm_inputs_key()`
  + `_done`. There is no body-emptiness completion path: a non-empty body never means
  "done", because too many sections can be legitimately empty (a focus-filtered summary,
  an extraction that found nothing, a trivial-only work-log, an empty-period review). A
  per-kind "can this be empty?" judgment proved intractable (it was wrong for concepts,
  themes, work-log, summaries, and narratives in turn), so the model removes the judgment
  entirely: the pipeline pre-stamps the input key as the stale-task reference and the
  skill stamps the marker when done; a cache hit is `marker == input key`, never inferred
  from the body. The pipeline, work-log, and synthesis derive both keys from these.
  Adding a `TargetKind` is compiler-forced to choose both.
  The skill-side mirror is enforced too: `tests/skill_contract.rs` iterates the full
  kind space via `strum::EnumIter` (macro-generated from the variant list, so the
  iteration can never drift from the enum — never hand-maintain a variant array) and
  requires every wire name and marker completion key to appear in the `/lore-process`
  skill files, with each kind's `llm_inputs` key on the same table row as its wire
  name — renaming or adding a kind fails the test until the skill documentation is
  updated.
- **Providers**:
  - `QueueLlmClient` — buffers tasks in memory; `flush` writes the whole run through
    `write_tasks_atomic` (temp + fsync + rename, cleaning the temp on rename failure) —
    the single queue-file writer, also used by `lore queue prune` to rewrite files, so
    every queue file on disk has identical durability and encoding guarantees.
    Invariant: a `.jsonl` file becomes visible only after its target pages
    were written, so it never references a page that doesn't exist.
  - `NoopLlmClient` — empty results (uses default trait impls). For dev/CI.
  - `MockLlmClient` — tests only, with configurable `summary`, `concepts`, `themes`.
    Behind the `test-util` feature (dependents enable it as a dev-dependency), so it is
    never compiled into the release binary.
