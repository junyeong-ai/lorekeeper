# Scheduling native jobs

## Contents

- [Registration and verification](#registration-and-verification)
- [Emitted commands](#emitted-commands)
- [Automation budget](#automation-budget)
- [Pauses and holds](#pauses-and-holds)
- [Execution profiles](#execution-profiles)
- [Daily and period recovery](#daily-and-period-recovery)
- [Knowledge jobs](#knowledge-jobs)
- [Workflow leases](#workflow-leases)
- [Runtime skills and environment](#runtime-skills-and-environment)
- [Timing and notifications](#timing-and-notifications)

## Registration and verification

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

A binary update can also leave registered jobs unable to launch: `launchctl print`
reports `last exit reason = OS_REASON_CODESIGNING` (a launch constraint violation),
and launchd may remove a job that keeps failing. A running job's installation claims
also make `self update` refuse, even with `--force`. Once no job is running (booting
out a running job terminates it), boot each job out, update, bootstrap each plist
again so launchd registers the replaced executable, then confirm one launch.

## Emitted commands

The scheduler emits native commands:

- `ingest.schedule`: `lore run daily --wait`.
- `graph.audit.schedule`: `lore run knowledge --wait`.
- Period schedules: `lore synthesis PERIOD --previous --defer --wait-for-vault`.
- Maintenance: `lore maintenance --prune-queue --wait-for-vault`. It waits for the
  workflow lease, so a running process or knowledge campaign yields to it.
- A configured task board: `lore task remind notify` every five minutes.

## Automation budget

With `execution.automation`, schedules add `--scheduled --json` to daily/process/
knowledge jobs and add one process job at `automation.process_schedule`. Process and
knowledge repeat ready batches within `max_run_seconds`; daily collects once.
Semantic execution time starts at the controller's first acquired semantic workflow
lease and excludes its later workflow acquisition waits. Those waits share
`max_wait_seconds`, which defaults to `max_run_seconds`; neither allowance renews
at a batch boundary. This waiting allowance does not bound source collection,
daily ingest's separate publication lease, or short writer-lock waits. Duration
limits stop new agent calls; returned results remain recordable.
The selected profile's `execution.work.max_calls` also caps the complete scheduled
invocation across those batches; a batch boundary cannot renew that allowance.
When workflow admission is contended, process/knowledge settle every launched call,
yield the lease and continue with fresh preparation. `runs[].yield_reason` identifies
these incomplete checkpoints; final success still requires normal completion checks.
Calls, execution/wait time and original-subject replan allowances remain cumulative.
Optional `automation.knowledge_schedule` overrides the knowledge job's timing;
omission retains `graph.audit.schedule`. It emits one stable job, not another audit
writer. Batch size, readiness and holds still use the existing graph audit policy.
Keep automatic timing changes in this policy to preserve new-policy Work identities.
Stagger process and knowledge opportunities when both need progress; shared usage
does not reserve an operation's share. Every batch or yielded checkpoint releases
the workflow lease. All scheduled jobs share `max_calls_per_day` in the physical
vault's UTC-day usage ledger. This required field accepts a positive integer or YAML
null for no daily cap. Unlimited daily usage still observes per-run calls, duration
and failure pauses; reservations remain recorded. Changing the cap preserves usage
and pauses. Persistent OS schedules offer later runs after an incomplete batch or
process interruption, subject to OS registration and host availability.
Reservations are durable before host launch and survive
restart/config changes; they count outer agent attempts, not internal model requests,
tokens or billing. Separate waiting and execution limits bound acquisition and new-call admission,
not completion of an active host, source collection or paid-result recording.
An exhausted daily allowance waits for a later UTC day or an explicitly
changed configured limit; a new process never grants a fresh allowance.
Without this policy, schedules and the one-batch defaults remain unchanged.
Ordinary manual `lore run` without `--scheduled` uses its normal batch/work budgets
outside this shared allowance and pause. Use `--scheduled` for a policy-controlled
manual trigger too; add `--wait` when it should wait for the workflow lease.

## Pauses and holds

Unknown or corrupt accounting refuses semantic execution. Fatal inference failures
pause that operation's semantic phase across restarts and day rollover. Daily source
collection still runs before this pause check; process and knowledge report a known
pause before waiting for the workflow lease. Reviews needing reassessment keep
completion nonzero without pausing automation; invalid reviews and failed reads remain
fatal. Daily checks reviews before and after period publication.
`lore status --json` reports the current UTC-day reservations, remaining configured
allowance and paused operations without changing the ledger. Each pause names the
failed stage that stopped its job and a bounded detail; the job's `--json` report keeps
the complete stage list. Read that cause before `--resume`. Unreadable accounting
is unavailable, never a zero-use success. This snapshot does not verify OS registration
or guarantee progress while the computer is off, asleep or out of agent allowance.
After repairing the cause, explicit `lore run OPERATION --scheduled --resume --json`
clears only its pause; it preserves usage, rejected proposals and exact semantic holds.
Unchanged holds spend no calls. A failed invocation of one request holds only that
exact input, without pausing the operation or earning semantic progress;
[bounded work](../../lore-process/references/bounded-work.md#native-holds-and-retries)
lists those holds and the commands that clear them.

## Execution profiles

Queue-mode scheduled jobs require an explicit execution profile. Selection is CLI
`--profile`, then `execution.operations`, then `execution.default_profile`.
An unavailable host, authentication error or denied action never switches hosts.
Native run outcomes distinguish completed, skipped, failed and cancelled stages;
`--json` reserves stdout for that report and sends stage output to stderr.

## Daily and period recovery

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

## Knowledge jobs

Knowledge jobs reserve their semantic budget for ready audit subjects before
draining unrelated queue work. The selected subject's actual evidence, context and
review inputs must have completed owned semantic work; unfinished scoped tasks or
unpublished results defer it. Unknown targets in malformed queue data remain a
blocker. `audit_deferred` reports encountered blockers. The runner verifies selected
assessments before releasing their workflow. Final completion also requires global
reconciliation and checks; completed audits do not turn an
unfinished global queue into a successful job.

## Workflow leases

The native runner holds the cooperative Unix workflow lease through semantic
reading and publication within each retained unit. Native daily jobs collect first, then fence publication
with a separate workflow lease before starting semantic processing. `--wait` waits
at that boundary; without it, busy publication refuses. Source cursor/admission
claims and network acquisition remain outside the publication lease. Standalone
ingest retains its short-writer behavior unless `--publication-workflow` is explicit.
Do not nest that option or reacquire the lease inside a skill. Cancellation and
session deadlines terminate the foreground process group and preserve unfinished
work. Manual agent sessions can use `lore workflow -- claude` or
`lore workflow -- codex` before reading inputs. Closing inherited descriptors or
detaching work is outside the workflow lease's guarantee. Windows is unsupported
for workflow leases and native agent jobs.

Native short writer acquisition waits cancellably for typed contention, rechecks
the pinned configuration before journal recovery and prepares current inputs after
waiting. Recording after a host call validates fresh scope before publication.
Other storage errors remain failures; waiting never retries a model invocation.

## Runtime skills and environment

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

## Timing and notifications

Calendar triggers use host local time; page dates use vault timezone. Launchd
coalesces missed runs after sleep; cron skips them. Neither backfills every missed
date. Unsupported launchd cron expressions are refused rather than approximated.
For isolated testing use `--namespace org.example.lore-test` and
`--log-dir /absolute/test/logs`; the renderer neither creates paths nor registers jobs.

OS notifications retire a reminder only after the notifier accepts it. Acceptance
is not proof the user saw it. A crash between acceptance and local retirement can
cause redelivery; failed submissions remain pending.
