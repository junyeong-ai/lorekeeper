---
name: lore-ingest
version: 0.61.3
description: Runs Lorekeeper collection and operations commands for configured sources, manual inboxes, period synthesis, schedules and maintenance. Collection writes structured pages and queues semantic work; lore-process completes that work.
argument-hint: "<subcommand> [args]"
disable-model-invocation: true
allowed-tools: |
  Bash(lore *)
  Bash(crontab *)
  Read
---

# Lorekeeper collection and operations

Run the CLI for the requested collection or operational action. Use command help
for current flags; read only the relevant reference:

| Task | Reference |
|---|---|
| Collect all/one source, backfill, retry, or inspect raw inbox preservation | [Ingestion](references/ingestion.md) before collecting |
| Select a CLI operation or period synthesis | [Commands](references/commands.md) |
| Install or change a schedule | [Scheduling](references/scheduling.md) |

For morning brief/task-board requests, use `/lore-day`; for wiki evidence lookup,
use `/lore-wiki`. Configuration discovery is `--config`/`LORE_CONFIG`, then
`./config.yaml`, then `~/.config/lorekeeper/config.yaml`. Relative `vault.root`
resolves against the config file's directory. Templates are embedded unless
`--template-dir` selects an override.

## Collection is not semantic completion

`lore ingest` collects and normalizes observations, writes pages atomically per
file, then publishes queue tasks. Document-unit sources (`manual`, `nodex`) write
documents; day-unit sources write daily aggregates. Follow the source descriptor,
not a hardcoded source-name rule. Concept pages aggregate evidence across sources.

In queue mode, run `/lore-process` afterward. It owns the typed publication,
reconciliation and completion checks. An empty inbox only means collection is
finished; it says nothing about pending synthesis or unresolved knowledge reviews.
Do not call queued pages fully knowledge-integrated.

A run is not a multi-page transaction. Never archive raw inputs after partial
writes or queue failure. Exact replay requires retained identical inputs; refetch
can return edited, missing or expired evidence. `lore ingest --date <past>` is a
refetch, not restoration from an immutable historical snapshot.

A filtered ingest cannot regenerate the full cross-source work-log. Personal
tracking requires the configured module, selected sources and adapter ownership;
manual/nodex/RSS/Drive cannot acquire authorship from mentions or inclusion alone.

## Operational boundaries

- Use `lore validate` for config and `lore status` for subsystem routing.
  `lore health` measures collection freshness, not semantic quality. It can exit
  nonzero for overdue sources; `--strict` also reports never-collected sources.
- `lore doctor` reports defects/unreadable pages and credentials in known issuer
  formats. Editing source text is not credential rotation; use the issuer.
- `lore queue prune` alone retires classified queue work. Maintenance preserves
  each source's latest collection state and permanent streaming evidence logs.
- `lore self status` checks deployed artifacts; `self deploy` restores copies from
  the running binary. `self update` refuses pending semantic work. Do not overwrite
  intentional local artifact edits without accounting for them.
- Scheduled pipelines do not auto-commit user edits. Review the vault separately.
- Data goes to stdout and diagnostics to stderr. Use JSON/bare-value contracts,
  not parsed terminal tables. Graph violations gate; hubs/orphans/open reviews are
  observations, not automatic failures.

Credentials belong to `lore init credentials`. Environment overrides include
Google client ID/secret/refresh token, Slack tokens, and supported Atlassian API
credentials. Rotating OAuth refresh tokens require the credential store. Source
setup belongs to `/lore-setup`; this skill does not discover or invent account IDs.
