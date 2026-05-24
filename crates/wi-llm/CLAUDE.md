# wi-llm

The `LlmClient` trait and its providers. The trait is the only seam the pipeline knows
about; provider choice is config-driven (`build_llm_client` in wi-cli).

- **Trait surface**: `summarize`, `extract_concepts`, and `flush` (default no-op).
  `flush` is the transactional commit point for buffered side-effects.
- **`LlmError::is_fatal()`** is true only for `QueueIo` (a persistence failure that must
  abort the run). Transient errors (network, rate limit, API) are non-fatal: callers log
  and degrade to an empty result so a flaky LLM never fails the whole ingest.
- **`TaskTarget { vault_path, kind: TargetKind }`** rides through the trait so queue mode
  records where each result should land. `TargetKind` variants are uniform
  (`*PersonalNarrative` for monthly/quarterly/annual, matching weekly); serde kebab-case
  names are the contract `/wi-process` reads (keep the SKILL.md table in sync).
- **Providers**:
  - `AnthropicClient` — direct Messages API, with retry/backoff on 429.
  - `QueueLlmClient` — buffers tasks in memory; `flush` writes the whole run to
    `<run-id>.jsonl.tmp`, fsyncs, and renames atomically (cleaning the temp on failure).
    Invariant: a `.jsonl` file becomes visible only after its target pages were written,
    so it never references a page that doesn't exist. `run_id` = timestamp + PID +
    process-global sequence (collision-free even for two clients in one process/second).
  - `NoopLlmClient` — empty results.
  - `MockLlmClient` — tests only.
