# Lorekeeper

Config-driven knowledge ingestion pipeline for Obsidian wikis. A Rust CLI (`lore`)
collects daily data from heterogeneous sources, deduplicates, classifies, extracts
concepts, and writes structured markdown pages. Includes graph analysis for wiki
structural health.

## Architecture

```
Data Sources              lore (Rust CLI)            Obsidian Vault (vault.dirs.*)
────────────              ───────────────            ──────────────────────────────
Google Drive ──┐          ┌─ Extract (per-source)    <daily>/{source-id}/
Gmail ─────────┤          ├─ Normalize → Event       <personal>/work-log/
Slack ─────────┼─ config ─┤  Deduplicate (cascade)   <personal>/{weekly,monthly,quarterly,annually}/
Jira ──────────┤  .yaml   ├─ Classify (labels)       <synthesis>/{weekly}/
Calendar ──────┤          ├─ Concepts (LLM)          <wiki>/concepts/
RSS/Atom ──────┤          ├─ Render (templates)      <wiki>/documents/
Manual inbox ──┘          ├─ Wiki index (catalog)    <wiki>/index.md
                          └─ Graph (lint, stale,     <wiki>/AGENTS.md
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
lore schema                        # generate <wiki>/AGENTS.md
lore wiki concepts                 # list all concept pages
lore graph lint                    # structural health check
```

## Config

User settings in `config.yaml` (gitignored); copy `config.example.yaml`.
Auto-discovered: `./config.yaml` → `~/.config/lorekeeper/config.yaml`.
`vault.root` resolves relative to the config file's directory, not the CWD.

## Cross-cutting invariants

- **Source ID = vault directory**: the key under `sources:` becomes `<daily>/{id}/`. Must not contain `/` or `\`, and must not be `.` or `..`.
- **Vault directories configurable**: all top-level vault paths (`<daily>`, `<personal>`, `<synthesis>`, `<wiki>`) are set via `vault.dirs.*` in config.yaml. Code uses `VaultPath` builders, never hardcoded strings.
- **Date derivation**: `timestamp.to_zoned(vault.timezone()).date()` — always via configured timezone, never UTC.
- **Multi-date batches**: events spanning several dates produce one `<daily>/` page per date.
- **Atomic ingest** (5 phases): plan → write daily/concept → work-log → flush LLM queue → commit dedup. Each source's dedup commits only after its own writes + flush succeed.
- **Domain logic single-sourced in lk-core**: slugify (NFKC), frontmatter, wikilink, text normalization. Zero duplicate implementations across crates.
- **i18n single source of truth**: `vault.locale` (ko/en) switches all labels. Templates use `{{ i18n.* }}`. `lore schema` generates `<wiki>/AGENTS.md` from the i18n bundle. Source content is never translated.
- **`--dry-run` is side-effect-free**: no vault writes, no dedup, no log.

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

