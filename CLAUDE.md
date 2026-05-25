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

