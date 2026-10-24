# Scheduling native jobs

`lore schedule` prints definitions; OS registration is separate. On macOS use
`lore schedule --format launchd --bin /absolute/path/to/lore`. On Linux review its
cron output and merge with `crontab -e`; piping into `crontab -` replaces other jobs.
Create the named log directory before loading launchd plists. Use `launchctl print`
and an actual run to verify registration. Regenerate and replace definitions after
changing configuration or environment; unload disabled jobs too. Uninstall does
not unregister OS jobs.

Terminal approval does not establish macOS protected-folder access for a launchd job.
After a binary update, protected-folder access may need renewed OS approval even
when signature verification and `--version` succeed. Before resuming jobs, use a
temporary launchd job with the same executable, config and environment to run the
read-only `lore queue check`; inspect actual vault access and termination separately
from unfinished-queue status. A permission prompt awaiting the user is not a passed check.
This probes only the command's read paths, not source access or a complete pipeline run.

The scheduler emits native commands:

- `ingest.schedule`: `lore run daily --wait`.
- `graph.audit.schedule`: `lore run knowledge --wait`.
- Period schedules: `lore synthesis PERIOD --previous --defer --wait-for-vault`.
- Maintenance: `lore maintenance --prune-queue --wait-for-vault`.
- A configured task board: `lore task remind notify` every five minutes.

Queue-mode scheduled jobs require an explicit execution profile. Selection is CLI
`--profile`, then `execution.operations`, then `execution.default_profile`.
An unavailable host, authentication error or denied action never switches hosts.
Native run outcomes distinguish completed, skipped, failed and cancelled stages;
`--json` reserves stdout for that report and sends stage output to stderr.

Daily collection waits for the short vault writer before semantic processing. A
partial source failure still permits independent queue/reconciliation stages;
a failed period stage stops dependent periods. Period triggers retain the exact
closed interval, not proof of completion. The daily job resumes base, quarter and
annual obligations in order. Inspect `lore synthesis pending --status --json` for
blocked evidence; backfill missed source dates explicitly.

Optional `synthesis.recovery` requires an explicit `start_date` and defaults
`max_periods_per_run` to 8. The daily job runs `synthesis pending --reconcile` once
before resuming periods. It registers only enabled modes' closed periods whose
start is on or after that boundary; an overlapping earlier period is excluded.
The budget counts examined slots, including existing intents, and the saved cursor
resumes fairly across modes. Disabling a mode pauses its cursor. Re-enabling it
continues that backlog. Registration supplies no source coverage or personal
work-log proof; existing readiness checks can still block processing. Explicit
historical `--date ... --defer` remains separate. Without recovery configuration,
calendar invocations alone do not reconstruct every missed period request.
Changing the activation date or vault timezone restarts only the recovery cursor;
existing intents remain. A prior intent in another timezone still requires an
explicit request under the intended timezone before it can process.

Knowledge jobs reserve their semantic budget for ready audit subjects before
draining unrelated queue work. The selected subject's actual evidence, context and
review inputs must have completed owned semantic work; unfinished scoped tasks or
unpublished results defer it. Unknown targets in malformed queue data remain a
blocker. `audit_deferred` reports encountered blockers. The runner verifies selected
assessments after final global reconciliation; completed audits do not turn an
unfinished global queue into a successful job.

The native runner holds the cooperative Unix workflow lease through semantic
reading and publication. Native daily jobs collect first, then fence publication
with a separate workflow lease before starting semantic processing. `--wait` waits
at that boundary; without it, busy publication refuses. Source cursor/admission
claims and network acquisition remain outside the publication lease. Standalone
ingest retains its short-writer behavior unless `--publication-workflow` is explicit.
Do not nest that option or reacquire the lease inside a skill. Cancellation and
session deadlines terminate the foreground process group and preserve unfinished
work. Manual agent sessions can use `lore workflow -- claude` or
`lore workflow -- codex` before reading inputs. Closing inherited descriptors or
detaching work is outside the manual exec lease's guarantee. Windows is unsupported
for workflow leases and native agent jobs.

Native short writer acquisition waits cancellably for typed contention, rechecks
the pinned configuration before journal recovery and prepares current inputs after
waiting. Recording after a host call validates fresh scope before publication.
Other storage errors remain failures; waiting never retries a model invocation.

Unattended skills come from the binary's data directory, independently of user or
project interactive deployment. The runtime verifies every required packaged file.
Claude loads explicit plugins and dependency permissions; Codex reads exact
packaged skill paths with a workspace-write sandbox. These are different host
permission models. Native result, source and target guards remain authoritative.

Schedules capture absolute config/data paths, PATH entries anchored to the
generation directory, and relevant existing `CLAUDE_CONFIG_DIR` or `CODEX_HOME`
directories. They do not copy credential values. Explicit standard proxy variables
retain casing and empty values; endpoint URLs must be credential-free and contain
no query, fragment or non-root path. Regenerate definitions when these change.

Calendar triggers use host local time; page dates use vault timezone. Launchd
coalesces missed runs after sleep; cron skips them. Neither backfills every missed
date. Unsupported launchd cron expressions are refused rather than approximated.
For isolated testing use `--namespace org.example.lore-test` and
`--log-dir /absolute/test/logs`; the renderer neither creates paths nor registers jobs.

OS notifications retire a reminder only after the notifier accepts it. Acceptance
is not proof the user saw it. A crash between acceptance and local retirement can
cause redelivery; failed submissions remain pending.
