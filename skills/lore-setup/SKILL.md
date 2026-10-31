---
name: lore-setup
description: Guides initial Lorekeeper setup or edits source configuration using workspace
  metadata. Discovers requested source IDs, preserves existing selections, and distinguishes
  config validation, native authentication, first processing and optional scheduling.
metadata:
  version: 0.110.0
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
   - Slack → [Slack discovery](references/slack.md)
   - Jira → [Jira discovery](references/jira.md)
   - Google (Calendar/Gmail) → [Google discovery](references/google.md)
   - Confluence → [Initial setup](references/config.md#initial-setup) and the Confluence example
   - Project repositories (`nodex`) → [Nodex binary selection](references/config.md#nodex-binary-selection)
   - Other supported sources → `config.example.yaml` for their parameters
   - Local DOCX/XLSX/PDF/images → [Document preprocessing](references/config.md#document-preprocessing)
3. **Write config** — start a new config from `config.starter.yaml` and add only the
   requested blocks from `config.example.yaml`. The catalog's sources are disabled;
   enable selected sources after filling their real scope. Use `references/config.md`
   for surrounding modules; omit personal tracking unless requested. Ask only for
   missing choices that change that scope. Location:
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
   auth/runtime checks. For complete onboarding, continue with the authorized first
   run and retrieval checks in `references/config.md`; `lore run daily` coordinates
   collection and semantic publication through the configured execution profile.
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
- Collection or operations without configuration work → `/lore-ingest`
- Semantic processing without setup work → `/lore-process`
- Search/audit vault → `/lore-wiki`
