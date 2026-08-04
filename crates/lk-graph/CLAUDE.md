# lk-graph

Link graph analysis. Pure deterministic — no HTTP, no LLM. The vault writes are the mutations
below: `index-sync`/`normalize` with `--fix`, and `backlinks-sync` and `merge` without
`--dry-run`. Every check re-scans the vault — markdown
parsing is rayon-parallel and cheap, so analysis is always computed from current
on-disk state, never from a cached snapshot.

- **deps**: `lk-core` (slugify, frontmatter, link) + `lk-vault` (the writer for the
  gated mutations) + `petgraph` + `rayon` + `walkdir`. No reqwest/tokio — independent of
  the ingestion stack.
- **Output type naming**: CLI-facing presentation structs in `output.rs` are `*Report`
  (`HubsReport`, `LintReport`, `BacklinksSyncReport`, …). Domain-module computation/
  operation outcomes are `*Result` (`ClusterResult`, `SuggestResult`, `MergeResult`,
  `BacklinksSyncResult`). A domain `*Result` may be wrapped by an `output.rs` `*Report`
  for display (e.g. `BacklinksSyncResult` → `BacklinksSyncReport`).
- **Config**: `config.yaml` `graph:` section (`GraphConfig` in lk-core).
  `scope.dirs` (derived from `vault.dirs.wiki` when absent), `metrics.*`
  (`min_hub_degree`, `orphan_exclude`), `cluster.*`.
  All `deny_unknown_fields`. Validated: relative, no `..`.
- **Link resolution happens at scan time** (`scan::parse_file`): every internal inline
  markdown-link destination is resolved against its page's own location
  (`lk_core::link::resolve_dest`, `.md` destinations only — anchors stripped, images/
  external schemes skipped), and each resolved link carries BOTH of the things its consumers
  ask for (`scan::Link`): the `dest` — the vault-relative path it names, NFC-normalized — and
  the `id` that address belongs to. They are not interchangeable. Only a path can answer "is
  there a file there", because slugifying is lossy: `Bad_Name.md` and `bad-name.md` share an
  id while only one of them exists, so an id-based existence check reports a link to the other
  one sound. Only an id can be a node key. The `id` is DERIVED from the `dest`, so the pair
  cannot drift. A destination that escapes the vault root is kept verbatim; it names no file,
  so `broken` reports it. Concept `aliases` frontmatter is registry metadata for the LLM's
  dedup (and Obsidian display), never link resolution.
- **One scan, three views** (`scan::VaultViews`): the walk is the vault ROOT, always, and the
  narrowing is a filter over the result. `scanned` is every page — the existence universe, and
  the view the mutators repoint links across. `link_sources` is `scope.dirs` ∪ every page
  directory: the pages this tool writes, and a stray link in a user's own note is neither its
  output nor its to repair. `pages` is the analysis scope — the graph's nodes, what
  `hubs`/`cluster`/`suggest-links`/`normalize` read.
  `scope.dirs` and `scope.exclude` narrow the last two, NEVER the first: an excluded page
  still exists, still resolves a link, and still has its own links repointed by a rename. A
  walk that had already dropped it cannot tell "not there" from "not looked at", in whichever
  direction a caller resolves it — which is why the mutating commands read these same views
  rather than building their own scan.
  Scope membership is decided on the page ID, so a vault whose directory is spelled `Wiki`
  while the config says `wiki` is still analysed; a raw prefix test answers no there while
  `is_dir` passes on a case-insensitive volume, and every command reports an empty vault, green.
  `scan_vault` skips dot-directories, so `.trash` — where Obsidian puts a DELETED page — never
  resolves. `VaultExistence` answers two separate questions and they must not be conflated:
  `is_resolvable` (a file exists at this ADDRESS, **catalogs included** — they are files, so a
  page linking one links something real) and `is_knowledge` (…and it is not a generated catalog
  — the orphan-connectivity question, where counting a link to `index.md` would exempt the very
  pages detection looks for). Reserved meta pages
  (`lk_core::vault_path::RESERVED_WIKI_FILES`) are never orphans or index-drift.
- **Index drift is asked of the catalog's own builder** (`index_drift::diff` →
  `lk_vault::build_index`): `index.md` is a materialized view, so drift is the difference
  between the catalog on disk and the one a re-derivation produces now. A second definition of
  what belongs in the catalog disagrees with the builder's on any page the builder does not
  catalog — drift reports it, `--fix` appends it, the next `wiki index` drops it, and the vault
  stays permanently red with two repairs undoing each other. For the same reason the repair is
  the whole catalog, resolving both directions at once, and no analysis-scope or
  `orphan_exclude` filter reaches it: an observation-tuning knob must not decide a violation.
- **`graph::broken_links` is a free function over (pages, existence), not a `WikiGraph`
  method** — a broken link involves no node, edge or community, so computing it inside the graph
  would scope it to `graph.scope.dirs` on the SOURCE side, and a link is broken wherever it was
  written (`queue apply` writes concept links on daily pages). Both `lint` and `broken` pass
  every scanned page; the scope narrowing applies only to the graph's nodes. Reserved meta pages
  are skipped as SOURCES — they are re-derived WHOLE, and `wiki refresh` runs before `graph lint`
  in the pipeline, so a stale link in one is repaired by the same run rather than reported — but
  they remain valid DESTINATIONS.
- **Exit codes**: 0 = every claim the vault makes holds, 1 = it contradicts itself, 2 = runtime
  error. Non-zero is reserved for a claim that is FALSE and has a named repair — a link whose
  destination is absent, a catalog that disagrees with the disk, a category outside the
  configured vocabulary, one name answering to two pages, two files answering to one address,
  a filename that disagrees with its normalized slug, a `source_count` no sweep could write. What a vault in good standing
  legitimately carries is reported and exits 0: `lint`'s observation channel, `orphans`,
  and `hubs`/`cluster`/`export`/`suggest-links`.
- **`LintReport` is two channels, and the split IS the exit code**: `violations`
  (broken/index-drift/invalid-categories/duplicate-concepts/address-collisions/unnormalized)
  decides it, `observations`
  (orphans/hubs/unresolved-conflicts) never does. Every extraction mints concepts before
  anything cites them, so an orphan-counting exit code is permanently non-zero and therefore
  carries no information — which is what had callers wrapping the command in `|| true` and
  skills naming the lists to ignore, and a broken link got ignored along with them.
  `Violations::count`/`Observations::count` are what the exit code and the summary line read,
  and each DESTRUCTURES its own struct — including the nested `IndexSyncReport`, which counts
  what it holds — so adding a field to a channel does not compile until it is counted. A test
  over a hand-built fixture cannot establish that, since a new field satisfies the compiler as
  `Vec::new()`; the serde test covers the other half, that the fields are counted correctly, one
  `.len()` per list, none doubled.
- **`suggest_links`**: pairs in the same Louvain community with no edge that share at
  least `graph.cluster.suggest_min_shared_neighbors` neighbors, ranked by their
  **Adamic–Adar index** (Σ 1/ln|N(z)| over shared neighbors z), descending. It runs on the
  analysis scope (`graph.scope.dirs`, the wiki by default), so daily/personal pages are
  never nodes or neighbors here. Two filters stack: the count floor rejects the
  single-shared-neighbor case outright, then Adamic–Adar discounts shared neighbors that are
  high-degree hubs — so co-citation by one busy document/exploration page can never outrank a
  real shared niche concept. Parameter-free weighting (no magic threshold). Read-only,
  deterministic.
- **`map::build_map`** (`lore wiki map` → `<wiki>/map.md`): MATERIALIZES the Louvain
  communities (which `cluster` otherwise computes and discards) into a navigable page —
  concepts grouped by citation cluster, hub-first, each linked relative to the map with a
  leaf display (`[x](concepts/x.md)`), under a `type: map` frontmatter. A read-only
  materialized view (regenerated whole, byte-deterministic) like `index.md`/`log.md` —
  pure markdown builder, the CLI writes it. It NEVER writes Related-section edges into
  concept pages: communities are co-citation, not curated relatedness, so the page is
  labelled a citation map. `map.md` is in
  `RESERVED_WIKI_FILES` (never an orphan/drift finding). This is the deterministic,
  embedding-free "navigate, don't retrieve" entry point AGENTS.md points agents to.
- **Mutations gated**: `index_drift::fix()`, `normalize::apply()`,
  `backlinks::sync_concept_backlinks` and `merge::merge_concepts` touch the filesystem — the
  first two only with `--fix`, the last two only without `--dry-run`. All renames pre-checked.
- **`normalize` reads two different page sets, and conflating them is a defect.** Rename
  candidates come from the ANALYSIS scope (`graph.scope.dirs`, the wiki): only the wiki's
  pages are addressed by slug, and slugifying a dated filename elsewhere (`2026-W30` →
  `2026-w30`) would rewrite it into a path the pipeline never writes. The link rewrite
  reads EVERY page dir, like `merge`, because a citation of a renamed page usually lives
  outside the wiki — a daily page is the ordinary case. Rewriting only the rename scope
  strands those citations, and `broken` cannot report it: it matches destinations at
  `path_slug`, so a link to the old spelling still resolves to the renamed page's id.
- **Age is not a signal.** There is deliberately no staleness/decay check: reference
  knowledge does not expire by going unmentioned, so "old and uncited" identifies
  nothing actionable and would misdirect curator attention. A concept becomes due for
  review when its EVIDENCE changes — which `backlinks-sync` detects by digesting the citation
  SET, and hands to the queue as a synthesis rewrite — or when its structure is defective
  (`lint`). Never because time passed.
- **`backlinks::sync_concept_backlinks` is the sole deriver of every evidence-dependent
  field on a concept page**: the `## Sources` body (`- [title](relative-path)` entries,
  destinations relative to the concept page), the frontmatter `source_count`, AND the
  `llm_inputs.synthesis` input its `## Synthesis` is owed against. One sweep because they are
  one fact — the set of pages citing the concept — and a page that could not record all three
  records none of them. Uses full-vault scope (not `graph.scope.dirs`) so
  `<daily>`/`<personal>`/`<synthesis>` pages are included.
  **The verdict is the candidate page against the page on disk**, not a field-by-field
  comparison: everything derived is rendered into the candidate, so whatever differs is by
  definition drift — which is how a source page RETITLED gets its citation line corrected
  while the synthesis, keyed on the SET, stays answered.
  **A page that already holds a written synthesis and has recorded no input is ADOPTED**: its
  prose becomes the answer for the evidence the page carries, and the rewrite is owed from the
  next time that evidence moves. Without it an upgrade replaces every authored synthesis in
  the vault in its first unattended run — the sweep and the drain are adjacent stages, so the
  warning reaches a scheduler's log seconds before the rewrite it warns about, and for a vault
  not in git that is gone. What is adopted has to BE an answer: a body of nothing but headings
  or a `> [!conflict]` callout is a marker somebody left, and adopting a lone callout has
  `lint` report an open disagreement nothing is scheduled to restate.
  **Adoption carries its citation count because above ONE it is a deferral.** At one citation
  the prose was written from the single source that exists and adopting it is simply right.
  Above one it is a January sentence standing as the answer for a set spanning six months —
  and since adoption stamps the page fully answered, `doctor` reads it green and nothing is
  queued, so the sweep's own report is the ONLY place that deferral is ever stated. It names
  how many, with the `llm_inputs.synthesis_done` lever that turns one back into work.
  **A concept nothing cites records no input and is owed nothing**: there is no evidence for a
  synthesis to answer to, a queued task would send a drain to read an empty set, and a
  recorded-but-unanswerable input would have `lore doctor` report it forever. A concept whose
  last citation was deleted KEEPS the input it already answered — its synthesis still states
  what the vault learned, and no rewrite could improve on it from a set that is now empty, and
  a citation that comes BACK returns the digest to a value the `_done` marker already matches,
  so the page costs no rewrite for having been away.
  **An input it never answered is WITHDRAWN instead**, and the condition is the citation set
  going empty — never "nothing is owed", which is true for three unrelated reasons. Under
  `SynthesisPolicy::Skip` a fully cited page would otherwise lose the input its queued task is
  keyed to, and a run whose whole purpose is to touch no LLM plane would destroy that work;
  a page with no synthesis heading is already handled by the queue's anchor check, and
  withdrawing it would remove the only durable trace that it owes something, where the
  heading-missing report names a repair a human can perform. The promise has to be withdrawn
  by the sweep that made it, because nothing else knows it was made: left behind, the queued
  task keeps classifying `current` and a drain writes a synthesis of pages the vault no longer
  holds. **Withdrawal returns a page to the ADOPTABLE state** — carrying prose and no markers
  is exactly what adoption is for — so a concept that loses every citation and later regains
  one has its owed rewrite converted into an adoption of the prose already there. Consistent
  rather than lossy (after withdrawal the page genuinely records owing nothing), and a
  multi-citation page still surfaces in the deferral line.
  It never calls an LLM: it returns `resynthesize`, and `lore graph backlinks-sync` in lk-cli
  turns that into queued work through the same `LlmClient` ingest uses. `--dry-run` enqueues
  nothing, like every other write it withholds. Only
  non-concept content pages qualify as sources — `<daily>`, `<personal>`, `<synthesis>`,
  `<wiki>/documents`, and `<wiki>/explorations` (a concept-to-concept link belongs in
  `## Related`, and navigation pages never appear).
  The actual heading text is resolved from `locale.strings()` at runtime (e.g.
  `Sources`/`Related` under `locale: en`, localized otherwise) — never a hardcoded
  literal. It is ALSO the SOLE owner of the frontmatter `source_count` (= number of
  incoming citations): ingest preserves the on-disk value across re-render (0 for a new
  page) and `backlinks-sync` re-derives the real count from the link graph, so it
  reflects source deletions and can never be inflated by a crash or an idempotent re-ingest.
  The `## Sources` body is the single source-of-truth for citations — concept
  frontmatter carries no `sources` array.
- **`merge::merge_concepts`** (`lore graph merge <from> <into>`) folds a duplicate
  concept into a canonical one: it repoints every link that RESOLVES to `from`'s page —
  any spelling: canonical relative, dot-relative, the OKF `/`-absolute form — at `into`
  across the FULL vault (display text and heading anchors preserved verbatim), folds
  `from`'s title + aliases into `into`'s `aliases` (so the synonym stays in the dedup
  registry — durable because ingest preserves concept `aliases` across re-render), then
  deletes the `from` page. It never copies/fabricates prose. **Authored-body guard**:
  `concept_has_authored_body` is section-aware — a `- [title](dest)` bullet is
  machine-owned ONLY under the `## Sources` heading (matched across all locales, column-0
  exact like `backlinks-sync`); bullets under `## Related` (human-curated) or any
  synthesis prose count as authored, so the merge ABORTS before mutating unless
  `--force`. `--dry-run` previews without the gate firing. Run `backlinks-sync` afterward
  to re-derive the merged concept's `## Sources` + `source_count`. Duplicate *detection*
  (below) only reports the pages that answer to one name; this is the execution
  counterpart a human triggers, because it deletes a page.
- **`## Related` is NOT machine-written.** Louvain communities encode
  "co-cited together" (the in-scope graph is dominated by document/exploration→concept edges), not
  topical relatedness, so auto-writing community co-membership as Related edges
  manufactures co-occurrence noise and self-reinforcing cliques. Related links are
  instead curated via `lore-wiki audit`: `suggest_links` proposes candidates and an
  LLM confirms genuine relationships before any edge is written. `## Sources`
  (citation-derived, `backlinks-sync`) is the only machine-maintained concept relation.
- **`concept_lint::scan_concept_pages`** reads `{wiki}/concepts/*.md` ONCE into `Vec<ConceptPage>`
  (slug = file stem, path, category, `names` = slug + `title` + `aliases`, body), sorted by
  slug. The three concept lints below are pure functions over `&[ConceptPage]` — `graph lint`
  walks the concepts dir a single time, not once per check. A page with malformed frontmatter
  still yields one (slug from file stem, no category, slug-only names, empty body) so
  slug-only checks see it while content checks skip it.
- **`concept_lint::find_invalid_categories`**: surfaces concept pages whose `category`
  frontmatter value is not in `config.concepts.categories[].id`. The ingest
  pipeline strips invalid categories synchronously, but queue-mode concept
  page creation is done by `/lore-process` which can emit a category the
  skill invented. Lint reports them so `graph lint` exits non-zero and the
  drift is observable; nothing is mutated automatically. Empty configured
  list (categorisation off) suppresses every finding. A page with no `category` field —
  or an EMPTY one — is not flagged: that is the uncategorised state, and it is what the
  rest of the vault already means by an empty value (`templates/concept.md.jinja` renders
  the field under `{% if category %}`, `wiki concepts` filters it out of the registry).
- **`concept_lint::find_duplicate_concepts` reports NAME COLLISIONS, not similarity.**
  Each page claims a set of names — its slug, `title`, and every `aliases` entry — and a
  finding is two pages claiming one name. `lk_core::concept::identity_key` is that rule —
  `slugify` (the normalization that mints page ids and keys the pipeline's alias index)
  keeping only the breaks that mean something: every break is typography (`vector-db` ~
  `vectordb` ~ `Vector DB`, and `claude-35` ~ `claude35`) EXCEPT one between two numerals,
  which is the name (`claude-3-5` ≠ `claude-35`). Pages are grouped by key in a `BTreeMap`
  — ordered rather than hashed, so output order needs no final sort — which costs a log
  factor no vault will notice and replaces the O(n²) pair scan the similarity check needed.
  **A finding is therefore never a similarity guess**: the two names reduce to one identity
  under a rule with no score and no threshold. The rule does NOT fold only typography: `C`,
  `C++` and `C#` all slugify to `c`, so they claim one ADDRESS and the pair is genuinely
  reportable — whichever page owns `c` is where every extraction naming any of them lands. The
  remedy is to disambiguate the name, and `identity_key` records why the addressing cannot do it
  for you. What the lint CANNOT see is the defect that
  never becomes two pages — the router folding an extraction into an established page
  leaves nothing to compare — which is why the fold itself has to be narrow rather than
  the lint forgiving.
  **There is deliberately no score, no threshold, and no morphology.** The predecessor
  scored slug character bigrams (Sørensen-Dice ≥ `concept_near_duplicate_threshold`, since
  removed from config); measured on a 1,599-concept vault it returned 298 findings
  containing one real duplicate, because character overlap is morphology — it fires on
  shared namespace prefixes (`amazon-sagemaker-ai` ~ `amazon-sagemaker-hyperpod`), shared
  head nouns (`robot-foundation-model` ~ `tabular-foundation-model`) and coincidence
  (`agentops` ~ `gentoo`) while missing acronym pairs outright. Two softer keys were
  measured on the same vault and cut for the same reason: an order-insensitive token
  multiset found NOTHING the exact key did not (no two slugs are permutations) while
  assuming word order carries no meaning, and per-token plural stripping bought exactly one
  finding at the cost of collapsing `http` onto `https`. A permanently-red lint is worse
  than a silent one, so precision wins and recall goes where it can be judged. The exact
  rule also needs no `is_version_variant` escape hatch: `gpt-4`/`gpt-5` and
  `gemini-3-1-flash-lite`/`gemini-3-5-flash-lite` simply claim different names. Everything
  about MEANING — plurals, acronyms (`k8s` ↔ `kubernetes`), a team's shorthand — is out of
  scope BY CONSTRUCTION and belongs to `/lore-wiki audit` layer 5. Read-only; `graph merge`
  is the remedy a human triggers.
- **`concept_lint::find_unresolved_conflicts`**: reports concept pages whose body carries an
  unresolved `> [!conflict]` callout — a contradiction the drain that wrote the synthesis
  observed between two of the cited sources. The marker lives in the LLM-owned synthesis body
  (NOT frontmatter, which ingest re-render regenerates from the template), so it survives
  re-ingest via `preserved_synthesis` — but NOT across a synthesis rewrite, which replaces the
  section wholesale. A callout is therefore a statement about the last rewrite rather than a
  durable record: it exists while each rewrite keeps finding the disagreement, and nothing
  mechanically preserves it. The old `audited_sources_hash` gave durability only because
  nothing rewrote that section; claiming it still holds would be an assertion about an LLM
  re-deriving the same judgment, stated as an invariant. The scan is fence-aware (a callout quoted in a
  code block is content, not a marker) and matches the callout type `conflict`
  exactly — `[!note]`/`[!warning]` never fire. Read-only continuous tracking: the
  lint surfaces every open callout; what closes one is the disagreement going away in the
  sources, which the next rewrite then stops restating.
  This is the only contradiction mechanism — there is no automatic contradiction *detection*
  (it would false-positive on emphasis differences); flagging is an explicit LLM judgment,
  made where the sources are already being read, and `/lore-wiki audit` judges the open ones
  rather than hunting for new ones.
