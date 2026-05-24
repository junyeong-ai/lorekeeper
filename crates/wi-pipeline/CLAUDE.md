# wi-pipeline

Deterministic transform stages between `wi-source` and `wi-vault`. Shares an
`Arc<PipelineContext>` (engine, llm, dirs, perf, identity, timezone) with the
`Synthesizer`.

- **`Pipeline::plan` is per-source**; it returns that source's daily pages and merges
  any extracted concepts into a **run-level** `Mutex<ConceptDrafts>`. Concept pages are a
  cross-source aggregate, rendered ONCE via `render_concept_pages()` after all sources
  are planned — never per source (that would let a later source's write clobber an
  earlier one). `commit()` records dedup and must run only after writes + flush succeed.
- **`new_dry_run`** opens the dedup cache read-only (no file creation).
- **DedupCache**: cascade is `event-id → url → title`. Persisted-table lookups are gated
  on the cache being present, but the intra-batch (seen-id/url + novel-title) checks
  ALWAYS run — so a dry-run with no cache still matches a real run. Titles are keyed
  `{date}:{title}` and scanned by date-prefix range. The cache is recreated only on a
  recoverable mismatch — a schema-type change or an outdated on-disk format after a redb
  major upgrade (`DatabaseError::UpgradeRequired`) — never on I/O/corruption errors.
- **classify**: `flag_personal` matches identity tokens with `contains_bounded`
  (alphanumeric/`._%+-` word boundary) to avoid name/email substring false positives
  (`kim`⊄`kimberly`, `test@x.com`⊄`test@x.com.au`); `@` is intentionally excluded so
  Slack `<@U…>` mentions still match. `classify_by_keywords` reads `SourceConfig.classify`
  and uses plain substring — correct for CJK keywords (no word boundaries).
- **Concept merge** reads existing `created`/`updated` frontmatter (the keys actually
  written), preserves the original title + strongest confidence (extracted > inferred),
  and dedupes `sources`/`reference_count`.
- **Synthesizer** methods are `weekly_synthesis` + `*_personal`; they share
  `summarize_or_warn` (propagates only fatal LLM errors) and `render_or_fallback`.
- Fallback renderers must emit the same `##` anchors the templates use (so `/wi-process`
  can find the section) and the same frontmatter keys synthesis reads (e.g. work-log
  `categories`).
