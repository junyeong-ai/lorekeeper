# lk-queue

Semantic task queue abstraction. The `LlmClient` trait is the only seam the pipeline
knows about; provider choice is config-driven (`build_llm_client` in lk-cli).

- **Trait surface**: `summarize`, `extract_concepts`, `identify_themes`, `classify`,
  and `flush` (default no-op). `identify_themes` returns structured `Vec<Theme>` (JSON
  parsed); `classify` returns `Option<String>` (category name or None). `flush` is the
  transactional commit point for buffered side-effects. `identify_themes` and `classify`
  have default no-op implementations so noop/mock clients work without overrides.
- **Concept dedup context**: `ExtractConceptsRequest` carries `existing_concepts:
  Vec<ExistingConceptRef>` (slug + name of vault concepts) and `categories:
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
- **`ClassifyRequest`** does not carry a `TaskTarget` because classification is an
  in-memory judgment (sets `Event.work_category`), not a vault write.
- **Providers**:
  - `QueueLlmClient` — buffers tasks in memory; `flush` writes the whole run to
    `<run-id>.jsonl.tmp`, fsyncs, and renames atomically (cleaning the temp on rename
    failure). Invariant: a `.jsonl` file becomes visible only after its target pages
    were written, so it never references a page that doesn't exist.
  - `NoopLlmClient` — empty results (uses default trait impls). For dev/CI.
  - `MockLlmClient` — tests only, with configurable `summary`, `concepts`, `themes`.
