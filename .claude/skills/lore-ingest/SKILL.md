---
name: lore-ingest
version: 0.21.2
description: Daily knowledge ingestion pipeline — collects from Gmail, Google Drive, Google Calendar, Slack, Jira, RSS, and a manual inbox into an Obsidian vault. Deduplicates, classifies, extracts concepts, writes structured pages. Optionally tracks your own work into a work-log when the `personal:` module is configured. Idempotent, phased ingest with a no-data-loss guarantee.
argument-hint: "<subcommand> [args]"
disable-model-invocation: true
allowed-tools: |
  Bash(lore *)
  Bash(crontab *)
---

# lore-ingest — Daily knowledge ingestion for Obsidian

Run the `lore` CLI for ad-hoc ingest, synthesis, and operations. The
CLI is config-driven (`./config.yaml` or `LORE_CONFIG`) and writes to
an Obsidian vault. All commands accept `--config <path>` to override.

## Command reference

| Command | Purpose |
|---------|---------|
| `lore validate` | Parse + validate config, print summary |
| `lore ingest` | Collect ALL enabled sources, write pages; render the cross-source work-log when a `personal:` module is configured |
| `lore ingest <source>` | Refresh one source's pages only — never rewrites the work-log (it sees a subset) |
| `lore ingest --dry-run` | Preview without vault writes |
| `lore ingest --date YYYY-MM-DD` | Re-materialize a specific day — RE-FETCHES it; see Safety notes before using it on a past date |
| `lore synthesis weekly [--previous]` | Weekly synthesis + personal review |
| `lore synthesis monthly [--previous]` | Monthly performance review |
| `lore synthesis quarterly [--previous]` | Quarterly review with category stats |
| `lore synthesis annual [--previous]` | Annual review from quarterly reviews |
| `lore agenda [--date YYYY-MM-DD]` | The day read off the task board: what is committed to it, what woke, what is due. A view — it writes nothing, and names `lore task sync` when an editor has changed the board since |
| `lore task add <text> [--state] [--due] [--link URL] [--label]` | Write down something to do (default section: `next`). `--link` records where it came from — the Slack thread, the Jira issue, the mail — as an absolute URL in the task's own text, so it survives onto the archived page and stays out of the link graph. A vault path is refused |
| `lore task list [--state] [--json]` | The board, by section |
| `lore task done <id> [--note]` | Close a task. The note becomes the archived page's body, and through it the concept extraction — which is how work performed compounds the way work read does |
| `lore task drop <id>` · `move <id> <state>` · `wait <id> --until <date>` | Take it off the board, move it between sections, park it until a day |
| `lore task propose` | Put what the sources say is still OPEN into the board's proposed section. An observation proposes and never creates: accept by dragging the line into another section, decline with `lore task drop`. Nothing is proposed twice — the board says what is open and the history says what was answered |
| `lore task candidate --source <id> --summary <text> --url <URL>` | Name work read out of a page by JUDGMENT, for the next `propose` to offer. Refused for a source `personal.tasks.propose_from` does not name, because unlike a status field a reading of prose can be wrong |
| `lore task sync` | Record what an editor did: adopt lines typed by hand, close lines ticked, wake what is due |
| `lore task rollover` | Close the day — carry every still-committed task, counting each carry so a task carried too long reads as the diagnosis it is |
| `lore resolve <name>` | Which concept page a name addresses, by the rule ingest routes an extraction by. Exit 0 owned, 1 absent, 2 more than one page answers to it |
| `lore status` | One line per subsystem — the installation, source currency, the LLM queue, page contracts, the link graph — each naming the command that owns it. Reports without gating; the per-source timestamps are `lore health` |
| `lore health [--strict]` | Warn if any source is overdue vs `ingest.schedule` (2 missed fires; 48h fallback). `--strict` also fails when a source has NEVER ingested, which a first install has and a broken one looks identical to |
| `lore performance` | Performance category distribution |
| `lore doctor` | Audit materialized pages against the contracts they must satisfy — text cleanliness, a section whose input was recorded and never answered, and credentials in issuer-published forms. Non-zero on a defect or an unreadable page; a credential is REPORTED and does not gate, because its repair is at the issuer and editing the page would not undo the leak |
| `lore schedule --format launchd --bin <abs> --pipeline-dir <dir>` | Print scheduled-task definitions. Both flags are required in practice — see the notes below; the bare form is rarely the one you want |
| `lore maintenance [--dry-run]` | Prune operational history (ingest log, drained queue files) past `maintenance.retention_days` (default 90d). Streaming event logs are permanent, and each source's latest log entry survives any horizon — it is the state `lore health` reads, not history |
| `lore queue prune [--dry-run]` | Leave the queue holding only work that still needs an LLM session: drop dead tasks (stale / missing-target), retire a run whose every task is already answered |
| `lore self status` | What this installation is, and whether every deployed copy — skills, pipelines, templates, config example, `AGENTS.md` — still matches the running binary. Non-zero when any differs |
| `lore self deploy` | Rewrite those copies from what the binary carries. This is the repair `lore self status` reports, and what an install and an update both run |
| `lore self update [--version V]` | Replace the binary with a published release, then redeploy. Refuses while the queue still holds work, and refuses a release older than the running one unless `--version` names it deliberately |
| `lore self uninstall` | Remove the binary and everything it deployed. The vault is never touched |

## Trigger mapping

- "ingest today" → `lore ingest`
- "ingest email only" → `lore ingest email-digest`
- "weekly review" → `lore synthesis weekly --previous`
- "monthly review" → `lore synthesis monthly --previous`
- "quarterly review" → `lore synthesis quarterly --previous`
- "annual review" → `lore synthesis annual --previous`
- "show performance" → `lore performance`
- "오늘 뭐 하지" / "what's on today" → the `lore-day` skill, which runs the board off `lore agenda --json`
- "이거 해야 해" / "add a task" → `lore task add`
- "오늘 뭐 해야 하지" / "what should I pick up" → the `lore-day` skill; the board and its proposals are its ground, not this one's
- "check status" → `lore status`
- "health check" → `lore health`
- "generate cron" / "schedule it" → `lore schedule` (read the flag notes first)
- "prune old logs" → `lore maintenance`
- "clean dead queue tasks" → `lore queue prune`
- "check the vault for defects" → `lore doctor`
- "is my install current" / "update lore" → `lore self status` / `lore self update`

## Scheduling flags

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
  a concept. Only those two jobs take it — the janitors and monthly+ syntheses have no
  LLM stage.

Every emitted path must be absolute (`--bin`, `--pipeline-dir`): launchd searches no
`PATH` and expands no `~`.

## Output semantics

- Progress and diagnostics → **stderr**
- Data (cron lines) → **stdout**
- Exit codes: `0` success, non-zero on any failure. Findings-style commands
  have their own conventions: `lore health` exits `1` when a source is overdue,
  `lore graph *` uses `0` no contradiction / `1` the vault contradicts itself
  (broken link, index drift, unknown category, one name on two pages, two files on
  one address, an unnormalized slug) / `2` runtime error. A report about a healthy vault exits
  `0` even when it lists things — orphans, hubs, open conflicts, the audit
  worklist — so no `lore graph` call needs its exit code suppressed.

## Configuration

Required: `./config.yaml` or `LORE_CONFIG` / `--config`.

Relative `vault.root` resolves against the config file's parent
directory, not CWD.

Templates are embedded in the binary. Override with `--template-dir`.

## Credentials

| Source | Env vars |
|--------|---------|
| Google (Gmail/Drive/Calendar) | `LORE_GOOGLE_CLIENT_ID`, `LORE_GOOGLE_CLIENT_SECRET`, `LORE_GOOGLE_REFRESH_TOKEN` |
| Slack | `LORE_SLACK_TOKEN` |
| Atlassian (Jira/Confluence) | `LORE_ATLASSIAN_SITE_URL` + `LORE_ATLASSIAN_PAT`, or `+ LORE_ATLASSIAN_EMAIL` + `LORE_ATLASSIAN_API_TOKEN` |

OAuth is not env-supplied: its refresh token rotates and must be written back, so `lore init credentials` owns it.

Default provider is `queue` (buffers tasks to JSONL for `/lore-process`).
`provider: noop` selects `NoopLlmClient` (no summarisation/concepts).

## Ingest flow

There is no commit step. A daily page is re-rendered in full every run, so any
write or flush failure just leaves the affected pages for the next run to reproduce
byte-identically (idempotent — that is the no-data-loss guarantee). Per-source
transactionality applies only to the LLM queue: each source opens a queue boundary
and a source whose plan fails rolls back its own buffered tasks, so the flushed
queue never references an unwritten page.

1. **Plan each source** — fetch, normalize, intra-batch dedup, classify. A source
   that fails is recorded and skipped; the run continues with the rest.
2. **Write pages** — atomic per file (tmp + rename). `manual` — and only `manual` — writes
   `<wiki>/documents/{slug}.md`; every other source type writes
   `<daily>/{source-id}/DATE.md`. Concept pages are a cross-source aggregate, rendered once
   after all sources plan.
3. **Write work-log** — personal events only, and only when the optional `personal:`
   module is configured (a source must be in `personal.tracked_sources` AND match its
   adapter `is_self`; `manual`/RSS/Drive have no authorship, so never produce work-log
   entries even if listed). FULL ingest only: a source-filtered run skips this step —
   its event set is a structural subset and would overwrite the complete page.
4. **Flush LLM queue** — one atomic JSONL task file (queue mode).
5. **Archive** — `manual` inbox files move to `archived/{date}/`, only after every
   vault write and the queue flush succeeded, so a mid-run failure leaves them for retry.

In **queue mode** (the default), phases 1–5 leave summary/concept/work-log
sections empty and emit JSONL tasks. They are NOT knowledge yet — run
**`/lore-process`** afterward to drain the queue (fill summaries, create/merge
concept pages, work-log topic synthesis). Its own Finalize step reconciles the
graph and the generated wiki pages; that list lives there rather than being
restated here, because a partial copy of it is how the knowledge timeline went
stale. A daily run is `lore ingest` → `/lore-process`, not `lore ingest` alone.

## Safety notes

- Re-running for TODAY is safe: a daily page is a materialized view re-rendered in full
  from the source window, and the same window returns the same events, so a duplicate run
  reproduces the same bytes. Only wasteful, never corrupting.
- `lore ingest --date <past>` re-materializes that day — and RE-FETCHES it. A source whose
  window has passed returns fewer events than the page already holds (an RSS feed drops
  older items; a Gmail `newer_than:` query stops matching), and the re-render replaces the
  event list with what came back. Measured: a page holding 25 events, re-ingested two months
  later, kept 10. So it repairs a page whose source can still reproduce it, and truncates one
  whose source cannot. `--dry-run` reports the event count it would write; compare that
  against the count the page states before running it for real.
- An LLM-owned section (a summary, refined events, concept links) survives a re-render only
  while its `llm_inputs.<key>_done` marker matches the input the render computes. That is the
  ordinary case, and the section is spliced through from the page on disk. Where the marker is
  absent or stale the section is ANSWERED AGAIN, which means it is written empty and re-queued
  — so a body nobody recorded (written by hand, or by a drain that did not stamp) is lost.
  `lore ingest` names each section it is about to empty before it writes; `lore doctor` lists
  the pages in that state.
