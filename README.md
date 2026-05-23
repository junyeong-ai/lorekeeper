# wiki-ingest

Config-driven knowledge ingestion pipeline for Obsidian wikis.

Collects daily data from heterogeneous sources (Gmail, Google Drive, Slack, Jira, Google Calendar), deduplicates, classifies, extracts concepts via LLM, and writes structured markdown pages to an Obsidian vault. Also tracks personal work for performance reviews (weekly / monthly / quarterly / annual).

## Install

**macOS / Linux** (one-liner — downloads prebuilt binary, templates, and Claude Code skill):
```bash
curl -fsSL https://raw.githubusercontent.com/junyeong-ai/wiki-ingest/main/scripts/install.sh | bash
```

**Windows** (PowerShell):
```powershell
irm https://raw.githubusercontent.com/junyeong-ai/wiki-ingest/main/scripts/install.ps1 | iex
```

The installer:
- Downloads the prebuilt `wi` binary to `~/.local/bin` (configurable)
- Installs `templates/` to `$XDG_DATA_HOME/wi-ingest/templates/`
- Installs the Claude Code skill to `~/.claude/skills/wiki-ingest/`
- Verifies SHA256 checksums
- Adds quarantine strip + ad-hoc codesign on macOS
- Checks `PATH` and prints next steps

Install flags: `--version`, `--install-dir`, `--data-dir`, `--skill {user,project,none}`,
`--from-source`, `--force`, `--yes`, `--dry-run`. Env vars: `WI_INSTALL_*`.

Uninstall: `./scripts/uninstall.sh [--yes] [--keep-data]`.

## Quick Start

```bash
# 1. Install (see above)

# 2. Create your config
cp config.example.yaml config.yaml
$EDITOR config.yaml

# 3. Set credentials (env vars or <vault>/.wiki-ingest/credentials.json)
export WI_GOOGLE_CLIENT_ID=...
export WI_GOOGLE_CLIENT_SECRET=...
export WI_GOOGLE_REFRESH_TOKEN=...
export WI_SLACK_TOKEN=xoxb-...
export WI_JIRA_URL=https://...atlassian.net
export WI_JIRA_EMAIL=you@company.com
export WI_JIRA_TOKEN=...
export ANTHROPIC_API_KEY=sk-ant-...    # optional

# 4. Verify
wi validate

# 5. Run
wi ingest

# 6. Register schedule
wi schedule | crontab -
```

## Build from source

```bash
cargo build --release
./target/release/wi --help
```

Or via the installer with `--from-source` (requires Rust toolchain).

## Architecture

```
Data Sources              wi (Rust CLI)              Obsidian Vault
────────────              ────────────               ──────────────
Google Drive ──┐          ┌─ Extract (per-source)    daily/{source-id}/
Gmail ─────────┤          ├─ Normalize → Event       me/work-log/
Slack ─────────┼─ config ─┤  Deduplicate (cascade)   weekly/ monthly/
Jira ──────────┤  .yaml   ├─ Classify (labels)       quarterly/ annually/
Calendar ──────┘          ├─ Concepts (LLM)          wiki/concepts/
                          └─ Render (templates)
```

## Commands

| Command | Purpose |
|---------|---------|
| `wi validate` | Verify config.yaml |
| `wi ingest [source]` | Run ingestion (all enabled sources or single ID) |
| `wi ingest --dry-run` | Preview without writing to vault |
| `wi ingest --force` | Skip dedup, re-ingest everything |
| `wi synthesis weekly` | Generate weekly cross-source synthesis + personal summary |
| `wi synthesis monthly` | Aggregate work-log into monthly summary |
| `wi synthesis quarterly` | Generate quarterly performance review |
| `wi synthesis annual` | Generate annual performance review |
| `wi ingest --date YYYY-MM-DD` | Override target date (filters events) |
| `wi status` | Show last ingest time per source |
| `wi health` | Check staleness (warn if no ingest in 48h, exit 0 on first install) |
| `wi health --strict` | Also exit 1 if any source has never been ingested |
| `wi performance` | Show work category distribution |
| `wi schedule` | Print crontab entries (uses plain `wi` for PATH lookup) |
| `wi schedule --bin /full/path/wi` | Override bin path in cron lines |
| `wi maintenance` | Prune ingest log and dedup cache entries older than 90 days |

## LLM provider modes

`llm.provider` in `config.yaml` selects how semantic work (summarize, concept extraction) is performed:

| Mode | Default | Best for |
|------|:-------:|---------|
| `queue` | ✓ | Daily Claude Code users. Pipeline emits JSONL tasks to `<vault>/.wiki-ingest/queue/`; the `/wi-process` skill drains them using Claude Code's native LLM session — no API key, no separate billing. |
| `noop` |  | Development, CI, or vault-only sources where you only need Rust templating without semantic enrichment. |
| `anthropic` |  | Unattended cron on headless servers (no Claude Code session available). Requires `ANTHROPIC_API_KEY`; pipeline does end-to-end work in one process. |

Workflow in `queue` mode:
1. `wi ingest` (cron-scheduled) — fetches sources, dedups, writes structural pages, queues semantic tasks
2. `/wi-process` (run in Claude Code) — drains the queue, fills summaries, creates/merges concept pages

The skill is **fully idempotent**: re-running on a partially-processed queue file is safe because vault edits replace section content rather than append, and concept page merging preserves accumulated state.

## Workspace Structure

```
crates/
  wi-core/      Domain types, config, error, vault path builder
  wi-vault/     Obsidian vault I/O: read, write, frontmatter, templates, log
  wi-source/    Source adapters: Gmail, Drive, Slack, Jira, Calendar
  wi-pipeline/  Transform stages: normalize, dedup, classify, render, synthesis
  wi-llm/       LLM client abstraction (Claude API + Noop fallback)
  wi-cli/       Binary entry point (wi)

templates/      Jinja2 markdown templates (.md.jinja)
```

## Output Model

**Primary** (per-source): `daily/{source-id}/YYYY-MM-DD.md` — one page per event date (events from multi-day batches are split correctly)

**Derived** (cross-source):
- `me/work-log/YYYY-MM-DD.md` — aggregated from sources with `track_personal: true`, grouped by date
- `weekly/synthesis/YYYY-W{nn}.md` — cross-source weekly themes
- `weekly/me/YYYY-W{nn}.md` — personal weekly summary
- `monthly/me/YYYY-MM.md` — monthly work summary
- `quarterly/me/YYYY-Q{n}.md` — quarterly performance review
- `annually/me/YYYY.md` — annual performance review
- `wiki/concepts/{slug}.md` — extracted concepts (merge-on-rewrite: `reference_count` and `sources` accumulate)

## Templates

Templates live in `templates/` and use Jinja2 syntax (minijinja). Lookup order:

1. `{source-id}.md.jinja` — user override per source ID (optional)
2. `{source-type}.md.jinja` — default per source type (`gmail`, `google-drive`, `slack-channel`, `slack-search`, `jira`, `google-calendar`)
3. Embedded fallback

Periodic templates: `work-log`, `weekly-synthesis`, `weekly-personal`, `monthly-summary`, `quarterly-review`, `annual-review`, `concept`.

## Timezone

`vault.timezone` controls how `Timestamp → Date` is derived. Set to an IANA name (e.g., `Asia/Seoul`, `America/New_York`) or `system`. An item received at `2026-05-22T23:00:00Z` lands in:
- `2026-05-22.md` with `timezone: UTC` or `timezone: Europe/London` (winter)
- `2026-05-23.md` with `timezone: Asia/Seoul` (KST = UTC+9)

## Scheduling

```bash
# Generate cron entries from config
wi schedule > /tmp/wi-cron.txt
crontab /tmp/wi-cron.txt
```

Each source's `schedule:` field in config.yaml uses standard cron syntax. The weekly synthesis runs on the schedule from `synthesis.weekly.schedule`.

## Credentials

Two ways to provide credentials, env vars take precedence over file:

**Environment variables** (recommended for development):
- `WI_GOOGLE_CLIENT_ID`, `WI_GOOGLE_CLIENT_SECRET`, `WI_GOOGLE_REFRESH_TOKEN`
- `WI_SLACK_TOKEN`
- `WI_JIRA_URL`, `WI_JIRA_EMAIL`, `WI_JIRA_TOKEN`
- `ANTHROPIC_API_KEY` (optional; falls back to no-op LLM)

**Credentials file** at `{vault_root}/.wiki-ingest/credentials.json`:
```json
{
  "google": { "client_id": "...", "client_secret": "...", "refresh_token": "..." },
  "slack":  { "bot_token": "xoxb-..." },
  "jira":   { "base_url": "https://...", "email": "...", "api_token": "..." }
}
```

## Development

```bash
cargo check                  # Type check
cargo test                   # Run all tests
cargo clippy -- -D warnings  # Lint (must be clean)
cargo fmt                    # Format
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
