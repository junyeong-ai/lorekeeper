---
name: lore-setup
version: 0.18.0
description: Build or edit a Lorekeeper config.yaml by inspecting the user's real workspace — discovers Slack channel IDs, Jira projects and custom-field IDs, Google calendars, and Gmail query categories via their CLIs, then writes a validated config. Use when the user wants help setting up Lorekeeper, adding a source, or finding the concrete IDs a source needs. Read-only against the user's accounts; only writes config.yaml after confirmation.
when_to_use: |
  setup Lorekeeper, configure sources, build config,
  find channel id, find jira project, add source,
  find calendar, build my config, edit config
argument-hint: "[source-type]"
allowed-tools: |
  Bash(slack-cli *)
  Bash(atlassian-cli *)
  Bash(gws *)
  Bash(jq *)
  Bash(grep *)
  Bash(ls *)
  Bash(lore validate)
  Bash(lore schema)
  Bash(lore config *)
  Bash(lore ingest --dry-run *)
  Read
  Edit
  Write
---

# lore-setup — Interactive config builder

Inspects the user's workspace to discover concrete IDs and conditions,
then writes a validated `config.yaml`. Eliminates manual lookup of
channel IDs, project keys, and custom-field IDs.

## Workflow

1. **Source selection** — confirm which sources to collect
   (Gmail / Calendar / Jira / Slack). If unclear, describe each
   source's value in one line and recommend.
2. **Discovery** — read the reference file for each selected source
   and run its CLI commands to find IDs/conditions (assume per-account
   auth is already done; if not, guide the user to the relevant CLI login):
   - Slack → `references/slack.md`
   - Jira → `references/jira.md`
   - Google (Calendar/Gmail) → `references/google.md`
3. **Write config** — use `references/config.md` schema. Location:
   `./config.yaml` (repo) or `~/.config/lorekeeper/config.yaml`
   (binary install). If a file already exists, merge the source block
   only — never overwrite.
4. **Validate** — `lore validate`, then `lore schema` if it warns that AGENTS.md is missing
   or out of date (every page-writing skill hard-depends on that file), then
   `lore ingest --dry-run` to verify auth and extraction.
   `lore schema` REPLACES `<wiki>/AGENTS.md` wholesale, and `validate` warns whenever the
   file differs for any reason — including a hand edit, which the bytes cannot be told apart
   from a generator change. It reports `Replaced` rather than `Wrote` when a file was there;
   if the vault's copy carries anything hand-written, copy it out first, because the next run
   of this step takes it.

## Design principles

- **Preserve source language** — only labels switch via `vault.locale: ko|en`.
- **Jira uses `updated` field** — daily snapshot of touched issues.
  Start/due dates are display-only (`start_date_field` is instance-specific
  custom-field ID). Never query by date range.
- **Slack = full channel** for team awareness. Personal messages auto-split
  to work-log when the source is in `personal.tracked_sources` (+ `identity.slack_id`).
  Use `watch_users` only to narrow to specific people.
- **Gmail: recommend `category:primary`** — filters out bot/notification
  noise (GitHub, CI, etc.).
- Credentials via `lore init credentials` — this skill only fills IDs/conditions.

## When NOT to invoke

- Credentials (tokens/secrets) → `lore init credentials`
- Run ingestion → `/lore-ingest`
- Drain queue → `/lore-process`
- Search/audit vault → `/lore-wiki`
