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
- **`backlinks::sync_concept_backlinks`**: rewrites the `## Sources` section on
  each concept page to match the wikilink graph. Uses full-vault scope (not
  `graph.scope.dirs`) so `<daily>`/`<personal>`/`<synthesis>` pages are included. Only event/document
  pages qualify as sources (concept-to-concept links belong in `## Related`).
  The actual heading text is resolved from `locale.strings()` at runtime (e.g.
  `Sources`/`Related` under `locale: en`, localized otherwise) — never a hardcoded
  literal. It is ALSO the SOLE owner of the frontmatter `source_count` (= number of
  incoming citations): ingest preserves the on-disk value across re-render (0 for a new
  page) and `backlinks-sync` re-derives the real count from the wikilink graph, so it
  reflects source deletions and can never be inflated by a crash or `--force` re-ingest.
  The `## Sources` body is the single source-of-truth for citations — concept
  frontmatter carries no `sources` array.
- **`merge::merge_concepts`** (`lore graph merge <from> <into>`) folds a duplicate
  concept into a canonical one: it rewires every wikilink targeting `from` (bare slug AND
  path id, anchors/aliases preserved) to `into` across the FULL vault, then deletes the
  `from` page. It never copies/fabricates prose. **Authored-body guard**:
  `concept_has_authored_body` is section-aware — a `- [[…]]` bullet is machine-owned ONLY
  under the `## Sources` heading (matched across all locales, column-0 exact like
  `backlinks-sync`); bullets under `## Related` (human-curated) or any synthesis prose
  count as authored, so the merge ABORTS before mutating unless `--force`. `--dry-run`
  previews without the gate firing. Run `backlinks-sync` afterward to re-derive the merged
  concept's `## Sources` + `source_count`. Near-dup *detection* (below) only reports
  candidates; this is the execution counterpart a human triggers.
- **`## Related` is NOT machine-written.** Louvain communities encode
  "co-cited together" (the graph is dominated by daily/document→concept edges), not
  topical relatedness, so auto-writing community co-membership as `[[related]]` edges
  manufactures co-occurrence noise and self-reinforcing cliques. Related links are
  instead curated via `lore-wiki audit`: `suggest_links` proposes candidates and an
  LLM confirms genuine relationships before any edge is written. `## Sources`
  (citation-derived, `backlinks-sync`) is the only machine-maintained concept relation.
- **`concepts::scan_concept_pages`** reads `{wiki}/concepts/*.md` ONCE into `Vec<ConceptPage>`
  (slug = file stem, rel_path, category, body), sorted by slug. The three concept lints below
  are pure functions over `&[ConceptPage]` — `graph lint` walks the concepts dir a single
  time, not once per check. A page with malformed frontmatter still yields one (slug from file
  stem, no category, empty body) so slug-only checks see it while content checks skip it.
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
  Sørensen-Dice similarity (on separator-stripped slugs) ≥
  `graph.graph.concept_near_duplicate_threshold` (default 0.6) — variant-spelling
  duplicates (`vector-db` ~ `vector-database` = 0.6) the LLM dedup hint missed. Digit-boundary
  version variants (`gpt-4`/`gpt-4o`, `claude-3`/`claude-3-5`) are deliberately distinct
  concepts and are skipped (`is_version_variant`) — that orthogonal exclusion is why the
  threshold can favor recall at 0.6 without model-version false positives. Read-only merge
  candidates surfaced in `graph lint`; a human decides.
- **`concepts::unresolved_conflicts`**: reports concept pages whose body carries an
  unresolved `> [!conflict]` callout — a contradiction `/lore-wiki audit` flagged
  between cited sources. The marker lives in the LLM-owned synthesis body (NOT
  frontmatter, which ingest re-render regenerates from the template), so it survives
  re-ingest via `preserved_synthesis`. The scan is fence-aware (a callout quoted in a
  code block is content, not a marker) and matches the callout type `conflict`
  exactly — `[!note]`/`[!warning]` never fire. Read-only continuous tracking: the
  lint surfaces it until a human resolves the contradiction and deletes the callout.
  This is the only contradiction mechanism — there is no automatic contradiction
  *detection* (it would false-positive on emphasis differences); flagging is an
  explicit LLM/human judgment in the audit skill.
