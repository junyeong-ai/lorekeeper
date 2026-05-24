# wi-cli

The `wi` binary. `main.rs` defines the clap surface; `commands/` has one module per
subcommand; `commands/mod.rs` holds shared helpers (`find_config`, `load_config`,
`build_llm_client`, `resolve_template_dir`, `parse_date`).

- **Global flags** `--config`/`WI_CONFIG` and `--template-dir`/`WI_TEMPLATE_DIR` are
  injected into the cron lines `wi schedule` generates, so scheduled runs don't depend on
  CWD. `--template-dir` falls back to `XDG_DATA_HOME/wi-ingest/templates`.
- **`build_llm_client`** maps `llm.provider` to a client; `anthropic` with a missing
  `ANTHROPIC_API_KEY` warns and degrades to `NoopLlmClient`.
- **`wi ingest`** owns the 5-phase flow and the exit code: any source/extract/pipeline
  failure (`had_failure`) or write/flush failure returns non-zero — including under
  `--dry-run`. Dry-run uses `Pipeline::new_dry_run`, skips the tmp sweep, and does not
  write the ingest log, so it leaves the vault untouched.
- **`wi ingest` startup** sweeps stale `*.jsonl.tmp` (>1h) from crashed runs and, in
  queue mode, warns if pending queue files exist (run `/wi-process` first to avoid
  duplicate LLM work). In `anthropic` mode, pending queue files are a hard error.
- **`wi maintenance`** prunes the ingest log, dedup cache, and drained `queue/processed/`
  files past the 90-day retention; it must not overlap a running `wi ingest` (redb
  single-writer). It never touches live `*.jsonl.tmp` — the ingest startup sweep does.
- **`wi synthesis <period>`** rejects `--date`/`--year` together with `--previous`
  (clap `conflicts_with`), and flushes the LLM queue after writing its pages.
- **`wi init credentials`** (in `init.rs`) is the interactive credential wizard. UX
  (dialoguer prompts, masked secrets, TTY guard) lives here; the JSON shape + atomic
  `0600` write live in `wi_source::credentials` (`from_file`/`save`). `Init` is a
  subcommand-of-subcommand so future scaffolders (`wi init config`) slot in cleanly.
