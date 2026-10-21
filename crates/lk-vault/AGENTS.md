# lk-vault

Vault page I/O, templates, evidence stores and read/navigation views. Markdown is
canonical; SQLite navigation is derived.

## Pages, sections and templates

- `writer.rs`: sync writes delegate to `lk_core::fs::write_atomic`; async writes
  use `spawn_blocking` around the same durable primitive. Never write final paths
  directly or introduce a second atomic implementation.
- `lk_core::frontmatter::parse_page`: parse frontmatter only at standalone first-line/closing `---`
  delimiters (BOM/CRLF supported). `VaultPage.original` retains exact input for
  optimistic guards; never reconstruct it from parsed fields.
- `VaultStore::read_frontmatter` projects metadata without validating the body. Use it
  for name/address inventories; publication still reads and guards the complete page.
- `section.rs`: section reads/replacements use root-level CommonMark H2 boundaries,
  excluding frontmatter, nested blocks, code and HTML. Only exact ATX headings are
  owned anchors; duplicates refuse reads/replacement. Preserve surrounding bytes.
  Read with `resolve_section` across locales, preferring nonblank when both exist;
  legitimate blank remains present. Write current locale headings only.
  Semantic bodies use `replace_section_checked`, including cached and grounding prose;
  checked replacement preserves all page boundaries or refuses the write.
- Frontmatter setters stay within the real block: `set_frontmatter_field` changes
  a top-level key; `set_llm_input` changes a child of block-style `llm_inputs`;
  `record_llm_input` may establish that mapping. Missing placement returns `None`,
  never appends markers to body. Callers serialize values and handle failure.
- Read `continuation_span` before changing setters: deeper indentation belongs to
  an entry; siblings end it; blank lines don't belong/end; comments belong to the
  following entry except inside per-line-tracked block scalars (including `- |`).
  Infer child indentation from the first real child, not a comment.
- `template.rs`/`templates/`: daily children extend `_daily_base` and override only
  source-specific blocks. Partials aren't page defaults. Concept entries already
  include bullets: emit `{{ c }}`, not `- {{ c }}`. LLM-derived YAML strings use JSON
  serialization. Only NotFound means no user override; parse/I/O errors propagate.
  Keep the exhaustive daily-template render contract test when adding a child.
  Scalar titles/authors use `markdown_text`; source bodies use `markdown_fragment`,
  source links `markdown_link`. Fragment rendering owns heading containment,
  reference/footnote isolation and code closure without changing event identity.
  EOF-terminated raw HTML displays as literal `html` code; balanced HTML stays markup.
  Templates own generated separators; rendering never collapses whitespace across
  injected source, semantic bodies or frontmatter.

## Publication and paths (`mutation.rs`, `address.rs`, `review.rs`)

- Cooperating writers acquire `MutationSession`, recover pending work, then derive
  and publish one guarded `MutationPlan`. Preflight every expected file; persist
  full before/after steps before edits. Recovery accepts expected states only and
  preserves conflicts. Lock order is mutation session before narrower queue locks.
- Preflight rejects conflicting publication addresses resolving to one file entry,
  including absent case/Unicode aliases and missing descendants under directory aliases.
  Missing names use disposable directory-only probes beneath their existing parent;
  cleanup must finish before journaling. No string folding establishes identity.
  Pure repeated read guards
  and explicit spelling renames remain valid; hard links are distinct entries.
  After a retained guard is evaluated, later owned steps must preserve it.
- Persist plan once; progress receipts bind its revision. Remove/sync plan before
  clearing progress to avoid replaying earlier steps over later same-file state.
  Journal private data and durable directory creation use shared primitives.
- Reads use `page_path`, writes `writable_page_path`. Ancestors must resolve inside
  the vault; internal directory aliases are supported, leaf symlinks refused for
  publication. Hard-link entries are distinct owners, not case-only aliases.
  These checks do not freeze external editors/symlink replacement.
- `IngestLog` propagates real I/O errors, treats NotFound as empty, warns/skips
  malformed history lines. `find_last_collection` counts successful empty/Skipped
  observations, not Failed. Retention must preserve that latest collected line.

## Reviews and authored evidence (`review.rs`)

- `knowledge.rs` validates guarded metadata/review curation as one plan, archives prior
  authored states and validates evidence against proposed final pages. Preview validates
  publication addresses without changing pages or journals; it removes namespace probes.
  Category changes use configured IDs; omitted values keep state, null clears category,
  and an explicit empty tags list clears tags. Tags describe the concept, not all its sources.
  Retired concepts keep addresses and evidence; normal discovery excludes them. Curation
  never owns synthesis/related/source sections or completion markers.
- Reviews are authored explorations: effective state is invalid on errors, needs
  reassessment on changed evidence, otherwise authored status. Open isn't invalid.
  Evidence pins the current assessment, not a union of all historical revisions.
  Associate subjects/evidence by verified page owners while preserving declared
  addresses. Symlink aliases share a review scope; hard-link entries remain distinct.
  Recover typed subjects from the same metadata even when other review fields are
  invalid; discovery and audit share that scope without accepting the assessment.
  Review metadata is a mapping with named fields, never a positional array.
- Before reassessment archive the review itself; read changed evidence and record a
  dated judgment, preserving archive paths/revisions. Explicitly replace current
  evidence after review. Never retag stale prose, delete history or count a review
  archive as independent corroboration.
- `.lorekeeper/evidence/<revision>.md` keeps the first exact source snapshot for an
  authored revision, verifies hash on read and never overwrites. Missing current
  source is not an integrity failure if its recorded archive verifies. URL or
  citation counts don't establish independence.

## Read surfaces

- `context.rs`: return explicit links, graph evidence, source-declared relations and
  scoped reviews from one prepared SQLite transaction. Preserve each authority and
  distinguish unavailable capture from an available empty set. Pagination bounds
  each group independently; a shared offset never establishes a semantic relation.
- `search.rs`: exact identity, whole-name spacing candidates and contiguous visible
  phrases precede scattered terms, then weakest field/source count/exact path. Retrieval
  folding never changes identity. `--phrase` and block-bounded `--spacing` are explicit;
  raw body matching rejects them. Tags and authored knowledge lifecycle filter separately
  from source lifecycle. Review subjects are scoped assessments, not citations. Summary means opening statement, not whole synthesis.
  Default scope concepts/documents/explorations; source scope adds managed pages,
  never task boards. Concept vocabulary honors declared non-concept types, retaining
  untyped legacy concepts. Resolve aliases across scope before metadata filtering;
  ambiguity doesn't expand. Declared names match phrases, other queries every term.
- Matching/excerpts share CommonMark visible-text projection: formatting/soft breaks
  preserve phrases; blocks/list items/table cells separate; link destinations and
  markup are excluded. Raw HTML contributes parsed text with the same block boundaries;
  scripts, styles, head metadata, templates and embedded-content fallback containers
  (`noscript`, `iframe`, `noembed`, `noframes`) do not contribute text. Form choices
  and legends remain separate text blocks; this projection does not evaluate CSS.
  ASCII-alphanumeric term edges require boundaries, CJK allows attached particles.
- Browse filters/facets/pagination have distinct counts: facets before metadata/date
  filters, total after, pagination last. Each facet pages distinct values in exact-value
  order, independently of hit pagination. Search previews at most 3 scoped reviews;
  captured context reads only the selected review rows, with complete inventory counts.
  Created-date sort is browse-only, exact-path
  ties, unknown dates last/excluded by bounds; authored date isn't collection time.
- `document.rs`: CommonMark outline/section IDs pin full file revision; duplicate
  headings stay distinct, selected sections include nested children. Character
  pagination uses Unicode scalars. Later reads must check the returned revision.
- `links.rs`/`indexed_links.rs`: explicit occurrences in source order including
  repeats, physical lines including frontmatter. Live target filters use lexical
  addresses/verified filesystem aliases, not names; missing links remain visible.
  Prepared links use captured lexical addresses only. Context ≤400 Unicode chars
  with truncation flag; do not clip destination/label into a different address.
- `provenance.rs`: resolve exact typed repository/provider namespace and document identities, never
  title/URL/alias guesses. Duplicate/missing owners stay ambiguous/unresolved;
  verified symlink spellings share an owner, hard links don't. Revision covers the
  collection/query, including added owners and malformed/legacy coverage counts.
  This read-only view creates no graph edge or semantic current-truth judgment.
- `brief.rs`: day membership comes from that day's pages, not concept.updated.
  Learned concepts carry opening lines, revisited names only; resolver metadata
  verifies real addresses without reading every concept body.

## Navigation (`search_index.rs`, `navigation.rs`)

- `prepare_search` atomically commits a reconstructible FTS5 snapshot; failed reads
  preserve old snapshot. Explicit `--snapshot` pins captured corpus/config/paths/full
  revisions; default search is live. External edits aren't frozen during capture.
  Scope facets and bounded SQLite planner statistics are prepared in that transaction;
  read-only queries never optimize or refresh the capture.
  Projection/schema changes bump snapshot version.
  Per-page graph capture membership distinguishes uncaptured concepts from captured empty
  evidence; missing evidence rows for captured concepts remain errors.
  Indexed name/tag/document identities and declared relations avoid full vocabulary/collection
  reads; selected scalar rows use SQL filters/LIMIT/OFFSET. FTS narrows candidates only;
  shared matcher decides hits.
  Body search scans captured full scope without visible-text FTS omissions.
- Refresh prepares SQLite and publishes only the small configured `index.md` guide.
  `plan_navigation_entry` captures exact prior bytes/absence for the mutation journal;
  apply it only after successful snapshot preparation. Its opaque ownership marker
  must be the first body line. Unmarked files, authored frontmatter, malformed pages,
  nonregular entries and leaf symlinks refuse publication.
- Navigation never generates, discovers, retires or migrates other Markdown output.
  Only the configured guide and schema entry addresses are reserved from source
  searches. Similarly named authored files and directories remain ordinary content.

## Immutable originals (`manual_original.rs`, `rss_original.rs`, `rss_evidence.rs`)

- Manual originals are separate from review archives: stream-verify captured files,
  exclusive-publish objects/exact hard-linked members, manifest last. Same bytes
  reuse; occupied/corrupt objects fail, no suffix overwrite fallback. Refuse store
  symlinks. Inventory isn't a promise of complete browser offline capture.
- Original reads verify manifest/selected member, never mutable `source_file`.
  Legacy missing originals report unavailable. Bound roster/revision pagination;
  raw UTF-8 preview ≤64KiB, binary validated path. Verify large members without
  retaining their full bytes in memory. Current acquisition settings do not restrict
  historical manifests. Only listed digest-verified
  members are graph-resolvable, not hidden knowledge nodes.
- RSS store is `.lorekeeper/evidence/rss/`: event JSON before complete manifest.
  Metadata inventory does not verify every unselected payload. Selected content is
  canonical/digest/identity checked and character-paginated; corrupt/missing/ambiguous
  or symlink evidence refuses. Legacy discovery remains `legacy_unbound`, not proof
  that a page historically derived from matching source/date/config.
- Selections require matching origin type/source/date, generated concept link and
  explicit membership. New pairs belong to provided manifest; carried pairs keep
  verifying historical originals. Inventory revision pins membership; event payload
  revision pins selected text. No title/position inference or automatic migration.
- Journal guards bind verified canonical originals and recheck preserved dependencies
  during recovery, not a fresh unverified baseline. Review evidence stays page-only
  and rejects selectors. SQLite link previews show ≤20 selected pairs plus explicit
  total/truncation; full inventory is separate, not an event fulltext index.
