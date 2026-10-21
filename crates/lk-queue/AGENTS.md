# lk-queue

Typed semantic requests/results and buffered JSONL persistence. CLI application,
classification and recovery live in `lk-cli/src/commands/queue*`; page publication
lives in `lk-vault`/`lk-pipeline`.

## Requests and cache identity (`lib.rs`)

- `LlmClient` is the pipeline seam: summarize, extract, themes, concept synthesis,
  flush, begin/rollback source. Classification is deterministic, not an LLM method.
  Backlinks-sync owns automatic citation-set derivation; explicit resynthesize and
  regenerate also use guarded queue publication. Source rollback must
  discard a failed plan's buffered requests. CLI producers drain requests with
  `take_requests` into the same recoverable mutation plan as their target pages;
  `flush` is only for targets already durably published.
- `TaskRequest` supplies locale, task input, cache hash and target through one
  builder. Every payload states vault locale and source-specific focus/context;
  concept discovery uses public resolve/search, not an embedded growing registry.
- `task_input()` contains worker context; `cache_identity()` only output-affecting
  fields. Keep their distinction and backwards-compatible serialization explicit:
  summary hashes text/max_sentences/locale/focus; themes text/max_themes/locale;
  extraction text/source_id/date/concept_policy/focus/sorted categories and optional RSS manifest.
  Concept synthesis uses shared `citation_digest`, not a second hash implementation.
- Locale changes rederive summary/themes, not accumulated concept extraction or
  authored synthesis. Target/heading/source_type are payload context, not cache
  identity. An already queued task keeps its enqueue locale; do not invent a
  cross-run pending-state cache to change that behavior implicitly.
- `TaskTarget.anchor` is the rendered heading, not a worker-inferred kind table.
  Synthesis optionally includes `related_anchor`: both sections share one input
  and marker. `QueueTask::anchors` must enumerate every owned section.
- `TargetKind::{page_format,llm_inputs_key,completion_key}` own page formats and
  marker names for every kind. Incompatible declared formats stay unreadable;
  matching markers never authorize their publication, pruning or cache reuse.
  Completion is matching `_done`, including empty results, never body fullness.
  Keep `tests/skill_contract.rs` exhaustive through `EnumIter`, not a hand list.

## Pending work (`queue.rs`)

- A pending task must target a published page. Under the queue claim,
  `new_requests` reuses only pending requests with equal kind/hash/full input/target;
  attempt ID/time do not matter. Processed history cannot suppress fresh work.
  Journal newly serialized requests after target edits in the same publication plan.
- Publish JSONL atomically through shared file primitives; preserve the buffer on
  I/O/lock/publication failure. `QueueIo` is fatal. Preserve malformed pending lines
  byte-for-byte and report them rather than reusing or discarding unknown work.
- Retained input is reconstructed as its typed request and its hash recomputed
  before classification, packet export, regeneration or apply. Matching markers
  cannot excuse malformed inputs or an unsupported policy.
- CLI status is authoritative: only `current` is work. Stale/missing targets are
  distinct from `done`; unreadable tasks remain for diagnosis. An unparseable line
  blocks its file rather than licensing partial destructive pruning. Prune retains
  answered history until the whole run can retire to `processed/`.
- Explicit regeneration journals the replaced pending request's exact bytes into
  processed history with the new request and completion invalidation. Preserve
  sibling lines verbatim; unknown historical policy cannot keep superseded work
  active after the new answer completes. A malformed run refuses replacement.
  Refuse another exact current generation request, ignoring only its
  `regenerated_from` provenance and the selected pending request being replaced;
  matching Done requests still pending must be pruned before another request;
  completed history does not prevent a deliberate later regeneration.

## Staged results and publication

- Workers write typed values only: section/synthesis JSON or extraction
  `TaskResult`. Never edit page sections or markers directly. Use the task's exact
  identity and the target revision captured before reading; do not relabel a stale
  answer. Synthesis carries both core and related bodies.
- Equivalent typed proposals for one task/input publish once. Competing current
  section, synthesis or extraction values remain unpublished and retain every
  result file for review; filename order never selects an answer.
- The CLI applier requires matching pending work, validated retained input/policy,
  exact target/input identity, every owned anchor and current source dependencies.
  Bodies and completion publish together. Extraction concept/origin edits share a
  recoverable mutation plan. Lock order is vault mutation session then queue claim;
  external editors do not participate.
- Extraction requires retained validated input; an orphan result cannot supply its
  missing read scope. Permanent malformed results retain bytes plus diagnostics in
  quarantine; actual I/O failures remain retryable errors. Do not make one corrupt
  result strand all valid answers or silently delete it.
- Concept extraction carries a captured `context_revision`, explicit discovery
  queries and per-concept `new` or revision-pinned `reuse` admission. Discovery
  supplies candidates, never semantic identity. New owners require nonblank
  source-grounded synthesis; actual support still requires semantic review.
  Synthesis also pins its full source/review context. Changed context refuses
  publication rather than relabeling an old answer.

## RSS evidence policy

A provided bound manifest selects `concept-extraction-rss-v3` and enters cache
identity; selectors select `concept-synthesis-rss-v2`. Page-only extraction uses
`concept-extraction-v2`. Typed validation refuses malformed or downgraded requests.
Every extraction explicitly carries concept_policy through its request, hash and result.
Disabled extraction requests are invalid; existing-only cannot create a new owner.
Selections may name only provided immutable event/payload pairs; omission does not
retract prior evidence. Original membership/content guards remain necessary even
when a daily page's interpretation changed independently.

`NoopLlmClient` returns empty values. `MockLlmClient` is `test-util` only, never a
release provider. Provider selection belongs to CLI `build_llm_client`.
