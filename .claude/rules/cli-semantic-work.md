---
paths:
  - "crates/lk-cli/src/commands/work.rs"
  - "crates/lk-cli/src/commands/work/**"
---

## Resumable semantic Work (`commands/work.rs`)

- `commands/work.rs` binds retained input and audit dependencies to resumable derived
  work. Oversized requests use a fresh configured agent per inspection/reduction and a
  bounded finalizer. Unplanned whole-input summarize/refine/themes use one tool-free
  section request, including scoped reviews and generation rules; other small requests
  retain ordinary batching.
- Bounded section finalizers use the same structured tool-result host as extraction
  and synthesis for necessary targeted source/review reads. Keep inspection workers
  and whole-input section calls tool-free. `source_read` only exposes this exact
  Work's planned resources; it neither replaces fresh guards nor creates observations.
  Event refinement observations retain separate event claims, not a thematic digest.
- Complete-envelope capacity defers only that target without clipping or rewriting paid
  Work. `execution.work` limits serialized request/result bytes and agent calls per
  native batch. Scheduled invocations share that call allowance across batches. Exhaustion retains unfinished work and
  fails explicitly, without clipping evidence.
  Fresh planning capacity reports a minimum byte requirement; completed request capacity
  reports measured bytes. Keep both local to their admitted target. Stored corruption,
  invalid evidence and I/O remain strict; shared preparation preserves independent work.
- Resume current explicit Work plans even when the remaining batch fits whole input;
  validate their exact manifest before using recorded observations.
- Context pages (selected owners, discovery matches, audit related pages) follow one
  `ContextReading` per preparation (`work/resources.rs`). An audit Work reads only the
  candidate's bounded `related_pages` as resources. Its `context_cards` stay outside the
  Work, its context and its inspections: only the assessment request shows them
  (`work/audit.rs`), by address and title with authority text that they were not read,
  within their own byte allowance and the room the complete request leaves, counting the
  rest as omitted, so cards never displace evidence or defer a subject. New Work reads
  context pages as `lk_core::concept::semantic_text`: parsed frontmatter without `BOOKKEEPING_FIELDS`, so a
  citation or task-state update leaves the Work current whatever its YAML spelling; an
  authored change does not, and an unparseable page keeps its exact bytes. Work planned
  before 0.144.0 read exact bytes. `expected()` decides once which Work a preparation
  continues and in which reading: a stored semantic Work, then a stored exact-bytes Work
  while every page it read keeps those bytes, otherwise a new semantic plan. A reading over
  the input bound has no stored Work and leaves the choice to the other. Named Work, drafted
  queue results and audit publication are checked against that one choice, made before page
  snapshots leave the preparation, so Work that can run can publish; convert a fresh capture
  with `read_as` before comparing it (roster admission). A stored Work continues through the
  Work protocol whatever its part count. 0.144.0's line projection is not reconstructed: its
  Work replans, and a hold bound only to an input revision follows the semantic reading.
- A synthesis request reads its context-only pages (direct links and ranked discovery
  matches no subject, citation or review evidence also reads), and a screened extraction
  its selected owners, through its context snapshot
  (`work/snapshot.rs`, `.lorekeeper/queue/context/`): the exact page bytes its Work first
  read, recorded in the journal that persists its Work and guarded wherever the request is
  staged or published. A snapshotted page is read as it is now except a section whose
  current body the page records as Lore's own write (`llm_inputs.generated_sections`,
  `lk_vault::record_generated_sections`: each heading with the digest of the body written
  under it), which keeps its recorded bytes in the current line ending, gaining a line break
  only where it would otherwise run into the current text around it, so a page only Lore
  rewrote composes to its recorded bytes. Concept synthesis publication records the
  synthesis and related sections it writes, and backlinks reconciliation the sources section
  it rewrites; a concept rerender writes the record of exactly the bodies it splices back
  that still match it: its template never receives the record, and whatever a template
  emits under that field is cleared. Nothing else records, section results included: a document render rebuilds its `llm_inputs`, so its
  summary and related concepts are read as they are now, as is a body edited after Lore
  wrote it and every other section. A neighbor's resynthesis therefore does not replan the
  request, which would otherwise rarely finish while its neighbors keep changing, while an
  authored correction (frontmatter, authored prose or an edit to a body Lore wrote) is read
  at once and recorded. A request that never persisted Work has no snapshot and reads its
  pages as they are now; a published synthesis is not refreshed when a neighbor later
  changes. Membership still binds: a page that joins the context is read as it is now, one
  that leaves is dropped at the next record. Live bytes stay in `Prepared.pages`, so
  publication still guards every page as it is. A reused extraction owner is published
  against two pins: its live file revision, which Rust takes at assembly from the capture
  whose reading matched the Work, and the result's context revision, which publication
  compares with the owner as read now, so an authored change to the owner's identity or
  scope after planning needs a fresh judgment. Audit related pages read current bytes:
  audit currency has its own contract.
- A newly persisted Work reuses, through the attributed reuse path and its page guards,
  the observation of each unrefined inspection whose content revision (partition policy,
  semantic purpose and exact assigned evidence) another stored Work recorded fresh.
  The semantic purpose is the Work context without its subject and without the inventory
  of its other resources (selected owners, scoped review set, audit screening state): an
  inspection reads only its assigned ranges, so a changed roster around identical evidence
  keeps the observation. `.lorekeeper/cache/observations/` maps a content revision to that
  origin; only fresh unrefined inspection records enter it, inside their own journal. A
  missing origin is a miss. The index is disposable: maintenance rebuilds it from stored
  Work and drops entries whose origin is gone. Existing Works, reductions and refined
  nodes are never seeded. The partition policy stays in the address because the inspect
  input names it (`input.policy`): the same range under two policies is a different
  request, so plans under different policies share no observation. A Work planned under
  byte-filled parts (`semantic-ranges-v2`) continues under its stored plan while its reads
  are unchanged (`tests/fixtures/audit-work-0.163.1`); once they change it replans as
  `semantic-ranges-v3` and pays its inspections again.
- `work source` pages one planned resource from the Work's stored plan: `--resource`
  selects it by roster index, and its default is the unique retained task input. Only that
  resource is read, from its current bytes (the task's retained input, or the page read
  exactly, semantically or through the request's context snapshot), and it is served only
  when its text digest is the plan's; any other change refuses as changed context. The
  Work's scope is not rebuilt, so a page read is no completion or currency proof:
  publication guards the owned inputs. A queue
  Work's native finalizer can therefore reread a cited page or selected owner that a lossy
  observation summarized, instead of only refusing; audit requests carry no `source_read`. Literal seeks identify text
  positions, never events. Bound complete page JSON and
  scalar counts, report continuation, and never record coverage/completion or recover
  journals. Native `source_read` metadata supplies this capability without source bodies.
- Fresh source inspection uses the shared scalar workload policy (`semantic-ranges-v3`
  plans with `max_part_characters`) as well as complete JSON byte admission. Apply it to
  Work partitioning, whole-input task batches and raw screening context. A v3 part holds
  whole pages, and a page no part holds whole is split across parts of its own. Pages fill
  parts in groups that close after a page selected by its address digest with probability
  proportional to its size (content-defined, as `lk_core::semantic` states), so an edit to
  one page changes only the parts of its group, and of the next group when the edit
  changes whether that page closes its own; inspections elsewhere keep their part
  identity and reuse their observations. Exact retained byte-only `semantic-ranges-v1` and
  byte-filled `semantic-ranges-v2` plans keep their policy (`work::planned` tries each);
  persist the selected manifest, never rebuild it with a newer default.
- On `semantic-ranges-v1` Work, a saved inspection/reduction action stays held for
  automatic admission across native-contract changes only when its original canonical
  input revision still hashes the complete current request. Verify the whole request
  rather than trusting its supplied revision field; keep the original receipt bytes.
  This hold never authorizes refinement: refinement still requires the exact current
  request and native contract. Actions on `semantic-ranges-v2` and `semantic-ranges-v3`
  Work match the exact contract.
- Only freshly received unambiguous authored-text or evidence-reference violations can
  create native observation-rejection holds. Validate stored state, current identity
  and question inheritance first; canonical node ranges own evidence membership, not
  the displayed citation roster. Use one fresh evaluation to choose recording or
  rejection. Guard exact requests, contracts and captured responses; stored corruption
  and unknown failures remain strict. Preserve old paid bytes and exact v1 receipt
  snapshots when adding v2 evidence entries. Bound append-only history and keep
  unchanged holds zero-call.
- Saved section finalizers capture fresh scoped reviews separately from source-only
  Work; guard the complete final request after the host and its context again at apply.
  Do not incorporate reviews into old Work identities or rewrite paid nodes to acquire
  that scope.
- Scoped section completion and restaging share the same fresh review-context check.
  Native recovery withdraws only a singleton, current, otherwise valid section proposal
  with an exact fresh semantic-context rejection. Journal its proposal, rejection and
  prior completion bytes before withdrawal, under vault then queue claims. Prefix task
  and scope guards for replay. Admit one withdrawal per task/input/current context;
  unchanged retries remain held. Published completions, legacy plans, unscoped results,
  conflicting or invalid proposals and semantic actions never gain recovery authority.
  Retain recovery receipts with pending, staged and processed queue references.
- New work fixes reduction topology from the exact envelope plus each child's authored
  result allowance; every child and its evidence mapping participates. A reduction can
  retain unchanged inherited questions in a self-contained versioned node snapshot. Its
  authored result and position metadata obey the result bound; the snapshot obeys the
  input bound. Full reconstructed questions participate in every parent and finalizer
  input. Actual complete-input overflow defers that Work before invocation, never clips
  or replans it. Persist the policy and preserve missing-policy binary plans, node IDs
  and seals without rewriting them on resume.
- Explicit inspection refinement for queue and audit Work requires a current exact processing action. Keep
  the canonical plan, prior positive outputs and original action bytes unchanged.
  Capture its bounded sidecar and original-action revision with the expanded tree; guard
  that snapshot or absence through recording, completion and historical retention. New
  records bind refinement provenance and compose with retained-question snapshots. Use
  the existing byte-budgeted reduction owner, preserve complete original coverage, and
  retain legacy audit v1 and queue v2 sidecars without converting paid bytes;
  earlier readers refuse unsupported formats. Refuse refined cross-Work reuse without its complete
  closure. Never infer splits from action prose or retry unchanged holds automatically.
- New-policy original Inspect alone may return `needs-smaller-inspection`. Retain its
  exact request/contract/captured response and one strictly smaller expansion atomically
  using v3 sidecars; keep old original-action files unchanged. Children and Reduce cannot
  request automatic refinement. Expansion input capacity retains the typed hold; unknown
  errors stay strict. A refined outcome is durable action progress, not a paid observation,
  and must preserve the same call/parallel budget, cancellation and fresh guard boundaries.
- Native queue extraction/synthesis may finalize a disjoint current frontier covering
  every planned inspection when all observations, evidence and rules fit the existing
  input limit and no processing blocker remains. Its distinct frontier seal never marks
  a missing root complete or rewrites paid nodes. Missing/stale/oversized/blocked
  coverage retains normal reduction; source-only work, sections and audits are excluded.
  Root/frontier model views share one complete resource guard table and exact cited
  indices. Preserve every guard value and observation; the canonical completion and
  its input revision stay in Rust, separate from the projected host request.
  An exact frontier-finalizer hold also retains normal Reduce admission; do not block
  its whole Work by identity. Keep the next node's own exact hold and capacity checks.
  Distinguish discovery recovery: its current hold still defers the whole target.
  Carry the same captured expanded tree through frontier coverage, sealing, publication
  and historical retention, including every explicit refinement child.
- Replacing an active finalization action journals its exact old bytes in the validated
  history sidecar. Preserve historical matching and reference-aware retention together.
  Revalidate the exact full request and completion proof after host execution and at
  apply. Corrected observations require a newly generated result; preserve old
  proposals. Every sealing path validates the captured prior seal/final pair and
  publication receipt or absence, including same-kind updates. Published authority and
  result are immutable; exact retries preserve original bytes. Unpublished replacement
  journals the prior final bytes and publication absence in the same mutation.
- Publication or source consumption closes observation writes. The shared record owner
  validates that exact completion proof before refusing changes, including reuse
  destinations and late native results. Equal content, origin and refinement may retry
  without rewriting the captured node bytes; preserve its raw snapshot guard. Unpublished
  amendments remain valid. Guard closure receipt bytes or absence through apply/recovery;
  structural completion alone never closes a Work. A published frontier is closed even
  when its unused root reduction is unfinished. Historical inconsistent state refuses
  rather than being repaired or given a new identity.
- Completion requires exact sealed typed results and source/work guards in the canonical
  publication journal. Keep the queue target's guard in its publication edit: a separate
  unchanged guard for the same target would reject the journal's own recovery.
- Load canonical final/staged results through direct-file and vault-address checks before
  reading, including existing destinations during finalization. Never overwrite a staged
  result unrelated to the requested result or a captured unpublished audit completion.
  Validate unique JSON fields before completion hashing, restaging, authority replacement
  or retention, using the same captured raw bytes for mutation guards.
- Prepare ready native nodes and their progress from one captured evaluation. Refresh
  the Work's own scope after the host returns before recording. Audit and full-context
  extraction Work read their discovery context, so an added match replans them (a
  screened audit once its screening reopens) and refuse a result of the earlier reads,
  while an audit's receipt stays current, because context is outside its currency key
  (crates/lk-graph/AGENTS.md); the replanned Work reuses every unchanged inspection;
  screened extraction evidence reads only its source and selected owners, so candidate
  churn does not. Never retain a candidate capture across host execution.
- Queue preparation checks the next exact evidence request's current processing action
  in that checkpoint, after reusable root/frontier completion. A later held node cannot
  suppress an earlier ready request. Keep the pre-host recheck for changed state; do not
  replace complete preparation with a Work-ID hold filter.
- Workload is a presentation projection of the same validated ranges and guards. Keep
  Unicode-scalar inspection counts and authority groups separate from request bytes;
  reducers count child observations. Paginate status workloads with ready IDs. Native
  telemetry uses the actual prepared request, never another discovery capture. Refinement
  previews count observation nodes before/after, including the retained parent; these are
  not token, billing, readability or completion estimates. Never add these projections to
  canonical plans, model inputs or request/contract hashes, or infer a split from them.
