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
Slack ─────────┼─ config ─┤  Collapse dup (intra-batch)<personal>/{weekly,monthly,quarterly,annual}/
Jira ──────────┤  .yaml   ├─ Classify (labels)       <synthesis>/{weekly}/
Calendar ──────┤          ├─ Concepts (LLM)          <wiki>/concepts/
RSS/Atom ──────┤          ├─ Render (templates)      <wiki>/documents/
Manual inbox ──┘          ├─ Wiki index (catalog)    <wiki>/index.md (by-topic)
                          ├─ Wiki log (timeline)     <wiki>/log.md (by-time)
                          └─ Graph (lint, stale,     <wiki>/AGENTS.md
                               cluster, backlinks,
                               alias, audit)
```

## Workspace

Each crate has its own `CLAUDE.md` with the invariants for working inside it
(loaded on demand when you open files there).

```
crates/
  lk-core/      Domain types, config, i18n, slugify (NFKC), frontmatter, wikilink, vault paths
  lk-vault/     Obsidian vault I/O: atomic write, templates (embedded), ingest log
  lk-source/    Source adapters + factory, markdown normalization (ADF/HTML/Slack→MD)
  lk-pipeline/  Pipeline (per-source plan), intra-batch dedup, classify, concepts, synthesis
  lk-queue/     Semantic task queue: LlmClient trait, QueueLlmClient (JSONL), noop (+ mock for tests)
  lk-graph/     Wikilink graph analysis: lint, hubs, cluster, suggest-links (no HTTP/async)
  lk-cli/       Binary `lore` — one module per subcommand under commands/
templates/      Jinja2 markdown templates (.md.jinja), compiled into the binary
```

## Development

```bash
cargo check                        # type check
cargo clippy --workspace --all-targets -- -D warnings  # lint (must be clean; --all-targets covers tests)
cargo fmt                          # format
cargo nextest run --workspace      # tests
lore validate                      # verify config.yaml + source params
lore ingest ai-news                # run a single source
lore schema                        # generate <wiki>/AGENTS.md
lore wiki concepts                 # list all concept pages
lore graph lint                    # structural health check
lore queue status                  # classify pending LLM tasks (current/stale/missing)
```

## Config

User settings in `config.yaml` (gitignored); copy `config.example.yaml`.
Auto-discovered: `./config.yaml` → `~/.config/lorekeeper/config.yaml`.
`vault.root` resolves relative to the config file's directory, not the CWD.

## Cross-cutting invariants

- **Source ID = vault directory**: the key under `sources:` becomes `<daily>/{id}/`. Must not contain `/` or `\`, and must not be `.` or `..`.
- **Vault directories configurable**: all top-level vault paths (`<daily>`, `<personal>`, `<synthesis>`, `<wiki>`) are set via `vault.dirs.*` in config.yaml. Their fixed leaf subdirectories (`concepts`, `documents`, `explorations`, `work-log`) are single-sourced as `lk_core::vault_path` constants. Every crate builds paths through `VaultPath` builders or those constants — never an inline string literal.
- **Date derivation**: `timestamp.to_zoned(vault.timezone()).date()` — always via configured timezone, never UTC.
- **Multi-date batches**: events spanning several dates produce one `<daily>/` page per date.
- **Ownership decided by the adapter**: each source sets `RawItem::is_self` by exact-matching its structured authorship field against the user — Gmail `From`/Calendar organizer-or-attendee vs `identity.email`, Slack author vs `identity.slack_id`, Jira assignee vs the authenticated `/myself` account. `is_personal` = `is_self && track_personal`. The pipeline never infers ownership from free-form text, so recipients/CCs/mentions never pollute the personal work-log or reviews.
- **The vault is realized-only — forecasts never materialize** (`event.date > today`): an event dated after today is a FORECAST (a calendar look-ahead event that hasn't happened), so the knowledge vault writes **no page for it at all** — `Pipeline::plan` skips a forecast date entirely (no daily page, no concepts, no citations). This is the single chokepoint that keeps every downstream consumer (work-log, synthesis, backlinks, orphans) clean by construction: a page that doesn't exist can't be read, cited, or counted, so the invariant needs no per-consumer guard. A forecast becomes knowledge only once its date arrives and the normal ingest writes it. Two defense-in-depth gates enforce the same `today` boundary (wall-clock, vault tz, independent of `--date`) on the paths that don't go through page rendering: `render_work_log` drops future events (the work-log is event-driven, not page-driven) and `Synthesizer::read_date_range` caps reads at today (so a review or weekly-theme run reflects only realized days). Ownership (`is_personal`) is time-agnostic.
- **`source_count` owned by `backlinks-sync`**: ingest writes `0`; `lore graph backlinks-sync` re-derives the exact citation count from the wikilink graph.
- **Stale LLM tasks are caught deterministically**: `lore queue status` classifies each pending task `current`/`stale`/`missing-target` against its target page in tested Rust; `/lore-process` processes only `current` tasks.
- **Idempotent ingest**: plan → write daily/concept → work-log → flush LLM queue → log, then an archive hook for the `manual` source. There is no commit step: a daily page is re-rendered in full each run, so any write or flush failure just leaves the affected pages for the next run to reproduce byte-identically. The `manual` archive runs only when every vault write and the queue flush succeeded — its knowledge is durably materialized by then, so another source's fetch failure doesn't block it, while any write/flush failure leaves inbox files for retry.
- **Daily pages re-render in full each run; STREAMING sources project from an event log.** A complete-refetch source (Gmail/Jira/Calendar/Slack/Drive) reproduces its whole window on demand, so it renders directly from the fetch. A streaming source (RSS — `SourceType::is_streaming`, a rolling capped feed) can't, so it projects its page from a durable per-date event log (`.lorekeeper/events/{source}/{date}.jsonl`, raw pre-LLM events): each run UNIONs the fetch with the stored log (`EventId` key, fresh wins) so an item that scrolled out of the feed is never lost. The log is the OPPOSITE of a suppression cache — it never blocks regeneration, it enables it: a deleted page self-heals from it and `lore ingest --date <past>` repairs any day. Duplication is converged where it adds value — the concept/graph layer (one concept = one page) — and preserved at the raw layer, where every observation is provenance.
- **Daily pages are materialized views**: structural fields (frontmatter, raw event list, headings) are re-rendered every ingest; semantic fields (summary, refined events, concept wiki-links) are LLM-owned, preserved across re-renders, and invalidated by a BLAKE3-128 hash in the page's `llm_inputs` frontmatter — so re-ingesting unchanged data enqueues zero LLM tasks. Completion detection and cache-shape mechanics live in `lk-pipeline`.
- **Domain logic single-sourced in lk-core**: slugify (NFKC), frontmatter, wikilink, blank-line collapsing. Zero duplicate implementations across crates. (Rich-text→Markdown conversion — ADF/HTML/Slack — is single-sourced separately in `lk-source::markdown`.)
- **i18n single source of truth**: `vault.locale` (ko/en) switches all labels. Templates use `{{ i18n.* }}`. `lore schema` generates `<wiki>/AGENTS.md` from the i18n bundle. Source content is never translated.
- **`--dry-run` is side-effect-free**: no vault writes, no log.

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
| `manual` | Local inbox | User-curated files dropped in `inbox/` (md/txt/markdown/html/htm by default; archives consumed files once this source's vault writes and the queue flush succeed) |

