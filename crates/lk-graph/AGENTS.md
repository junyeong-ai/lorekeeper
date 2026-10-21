# lk-graph

Deterministic graph analysis and Markdown mutations; no HTTP, LLM, or async runtime.
Follow the root AGENTS.md for shared mutation/journal rules. Live graph mutations
read current Markdown. Prepared SQLite relationship queries belong to `lk-vault`
and CLI discovery, not a second graph authority.

## Addresses, scopes, and scan views (`scan.rs`)

- `GraphConfig` rejects unknown fields and escaping paths. Page addresses retain
  case and punctuation; optional `.md` omission is a resolver key, not a rename.
  A slug is diagnostic/address information, not an inferred semantic alias.
- Filesystem aliases use actual same-file resolution in `build_at`, never slug
  similarity. Escaping/traversal is an error, not an unresolved harmless link.
- Keep three views distinct: scanned existence, configured link sources, and
  analysis pages. Excluded pages can still resolve and be repointed. Scope prefixes
  are folded consistently; do not let an analysis filter hide broken source links.
- Hidden directories are skipped except the exact verified evidence archive path:
  an archive is resolvable only through its existing hash validator. It is NOT a
  knowledge node, citation source, connectivity participant, or repoint target.
- `is_resolvable` is broader than `is_knowledge`. Reserved entry/schema guides are valid link
  destinations but not knowledge/source nodes. Declared task boards likewise exist
  as destinations while excluded from graph analysis/source and broken-link scans.
- Standalone broken-link checks inspect the full scanned source universe except
  reserved meta pages/task boards, not only the configured analysis subset.

## Derived navigation and diagnostics

- Markdown remains canonical; SQLite discovery belongs to lk-vault. This crate
  computes live topology and structural checks, without Markdown catalog/map exports
  or a catalog-currency check. Only entry/schema guides are reserved meta pages;
  ordinary `log.md`, `map.md` or `catalog/` addresses are not implicitly generated.
- Community clustering is topology, not taxonomy or semantic equivalence. Preserve
  original destinations; suggestions guide judgment rather than establish relations.
- Exit codes distinguish clean (0), structural violations (1), and execution errors
  (2, taking priority). Error JSON has unavailable checks as null, not reassuring
  zeroes. Destructure check counts exhaustively so new checks cannot disappear.
- Adamic–Adar proposals require configured shared-neighbor evidence within the same
  community and deterministic ordering. Their score is not a meaning judgment.
  Evidence changes, not age decay, drive reevaluation.
- `graph.rs::page_neighbors` returns direct incoming/outgoing links. CLI discovery
  applies declared direction/type/scope before pagination; facets describe the
  pre-filter scope. Direct links do not establish causation.

## Incremental semantic maintenance (`audit.rs`)

- Audit candidates explicitly distinguish concepts from declared source documents
  under the configured wiki documents collection, including nested addresses.
  Concept evidence retains the shared backlink authority and existing input identity.
  A document's own page is its subject input, never a fabricated self-citation;
  direct concept/document links are revision-pinned context, not inferred evidence
  or a transitive audit. Scoped reviews retain explicit evidence for either kind.
  Receipts describe completed reads, never factual confidence or original/provider
  evidence that was not actually read. Aggregate and per-kind counts retain this scope.
- Exact selected planning evaluates only selected discovery recipes;
  `coverage_scope` distinguishes selection counts from complete corpus accounting.
  Assessment recording evaluates current corpus inputs to preserve exact pending
  first-discovery order. Reuse prepared lexical discovery only after verifying its
  complete live knowledge inventory and exact file bytes. Missing or stale captures
  use the same complete live matcher without preparing or mutating navigation.
- Preserve pending first-discovery order and rotate changed/first-time work. A bounded
  weekly batch must not starve old work or grow into an unbounded full-corpus audit.
  Native admission journals pending order before external discovery or assessment,
  under the writer claim with exact selected-input and readiness guards. It creates
  no assessment receipt and does not advance a class turn; planning stays read-only.
  Native admission can defer unavailable subjects through a readiness callback;
  preserve complete coverage counts and pending order, and do not consume a class
  turn or create a receipt merely because a subject is deferred.
  Whole-input library planning defers individually oversized inputs without consuming
  a class turn, allowing later fitting work. Partitioned CLI/native planning admits
  them fairly and preserves the measured oversized status. Recording/completion then
  requires exact completed-work proof for each exempted subject in the same guarded
  publication plan; ordinary subjects retain their combined whole-input bound.
  Exact selections retain their complete requested scope.
- Scope reviews by their declared subjects, the subject's `knowledge_review` pointer,
  and prior receipt review paths. Omitting result review paths cannot bypass a missing,
  invalid or stale scoped review. Valid open findings remain durable follow-up.
  Bind authored review/evidence revisions and guard exact read bytes when recording.
  Read/count verified prior evidence only when current evidence differs or is missing;
  an identical archive or bookkeeping-only change does not reopen semantic work.
  Resolve review subject aliases through the shared verified page owner; retain authored
  addresses and keep hard-link entries distinct from symlink aliases.
- Audit completion follows native merge receipts from the same workflow to a currently
  assessed survivor. Merge receipts join the page mutation journal and retain only
  the latest merging workflow. A correlation ID never authorizes a lease bypass.
  Only concept slots can follow those merges; a document requires a current assessed
  document at its exact selected address.

## Concept names and diagnostics (`concept_lint.rs`)

Names are declared title and aliases (file-stem fallback when title is missing),
not an additional implicit filename alias. Duplicate declared names report ambiguity;
they never authorize an automatic merge. A malformed frontmatter page retains its
file address for address-only diagnostics, not invented parsed metadata/body.

An empty category configuration disables that check; missing/null/empty category is
uncategorized, while a present invalidly typed value remains diagnosable. Conflict
callouts are fence-aware informational content. Durable open/resolved judgments
belong to exploration reviews, not a machine-persisted callout inferred by location.

## Repointing and merging (`normalize.rs`, `merge.rs`)

- Choose normalization candidates in analysis scope, but rewrite affected links
  throughout the vault. A merge must find all supported address spellings, retain
  labels/anchors, and fold declared title/aliases without copying prose blindly.
- Merge endpoints are exact collection-relative addresses, preserving nesting,
  case and punctuation. Publication requires both full file revisions from the
  caller's reads; a preview exposes revisions but does not establish equivalence.
- Related prose is authored content too: refuse merging it unless explicitly forced.
  Force does not bypass unreadable metadata or immutable evidence protection.
  A dry run previews without writes; backlink reconciliation follows publication.
- Repoint validated RSS event bindings with their typed addresses/original guards;
  changing visible links alone must not leave stale provenance declarations.

## Backlinks and synthesis requests (`backlinks.rs`)

- Reconciliation and concept lints accept declared concepts and untyped legacy pages
  in the concepts collection; explicit other types are excluded. Exact evidence capture
  and resynthesis require declared concepts. Use the shared frontmatter role predicates.
  Preserve legacy bookkeeping/adoption, but report/skip an unchanged legacy target when
  it needs new typed synthesis; never infer its type or record an unanswerable promise.
- This is the sole derivation of Sources, `source_count`, and synthesis input.
  Derive citations from actual eligible source body links with exact authored
  revisions. Update the three together; unreadable sources forbid publication.
  Symlink spellings share one verified source owner; hard-link entries remain distinct.
  Refuse differing alias-relative citation contexts rather than choosing or unioning them.
  Report discarded anchor sections rather than pretending their evidence remained.
- Generated Sources is a bounded, stable discovery instruction; complete citations
  live in the rebuildable prepared SQLite view. Migrate only exact legacy generated
  lists recognized by the shared structural classifier; retain/report custom Sources
  prose. Compact/custom bodies have no prior complete set: added/removed deltas are
  null, never invented empty lists. Merge uses the same ownership classifier.
  `source_revision` remains exact: the one-time body migration reopens affected audit
  receipts, preserving judgments and evidence guards rather than relabeling them.
- Sources are managed non-concept content, excluding entry/schema guides and task boards. Do not
  revive a frontmatter `sources` array as authority. Validated RSS event dependencies
  contribute selected event digests; `source_count` still counts origin pages.
- Authored concepts with substantive prose but no input may be adopted. Headings
  and callout-only content do not qualify; defer/report multi-citation adoption
  when required instead of silently manufacturing a completed input.
  Classify actual Markdown blocks: ordinary quotations and literal code are authored
  content, while callouts require their explicit marker in that quote's scope.
- An empty citation set creates no new synthesis request. Preserve an answered
  input/done pair. Withdraw only an unanswered input whose evidence became empty;
  Skip or a missing anchor is not permission to erase an answer.
- Explicit resynthesis requires an exact readable concept, nonempty valid citations,
  and real owned anchors. CLI journals request and typed task under vault→queue
  lock order, reusing matching current work and preserving body. Dry run is inert;
  historical task regeneration is a separate retained-task contract.

## Citation retraction (`lk-cli/commands/graph/retract_citation.rs`)

A normal retraction removes an explicitly generated standalone concept citation,
with exact source/concept revisions. Refuse mixed authored prose, ambiguity, pending
overlapping tasks, or unpublished results. Archive old evidence and use the existing
transaction/journal; do not directly edit concept markers or run incidental backlink
writes. Preview does not recover journals.

For typed RSS pair retraction, `GeneratedOnly` removes only selected generated
occurrences/declarations. The last pair may remove a generated link; authored prose
still provides page-level evidence. Ordinary whole-link removal remains strict and
must not be weakened to accommodate a partial pair operation.
