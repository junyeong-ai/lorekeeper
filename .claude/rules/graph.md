---
paths: ["crates/lk-graph/**/*.rs"]
---

- Pure deterministic — no HTTP, no LLM, no async. deps: lk-core + petgraph + rayon + walkdir.
- Domain rules single-sourced from lk-core: `slugify` (NFKC), `frontmatter::parse_page`, `wikilink::extract_wikilinks`.
- Wikilink resolution is filename-based (`[[concept-a]]` matches any `concept-a.md` regardless of depth). Anchors stripped before resolution.
- Exit codes: 0 = ok/no findings, 1 = findings, 2 = runtime error.
- `backlinks-sync` scans full vault (not `graph.scope.dirs`) so daily/me pages are included. Only event/document pages qualify as sources — concept-to-concept links belong in Related, not Sources.
- `stale` reports frontmatter `updated` (or `created` fallback) older than threshold. Groups by path prefix.
- Mutations gated: `index::fix()` and `normalize::apply()` only with `--fix`; `backlinks-sync` only without `--dry-run`.
