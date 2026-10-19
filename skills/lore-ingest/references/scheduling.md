# Scheduling

`lore schedule` prints definitions; OS registration is separate. Choose the scheduler
and entrypoint for the requested operation:

- On a Mac that sleeps, use **`--format launchd`**. Its calendar timer coalesces
  missed runs after sleep; cron skips them. Neither backfills every missed source
  date. Unsupported launchd cron steps/ranges are refused rather than approximated.
- For queue-mode semantic automation, use **`--pipeline-dir <dir>`**. Daily and
  weekly jobs use the installed scripts to collect/enqueue, drain, apply and
  reconcile. Period jobs retain exact closed-period intents with `--defer`; the
  daily pipeline resumes ready periods in dependency order after collection and
  semantic processing. Maintenance runs retention and queue pruning sequentially.
  A configured task board adds reminders.

Review generated definitions before registration. On Linux, merge Lorekeeper
entries with `crontab -e`; piping into `crontab -` replaces every existing job.
On macOS, save each plist at its named LaunchAgents path, create the log directory,
and `launchctl bootstrap gui/$(id -u) <absolute-plist-path>`. Check `launchctl print`
and an actual execution log. After changes, boot out the old job before loading its
replacement; unload removed/disabled jobs too. Uninstall does not unregister jobs.
The host's local clock drives calendar triggers; vault timezone drives page dates.

Independent triggers do not guarantee completion order after sleep or a slow run.
A deferred request records the intended interval, not a completed review. Resume
checks observed collection scope and semantic inputs; a timer or fresh `health`
result does not certify a completed period. Direct synthesis without `--defer`
remains an explicitly requested preview over currently persisted inputs.

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

For an isolated macOS installation, generate definitions with
`--format launchd --namespace org.example.lore-test --log-dir /absolute/test/logs`.
The namespace changes labels and plist filenames in the native renderer; the
explicit log directory keeps test output separate too. Neither option registers
jobs or creates directories. With no options, existing `com.lorekeeper` labels and
log paths remain unchanged. Use only the generated namespace when loading,
kicking or unloading test jobs; never rewrite production labels by text replacement.

Without `--pipeline-dir`, ingest still collects and queues work, while scheduled
period commands only retain intents (`--defer`). There is no automatic semantic
consumer or pending-period resume. Use the installed daily pipeline, or manually
run `/lore-process`, `lore synthesis pending --wait-for-vault`, and process/resume
its resulting work until the requested periods settle. Inspect
`lore synthesis pending --status --json`; blocked evidence is not completion.

Deferred `--previous --defer` triggers retain the exact closed interval they name.
The daily pipeline resumes `base`, then `quarter`, then `annual`, processing queued
semantic results between stages. A closing-day observation must cover the configured
source scope; failed, partial or unverified reach can leave the obligation blocked.
`lore synthesis pending --status --json` reports each period's state, reason and
evidence scope. `no-data` is distinct from missing collection, and retained historical
completion does not certify historical collection coverage. After a long sleep,
backfill missing source windows explicitly; pending periods do not fetch them or
invent obligations for every missed scheduler date.

Headless pipelines load the selected directory containing `SKILL.md` with Claude
Code `--plugin-dir`, and invoke its namespaced skill (for example,
`/lore-process:lore-process`). `--add-dir` grants filesystem access; it does not
register an arbitrary skill directory. Keep the selected plugin and its references
from the same deployed version. Knowledge maintenance also registers the required
lore-process plugin so the audit can complete typed semantic repairs in its session.
A dependency is callable only through its explicitly permitted qualified skill name,
with or without arguments. A missing required skill refuses the consumer before launch.
The workflow lease owns the whole foreground
consumer session; never reacquire it inside the skill.

An explicit `CLAUDE_CONFIG_DIR` selects the Claude Code profile for scheduled
consumers too. With `--pipeline-dir`, schedule validates this existing directory
and captures its canonical absolute path in cron/launchd environments. An unset
value keeps Claude's default behavior. Only the profile path is captured; API keys
or credential contents are not copied. Regenerate and reload definitions after
changing the selected profile.

Schedule definitions also preserve explicitly set `HTTP_PROXY`, `HTTPS_PROXY`,
`ALL_PROXY`, `NO_PROXY` and their lowercase forms, including empty values. Casing
and client precedence remain unchanged. Proxy endpoints must be absolute,
credential-free URLs (scheme, host, optional port/root slash; no userinfo, query,
fragment or other path); `NO_PROXY` is preserved as text. Invalid UTF-8/control
characters are refused without printing values. Regenerate and reload definitions
when network settings change; credentials/API keys are not captured.
