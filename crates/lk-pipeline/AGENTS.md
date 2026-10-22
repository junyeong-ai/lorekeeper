# lk-pipeline

Deterministic transforms from source observations to planned vault pages.
`PipelineContext` shares templates, LLM, dirs, timezone, locale and optional personal
configuration. Publication is guarded, not a second uncoordinated render path.

## Source planning (`lib.rs`, `normalize.rs`, `dedup.rs`, `event_log.rs`)

- One mutable `Pipeline` owns an ingest run. Plan per source; accumulate concepts
  across sources and render them once. Stage/read every concept before committing
  any to the accumulator on plan, documents and applied-extraction paths, so a
  failed source cannot leave publishable orphan drafts.
- Original RSS payloads join the source-transaction planning budgets: enforce the
  source and run bounds before semantic staging, and commit run bytes only after a
  successful plan. A rejected source must leave capacity for later fitting sources.
- Normalize event identity from external_id or JSON `[title, body]`. Dedup only
  exact `EventId` within one fetch, first wins; URL/title/body similarity is not
  identity. Cross-source observations remain separate provenance.
  Trim title padding but retain the exact body, including code, raw HTML and line
  endings. The render boundary owns heading containment and presentation syntax.
- Canonicalize through `Event::canonical_cmp` (newest, then id) before bucketing,
  hashing and rendering. Adapter return order must not change bytes/cache hashes.
- Streaming sources union fetched and stored raw events per date, fresh ID wins.
  Corrupt event-log lines fail without rewriting: skipping would destroy retained
  observations. Complete-refetch sources render their fetched full civil days.
  RSS retains an established event ID only from exact framed identity or a fresh
  entry's computed legacy ID plus matching recorded feed. Competing owners fail;
  stored-only observations and prior immutable event selectors remain unchanged.
- Derive dates in configured timezone; future events never materialize. Adapters
  own `is_self`; `assign_personal` additionally requires configured tracked sources.
  Keyword classification is ordered first-match; bounded Latin matching and CJK
  particle matching stay in the shared classifier. No match remains uncategorized.

## Materialized semantic sections (`llm_cache.rs`, `render.rs`)

1. Compute request hash; lookup the existing page through `VaultStore`.
2. Cache only when `<key>_done` equals current input hash, even for an empty body.
3. Render structural fields/headings from embedded templates, then splice preserved
   semantic bodies. If a template lacks a cached heading, skip the write rather
   than lose answered text.
4. Always stamp current `<key>`; re-emit `_done` only on a verified cache hit.

Use `TargetKind` keys and locale-tolerant vault section resolution. Deleting `_done`
invalidates a view; deleting prose does not. Never infer completion from emptiness.
Completion requires the page's declared `type` to match its semantic task kind.
Use one template path for daily, document, work-log and rollup pages.

Concept links alone **accumulate** through `render::accumulate_concepts` on render
and apply. Carry only valid concept destinations and preserve their addresses;
never reconstruct concepts from display labels. Other semantic bodies replace.
Retention keeps citations through pending extraction but does not certify their
continued factual support; explicit reviewed retraction is separate.
Disabling extraction removes its current/completion markers and admits no new task;
retain the existing Related Concepts body and RSS selectors exactly. The compatible
template variable `extract_concepts` controls section presence (enabled policy or a
retained section), never task admission. A missing/duplicate retained anchor refuses
source publication instead of dropping evidence. New disabled pages omit that section.

## Document identity and publication (`document_identity.rs`)

- Address by exact `(repository, document id)` ownership, with explicit legacy
  source_file/source_url fallback. Load all document descendants once and retain
  nested addresses. Unique owner keeps its address after title changes; multiple
  owners or overlapping document observations refuse before rendering.
  `DocumentOwnership` shares this inventory and unique-owner resolution with manual
  publication receipts; never replace structured identity with a raw location check.
- Resolve established ownership and verified legacy adoption before deriving a new
  address. New addresses retain title/source_file slugs; when neither yields a slug,
  a structured source identity supplies a deterministic hashed address through the
  same collision guards. Without either, refuse rather than silently drop a document.
- Repeated identical events may collapse; different payloads under one ID fail.
  Reserve identities only after a successful plan. New-title collisions use the
  document's event suffix, lengthening through full hash then numeric tails; never
  assume a suffix is unique or overwrite a different document.
- Legacy adoption requires exact location and matching source record ID. Similar
  titles/content never authorize adoption; removed sources are not auto-deleted.
- `source_record` provenance participates in summary/extraction input. A metadata
  status change may change applicability even with identical primary body; archived
  does not mean false. Keep original content separate from interpretation.
- Relocate parsed manual-original member links to the selected document address
  before constructing semantic input or rendering, including nested existing owners.
  Keep raw observations and their identity unchanged; relocated projections still
  obey configured output and source batch limits.
- Capture `VaultPage.original` when reading. `RenderResult.expected_original`
  guards the exact prior bytes/absence; never reread a newer baseline at publish
  time. Use `plan_publication`/`publish_expected` or document publication with its
  evidence archive in the same mutation plan.
- Return processing recipes only for materialized document outputs. Bind source
  policy, normalization/rendering, actual template inputs and decoder identity;
  exclude schedules, accounts and batch size. Queue versus noop publication is
  relevant: a prior noop receipt cannot establish that semantic requests exist.
  Projection-capacity errors stay typed so bounded manual retries cannot hide
  rendering or identity failures.

## Concept ownership (`concept_draft.rs`)

- Resolve through shared `ConceptRegistry`; unreadable/malformed names fail and
  ambiguity refuses staging. Register an allocated unoccupied address before the
  next concept. First established title/category/aliases win; warn category
  disagreement, widen dates only, preserve `source_count` (backlinks-sync owns it).
  Source concept_policy controls admission: disabled enqueues no extraction, existing-only
  refuses absent owners, and supported-new permits supported new subjects. Queue results
  carry the task's exact policy; changing policy changes extraction input identity.
- Applied extraction explicitly admits new or reuses an exact revision-pinned owner.
  New names/aliases cannot claim an established owner and require nonblank grounding.
  Separate new proposals never coalesce by name alone, including unpublished drafts;
  publish the selected owner and rediscover before explicitly reusing it.
  On a failed extraction discard transient name reservations, retaining committed
  drafts; rebuild the registry lazily instead of copying it for every result.
- Resolve each proposed alias; accept only absent or same-owner names, including
  earlier decisions in this result. Never approximate. Keep established aliases.
  Result ordering determines first ownership; do not silently merge rival pages.
- Refused aliases that created a rival must reach the writer on the last materialized
  page while both owners exist. Report, don't auto-merge: semantic equivalence is a
  judgment. Preserve this record across partial-write/retry behavior.
- Preserve all non-structural frontmatter, including authored lifecycle/review/tags,
  across rerenders. A new citation never reactivates retired knowledge. Reject invalid
  lifecycle values before staging. Structural fields retain their existing owners.
- Carry existing synthesis input/completion markers unchanged. A newly created
  single-grounding page records its completed origin revision and shared citation
  digest as input/done, so backlinks cannot adopt one sentence as a synthesis of
  many sources. Multiple evidence does not pretend a single grounding answered all.

## Rollups (`synthesis.rs`, `work_log.rs`)

- `personal=None` means no work-log or personal reviews; weekly cross-source themes
  remain. A full ingest alone rewrites the cross-source work-log.
- `work_log_inputs` projects the same normalized personal events and resolved
  categories used by rendering. Keep its canonical ordering and policy fingerprint
  aligned with the renderer; query coverage remains a separate receipt contract.
- Theme = weekly cross-source cluster; topic = daily per-source activity group.
  Keep those distinct from contribution categories and graph communities.
- Each rollup uses the same cache/section contract. Transient LLM failure yields
  `SynthesisSection::failed()` and skips publication, not an empty completed input.
- Fallback is per missing child: quarter month→weeks, year quarter→months→weeks.
  Narrative overlap from boundary weeks is acceptable; numeric totals use raw
  work-log's exact date range. Higher rollups never consume raw daily narratives.
  Date-range reads cap at today independently of command `--date`.

## RSS originals and selectors

- Capture the accumulated realized day's normalized collection **before** classify.
  `RenderResult` carries original capture and typed manifest reference; CLI publishes
  immutable originals before page planning and journals dependency guards with it.
  Limits: 4MiB/event, 64MiB/source, 256MiB per CLI-planned RSS set. Dry-run is memory-only.
- New binding is not historical proof about old pages/legacy logs. Page-only task
  behavior remains unchanged unless explicit event-aware policy is selected.
- Each concept from a manifest-bearing extraction requires an explicit article selection;
  empty extraction is valid. New selections come only from the request's provided manifest; accumulate links
  and declarations under the exact origin guard. Carried selections retain and
  verify their historical manifest even after current collection changes. Daily
  rerenders preserve declarations; omission is not retraction.
