# Scheduling

`lore schedule` prints definitions; OS registration is separate. Choose the scheduler
and entrypoint for the requested operation:

- On a Mac that sleeps, use **`--format launchd`**. Its calendar timer coalesces
  missed runs after sleep; cron skips them. Neither backfills every missed source
  date. Unsupported launchd cron steps/ranges are refused rather than approximated.
- For queue-mode semantic automation, use **`--pipeline-dir <dir>`**. Daily and
  weekly jobs use the installed scripts to collect/enqueue, drain, apply and
  reconcile. Bare subcommands need a separate semantic consumer. Period scripts
  enqueue monthly/quarterly/annual requests for the daily drain; maintenance runs
  retention and queue pruning sequentially. A configured task board adds reminders.

Review generated definitions before registration. On Linux, merge Lorekeeper
entries with `crontab -e`; piping into `crontab -` replaces every existing job.
On macOS, save each plist at its named LaunchAgents path, create the log directory,
and `launchctl bootstrap gui/$(id -u) <absolute-plist-path>`. Check `launchctl print`
and an actual execution log. After changes, boot out the old job before loading its
replacement; unload removed/disabled jobs too. Uninstall does not unregister jobs.
The host's local clock drives calendar triggers; vault timezone drives page dates.

Independent triggers do not guarantee completion order after sleep or a slow run.
A period request captures persisted inputs at execution time, not after tomorrow's
collection. Do not equate a registered timer or a fresh `health` result with complete
period or semantic processing.

Every emitted path must be absolute (`--bin`, `--pipeline-dir`): launchd searches no
`PATH` and expands no `~`.

The daily pipeline calls `lore ingest --wait-for-vault`, so another cooperating
writer delays collection instead of causing an immediate busy failure. Manual
ingest still refuses a busy writer by default. `--dry-run` never takes that writer
lock, even with `--wait-for-vault`.

For a project skill installation, set `LORE_SKILL_DIR` when generating the
schedule. With `--pipeline-dir`, an explicit value must name an existing directory;
it is resolved against the generation command's working directory and captured as
an absolute path in both cron and launchd environments. The pipeline checks the
particular skill it needs at runtime. With no override, the runtime default remains
`$HOME/.claude/skills`. Regenerate and reload the scheduled definitions after changing
the selected directory.
