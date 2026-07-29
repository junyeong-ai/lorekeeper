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
                          ├─ Wiki map (clusters)     <wiki>/map.md (by-citation)
                          └─ Graph (lint, cluster,   <wiki>/AGENTS.md
                               suggest-links, merge,
                               backlinks-sync, …)
```

## Workspace

Each crate has its own `CLAUDE.md` with the invariants for working inside it
(loaded on demand when you open files there).

```
crates/
  lk-core/      Domain types, config, i18n, slugify (NFKC), frontmatter, link, vault paths, atomic write
  lk-vault/     Obsidian vault I/O: atomic write, templates (embedded), ingest log
  lk-source/    Source adapters + factory, markdown normalization (ADF/HTML/Slack→MD)
  lk-pipeline/  Pipeline (per-source plan), intra-batch dedup, classify, concepts, synthesis
  lk-queue/     Semantic task queue: LlmClient trait, QueueLlmClient (JSONL), noop (+ mock for tests)
  lk-graph/     Link graph analysis: lint, hubs, cluster, suggest-links (no HTTP/async)
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
lore doctor                        # vault text-cleanliness audit (non-zero on defects)
lore queue status                  # classify pending LLM tasks (current/stale/missing)
lore queue count                   # bare integer: current tasks (machine contract for scripts)
lore queue apply                   # materialize drained concept results into pages + links
lore config vault-root             # bare path (machine contract for scripts)
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
- **Ownership decided by the adapter**: each source sets `RawItem::is_self` by exact-matching its structured authorship field against the user — Gmail `From`/Calendar organizer-or-attendee vs `identity.email`, Slack author vs `identity.slack_id`, Jira assignee vs the authenticated `/myself` account. `is_personal` = `is_self && source ∈ personal.tracked_sources` (so an absent `personal:` module means nothing is personal). The pipeline never infers ownership from free-form text, so recipients/CCs/mentions never pollute the personal work-log or reviews.
- **The personal module is optional; the core is domain-neutral.** `config.personal` (`Option<PersonalConfig>`) owns the entire personal-productivity subsystem: the work-log, the weekly/monthly/quarterly/annual reviews, the contribution taxonomy (`performance_categories` + maps + the classify `performance_category` bridge), and `tracked_sources` (which sources count as "mine"). ABSENT (the default) = a pure knowledge engine: no work-log, no reviews, no `is_personal`, and `lore schema` omits those page formats from `AGENTS.md`. Every personal behavior is gated on `config.personal.is_some()`; nothing in the ingest→concept→wiki→graph core depends on it. Cross-source weekly *synthesis* (themes, `<synthesis>/weekly/`) is core knowledge synthesis and stays regardless.
- **The vault is realized-only — forecasts never materialize** (`event.date > today`): a date after today is a FORECAST, so `Pipeline::plan` writes NO page for it (no daily page, concepts, or citations) — one chokepoint that keeps every downstream consumer clean by construction. Two defense-in-depth gates enforce the same `today` boundary (wall-clock, vault tz, independent of `--date`) off the page-render path: `render_work_log` drops future events and `Synthesizer::read_date_range` caps reads at today. A forecast becomes knowledge once its date arrives. `is_personal` is time-agnostic.
- **Atlassian auth is one facade over named instances.** `credentials.atlassian` is a MAP of instances (`default`, `cloud`, `onprem`, …) because one org routinely runs a Cloud tenant *and* an on-prem Data Center wiki; a source picks one with its typed `instance` field (optional when only one exists) — credential routing is a source-level concern the factory resolves, so it sits beside `classify` rather than inside the untyped `params`. Each instance declares `method`: `oauth` (Cloud), `api-token` (Cloud Basic), or `pat` (Data Center Bearer). Each variant carries only its own fields, so an impossible pairing is unrepresentable — and **`Deployment` (Cloud vs DataCenter) is DERIVED from the method, never configured**, since each credential form exists on exactly one deployment. `AtlassianAuth` answers the only three questions adapters ask (base URL, `Authorization` header, REST dialect); adapters never branch on the method, so a new method is a change in `lk-source::atlassian` alone. **OAuth exists because an IP allowlist blocks API tokens outright** (`403 "your IP address is not listed"`) while honoring an org-approved app — off the corporate network it is the only Cloud auth that reaches the API, and `AtlassianAuth::explain_failure` says exactly that at the point of failure — keyed on the CONFIGURED METHOD and the status code, never on matching text in the provider's body, so an unrelated 403 is never mislabelled an allowlist problem. **Atlassian ROTATES refresh tokens** (each refresh invalidates the one it used), so: the successor is persisted to `credentials.json` as part of completing a refresh; one `AtlassianAuth` per instance is SHARED across every adapter in a run (two would invalidate each other mid-run); the refresh request is deliberately NOT retried (a retry after a committed rotation strands the grant permanently, whereas failing the run self-heals next time); and Lorekeeper needs its OWN OAuth app — a grant shared with another client breaks both daily.
- **`source_count` owned by `backlinks-sync`**: ingest writes `0`; `lore graph backlinks-sync` re-derives the exact citation count from the link graph.
- **Whoever fills a section stamps its marker in the same edit.** `llm_cache` decides a section's fate purely on `llm_inputs.<key>_done`, never on whether the body looks filled — so a section written without one is erased by the next render and its task re-enqueued forever. `lk_vault::set_llm_input` is the single writer for those nested markers (`set_frontmatter_field` owns top-level keys and cannot reach them).
- **A concept's slug is resolved once, never re-derived.** A page's id is NOT always `slugify(title)`: a renamed or merged concept keeps its original id and records the other names as aliases, which is what keeps existing citations resolving. `ConceptDrafts` builds an alias index from the vault and returns the resolved `ConceptIdentity` from every merge; `render::concept_links` renders from THAT, so a citation and the page it points at cannot disagree. Computing the slug in both places is what mints a second page beside the canonical one and splits its citations. The index seeds each page's own slug before any name, so a page owns its address even when no name of its own reproduces it — `lore graph lint` reports two pages answering to one name as a duplicate.
- **`day_window` bounds land on civil midnight**, with padding rounded outward to whole days. The pipeline renders a daily page for every date a batch touches and a complete-refetch source renders it from the fetch alone, so a window ending mid-day hands it a PARTIAL day that overwrites that date's page with the fragment. Absolute-hour arithmetic cannot guarantee this: on a DST fall-back day `day_start - 24h` lands an hour AFTER the previous midnight.
- **The queue carries requests one way and VALUES back.** A drained task that writes one section of one page writes it directly; concept extraction does not. Its output lands on pages SHARED between origin pages, under merge rules (preserved `## Synthesis`, aliases, established category, `source_count`) that already exist as tested Rust in `ConceptDrafts` — so `/lore-process` reports the concepts it found — with a grounding sentence used only when a page is CREATED — to `queue/results/*.json` and `lore queue apply` materializes them through that same path, rendering the origin page's links with the same `concept_links` builder. One implementation of the merge, extraction reduced to a pure function of the page it read, and no cross-page write set for a drain to collide on. A missing anchor section is an ERROR there, never a silent no-op: `replace_section` returns the page unchanged when the heading is absent, which would delete the result and report success.
- **Stale LLM tasks are caught deterministically**: `lore queue status` classifies each pending task `current`/`stale`/`missing-target` against its target page in tested Rust; `/lore-process` processes only `current` tasks. `lore queue prune` removes `stale`/`missing-target` tasks with the same classification — no LLM session needed to clear dead work.
- **Idempotent ingest**: plan → write daily/concept → work-log → flush LLM queue → log, then an archive hook for the `manual` source. There is no commit step: a daily page is re-rendered in full each run, so any write or flush failure just leaves the affected pages for the next run to reproduce byte-identically. The `manual` archive runs only when every vault write and the queue flush succeeded — its knowledge is durably materialized by then, so another source's fetch failure doesn't block it, while any write/flush failure leaves inbox files for retry.
- **A living document carries its version in its identity.** Every other source reads immutable items (a sent mail, a posted message); a Confluence page is edited in place. So the confluence adapter's `external_id` is `confluence:{page_id}:v{version}` — an unchanged page re-fetched tomorrow reproduces the same id (dedup absorbs it, the `llm_inputs` hash keeps the LLM idle), while an edit mints a NEW event that flows through summarize/concept extraction again. Freshness therefore falls out of the existing materialized-view machinery with no reconciliation pass. Ownership is **last-writer**, not contributor: `is_self` compares the *current version's* author to the authenticated account, because CQL's `contributor = currentUser()` also matches pages someone else last edited — knowledge to read, never the user's own work-log entry.
- **The scheduled pipelines compose `lore`; they never orchestrate inside it.** `scripts/lore-pipeline.sh` holds the shared stage runner and the two commands that exist so a script never greps prose (`config vault-root`, `queue count`); `lore-daily.sh` and `lore-weekly.sh` are the stage lists. Putting `claude -p` invocation syntax into `config.yaml` was considered and rejected — it would bind the pipeline's config to one CLI's flags. The one thing a skill cannot know is supplied at the invocation (`--append-system-prompt`): that the run is unattended, so a turn must not end on a question or an unkept promise. Deliberately absent from it is any instruction to verify or re-check — current models do that unprompted, and asking compounds into wasted work.
- **`lore schedule` emits cron OR launchd** (`--format`). On macOS launchd is correct: `StartCalendarInterval` runs a job missed during sleep as soon as the machine wakes, whereas cron silently skips it — and a closed laptop at 09:00 is the normal case. Cron syntax launchd cannot express (`*/5`, `1-5`) is REFUSED, never approximated, since a silently different schedule is worse than a rejected one; every emitted path must be absolute (`--bin`, `--pipeline-dir`) because launchd searches no `PATH` and expands no `~`.
- **`--pipeline-dir` is what schedules the pipelines rather than their first stage.** `lore ingest` and `lore synthesis weekly` are stage ONE of `lore-daily.sh`/`lore-weekly.sh`; the queue drain and `lore queue apply` exist only in the scripts, so scheduling the bare subcommands ingests every morning and never fills a summary or materializes a concept. With the flag those two jobs run the scripts (and only those two — the janitors and monthly+ syntheses have no LLM stage, which is why the scripts say they belong on their own schedules). A scheduled job starts with almost no environment, so the emitted entry also carries `PATH`/`LORE_BIN`/`LORE_CONFIG`/`CLAUDE_BIN` — all INHERITED from the interactive session `lore schedule` runs in, never invented, since a guessed value reproduces exactly the silently-broken job the flag exists to prevent.
- **One all-source ingest is the scheduling unit**: the work-log is a cross-source daily aggregate, so `ingest.schedule` is the single ingest cron key and `lore schedule` emits ONE `lore ingest` line — never per source. The work-log renders only on a full ingest: a filtered `lore ingest <source>` sees a structural subset of personal events and never rewrites the cross-source page (a transient source failure inside a full run still writes — loud non-zero exit, complete again on the next full run).
- **Daily pages re-render in full each run; STREAMING sources project from an event log.** A complete-refetch source (Gmail/Jira/Calendar/Slack/Drive) reproduces its whole window on demand and renders from the fetch. A streaming source (RSS — `SourceType::descriptor().streaming`, a rolling capped feed) can't, so it projects from a durable per-date event log (`.lorekeeper/events/{source}/{date}.jsonl`, raw pre-LLM events): each run UNIONs fetch + stored log (`EventId` key, fresh wins), so a scrolled-out item is never lost and a deleted page self-heals (`lore ingest --date <past>` repairs any day). Raw-layer duplication is provenance; convergence happens at the concept/graph layer (one concept = one page).
- **Daily pages are materialized views**: structural fields (frontmatter, raw event list, headings) are re-rendered every ingest; semantic fields (summary, refined events, concept links) are LLM-owned, preserved across re-renders, and invalidated by a BLAKE3-128 hash in the page's `llm_inputs` frontmatter — so re-ingesting unchanged data enqueues zero LLM tasks. Completion detection and cache-shape mechanics live in `lk-pipeline`.
- **A citation is created by the same act that creates the concept page, so it lasts as long as the page does.** Concept links are the one LLM-owned section whose result creates durable pages OUTSIDE the page carrying it, and `backlinks-sync` re-derives every one of those pages' sources section and `source_count` from exactly those forward links. A section that restated the newest extraction would therefore not merely drop a link: it would strip a page that still asserts knowledge of the evidence justifying it, permanently, since nothing else records what the superseded extraction had cited. So a page's concept links only ever ACCUMULATE — one rule (`render::accumulate_concepts`) applied by both writers, the plan render and `lore queue apply` — while every other LLM section states something about its own input and is replaced wholesale. What accumulating rests on is that a page's set of OBSERVATIONS only grows — a streaming source unions each fetch into that date's event log, a complete-refetch source's date is a closed window — NOT on any one observation's text being fixed, which for every adapter but `confluence` and `manual` (the two that key identity to content) it is not. So a concept extracted before an in-place edit keeps its citation after it: the citation records that this page's material named the concept when observed, which stays true, and the alternative is the loss above. Carried links are read back at the address the link builder writes, which is narrower than the graph's edge gate on purpose — a nested destination re-rendered through that builder would resolve nowhere, and the pipeline never writes one.
- **The vault at rest is an Open Knowledge Format bundle.** Every page carries a `type` frontmatter field naming its page format (`concept`, `daily`, `document`, … — the AGENTS.md format ids; OKF's one required key), and every internal reference is an inline markdown link `[Display](relative/path.md)` — destination relative to the containing page, `.md`-suffixed, CommonMark percent-encoded — the one form Obsidian, GitHub, and OKF consumers all resolve. Never `[[wikilinks]]`. `lk_core::link` single-sources the whole vocabulary (build/extract/rewrite/resolve), and `render::concepts_dir_dest` computes a page's concept-link base once so nothing downstream does path arithmetic. Rewriters (merge/normalize) match destinations at the id level (`path_slug`) with the same `.md` gate as extraction, so the set of links they repoint is definitionally the set scan counts as edges. `okf_version` stays undeclared by design (it may only live in a bundle-root index.md, which would claim a file in the user's vault root; the spec mandates best-effort consumption without it).
- **Domain logic single-sourced in lk-core**: slugify (NFKC) + `identity_key` (slugify keeping only the separators that mean something — the identity a name claims, as opposed to the address it is written at. Every break is typography — `Vector DB`/`vector-db`/`vectordb` are one name, as are `claude-35`/`claude35` — except one between two NUMERALS, which is the name, since positional notation makes `3-5` two numerals and `35` one, so `Claude 3.5` ≠ `Claude 35`. Shared by the pipeline's alias index and the graph's duplicate lint so the two cannot disagree on what "the same name" is — and because the index ACTS on that answer while the lint only reports it, the fold has to be one no reviewer would overturn), frontmatter, link, blank-line collapsing, atomic file write (`fs::write_atomic` — temp+fsync+rename+dir-fsync with a per-writer-unique temp; the one sync atomic writer every crate uses, `lk_vault::VaultWriter` being its async sibling). Zero duplicate implementations across crates. (Rich-text→Markdown conversion — ADF/HTML/Slack — is single-sourced separately in `lk-source::markdown`.)
- **i18n single source of truth**: `vault.locale` (ko/en) switches all labels. Templates use `{{ i18n.* }}`. `lore schema` generates `<wiki>/AGENTS.md` with its page-format **headings/labels** drawn from the i18n bundle, while its structural/instructional **prose stays English by design** (an agent-facing spec; only the vault tokens it cites are localized). Source content is never translated.
- **`--dry-run` is side-effect-free**: no vault writes, no log.

## Source types

| Type | Adapter | Use for |
|------|---------|---------|
| `google-drive` | Drive API | File-based sources (curated docs in a Drive folder) |
| `gmail` | Gmail API | Email digest; newsletters split out via a Gmail label (`label:` / `-label:`) |
| `slack-channel` | Slack API | Channel reader (threads, bot filter, watch_users) |
| `slack-search` | Slack API | Keyword trend search (user token required) |
| `jira` | Jira REST API | Issue tracking (ADF→Markdown, status/period snapshot) |
| `confluence` | Confluence REST API | Wiki pages I wrote/edited (CQL, storage-format→Markdown); version-keyed so an edit re-enters the pipeline |
| `google-calendar` | Calendar API | Schedule tracking (HTML→Markdown) |
| `rss` | RSS/Atom (`feed-rs`) | External knowledge feeds (vendor blogs, news) → concepts; no auth, multi-feed, per-feed error isolation |
| `manual` | Local inbox | User-curated files dropped in `inbox/` (md/txt/markdown/html/htm by default; archives consumed files once this source's vault writes and the queue flush succeed) |

## Naming conventions

Single-sourced so new code reads like existing code without guessing:

- **Constructors**: `new` (cheap, infallible-or-trivially-validating field construction — no I/O, no sub-system building) · `from_*` (convert ONE value, e.g. `from_path`) · `build_*` / `::build` (computed/fallible construction — I/O, sub-object construction, or derivation; free fn OR associated method, e.g. `build_source`, `build_index`, `WikiGraph::build`, `VaultExistence::build`, `TemplateEngine::build`, `PipelineContext::build`) · `with_*` (builder mutator). Test-fixture factories use `build_*` (e.g. `build_event`, `build_page`) — never an off-vocabulary verb like `make_*`.
- **Accessors**: `find_*` (search, may return empty) · `load_*` (read from disk/IO, e.g. `load_config`, `Credentials::load`/`load_file`) · `resolve_*` (name → value/address) · `get_*` (in-memory present accessor) · `lookup` (keyed cache query).
- **Return types**: `*Result` = domain computation outcome · `*Report` = a CLI-facing presentation of one (lk-graph `output.rs`), introduced ONLY when display adds fields/structure beyond the `*Result` (counts, applied/fixed flags); a display-ready `*Result` (e.g. `ClusterResult`, `MergeResult`) is printed directly — an empty pass-through `*Report` is ceremony, not consistency.
- **Predicates** `is_*` (state) / `has_*` (possession), always prefixed (e.g. `is_resolvable`, not `resolves`). Each crate's public error SURFACE is a domain-named, typed `<Crate>Error` (a leaf parse/validate helper may return a `String` message that a caller wraps into its typed error — the payload is text, the surface is not); most crates have a single `<Crate>Error`, but a foundational crate may carry several domain errors (lk-core `ConfigError`). Enum variants PascalCase, `#[serde(rename_all = "kebab-case")]` on the wire · types spell words in full (`GraphCommand`, `CategoryReference`, `PipelineContext` — never `Cmd`/`Ref`/`Ctx`), locals may abbreviate (`ctx`).

