# wiki-ingest

Config-driven knowledge ingestion pipeline for Obsidian wikis.
Rust CLI that collects daily data from heterogeneous sources, deduplicates,
classifies, extracts concepts, and writes structured markdown pages.

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

## Workspace

```
crates/
  wi-core/      Domain types, config (incl. timezone, source_category_map), validation
  wi-vault/     Obsidian vault I/O: read, write (atomic), frontmatter (CRLF-aware), templates, log
  wi-source/    Source adapters: Gmail, Drive, Slack, Jira, Calendar + Credentials
  wi-pipeline/  PipelineContext, Pipeline (per-date grouping), Synthesizer, concept pages, worklog
  wi-llm/       LlmClient trait (Claude + Noop + Mock)
  wi-cli/       Binary `wi` — commands/ submodules per subcommand
```

## Key Design Decisions

- **Source ID = vault directory**: `sources.{id}` config key becomes `daily/{id}/` output path.
- **Template lookup**: `{source-id}.md.jinja` (user override) → `{source-type}.md.jinja` (default) → embedded fallback.
- **Date derivation**: `item.timestamp.to_zoned(config.vault.timezone()).date()` — never UTC by accident.
- **Pipeline shares context**: `Arc<PipelineContext>` between `Pipeline` and `Synthesizer` (engine, llm, dirs, perf, identity, timezone) for DRY.
- **Concept pages persist**: `wiki/concepts/{slug}.md` is written + merged (mention_count, sources accumulate across runs). Slugs are re-normalized via `slugify()` to prevent path-injection from LLM output.
- **Multi-date events**: events spanning multiple dates produce one `daily/` page per date (not collapsed into first event's date). LLM summarize+extract runs per date.
- **LLM provider modes** (`llm.provider`):
  - `anthropic` — direct Messages API (unattended cron, requires `ANTHROPIC_API_KEY`)
  - `queue` — Pipeline emits JSONL tasks to `<vault>/.wiki-ingest/queue/`; a Claude Code skill (`/wi-process`) drains them using its native LLM session (no API key, no separate billing). Best for daily Claude Code users.
  - `noop` — no LLM work; daily pages render without summary/concept content.
- **LLM graceful degradation**: `anthropic` mode without `ANTHROPIC_API_KEY` falls back to `NoopLlmClient` with a warning. LLM errors are logged via `tracing::warn` and fall back to empty results so ingest never fails on semantic work alone.
- **Dedup retention**: `wi maintenance` prunes both ingest log and dedup cache entries older than 90 days. Must not overlap a running `wi ingest`.
- **Atomic 4-phase ingest** (`wi ingest`): (1) plan all sources, (2) write daily+concept pages, (3) write aggregated work-log, (4) commit dedup. Any failure aborts at the affected phase and dedup is NOT committed, so re-running is idempotent and lossless.
- **Schedule subcommands honor `--previous`**: `wi synthesis weekly --previous` synthesizes the just-completed period instead of the current one. `wi schedule` emits `--previous` automatically in generated cron lines.
- **Global CLI flags**: `--config <path>` / `WI_CONFIG` and `--template-dir <path>` / `WI_TEMPLATE_DIR` are global. `wi schedule` injects these into generated cron lines so scheduled tasks don't depend on CWD.
- **Relative vault.root**: resolved against the config file's parent directory, not the process CWD.
- **Per-source category mapping**: `performance.source_category_map` (by source ID) → `source_type_category_map` (by source type) → event `classification` → `uncategorized_label`.

## Development

```bash
cargo check                    # type check
cargo test                     # run tests
cargo clippy                   # lint
cargo run -- validate          # verify config.yaml
cargo run -- ingest ai-news    # run single source
cargo run -- status            # show ingest status
```

## Config

User-specific settings live in `config.yaml` (gitignored).
Copy `config.example.yaml` to get started.

Key principle: **source ID = vault directory name**.
The key under `sources:` determines both the adapter configuration
and the output path (`daily/{source-id}/YYYY-MM-DD.md`).

## Source Types

| Type | Adapter | Use For |
|------|---------|---------|
| `google-drive` | Drive API | File-based sources (newsletters) |
| `gmail` | Gmail API | Email digest |
| `slack-channel` | Slack API | Channel message reader |
| `slack-search` | Slack API | Keyword trend search |
| `jira` | Jira REST API | Issue/ticket tracking |
| `google-calendar` | Calendar API | Schedule/meeting tracking |

## Output Model

**Primary** (per-source): `daily/{source-id}/YYYY-MM-DD.md`
**Derived** (cross-source):
- `me/work-log/` — aggregated from sources with `track_personal: true`
- `weekly/synthesis/` — cross-source weekly themes
- `weekly/me/`, `monthly/me/`, `quarterly/me/`, `annually/me/` — performance tracking
- `wiki/concepts/` — extracted concepts
