# lk-cli

The `lore` binary. `main.rs` defines the clap surface; `commands/` has one module per
subcommand; `commands/mod.rs` holds shared helpers (`find_config`, `load_config`,
`build_llm_client`, `parse_date`).

- **Global flags** `--config`/`LORE_CONFIG` and `--template-dir`/`LORE_TEMPLATE_DIR` are
  injected into the cron lines `lore schedule` generates, so scheduled runs don't depend on
  CWD. Templates are embedded in the binary by default; `--template-dir` overrides them.
- **`find_config`** resolves in order: `--config`/`LORE_CONFIG` → `./config.yaml` →
  `~/.config/lorekeeper/config.yaml` (XDG). The XDG path is what makes a binary-only install
  (no repo) work. There is no `config.example.yaml` fallback — running with the example's
  placeholder values silently is a footgun. A vault-relative config can't be auto-found
  (the vault path lives *inside* the config). It `canonicalize`s the located path at this
  single I/O boundary, so the returned path is always absolute: `Config::load` then resolves
  a relative `vault.root` against an absolute config dir, making `vault.root` — and every
  path derived from it in every crate — CWD-independent by construction (lk-core needs no
  `current_dir`).
- **`build_llm_client`** maps `llm.provider` to a client (`queue` or `noop`).
- **`lore ingest`** owns the ingest phase flow and the exit code: any source/extract/pipeline
  failure (`had_failure`) or write/flush failure returns non-zero — including under
  `--dry-run`. Dry-run plans normally but skips every vault write, the tmp sweep, and the
  ingest log, so it leaves the vault untouched.
- **`lore ingest` startup** sweeps stale `*.jsonl.tmp` (>1h) from crashed runs and
  warns if pending queue files exist (run `/lore-process` first to avoid duplicate
  LLM work).
- **`lore maintenance`** prunes the ingest log, drained `queue/processed/` files, and the
  streaming `events/{source}/{date}.jsonl` logs past `maintenance.retention_days` (default
  90; event logs prune by the recorded day, parsed from the filename). It never touches
  live `*.jsonl.tmp` — the ingest startup sweep does.
- **`lore synthesis <period>`** rejects `--date`/`--year` together with `--previous`
  (clap `conflicts_with`), and flushes the LLM queue after writing its pages.
- **`lore queue status`** (`commands/queue.rs`) is the authoritative stale-task guard:
  for each pending queue task it reads the target page's `llm_inputs.<key>` and classifies
  the task `current` / `stale` / `missing-target` by comparing against `task.cache_hash`.
  `/lore-process` consumes `--json` from it and processes only `current` tasks — the hash
  check lives here in tested Rust, never re-derived in skill prose.
- **`lore init credentials`** (in `init.rs`) is the interactive credential wizard. UX
  (dialoguer prompts, masked secrets, TTY guard) lives here; the JSON shape + atomic
  `0600` write live in `lk_source::credentials` (`from_file`/`save`). The Google branch
  can mint a refresh token via `lk_source::obtain_google_refresh_token` (browser OAuth
  loopback) or accept a pasted one. `Init` is a subcommand-of-subcommand so future
  scaffolders (`lore init config`) slot in cleanly.
