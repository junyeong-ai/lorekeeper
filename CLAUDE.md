# wiki-ingest

Config-driven knowledge ingestion pipeline for Obsidian wikis. A Rust CLI (`wi`)
collects daily data from heterogeneous sources, deduplicates, classifies, extracts
concepts, and writes structured markdown pages.

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

Each crate has its own `CLAUDE.md` with the invariants for working inside it
(loaded on demand when you open files there).

```
crates/
  wi-core/      Domain types, config + validation, vault path builder, slugify
  wi-vault/     Obsidian vault I/O: atomic write, frontmatter, templates, ingest log
  wi-source/    Source adapters + factory, per-adapter param validation
  wi-pipeline/  Pipeline (per-source plan/commit), dedup, classify, concepts, synthesis
  wi-llm/       LlmClient trait + providers: anthropic, queue, noop (+ mock for tests)
  wi-cli/       Binary `wi` — one module per subcommand under commands/
templates/      Jinja2 markdown templates (.md.jinja)
```

## Project-wide invariants

- **Source ID = vault directory**: the key under `sources:` becomes the `daily/{id}/`
  output path AND selects the adapter. It must not contain path separators.
- **Date derivation**: an event's date is `timestamp.to_zoned(vault.timezone()).date()`
  — always via the configured timezone, never UTC by accident.
- **Multi-date batches**: events spanning several dates produce one `daily/` page per
  date, not collapsed into the first event's date.
- **Atomic ingest** (`wi ingest`, 5 phases): plan all sources → write daily/concept
  pages → write work-log → flush LLM queue (atomic temp+rename) → commit dedup. The
  flush precedes the dedup commit, so a crash between them re-extracts and re-queues on
  the next run. Any failure aborts before dedup commit and exits non-zero, so re-running
  is idempotent and lossless.
- **LLM provider modes** (`llm.provider`, default `queue`):
  - `queue` — pipeline emits JSONL tasks to `<vault>/.wiki-ingest/queue/`; the
    `/wi-process` Claude Code skill drains them with its native session (no API key).
  - `anthropic` — direct Messages API for unattended cron (needs `ANTHROPIC_API_KEY`;
    missing key degrades to noop with a warning).
  - `noop` — no semantic work; pages render with empty summary/concept sections.
- **`--dry-run` is side-effect-free**: no vault writes, no dedup file creation, no log
  writes. Use it to preview.

## Development

```bash
cargo check                    # type check
cargo clippy -- -D warnings    # lint (must be clean)
cargo fmt                      # format
cargo nextest run --workspace  # tests
cargo run -- validate          # verify config.yaml + source params
cargo run -- ingest ai-news    # run a single source
```

## Config

User settings live in `config.yaml` (gitignored); copy `config.example.yaml`.
The key under `sources:` is the source ID = vault directory name = adapter selector.
A relative `vault.root` resolves against the config file's directory, not the CWD.

## Source types

| Type | Adapter | Use for |
|------|---------|---------|
| `google-drive` | Drive API | File-based sources (newsletters) |
| `gmail` | Gmail API | Email digest |
| `slack-channel` | Slack API | Channel message reader |
| `slack-search` | Slack API | Keyword trend search |
| `jira` | Jira REST API | Issue/ticket tracking |
| `google-calendar` | Calendar API | Schedule/meeting tracking |

## Output model

**Primary** (per-source): `daily/{source-id}/YYYY-MM-DD.md`
**Derived** (cross-source):
- `me/work-log/` — aggregated from sources with `track_personal: true`
- `weekly/synthesis/` — cross-source weekly themes
- `weekly/me/`, `monthly/me/`, `quarterly/me/`, `annually/me/` — performance tracking
- `wiki/concepts/` — extracted concepts (merged across runs)
