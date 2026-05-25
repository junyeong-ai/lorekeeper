# lk-llm

The `LlmClient` trait and its providers. The trait is the only seam the pipeline knows
about; provider choice is config-driven (`build_llm_client` in lk-cli).

- **Trait surface**: `summarize`, `extract_concepts`, `identify_themes`, `classify`,
  and `flush` (default no-op). `identify_themes` returns structured `Vec<Theme>` (JSON
  parsed); `classify` returns `Option<String>` (category name or None). `flush` is the
  transactional commit point for buffered side-effects. `identify_themes` and `classify`
  have default no-op implementations so noop/mock clients work without overrides.
- **Concept dedup context**: `ExtractConceptsRequest` carries `existing_concepts:
  Vec<ExistingConceptRef>` (slug + name of vault concepts) and `categories:
  Vec<CategoryRef>` (config-driven category list). `anthropic` folds both into the
  system prompt so the LLM reuses established names and assigns valid categories;
  `queue` serializes them into the task `input` for `/lore-process`.
- **`focus`** (`Option<String>` on both requests, from `SourceConfig.focus`) is a
  source's natural-language relevance criterion. `anthropic` folds it into the prompt
  ("only content matching this focus … ignore anything off-topic"); `queue` serializes
  it into the task `input` so `/lore-process` applies the same filter. `None` = no
  filtering. This is how a broad source (a news aggregator carrying human-interest or
  politics) contributes focused tech knowledge without polluting the concept graph —
  the filter runs at LLM summarization/concept-extraction time, so off-topic items
  in broad sources are excluded from summaries and never become concept pages.
- **`LlmError::is_fatal()`** is true only for `QueueIo` (a persistence failure that must
  abort the run). Transient errors (network, rate limit, API) are non-fatal: callers log
  and degrade to an empty result so a flaky LLM never fails the whole ingest.
- **`TaskTarget { vault_path, kind, anchor }`** rides through the trait so queue mode
  records where each result should land. `anchor` is the exact `## …` heading the
  pipeline wrote, resolved from i18n at construction time — the skill locates by
  `target.anchor` instead of a hardcoded kind→heading table. `TargetKind` variants are
  uniform (`*PersonalNarrative` for monthly/quarterly/annual, matching weekly); serde
  kebab-case names stay for classification/logging.
- **`ClassifyRequest`** does not carry a `TaskTarget` because classification is an
  in-memory judgment (sets `Event.work_category`), not a vault write. Only `anthropic`
  mode performs a synchronous call; `queue` and `noop` return `None`, and the event
  stays unclassified (safe degradation — daily page renders it in the general section).
- **Providers**:
  - `AnthropicClient` — direct Messages API, with retry/backoff on 429. Implements
    all four semantic methods (`summarize`, `extract_concepts`, `identify_themes`,
    `classify`).
  - `QueueLlmClient` — buffers tasks in memory; `flush` writes the whole run to
    `<run-id>.jsonl.tmp`, fsyncs, and renames atomically (cleaning the temp on rename failure).
    Invariant: a `.jsonl` file becomes visible only after its target pages were written,
    so it never references a page that doesn't exist. `run_id` = timestamp + PID +
    process-global sequence (collision-free even for two clients in one process/second).
    Supports `TaskKind::Summarize`, `ExtractConcepts`, `IdentifyThemes`.
  - `NoopLlmClient` — empty results (uses default trait impls).
  - `MockLlmClient` — tests only, with configurable `summary`, `concepts`, `themes`.
