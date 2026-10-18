---
name: lore-setup
version: 0.61.0
description: Guides initial Lorekeeper setup or edits source configuration using workspace metadata. Discovers requested source IDs, preserves existing selections, and distinguishes config validation, native authentication, first processing and optional scheduling.
when_to_use: |
  Lorekeeper setup, configure Lorekeeper sources, add a Lorekeeper source,
  로어키퍼 초기 설정, 로어키퍼 수집 소스 추가
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
then writes a validated `config.yaml`. For a new installation, read
[initial setup](references/config.md#initial-setup) to separate configuration,
authentication, first processing and optional automation. An added source does not
reconfigure the rest of an existing vault.

## Workflow

1. **Source selection** — use the supported source types in `config.example.yaml`
   to select the sources matching the user's request. If unclear, explain the relevant
   choices briefly. Preserve source selections the user already made.
2. **Discovery** — read the reference file for each selected source
   and use available discovery CLIs to find IDs/conditions. Helper CLI login and
   Lore adapter credentials are separate; report missing prerequisites:
   - Slack → `references/slack.md`
   - Jira → `references/jira.md`
   - Google (Calendar/Gmail) → `references/google.md`
   - Confluence → `references/config.md`, Initial setup, and the Confluence example
   - Project repositories (`nodex`) → `references/config.md`, Nodex binary selection
   - Other supported sources → `config.example.yaml` for their parameters
3. **Write config** — use `references/config.md` schema within the user's authorized
   source selections. Ask only for missing choices that change that scope. Location:
   `./config.yaml` (repo) or `~/.config/lorekeeper/config.yaml`
   (binary install). If a file already exists, merge the source block
   only — never overwrite.
4. **Validate** — run `lore validate` for local config/params checks. For a config-only
   request, report outstanding schema/auth/runtime checks without performing them.
   When source access verification is requested, run `lore ingest --dry-run <source-id>`
   for the selected sources. For complete onboarding, generate a missing/outdated
   schema with `lore schema`. This replaces `<wiki>/AGENTS.md` wholesale; preserve any
   hand-written additions separately before replacing an existing file.

5. **Finish the requested setup** — a configuration-only change reports remaining
   auth/runtime checks. For complete onboarding, continue with the first real
   processing stages in `references/config.md`. Collection is the separate,
   user-invoked `/lore-ingest` step; `/lore-process` owns semantic publication.
   Do not report those stages complete after configuration. For requested automation,
   read the [scheduling procedure](../lore-ingest/references/scheduling.md).

## Design principles

- **Preserve source language** — `vault.locale: ko|en` sets generated prose and headings; source content and declared concept names retain their original language.
- **Jira uses `updated` field** — daily snapshot of touched issues.
  Start/due dates are display-only (`start_date_field` is instance-specific
  custom-field ID). Filter collection by `updated`, not start/due dates.
- **Slack = full channel** for team awareness. Personal messages auto-split
  to work-log when the source is in `personal.tracked_sources` (+ `identity.slack_id`).
  Use `watch_users` only to narrow to specific people.
- **Gmail scope follows the user's needs.** `category:primary` is an optional filter,
  not a definition of work mail; updates and notifications can contain useful evidence.
- Credentials via `lore init credentials` — this skill only fills IDs/conditions.

## When NOT to invoke

- Credentials (tokens/secrets) → `lore init credentials`
- Run ingestion → `/lore-ingest`
- Drain queue → `/lore-process`
- Search/audit vault → `/lore-wiki`
