# Lorekeeper

Config-driven knowledge ingestion pipeline for Obsidian wikis. A Rust CLI (`lore`)
collects daily data from heterogeneous sources, deduplicates, classifies, extracts
concepts, and writes structured markdown pages. Includes graph analysis for wiki
structural health.

## Architecture

```
Data Sources              lore (Rust CLI)            Obsidian Vault
────────────              ───────────────            ──────────────
Google Drive ──┐          ┌─ Extract (per-source)    daily/{source-id}/
Gmail ─────────┤          ├─ Normalize → Event       me/work-log/
Slack ─────────┼─ config ─┤  Deduplicate (cascade)   weekly/ monthly/
Jira ──────────┤  .yaml   ├─ Classify (labels)       quarterly/ annually/
Calendar ──────┤          ├─ Concepts (LLM)          wiki/concepts/
Manual inbox ──┘          ├─ Render (templates)      wiki/documents/
                          ├─ Wiki index (catalog)    wiki/index.md
                          └─ Graph (lint, stale,     wiki/AGENTS.md
                               cluster, backlinks)
```

## Workspace

Each crate has its own `CLAUDE.md` with the invariants for working inside it
(loaded on demand when you open files there).

```
crates/
  lk-core/      Domain types, config, i18n, slugify (NFKC), frontmatter, wikilink, vault paths
  lk-vault/     Obsidian vault I/O: atomic write, templates (embedded), ingest log
  lk-source/    Source adapters + factory, markdown normalization (ADF/HTML/Slack→MD)
  lk-pipeline/  Pipeline (per-source plan/commit), dedup, classify, concepts, synthesis
  lk-llm/       LlmClient trait + providers: anthropic, queue, noop (+ mock for tests)
  lk-graph/     Wikilink graph analysis: lint, hubs, cluster, suggest-links (no HTTP/async)
  lk-cli/       Binary `lore` — one module per subcommand under commands/
templates/      Jinja2 markdown templates (.md.jinja), compiled into the binary
```

## Project-wide invariants

- **Source ID = vault directory**: the key under `sources:` becomes the `daily/{id}/`
  output path AND selects the adapter. Must not contain path separators, `.`, or `..`.
- **Date derivation**: `timestamp.to_zoned(vault.timezone()).date()` — always via the
  configured timezone, never UTC by accident.
- **Multi-date batches**: events spanning several dates produce one `daily/` page per date.
- **Atomic ingest** (`lore ingest`, 5 phases): plan → write daily/concept → work-log →
  flush LLM queue (atomic temp+rename) → commit dedup. Source failures are isolated —
  each source's dedup commits only after its own writes + flush succeed; a failed source
  stays uncommitted so re-running is idempotent. The process exits non-zero if any source
  failed.
- **i18n**: `vault.locale` (ko/en) switches all labels/headings. Templates use
  `{{ i18n.* }}` from the Strings bundle; source content is never translated.
- **Single source of truth**: `lore schema` generates `wiki/AGENTS.md` from the i18n
  bundle, defining page formats and section ownership (machine vs LLM). Templates,
  queue `target.anchor`, and skills all derive from `lk-core::i18n`.
- **Domain logic is single-sourced in lk-core**: slugify (NFKC), frontmatter parsing,
  wikilink extraction, vault paths, text normalization (collapse_blank_lines). lk-vault,
  lk-source, lk-pipeline, and lk-graph all consume these — zero duplicate implementations.
- **LLM provider modes** (`llm.provider`, default `queue`):
  - `queue` — JSONL tasks to `.lorekeeper/queue/`; `/lore-process` drains with Claude Code.
  - `anthropic` — direct Messages API for unattended cron.
  - `noop` — no semantic work.
- **`--dry-run` is side-effect-free**: no vault writes, no dedup, no log.

## Development

```bash
cargo check                        # type check
cargo clippy -- -D warnings        # lint (must be clean)
cargo fmt                          # format
cargo nextest run --workspace      # tests
lore validate                      # verify config.yaml + source params
lore ingest ai-news                # run a single source
lore schema                        # generate wiki/AGENTS.md
lore graph lint                    # structural health check
```

## Config

User settings in `config.yaml` (gitignored); copy `config.example.yaml`.
Auto-discovered: `./config.yaml` → `~/.config/lorekeeper/config.yaml`.
`vault.root` resolves relative to the config file's directory, not the CWD.

## Source types

| Type | Adapter | Use for |
|------|---------|---------|
| `google-drive` | Drive API | File-based sources (curated docs in a Drive folder) |
| `gmail` | Gmail API | Email digest; newsletters split out via a Gmail label (`label:` / `-label:`) |
| `slack-channel` | Slack API | Channel reader (threads, bot filter, watch_users) |
| `slack-search` | Slack API | Keyword trend search (user token required) |
| `jira` | Jira REST API | Issue tracking (ADF→Markdown, status/period snapshot) |
| `google-calendar` | Calendar API | Schedule tracking (HTML→Markdown) |
| `rss` | RSS/Atom (`feed-rs`) | External knowledge feeds (vendor blogs, news) → concepts; no auth, multi-feed, per-feed error isolation |
| `manual` | Local inbox | User-curated files dropped in `inbox/` (md/txt/markdown/json by default; opt-in archive) |

