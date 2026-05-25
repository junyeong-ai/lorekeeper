---
paths: ["crates/lk-graph/**/*.rs"]
---

- Pure deterministic — no HTTP, no LLM, no async. deps: lk-core + petgraph + rayon + walkdir.
- Domain rules single-sourced from lk-core: `slugify` (NFKC), `frontmatter::parse_page`, `wikilink::extract_wikilinks` (skips fenced code blocks and inline code).
- Wikilink resolution (`scan::resolve_wikilink_target`): bare `[[concept-a]]` matches `concept-a.md` by filename (any depth); path `[[daily/x/2026-05-22]]` matches that page id (`/` preserved, not collapsed). Anchors stripped first.
- `hubs`/`cluster`/`suggest-links` use the `graph.scope.dirs` subgraph; `broken`/`orphans`/`index-sync` resolve against the full-vault existence universe (`scan::VaultExistence` via `WikiGraph::build_with_existence`) so cross-folder links aren't false positives. Reserved meta pages (`index.md`, `AGENTS.md` from `lk_core::vault_path::RESERVED_WIKI_FILES`) are never orphans/index-drift.
- Exit codes: 0 = ok/no findings, 1 = findings, 2 = runtime error.
- `backlinks-sync` scans full vault (not `graph.scope.dirs`) so daily/me pages are included. Only event/document pages qualify as sources — concept-to-concept links belong in Related, not Sources.
- `stale` reports frontmatter `updated` (or `created` fallback) older than threshold. Groups by path prefix.
- Mutations gated: `index::fix()` and `normalize::apply()` only with `--fix`; `backlinks-sync` only without `--dry-run`.
