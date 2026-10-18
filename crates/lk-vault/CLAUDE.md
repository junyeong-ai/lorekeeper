# lk-vault

Obsidian vault I/O. All writes go through here so atomicity lives in one place.

- **`VaultWriter::write_page` is atomic + durable**: it delegates to
  `lk_core::fs::write_atomic` (the single atomic-write implementation: per-writer-unique
  temp → fsync → rename → dir-fsync) on the blocking pool via `spawn_blocking`, since file
  I/O is blocking and must fsync. There is no second atomic-write implementation to drift —
  never write the final path directly.
- **Frontmatter parsing is line-based** (`frontmatter::parse_page`): a `---` is a
  delimiter only when it's the standalone first line and a later standalone `---` line.
  A leading BOM is stripped; CRLF is normalized to LF. A substring scan would
  mis-detect `---` inside a YAML value — don't reintroduce one. `VaultPage.original`
  retains the exact input before normalization for optimistic publication guards; it is
  skipped by serialization and must not be reconstructed from parsed fields.
- **`TemplateEngine::has_user_override` returns `Ok(false)` only for not-found**; a template
  that exists but fails to parse propagates `Err`. Callers must thread that through so a
  broken user template surfaces instead of silently falling back to the embedded
  renderer.
- **Daily templates inherit from `_daily_base.md.jinja`** (`{% extends %}`): the base owns
  the frontmatter (incl. the `llm_inputs` ladder, single-sourced) + summary, the generic
  `highlights` loop (any source with configured highlight sections), the events/concepts
  skeleton. Each per-type child (gmail/jira/slack-*/google-*/rss/confluence) is a thin override
  of only `{% block title %}`, `{% block items_heading %}` (events vs messages), and
  `{% block events %}` (item rendering). `_`-prefixed templates are partials — never a page
  `default_template`. `template::tests::every_daily_template_renders_with_expected_frontmatter`
  selects the children by partitioning `EMBEDDED` against a list of the NON-daily templates, so
  a new daily template joins it by default — a hand-list of the children omitted `confluence`,
  and a renamed block there left every Confluence page taking the base's generic title with the
  whole suite green. A missing block is caught by comparing the rendered title against that
  default, not by the key being present, because the key is present either way.
- **`concepts` entries arrive FULLY RENDERED, bullet included** — a template emits `{{ c }}`,
  never `- {{ c }}`. That section has a second writer, `lore queue apply`, which replaces it
  without going through any template, so a bullet stated in the template as well would be a
  second answer to what a citation looks like. A `--template-dir` override participates in this
  contract: a user template still carrying the prefix renders `- - [C](…)` on every page it
  owns, and nothing detects it. The daily and document legs are each pinned by a test so the
  embedded copies cannot drift; an override is the user's to keep in step.
- **`IngestLog`** distinguishes `NotFound` (→ empty/None, legitimate "never ingested")
  from real I/O errors (propagated). Malformed JSONL lines are `tracing::warn`-ed and
  skipped, not silently dropped — corruption stays observable without blanking history.
  **`find_last_collection` answers "when was this source last OBSERVED", not "when did it
  last produce something"** — the question `lore status` and `lore health` both actually
  ask. `LogStatus::is_collected` owns the split, exhaustively: `Skipped` is written at
  exactly one site (a fetch that succeeded and yielded no pages), so it carries an
  answer — "nothing happened" — and only `Failed` leaves the window unobserved. Counting
  an empty run as no run is what let a quiet source read as overdue indefinitely while it
  ingested correctly every morning, and a warning that never clears is one its reader
  stops reading.
- Frontmatter values derived from LLM output (e.g. concept `title`) are emitted as JSON
  strings (`serde_json::to_string` / `| tojson`) so quotes/colons can't break the YAML.
- **`section::{replace_section, section_body}`** operate on the body of a
  `## <heading>` section. `replace_section` rewrites it, `section_body` reads it
  as a `&str`. Both share `find_section`, which tracks fenced-code state so `## `
  lines inside ``` blocks are quoted content, not section boundaries, and trims
  trailing whitespace from heading lines. (`lk_pipeline::llm_cache` reads `section_body`
  to splice a preserved LLM-owned body back on a cache hit; completion is marker-signalled,
  never inferred from whether a section is empty.)
- **`index::build_catalog`** derives the complete navigation tree. `index.md` is its entry;
  at most 100 entries occur on each page, with generated children under `{wiki}/catalog/`.
  Small catalogs remain a single page. Relative links retain exact addresses; every page
  is reachable from the entry. Generated children are excluded from knowledge graphs.
  `IndexCatalog::publish` uses a mutation plan, removes only marked generated children,
  and refuses collisions with authored files. Index drift compares the whole tree.
  Read or parse failures abort rebuilding; missing optional directories are empty inputs.
- **SQLite navigation is the default refresh surface.** After a successful `prepare_search`,
  `NavigationCatalog::refresh_entrypoint` journals the small permanent `index.md` guide and
  retirement of only exact marked catalog/log/map outputs. The marker is the first
  body line (after any frontmatter), not a match anywhere in authored prose or code.
  Existing unmarked entries and
  marked files carrying a knowledge/source type refuse the whole plan before any edit.
  Unmarked children are preserved, including under overlapping configured source roots.
  New explicit Markdown exports mark their entry as well as children; old unmarked roots
  are never guessed to be generated from their title or shape. A failed snapshot capture
  never authorizes retirement. Markdown page bodies remain authoritative.
  Explicit refresh migration accepts only configured root `PATH=FILE_REVISION` approvals.
  `plan_entrypoint` validates every address/revision/type before snapshot capture; its one
  MutationPlan preserves full-byte originals and a content-addressed receipt before edits
  or retirement. Archive collisions never overwrite. Unmarked children remain untouched.
  The evidence archive's authored revision is not a substitute for these full-file guards.
- **Generated navigation shares one bound and publisher** (`navigation`): at most 100 rows
  or branches per page. Small heading groups pack whole into available space; only a group
  over the bound continues on multiple pages. Entry links name groups and continuation
  positions rather than ranges of long document titles. `NavigationHeading` keeps semantic
  labels separate from optional heading totals; detail headings render their totals while
  branch links use labels plus their own reachable-row count exactly once. The topic catalog uses `catalog/`,
  the citation map `catalog/map/`, and the timeline `catalog/log/`. Each view reads and
  removes only its own marked direct
  children. Unknown-file collisions refuse the complete publication. Children, entry page
  and stale removals use the caller's `MutationSession` and one journaled plan; callers do
  not acquire another session while holding one. Exact relative page addresses survive
  splitting. Detail addresses are reconstructible positions, not permanent knowledge ids.
- **`timeline::build_timeline_catalog`** derives the complete bounded knowledge timeline.
  `build_timeline` reads its entry string; publication uses the complete catalog. Entries
  remain reverse chronological by `created` ONLY, never `updated`; durable concepts,
  documents and explorations only. `created` may be a source's authored date, not when
  Lorekeeper collected it. Every dated entry stays reachable, including a day
  larger than one detail page. Undated pages have no timeline position; traversal, read
  and parse failures abort rebuilding and preserve previously published output.
- **Frontmatter writers are bounded by `frontmatter_block`** — the shared line range between
  the delimiters, recognized by the same first-line rule and `is_delimiter_line` predicate as
  `parse_page`. `set_frontmatter_field` sets a TOP-LEVEL (column-0) key — never an indented
  one nested under a mapping (e.g. `summary:` under `llm_inputs:`) — inserting before the
  closing `---` when absent; `set_llm_input` sets a key one level under the block-style
  `llm_inputs:` mapping, which `set_frontmatter_field` cannot reach. Both preserve a leading
  BOM, copy every other line byte for byte, and take `value` VERBATIM — the caller owns
  serialization (`serde_json::to_string`), so neither hand-rolls quoting. Both return
  `Option`: `None` when the key has no place in the block (no frontmatter at all; for
  `set_llm_input`, no block-style `llm_inputs:` line). That is the whole point of the shared
  range — a writer that cannot place a key writes NOTHING rather than falling through to the
  end of the document and appending it to the body, where `llm_cache` would never read it and
  the task would re-enqueue forever, growing the page each round. A caller turns `None` into a
  named error wherever the key it could not place was needed; `graph normalize` is the one that
  does not, because a page with no frontmatter has no stale `id` for it to rewrite — the graph
  id comes from the path.
- **What belongs to an entry is `continuation_span`'s answer**, and every defect found in
  these writers has been a wrong one. Each rule below fixed a page-corrupting bug, so change
  none of them casually: a continuation is a line indented DEEPER than the entry's own line
  (one function serves a top-level key and an `llm_inputs` child — a sibling ends the latter,
  a column-0 key the former); a BLANK line neither ends an entry nor belongs to it; a COMMENT
  belongs to whatever FOLLOWS it, at ANY indentation — except inside a BLOCK SCALAR
  (`key: |`, `key: >`), the one place a `#` line is value text. Which lines those are is
  tracked PER LINE, from the nearest preceding shallower line that OPENED one — key-bearing
  (`key: |`) or not (`- |`, a list item having no colon to look behind). The entry a span
  belongs to cannot answer for the lines under it: a span over `llm_inputs:` walks its
  children, and `llm_inputs:` is not a block scalar even when a child is. Replacing a key
  takes its whole
  span, so a block-style value never outlives the key it belonged to. And `set_llm_input`
  reads the child indentation from the first REAL child, never from the span's first line,
  which may be a comment indented past them.
- **`search::search` ranks by WHERE a query matched.** A hit
  is ordered by `MatchField` (`Identity` → `Name` → `Summary` → `Text`, the derived enum
  ordering IS the ranking) and then by `source_count`: both are facts a page states, so an
  answer is explainable hit by hit, which a weighted blend of the two would not be. Every term
  must appear, and a page is ranked by the WEAKEST field it needed — ranking by the strongest
  lets one common word in a title outrank a page discussing the whole phrase. `Summary` is the
  page's OPENING STATEMENT rather than its whole first section: a concept cited a hundred
  times accumulates a synthesis long enough to hold any three common words, and ordered by
  evidence it would otherwise answer every query in the vault. Default scope is concepts, documents and explorations.
  `--include-sources` adds managed daily, personal and synthesis pages, excluding the task board.
  A query naming a single concept matches its declared title and aliases as phrases.
  Other queries require every term. Metadata filters never narrow the vocabulary used to
  resolve alias ownership. Scattered words do not establish a match for a declared concept
  name, so Korean spacing variants registered as aliases reach the same prose. Ambiguous
  names do not expand through competing owners. A declared name may occur in another sense;
  excerpts expose that use for semantic review.
  **A term is matched as a word where the term itself is Latin, and anywhere where it is
  not.** Plain containment is what Korean needs — a noun carries its particle inside the same
  word (`에이전트를`), so a boundary requirement makes the vault's own language unsearchable —
  while Latin script separates words with spaces, so `ai` reached `daily`, `chain` and
  `guardrail` at the NAME rank, above every page that discusses the subject. `contains_term`
  therefore requires a boundary per EDGE, decided by the TERM: where a term begins or ends
  with an ASCII letter or digit, the character beside the match may not be one. Nothing
  inspects the page's language. **Search reads visible Markdown text through CommonMark parsing.**
  Soft breaks and inline formatting preserve phrases; block boundaries, list items and
  table cells separate them. Link destinations and HTML markup are excluded. Titles and
  aliases are independent name fields, so a phrase cannot span two names. Excerpts use
  the same text projection as matching. This read-only projection does not change graph
  link extraction or authored content.

- **`brief::build_brief` splits a day by whether the vault already held the concept.** Ten
  concepts enter a day and thirty are named again; what it did not hold is what was learned
  and carries the line its page opens with, while what it already held carries names only —
  the reader knows those, and restating them is the flood this reduces. `learned` is grouped
  by `category` so a reader skips a group whole; `revisited` is ordered by evidence.
  **Which concepts a day named is read off THAT DAY'S PAGES — its daily pages, and the document
  and exploration pages `created` on it — never off `updated` on the concept.** All three are
  read because the last two are what a skill writes DIRECTLY, with no daily page behind them.
  A concept page nothing cites is absent from every day by construction, which is the rule
  working rather than a gap: a page named by nothing was named on no day, and it is `lore graph
  lint`'s orphan finding. A concept page records when it
  was LAST cited and not every day it was, so a concept introduced on Tuesday and named again
  on Wednesday fell out of Tuesday's `learned` and, no longer new on Wednesday, out of every
  day: measured at 1 in 13 on one day of the reference vault, and it is the recurring concepts
  — the interesting ones — that vanish. A day's own links do not move. Bodies come from that day's pages and the
  concepts they name. `ExistingPageResolver` indexes concept addresses and filesystem metadata
  so an actual alias reaches its owner, while a nonexistent Linux case spelling is not credited.
  It does not read every concept body. Brief and graph share this filesystem resolver.

- **`VaultWriter::write_page_sync`** calls `lk_core::fs::write_atomic` directly (no tokio
  runtime). Used by graph commands — the same single atomic-write implementation as the
  async path, not a separate one.

- **Knowledge reviews are authored explorations.** Optional `review` metadata records status,
  evidence revisions and a resolution. `list_reviews` reports evidence drift and invalid
  records without changing a judgment. `ReviewEffectiveState` is the shared current-state
  policy: errors are invalid, changed evidence needs reassessment, otherwise the authored
  status stands. `review.evidence` is the evidence for the current explicit assessment,
  not a union of all historical versions. Before reassessment, archive the review itself
  with `wiki evidence --archive <review-path>`; preserve that archive path/revision and
  a dated judgment in the authored body. After actually reviewing and archiving the new
  evidence, explicitly replace the current evidence set. Never silently retag old prose,
  delete historical archives, or treat the archived review as independent source evidence.
  Verified historical archives preserve validity after source deletion.
  The bounded catalog shows reviews once in its first group with localized state labels;
  an evidence citation never labels the source itself as a conflict target.
  `inspect_evidence` groups exact recorded `source_url`
  values; neither URL count nor citation count claims independence.

- **Review evidence archives** retain the first raw source snapshot for an authored revision
  under `.lorekeeper/evidence/<revision>.md`. Reading verifies the content hash; publication
  never replaces an existing archive. Current source drift and historical availability are
  separate facts. Missing current evidence is not an integrity error when its recorded
  revision remains available in a verified archive.

- **`MutationSession`** holds the cooperating writer lock and recovers a pending journal
  before new plans. `MutationPlan` preflights every expected file, then records ordered
  creates, edits, renames and removals with their original and resulting content. Recovery
  accepts only the expected before/after states and preserves conflicts for reconciliation.
  Existing hard-link directory entries are not case-only rename aliases. Journal files are
  private; durable directory creation and progress use the shared atomic-write primitive.
  The plan is persisted once; small progress receipts bind the cursor to its plan revision.
  Completion removes and syncs the plan before clearing progress, so recovery cannot replay
  earlier edits over a later state of the same file.

- **Page addresses** pass through `page_path` for reads and `writable_page_path` for
  publication. Existing ancestors must resolve inside the configured vault, including
  when the leaf does not exist. Internal directory aliases are supported; publication
  refuses leaf symlinks rather than replacing their directory entries. This validates
  paths at access time, not an OS-level guarantee against concurrent symlink replacement.

- **Live discovery is bounded output over a complete scope scan.** Search and browse share the
  page reader. Browse sorts by title and exact path; search retains field ranking and uses
  exact path as the final tie-break. Category/type filters apply after alias ownership is
  resolved across the scope. Facets count query matches before filters, `total` counts after
  filters, and pagination runs last. Missing category/type uses an empty facet key. Live offsets
  reflect current files; an explicit prepared snapshot pins the captured corpus.
  Browse may sort by declared `created` newest first, with exact path ties and missing/invalid
  dates last. `created` may be source-authored, never inferred collection time. Inclusive
  date bounds exclude unknown dates; facets still count the scope before metadata/date filters.
  Hits expose bounded declared strings and valid/missing/invalid status, not arbitrary metadata
  objects. Date sorting/ranges are browse-only; the library refuses them with a text query.

- **`inspect_links`** reads one source snapshot and returns explicit internal link
  occurrences in source order, including repeats. Source line numbers include frontmatter;
  context is the literal link-bearing line. Target filters resolve lexical addresses and
  verified filesystem aliases, never concept-name guesses. Missing targets remain inspectable.
  `file_revision` pins pagination when supplied on the next request. This shares graph link
  extraction and does not infer relationship types or read target bodies.

- **`prepare_search`** creates a reconstructible SQLite FTS5 snapshot under `.lorekeeper/`.
  Explicit `--snapshot` queries use that captured corpus; default search reads live files.
  The snapshot identity binds schema version, vault/config context, paths and full revisions.
  Refresh commits atomically in SQLite; failed reads keep the previous snapshot. External
  editors are not frozen during capture. FTS trigram filtering only narrows candidates;
  short terms use exact substring candidates and the shared matcher decides every hit.
  Schema/projection changes require a snapshot version bump. No embeddings or daemon.
  Snapshot browse uses prepared scalar filters/counts and SQL LIMIT/OFFSET, decoding only
  selected metadata rows rather than all page payloads. Scalar projections and metadata
  must derive from the same captured page; projection/version changes require prepare again.
  Snapshot links use captured lexical addresses without filesystem-alias inference. Link
  context is capped at 400 Unicode characters with explicit `context_truncated` reporting.
  Body search reads the captured Markdown payload and its physical line offset through
  the same matcher as live search. It scans the selected snapshot's full scope, without
  visible-text FTS filtering that would omit code/link matches. Results identify the
  snapshot and captured file revisions; a later live read still checks that revision.
- **`inspect_document`** parses a CommonMark outline, paginates it, and reads bounded section
  text with Unicode scalar offsets. Section ids belong to the returned full file revision;
  duplicate headings stay distinct. A selected section includes its nested subsections.
  Callers pin subsequent reads with `--revision`, including revisions from search hits.
- **`load_concept_registry`** is shared by ingestion and CLI resolution. One operation loads
  the declared names once; unreadable or disappearing pages fail instead of implying absence.

- **`inspect_provenance`** reads only the configured document collection and resolves declared
  targets by the exact `(source_identity.repository, source_identity.document)` pair. It never
  guesses from titles, filenames, URLs or concept aliases. Verified filesystem symlink spellings
  share one owner, preferring its direct collection address; distinct files and hard-link entries
  remain distinct owners. All scanned addresses still bind the collection revision and coverage.
  Duplicate declarations retain all
  field locations in one row; missing and multiple owners remain unresolved and ambiguous.
  `superseded_by` is exposed even when nodex outgoing relations omit it. Center metadata issues are visible in the bounded entry stream; unrelated malformed
  records and unavailable legacy identities are counted in small coverage metadata. Compact
  declaring/target records omit full source_record; use wiki read for the complete record.
  Target candidates share the bounded entry stream. The revision binds the complete collection and query, so a newly added owner
  invalidates pagination even when the starting document did not change. This read-only view
  neither adds Markdown graph edges nor infers support, contradiction or current truth.

- **Manual originals** have a separate typed store, never the authored-page review archive.
  `publish_manual_original` validates captured bytes, exclusive-publishes content-addressed
  objects, hard-links exact member paths, and publishes the manifest last. Identical members
  across revisions share bytes; occupied/corrupt objects are errors, never overwrites or a
  suffix fallback. Store symlinks are refused. A complete manifest records listed byte
  inventory, not complete browser offline capture. `inspect_original` reads that manifest
  and verifies the entry plus any selected member, never mutable `source_file`; legacy pages
  report unavailable. Roster pagination is bounded/revision-pinned; coverage is a bounded
  sample with explicit total/truncation. Raw previews preserve UTF-8 bytes up to a 64 KiB
  boundary; binary members expose their validated path. Hidden originals are not knowledge
  nodes; graph resolvability only accepts exact manifest-listed, digest-verified members.

- **RSS originals** (`rss_original.rs`) use a separate typed namespace under
  `.lorekeeper/evidence/rss/`: exclusive-publish event JSON first, manifest last. Pages
  reference only complete validated manifests; publication/recovery guards exact original
  dependencies. A manifest is bounded metadata, not a check of every unselected payload;
  selected events are canonical/digest/identity checked and title/body text is character-paginated.
  Missing/corrupt/ambiguous or symlinked evidence is refused. Legacy event-log discovery is
  read-only `legacy_unbound`; matching source/date/config does not prove page derivation.
  No historical backfill, evidence pruning or event-level citation migration is implied.

- **RSS event selections** share phase1 immutable objects rather than copying article bodies.
  The origin's type/source/date, generated link and explicit membership must match. New
  selections belong to the provided manifest; carried selections validate their historical
  manifest and object even after the current manifest moves. Bounded selected reads use
  `wiki original ORIGIN --evidence-concept CONCEPT`; inventory revisions pin membership,
  while `--event ID --revision PAYLOAD_REV` pins the selected content. No title/position
  inference or legacy upgrade. Journal guards use verified canonical bytes, not a fresh
  unverified baseline; recovery rechecks preserved dependencies. Review evidence remains
  page-only and rejects event selectors explicitly. SQLite links project at most 20 selected
  pairs per generated link with total/truncated fields; full inventory is separately paginated.
  This is derived metadata, not a new article store or event fulltext index.
