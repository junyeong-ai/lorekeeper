# Scheduling

`lore schedule` with no flags emits cron entries running the bare subcommands. Both
defaults are usually wrong, and both fail SILENTLY:

- **`--format launchd` on macOS.** `StartCalendarInterval` runs a job missed while the
  machine slept as soon as it wakes; cron just skips it — and a closed laptop at 09:00
  is the normal case, so cron simply never ingests. Syntax launchd cannot express
  (`*/5`, `1-5`) is refused rather than approximated.
- **`--pipeline-dir <dir>`** points the ingest and weekly jobs at `lore-daily.sh` /
  `lore-weekly.sh`. `lore ingest` and `lore synthesis weekly` are only stage ONE of those
  scripts: the queue drain and `lore queue apply` live in the scripts alone, so without
  this flag the schedule ingests every morning and never fills a summary or materializes
  a concept. Monthly/quarterly/annual jobs also use their named entry scripts; these
  enqueue narrative tasks that the next daily drain processes. Maintenance uses
  `lore-maintenance.sh` to run retention and queue pruning sequentially. When a task
  board is configured, this flag also schedules the reminder notifier. The janitors
  have no LLM work.

Every emitted path must be absolute (`--bin`, `--pipeline-dir`): launchd searches no
`PATH` and expands no `~`.
