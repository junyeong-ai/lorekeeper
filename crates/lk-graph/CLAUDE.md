# lk-graph

Wikilink graph analysis. Pure deterministic — no HTTP, no LLM, no vault writes
(except `--fix` for index-sync and normalize).

- **deps**: `lk-core` (slugify, frontmatter, wikilink) + `petgraph` + `rayon` +
  `walkdir`. No reqwest/tokio — independent of the ingestion stack.
- **Domain rules are single-sourced**: slug normalization = `lk_core::slugify` (NFKC),
  frontmatter = `lk_core::frontmatter::parse_page`, wikilinks =
  `lk_core::wikilink::extract_wikilinks`. No second implementation.
- **Config**: `config.yaml` `graph:` section (`GraphConfig` in lk-core).
  `scope.dirs` (default `["wiki"]`), `min_hub_degree`, `orphan_exclude`,
  `cluster.*`. All `deny_unknown_fields`. Validated: non-empty, relative, no `..`.
- **Wikilink resolution**: filename-based (`[[concept-a]]` matches any
  `concept-a.md` regardless of directory depth). Anchors (`#heading`, `^block`)
  stripped before resolution.
- **Exit codes**: 0 = ok/no findings, 1 = findings, 2 = runtime error.
  `build`/`hubs`/`cluster`/`export`/`suggest-links` never exit 1.
- **`suggest_links`**: pairs in the same Louvain community with no edge, ranked
  by shared-neighbor count. Read-only, deterministic.
- **Mutations gated**: `index::fix()`, `normalize::apply()`, and
  `backlinks::sync_concept_backlinks` touch the filesystem — the first two only
  with `--fix`, backlinks only without `--dry-run`. All renames pre-checked.
- **`stale::find_stale`**: reports pages whose `updated` (or `created` fallback)
  frontmatter is older than a threshold. Groups by path prefix. Pure read.
- **`backlinks::sync_concept_backlinks`**: rewrites `## 출처`/`## Sources` on
  each concept page to match the wikilink graph. Uses full-vault scope (not
  `graph.scope.dirs`) so daily/me/weekly pages are included. Only event/document
  pages qualify as sources (concept-to-concept links belong in `## 관련`).
