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
- **Concept dedup context**: `ExtractConceptsRequest` carries `existing_concepts:
  Vec<ExistingConceptRef>` (slug + name + registered aliases of vault concepts, so an
  alias-only surface form is matched to the existing concept instead of forking a
  duplicate page) and `categories:
  Vec<CategoryRef>` (config-driven category list). The queue serializes both into the
  task `input` for `/lore-process`.
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
    `/lore-process` needs to do its work, including dedup/registry hints
    (`existing_concepts`).
  - `cache_identity()` — the subset hashed for caching. Restricted to fields that
    actually shape the LLM's output: `summarize` hashes `text` + `max_sentences` +
    `locale` + `focus`; `extract-concepts` hashes `text` + `source_id` + `date` + `focus`
    + `categories` (`source_id`/`date` scope a concept extraction to one source+day).
    Hints that help the LLM phrase its answer but don't change semantic correctness are
    excluded — `existing_concepts` and `source_type` are in `task_input` but NOT the
    identity, so adding a concept anywhere in the vault does NOT invalidate every other
    concept-extraction cache entry. `categories` is sorted by `id` so configuration field
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
  kind to its `llm_inputs.<key>` frontmatter field; **`TargetKind::cache_shape`** is
  the companion source for HOW completion is detected: `FillEmpty` (non-empty body)
  or `InPlace { completion_key }` (a second frontmatter key the skill stamps, used
  by `daily-refine-events` because its section is non-empty from render). The
  pipeline, work-log, and synthesis derive both from these; the skill's key table
  mirrors them. Adding a `TargetKind` is compiler-forced to choose a key and a shape.
- **Providers**:
  - `QueueLlmClient` — buffers tasks in memory; `flush` writes the whole run to
    `<run-id>.jsonl.tmp`, fsyncs, and renames atomically (cleaning the temp on rename
    failure). Invariant: a `.jsonl` file becomes visible only after its target pages
    were written, so it never references a page that doesn't exist.
  - `NoopLlmClient` — empty results (uses default trait impls). For dev/CI.
  - `MockLlmClient` — tests only, with configurable `summary`, `concepts`, `themes`.
    Behind the `test-util` feature (dependents enable it as a dev-dependency), so it is
    never compiled into the release binary.
