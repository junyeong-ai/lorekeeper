# Lorekeeper

Config-driven knowledge ingestion pipeline for Obsidian wikis.

Collects daily data from heterogeneous sources (Gmail, Google Drive, Slack, Jira, Google Calendar, RSS/Atom feeds), deduplicates, classifies, extracts concepts via LLM, and writes structured markdown pages to an Obsidian vault. Also tracks personal work for performance reviews (weekly / monthly / quarterly / annual).

## Install

**macOS / Linux** (one-liner — downloads prebuilt binary, templates, and Claude Code skills):
```bash
curl -fsSL https://raw.githubusercontent.com/junyeong-ai/lorekeeper/main/scripts/install.sh | bash
```

**Windows** (PowerShell):
```powershell
irm https://raw.githubusercontent.com/junyeong-ai/lorekeeper/main/scripts/install.ps1 | iex
```

The installer:
- Downloads the prebuilt `lore` binary to `~/.local/bin` (configurable)
- Installs `templates/` to `$XDG_DATA_HOME/lorekeeper/templates/`
- Installs the Claude Code skills (`lore-ingest`, `lore-process`, `lore-setup`, `lore-wiki`, `lore-capture`, `lore-extract`) to `~/.claude/skills/`
- Verifies SHA256 checksums
- Adds quarantine strip + ad-hoc codesign on macOS
- Checks `PATH` and prints next steps

Install flags: `--version`, `--install-dir`, `--data-dir`, `--skill {user,project,none}`,
`--from-source`, `--force`, `--yes`, `--dry-run`. Env vars: `LORE_INSTALL_*`.

Uninstall: `./scripts/uninstall.sh [--yes] [--keep-data]`.

## Quick Start

```bash
# 1. Install (see above)

# 2. Create your config — auto-discovered at ./config.yaml (repo) or
#    ~/.config/lorekeeper/config.yaml (binary-only install). Override with --config/LORE_CONFIG.
cp config.example.yaml config.yaml                                  # from a repo
# cp ~/.config/lorekeeper/config.example.yaml ~/.config/lorekeeper/config.yaml   # binary install
$EDITOR config.yaml

# 3. Set credentials — easiest is the interactive wizard:
lore init credentials
#    (or env vars / <vault>/.lorekeeper/credentials.json by hand)

# 4. Verify
lore validate

# 5. Run
lore ingest

# 6. Register schedule
lore schedule | crontab -
```

## Build from source

```bash
cargo build --release
./target/release/lore --help
```

Or via the installer with `--from-source` (requires Rust toolchain).

## Architecture

```
Data Sources              lore (Rust CLI)            Obsidian Vault
────────────              ───────────────            ──────────────
Google Drive ──┐          ┌─ Extract (per-source)    daily/{source-id}/
Gmail ─────────┤          ├─ Normalize → Event       me/work-log/
Slack ─────────┼─ config ─┤  Deduplicate (cascade)   me/{weekly,monthly,quarterly,annual}/
Jira ──────────┤  .yaml   ├─ Classify (labels)       synthesis/{weekly}/
Calendar ──────┤          ├─ Concepts (LLM)          wiki/concepts/
RSS/Atom ──────┤          ├─ Render (templates)      wiki/documents/
Manual inbox ──┘          ├─ Wiki index (catalog)    wiki/index.md (by-topic)
                          ├─ Wiki log (timeline)     wiki/log.md (by-time)
                          └─ Graph (lint, stale,     wiki/AGENTS.md
                               cluster, backlinks,
                               alias, audit)

Claude Code Skills        Semantic Plane             (same vault)
──────────────────        ──────────────             ────────────
/lore-ingest ────────── lore CLI wrapper ──────────→ daily/ me/ synthesis/ …
/lore-process ───────── LLM queue drain ──────────→ summaries + concepts
/lore-capture ───────── real-time capture ─────────→ wiki/documents/
/lore-extract ───────── batch repo extraction ─────→ wiki/documents/
/lore-wiki ──────────── query / audit / add ───────→ wiki/
/lore-setup ─────────── config builder ────────────→ config.yaml
```

## Commands

| Command | Purpose |
|---------|---------|
| `lore init credentials` | Interactive wizard — writes `<vault>/.lorekeeper/credentials.json` |
| `lore validate` | Verify config.yaml |
| `lore ingest [source]` | Run ingestion (all enabled sources or single ID) |
| `lore ingest --dry-run` | Preview without writing to vault |
| `lore ingest --force` | Skip dedup, re-ingest everything |
| `lore synthesis weekly` | Generate weekly cross-source synthesis + personal summary |
| `lore synthesis monthly` | Aggregate work-log into monthly summary |
| `lore synthesis quarterly` | Generate quarterly performance review |
| `lore synthesis annual` | Generate annual performance review |
| `lore ingest --date YYYY-MM-DD` | Override target date (filters events) |
| `lore status` | Show last ingest time per source |
| `lore health` | Check staleness (warn if no ingest in 48h, exit 0 on first install) |
| `lore health --strict` | Also exit 1 if any source has never been ingested |
| `lore performance` | Show work category distribution |
| `lore schedule` | Print crontab entries (uses plain `lore` for PATH lookup) |
| `lore schedule --bin /full/path/lore` | Override bin path in cron lines |
| `lore maintenance` | Prune ingest log, dedup cache, and drained queue files older than 90 days |
| `lore schema` | Generate `wiki/AGENTS.md` (page format schema from locale) |
| `lore graph lint` | Structural health: orphans, broken links, hubs, invalid categories, near-duplicates, alias conflicts, unresolved conflicts, index drift |
| `lore graph suggest-links` | Community-based cross-reference suggestions |
| `lore graph cluster` | Topic communities via Louvain modularity |
| `lore graph backlinks-sync` | Re-derive each concept's `## Sources` + `source_count` from the wikilink graph (resolves `[[alias]]` citations to the canonical concept) |
| `lore graph merge <from> <into>` | Fold a duplicate concept into a canonical one (rewires wikilinks, deletes `from`) |
| `lore graph stale --days N` | Report pages that are old AND no longer cited by recent activity (dormant, not just old) |
| `lore graph audit-candidates` | Concepts whose source set changed since their last contradiction audit |
| `lore graph audit-mark <slug>` | Record a concept as audited (drops it from `audit-candidates` until its sources change) |
| `lore wiki index` | Rebuild `wiki/index.md` — by-topic catalog |
| `lore wiki log` | Rebuild `wiki/log.md` — by-time knowledge timeline (recent window) |
| `lore wiki concepts` | List all concept pages |
| `lore queue status` | Classify pending LLM tasks: current / stale / missing-target |

## LLM provider modes

`llm.provider` in `config.yaml` selects how semantic work (summarize, concept extraction) is performed:

| Mode | Default | Best for |
|------|:-------:|---------|
| `queue` | ✓ | Daily Claude Code users. Pipeline emits JSONL tasks to `<vault>/.lorekeeper/queue/`; `/lore-process` drains them using Claude Code's native LLM session — no API key, no separate billing. |
| `noop` |  | Development, CI, or vault-only sources where you only need Rust templating without semantic enrichment. |

Workflow:
1. `lore ingest` (cron-scheduled) — fetches sources, dedups, writes structural pages, queues semantic tasks
2. `/lore-process` (run in Claude Code) — drains the queue, fills summaries, creates/merges concept pages
3. For unattended cron: `lore ingest && claude -p "/lore-process"`

For a single autonomous agent that chains ingest → `/lore-process` → graph reconcile
(→ Monday weekly synthesis), the installer ships the **`lore-daily-ingest`** scheduled
task to `~/.claude/scheduled-tasks/lore-daily-ingest/SKILL.md` (alongside the skills).
Point your cron or remote-agent runner at it; the source template is
[`scripts/lore-daily-ingest.md`](scripts/lore-daily-ingest.md).

The skill is **fully idempotent**: re-running on a partially-processed queue file is safe because vault edits replace section content rather than append, and concept page merging preserves accumulated state.

## Claude Code Skills

Six skills provide the Claude Code integration surface. All follow the `lore-{verb}` naming convention and are written in English (AI-native design).

| Skill | Purpose | Model-invocable |
|-------|---------|:---------------:|
| `/lore-ingest` | Daily source ingestion — wraps the `lore` CLI for ingest, synthesis, status, health, schedule | No (manual trigger) |
| `/lore-process` | Drain the LLM queue after ingest — fills summaries, extracts concepts, synthesises work-log topics | Yes |
| `/lore-setup` | Interactive config builder — discovers Slack channel IDs, Jira projects, Google calendars via CLIs | Yes |
| `/lore-wiki` | Semantic wiki operations — add sources, query with compounding, audit structural + semantic health | Yes |
| `/lore-capture` | Real-time knowledge capture — grab insights from active troubleshooting before context fades | Yes |
| `/lore-extract` | Batch project knowledge extraction — scan → manifest → run → audit workflow for existing docs | Yes |

**`/lore-capture`** and **`/lore-extract`** are the project-knowledge harvesting pair:
- **capture**: one insight at a time, during active work (high urgency, low volume)
- **extract**: entire documentation corpus, planned batch operation (low urgency, high volume)

Both write to `wiki/documents/` and `wiki/concepts/` (paths resolved from AGENTS.md), sharing the same concept dedup and graph infrastructure as daily ingestion.

### Extraction manifest

`/lore-extract` persists a manifest at `<vault>/.lorekeeper/extracts/<project>/manifest.yaml` during the scan phase. The manifest records discovered sources, transferability classifications, identifier strip patterns, and concept category mappings. Subsequent runs consume the manifest for consistency; re-scans diff against the previous state for incremental updates.

## Workspace Structure

```
crates/
  lk-core/      Domain types, config, error, vault path builder
  lk-vault/     Obsidian vault I/O: read, write, frontmatter, templates, log
  lk-source/    Source adapters: Gmail, Drive, Slack, Jira, Calendar, RSS, Manual
  lk-pipeline/  Transform stages: normalize, dedup, classify, render, synthesis
  lk-queue/     Semantic task queue: LlmClient trait, JSONL queue, noop, test mock
  lk-graph/     Wikilink graph analysis (lint, hubs, cluster, suggest-links)
  lk-cli/       Binary entry point (lore)

templates/      Jinja2 markdown templates (.md.jinja, embedded in the binary)
```

## Output Model

**Primary** (per-source): `daily/{source-id}/YYYY-MM-DD.md` — one page per event date (events from multi-day batches are split correctly)

**Derived** (cross-source):
- `me/work-log/YYYY-MM-DD.md` — aggregated from sources with `track_personal: true`, grouped by date
- `synthesis/weekly/YYYY-W{nn}.md` — cross-source weekly themes
- `me/weekly/YYYY-W{nn}.md` — personal weekly summary
- `me/monthly/YYYY-MM.md` — monthly work summary
- `me/quarterly/YYYY-Q{n}.md` — quarterly performance review
- `me/annual/YYYY.md` — annual performance review
- `wiki/concepts/{slug}.md` — extracted concepts; re-extraction merges in place (keeps `created`, title, and category; widens first/last-seen). `source_count` and the `## Sources` body are re-derived from the wikilink graph by `lore graph backlinks-sync` — there is no `sources` frontmatter array
- `wiki/explorations/{slug}.md` — reusable Q&A syntheses filed by `/lore-wiki query`

## Templates

Templates live in `templates/` and use Jinja2 syntax (minijinja). Lookup order:

1. `{source-id}.md.jinja` — user override per source ID (optional)
2. `{source-type}.md.jinja` — default per source type (`gmail`, `google-drive`, `slack-channel`, `slack-search`, `jira`, `google-calendar`, `rss`, `manual`)
3. Embedded fallback

Periodic templates: `work-log`, `weekly-synthesis`, `weekly-personal`, `monthly-summary`, `quarterly-review`, `annual-review`, `concept`, `document`.

## Timezone

`vault.timezone` controls how `Timestamp → Date` is derived. Set to an IANA name (e.g., `Asia/Seoul`, `America/New_York`) or `system`. An item received at `2026-05-22T23:00:00Z` lands in:
- `2026-05-22.md` with `timezone: UTC` or `timezone: Europe/London` (winter)
- `2026-05-23.md` with `timezone: Asia/Seoul` (KST = UTC+9)

## Scheduling

```bash
# Generate cron entries from config
lore schedule > /tmp/lore-cron.txt
crontab /tmp/lore-cron.txt
```

Each source's `schedule:` field in config.yaml uses standard cron syntax. The weekly synthesis runs on the schedule from `synthesis.weekly.schedule`.

## Credentials

Two ways to provide credentials, env vars take precedence over file:

**Environment variables** (recommended for development):
- `LORE_GOOGLE_CLIENT_ID`, `LORE_GOOGLE_CLIENT_SECRET`, `LORE_GOOGLE_REFRESH_TOKEN`
- `LORE_SLACK_TOKEN`
- `LORE_JIRA_URL`, `LORE_JIRA_EMAIL`, `LORE_JIRA_TOKEN`

**Interactive wizard** (easiest): `lore init credentials` prompts for each provider
(skip the ones you don't use), masks secret entry, and writes
`<vault>/.lorekeeper/credentials.json` with `0600` permissions. Re-running edits in
place — press enter to keep an existing secret.

For Google it can **mint the refresh token for you**: it opens the consent page in your
browser, captures the redirect on a localhost port, and stores the resulting token — no
OAuth Playground needed. This requires the OAuth client to be of type **"Desktop app"**
(Google then allows the `http://127.0.0.1` redirect automatically); it requests the
Gmail / Drive / Calendar **read-only** scopes.

**Credentials file by hand** (one file instead of seven env vars). Copy the template
and fill in values:
```bash
cp credentials.example.json "<vault_root>/.lorekeeper/credentials.json"
chmod 600 "<vault_root>/.lorekeeper/credentials.json"
$EDITOR "<vault_root>/.lorekeeper/credentials.json"
```
```json
{
  "google": { "client_id": "...", "client_secret": "...", "refresh_token": "..." },
  "slack":  { "bot_token": "xoxb-...", "user_token": "xoxp-..." },
  "jira":   { "base_url": "https://...", "email": "...", "api_token": "..." }
}
```
All three blocks are optional — keep only the sources you use. Matching env vars
(`LORE_GOOGLE_*`, `LORE_SLACK_TOKEN` / `LORE_SLACK_USER_TOKEN`, `LORE_JIRA_*`) override the file
per service.

**Slack tokens**: provide a bot token (`xoxb-`), a user token (`xoxp-`), or both. The
channel reader (`slack-channel`) accepts either; the keyword-trend search
(`slack-search`) requires a **user token** because Slack's `search.messages` API is not
available to bot tokens.

**Google `refresh_token`**: this is not shown in the Cloud Console alongside the
client ID/secret — it's minted once by completing the OAuth consent flow with
`access_type=offline`. Easiest: run `lore init credentials` and let it mint the token in
your browser (needs a "Desktop app" OAuth client). Manual alternative: the
[OAuth 2.0 Playground](https://developers.google.com/oauthplayground) with your own
client ID/secret and the Gmail/Drive/Calendar read-only scopes. The pipeline then uses
the refresh token to renew access tokens unattended.

## Development

```bash
cargo check                                            # Type check
cargo nextest run --workspace                          # Run all tests
cargo clippy --workspace --all-targets -- -D warnings  # Lint (must be clean)
cargo fmt                                              # Format
```

## Dependencies

Rust 1.95 / 2024 edition. Key crates:

- `tokio` (async runtime)
- `reqwest` (HTTP)
- `serde` / `serde_yaml_ng` / `serde_json` (serialization)
- `clap` (CLI)
- `jiff` (date/time)
- `minijinja` (templates)
- `redb` (embedded dedup cache)
- `strsim` (title similarity)
- `blake3` (event ID hashing)
- `thiserror` / `miette` (errors)
- `tracing` / `tracing-subscriber` (structured logging)
