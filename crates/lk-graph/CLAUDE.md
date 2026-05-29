# lk-graph

Wikilink graph analysis. Pure deterministic — no HTTP, no LLM. The only vault
writes are the gated mutations below (`index-sync`/`normalize` with `--fix`,
`backlinks-sync` without `--dry-run`) and the mtime scan cache
(`<vault>/.lorekeeper/graph-cache.json`, atomic temp+rename).

- **deps**: `lk-core` (slugify, frontmatter, wikilink) + `petgraph` + `rayon` +
  `walkdir`. No reqwest/tokio — independent of the ingestion stack.
- **Domain rules are single-sourced**: slug normalization = `lk_core::slugify` (NFKC),
  frontmatter = `lk_core::frontmatter::parse_page`, wikilinks =
  `lk_core::wikilink::extract_wikilinks`. No second implementation.
- **Config**: `config.yaml` `graph:` section (`GraphConfig` in lk-core).
  `scope.dirs` (derived from `vault.dirs.wiki` when absent), `min_hub_degree`,
  `orphan_exclude`, `cluster.*`. All `deny_unknown_fields`. Validated: relative,
  no `..`.
- **Wikilink resolution** (`scan::resolve_wikilink_target`): a bare target
  (`[[concept-a]]`) matches any `concept-a.md` by filename, regardless of depth;
  a path target (`[[<daily>/team-slack/2026-05-22]]`) matches that page id
  (per-segment slugified, `/` preserved — *not* collapsed to `daily-…`). Anchors
  (`#heading`, `^block`) stripped before resolution.
- **Integrity checks vs analysis scope**: `hubs`/`cluster`/`suggest-links`
  operate on the `graph.scope.dirs` subgraph. But
  `broken`/`orphans`/`index-sync` resolve against a full-vault *existence
  universe* (`scan::VaultExistence`, built via `build_with_existence`): a `<wiki>/`
  page linking a `<daily>/` page is not broken, and a concept linked only from
  `<daily>/` is not an orphan. Reserved meta pages (`index.md`, `AGENTS.md` —
  `lk_core::vault_path::RESERVED_WIKI_FILES`) are never orphans or index-drift.
- **Exit codes**: 0 = ok/no findings, 1 = findings, 2 = runtime error.
  `hubs`/`cluster`/`export`/`suggest-links` never exit 1.
- **`cache`**: mtime-based scan cache for `--incremental`. `build()` walks
  scope dirs and records per-file mtimes. `is_dirty()` compares against the
  cache; `save()` persists atomically. The CLI skips the full scan when
  `is_dirty()` returns false.
- **`suggest_links`**: pairs in the same Louvain community with no edge, ranked
  by shared-neighbor count. Read-only, deterministic.
- **Mutations gated**: `index::fix()`, `normalize::apply()`, and
  `backlinks::sync_concept_backlinks` touch the filesystem — the first two only
  with `--fix`, backlinks only without `--dry-run`. All renames pre-checked.
- **`stale::find_stale`**: reports pages whose `updated` (or `created` fallback)
  frontmatter is older than a threshold. Groups by path prefix. Pure read.
- **`backlinks::sync_concept_backlinks`**: rewrites `## 출처`/`## Sources` on
  each concept page to match the wikilink graph. Uses full-vault scope (not
  `graph.scope.dirs`) so `<daily>`/`<personal>`/`<synthesis>` pages are included. Only event/document
  pages qualify as sources (concept-to-concept links belong in `## 관련`).
  Note: the actual heading text (`출처`/`Sources`, `관련`/`Related`) is resolved
  from `locale.strings()` at runtime — the Korean/English forms shown here are
  examples for both locales, not hardcoded literals. It ALSO re-derives the
  frontmatter `source_count` (= number of incoming citations) so the count reflects
  source deletions; ingest only approximates it. The `## 출처` body is the single
  source-of-truth for citations — concept frontmatter carries no `sources` array.
- **`## 관련`/`## Related` is NOT machine-written.** Louvain communities encode
  "co-cited together" (the graph is dominated by daily/document→concept edges), not
  topical relatedness, so auto-writing community co-membership as `[[related]]` edges
  manufactures co-occurrence noise and self-reinforcing cliques. Related links are
  instead curated via `lore-wiki audit`: `suggest_links` proposes candidates and an
  LLM confirms genuine relationships before any edge is written. `## 출처`
  (citation-derived, `backlinks-sync`) is the only machine-maintained concept relation.
- **`concepts::invalid_categories`**: surfaces concept pages whose `category`
  frontmatter value is not in `config.concepts.categories[].id`. The ingest
  pipeline strips invalid categories synchronously, but queue-mode concept
  page creation is done by `/lore-process` which can emit a category the
  skill invented. Lint reports them so `graph lint` exits non-zero and the
  drift is observable; nothing is mutated automatically. Empty configured
  list (categorisation off) suppresses every finding. Pages without a
  `category` field are not flagged — that is the documented uncategorised
  state.
- **`concepts::near_duplicate_concepts`**: reports concept-slug pairs whose
  Sørensen-Dice similarity ≥ threshold (CLI default 0.6) — variant-spelling
  duplicates (`vector-db` ~ `vector-database`) the LLM dedup hint missed. Read-only
  merge candidates surfaced in `graph lint`; a human decides. The slug filename is
  the canonical identity (no frontmatter parse). Distinct slugs (~0.1) never fire.
