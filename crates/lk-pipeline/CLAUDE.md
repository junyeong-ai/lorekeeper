# lk-pipeline

Deterministic transform stages between `lk-source` and `lk-vault`. Shares an
`Arc<PipelineContext>` (engine, llm, dirs, perf, identity, timezone, locale,
concept_categories) with the `Synthesizer`.

- **`Pipeline::plan` is per-source**; it returns that source's daily pages and merges
  any extracted concepts into a **run-level** `Mutex<ConceptDrafts>`. Concept pages are a
  cross-source aggregate, rendered ONCE via `render_concept_pages()` after all sources
  are planned — never per source (that would let a later source's write clobber an
  earlier one). `commit()` records dedup and must run only after writes + flush succeed.
- **`new_dry_run`** opens the dedup cache read-only (no file creation).
- **DedupCache**: cascade is `event-id → content-hash → url → title` (first match =
  duplicate). Persisted-table lookups are gated on the cache being present, but the
  intra-batch (seen-id/url + novel-title) checks ALWAYS run — so a dry-run with no
  cache still matches a real run. URLs are canonicalised before lookup and storage:
  http→https, host lowercase, trailing slash removal, auth stripping, tracking-param
  removal (`utm_*`, `fbclid`, `gclid`, `msclkid`, `ttclid`, `twclid`, `wbraid`,
  `gbraid`, …) with resource-identifying params preserved and sorted. Titles are
  compared case-insensitively (both sides lowercased) and scanned across the
  full table (no date partition). The cache is recreated only on a recoverable mismatch — a schema-type
  change or an outdated on-disk format after a redb major upgrade
  (`DatabaseError::UpgradeRequired`) — never on I/O/corruption errors. On recreation
  the stale file is renamed to `*.redb.backup.{timestamp}-pid{pid}` (not deleted),
  preserving dedup history for manual recovery.
- **classify**: `flag_personal` matches identity tokens with `contains_bounded`
  (alphanumeric/`._%+-` word boundary) to avoid name/email substring false positives
  (`kim`⊄`kimberly`, `test@x.com`⊄`test@x.com.au`); `@` is intentionally excluded so
  Slack `<@U…>` mentions still match. `classify_by_keywords` reads
  `SourceConfig.classify` (ordered `Vec<ClassifyRule>`, first match wins) and uses
  `contains_bounded` (token-boundary match) — prevents substring false positives
  while remaining correct for CJK keywords. When `classify_with_llm` is true,
  unclassified events are sent to the LLM as a fallback (deferred in `queue` mode;
  no-op in `noop` mode).
- **Concept merge** reads existing `created`/`updated` frontmatter (the keys actually
  written), preserves the original title and category (established identity), and
  dedupes `sources`/`source_count`. Before extraction, `load_existing_concept_refs()`
  scans the vault's concept directory + in-memory drafts and passes them as
  `existing_concepts` in the LLM request, preventing duplicate concept creation.
- **Synthesizer** methods are `try_weekly_synthesis` + `try_*_personal`; they share
  `summarize_or_warn` (propagates only fatal LLM errors). `try_weekly_synthesis` uses
  `identify_themes` for structured JSON theme extraction. Every `TaskTarget` carries
  `anchor` — the exact `## …` heading resolved from i18n at construction time — so
  the skill never needs a hardcoded kind→heading table.
- **Synthesis fallback cascade**: quarterly reads monthly → weekly-personal → None;
  annual reads quarterly → monthly → None. Raw daily work-log is never fed directly
  to a higher-level synthesis (each level reads only pre-summarized pages).
- Fallback renderers must emit the same `##` anchors the templates use (so `/lore-process`
  can find the section) and the same frontmatter keys synthesis reads (e.g. work-log
  `categories`).
