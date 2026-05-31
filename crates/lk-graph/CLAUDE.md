# lk-graph

Wikilink graph analysis. Pure deterministic — no HTTP, no LLM. The only vault
writes are the gated mutations below (`index-sync`/`normalize` with `--fix`,
`backlinks-sync` without `--dry-run`) and the mtime scan cache
(`<vault>/.lorekeeper/graph-cache.json`, atomic temp+rename).

- **deps**: `lk-core` (slugify, frontmatter, wikilink) + `petgraph` + `rayon` +
  `walkdir`. No reqwest/tokio — independent of the ingestion stack.
- **Output type naming**: CLI-facing presentation structs in `output.rs` are `*Report`
  (`HubsReport`, `StaleReport`, `BacklinksSyncReport`, …). Domain-module computation/
  operation outcomes are `*Result` (`ClusterResult`, `SuggestResult`, `MergeResult`,
  `BacklinksSyncResult`). A domain `*Result` may be wrapped by an `output.rs` `*Report`
  for display (e.g. `BacklinksSyncResult` → `BacklinksSyncReport`).
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
- **Alias resolution** (lowest precedence: id → filename → alias). A concept's
  `aliases` frontmatter (slugified into `ScannedPage::aliases`, self-slug dropped)
  lets a bare `[[synonym]]` resolve to it. Applied CONSISTENTLY in all three
  resolvers — `VaultExistence` (`by_alias`), `WikiGraph` (`alias_to_node`), and
  `backlinks` (`alias_to_stem`, so `source_count` matches the graph). An alias never
  overrides a real id/filename, and when two concepts claim the same alias the
  smallest-id concept wins — order-independent, so all three resolvers pick the same
  concept regardless of scan order or concept-file nesting. `alias::find_alias_conflicts`
  surfaces the two ways this goes wrong (a `Duplicate` alias claimed by two concepts;
  one that `ShadowsRealPage`) as a `graph lint` finding — so the deterministic winner
  never calcifies silently. This is the deterministic, audit-friendly answer to synonyms
  (no embeddings): the LLM/human registers the alias, the graph resolves it.
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
- **`suggest_links`**: pairs in the same Louvain community with no edge that share at
  least `graph.cluster.suggest_min_shared_neighbors` neighbors (default 2), ranked by
  shared-neighbor count. The floor suppresses co-citation noise — a single shared neighbor
  usually means "co-cited by one daily note", not a real relationship. Read-only,
  deterministic.
- **Mutations gated**: `index::fix()`, `normalize::apply()`, and
  `backlinks::sync_concept_backlinks` touch the filesystem — the first two only
  with `--fix`, backlinks only without `--dry-run`. All renames pre-checked.
- **`stale::find_stale`**: reports pages that are **old AND dormant** — `updated`
  (or `created` fallback) older than the threshold AND no incoming citation from a
  page that is itself recent. Liveness is derived from the full-vault wikilink graph
  (so a concept cited by this week's daily notes is live, not stale), which is why
  the CLI scans every page dir but reports only the configured scope. Distinguishes
  "old" from "actually dormant" deterministically — no heuristic. Groups by path
  prefix. Pure read.
- **`audit`** — the contradiction worklist. `find_audit_candidates` (`graph
  audit-candidates`, pure read): a concept is a candidate iff `source_count >= 2` AND
  the BLAKE3-128 hash of its canonical `## Sources` body differs from the
  `audited_sources_hash` frontmatter marker. Hashing the source SET (not a count) is
  what makes it robust: a source swap that keeps the count constant still changes the
  hash and resurfaces the concept, while an unchanged set stays off the list — low-noise
  by construction (the same hash-as-change-detector pattern as `llm_inputs`).
  `mark_audited` (`graph audit-mark <slug>`) stamps the current hash via the
  single-sourced `lk_vault::set_frontmatter_field`; `/lore-wiki audit` calls it after
  reviewing. Deterministic selection in Rust; the contradiction *judgment* stays with
  the LLM/human. Sorted by `source_count` desc, then slug.
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
- **`concept_lint::scan_concept_pages`** reads `{wiki}/concepts/*.md` ONCE into `Vec<ConceptPage>`
  (slug = file stem, path, category, body), sorted by slug. The three concept lints below
  are pure functions over `&[ConceptPage]` — `graph lint` walks the concepts dir a single
  time, not once per check. A page with malformed frontmatter still yields one (slug from file
  stem, no category, empty body) so slug-only checks see it while content checks skip it.
- **`concept_lint::invalid_categories`**: surfaces concept pages whose `category`
  frontmatter value is not in `config.concepts.categories[].id`. The ingest
  pipeline strips invalid categories synchronously, but queue-mode concept
  page creation is done by `/lore-process` which can emit a category the
  skill invented. Lint reports them so `graph lint` exits non-zero and the
  drift is observable; nothing is mutated automatically. Empty configured
  list (categorisation off) suppresses every finding. Pages without a
  `category` field are not flagged — that is the documented uncategorised
  state.
- **`concept_lint::near_duplicate_concepts`**: reports concept-slug pairs whose
  Sørensen-Dice similarity (on separator-stripped slugs) ≥
  `graph.graph.concept_near_duplicate_threshold` (default 0.6) — variant-spelling
  duplicates (`vector-db` ~ `vector-database` = 0.6) the LLM dedup hint missed. Digit-boundary
  version variants (`gpt-4`/`gpt-4o`, `claude-3`/`claude-3-5`) are deliberately distinct
  concepts and are skipped (`is_version_variant`) — that orthogonal exclusion is why the
  threshold can favor recall at 0.6 without model-version false positives. Pairs are found
  via a **character-bigram inverted index** (only slugs sharing a bigram are scored), so the
  scan is near-linear, not O(n²), as the vault grows — safe because Sørensen-Dice > 0 implies
  a shared bigram. Read-only merge candidates surfaced in `graph lint`; a human decides.
- **`concept_lint::unresolved_conflicts`**: reports concept pages whose body carries an
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
