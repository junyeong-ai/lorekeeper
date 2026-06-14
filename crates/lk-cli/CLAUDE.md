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
- **`build_llm_client`** maps `llm.provider` to a client: `queue` buffers pending tasks
  to `<vault>/.lorekeeper/queue/*.jsonl` for `/lore-process` to fill via the LLM; `noop`
  discards them (dry-run, CI, or ingest-only runs with no LLM work).
- **`lore ingest`** owns the ingest phase flow and the exit code: any source/extract/pipeline
  failure (`had_failure`) or write/flush failure returns non-zero — including under
  `--dry-run`. Dry-run plans normally but skips every vault write, the tmp sweep, and the
  ingest log, so it leaves the vault untouched.
- **Work-log gate**: the work-log renders only on a FULL ingest (`source.is_none()`) — a
  filtered `lore ingest <id>` sees a structural subset of personal events and must never
  rewrite the cross-source page (it prints why instead). A transient source failure inside
  a full run still writes: the failure is loud (non-zero exit) and the next full run
  re-renders the page complete, while blocking on `had_failure` would freeze the work-log
  on any persistently failing source, news feeds included.
- **`lore schedule`** emits ONE all-source `lore ingest` line from `ingest.schedule` —
  never per source — plus synthesis/maintenance/queue-prune lines from their own keys.
- **`lore ingest` startup** sweeps stale `*.jsonl.tmp` from crashed runs and
  warns if pending queue files exist (run `/lore-process` first to avoid duplicate
  LLM work).
- **`lore maintenance`** prunes the ingest log and drained `queue/processed/` files past
  `maintenance.retention_days` — operational history only. The streaming
  `events/{source}/{date}.jsonl` logs are the permanent raw layer (`lore ingest --date
  <past>` re-projects any day from them) and are NEVER pruned. It also never touches
  live `*.jsonl.tmp` — the ingest startup sweep does. With `maintenance.schedule` set,
  `lore schedule` emits crontab lines for it and for `lore queue prune`, so both
  janitors run unattended.
- **`lore synthesis <period>`** rejects `--date`/`--year` together with `--previous`
  (clap `conflicts_with`), and flushes the LLM queue after writing its pages.
- **`lore queue status`** (`commands/queue.rs`) is the authoritative stale-task guard:
  for each pending queue task it reads the target page's `llm_inputs.<key>` and classifies
  the task `current` / `stale` / `missing-target` by comparing against `task.cache_hash`.
  `/lore-process` consumes `--json` from it and processes only `current` tasks — the hash
  check lives here in tested Rust, never re-derived in skill prose.
- **`lore queue prune`** applies that same classification destructively: drops `stale` and
  `missing-target` tasks (exactly what `/lore-process` would discard without editing),
  rewriting files atomically via `lk_queue::write_tasks_atomic` — the one writer for queue
  files. All-current files stay byte-identical; a file left with no tasks is deleted (it
  never produced page edits, so nothing belongs in `processed/`). `--dry-run` reports the
  same counts with zero writes.
- **`lore init credentials`** (in `init.rs`) is the interactive credential wizard. UX
  (dialoguer prompts, masked secrets, TTY guard) lives here; the JSON shape + atomic
  `0600` write live in `lk_source::credentials` (`load_file`/`load`/`save`). The Google branch
  can mint a refresh token via `lk_source::build_google_refresh_token` (browser OAuth
  loopback) or accept a pasted one. `Init` is a subcommand-of-subcommand so future
  scaffolders (`lore init config`) slot in cleanly.
