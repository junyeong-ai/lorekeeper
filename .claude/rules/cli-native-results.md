---
paths:
  - "crates/lk-cli/src/commands/work.rs"
  - "crates/lk-cli/src/commands/work/state.rs"
  - "crates/lk-cli/src/commands/work/state/**"
  - "crates/lk-cli/src/commands/work/native.rs"
  - "crates/lk-cli/src/commands/work/section.rs"
  - "crates/lk-cli/src/commands/work/generation.rs"
  - "crates/lk-cli/src/commands/work/generation/**"
  - "crates/lk-cli/src/commands/work/audit.rs"
  - "crates/lk-cli/src/commands/work/preparation.rs"
  - "crates/lk-cli/src/commands/execution/observation.rs"
  - "crates/lk-cli/src/commands/execution/observation/**"
  - "crates/lk-cli/src/commands/execution/finalization.rs"
  - "crates/lk-cli/src/commands/execution/result.rs"
  - "crates/lk-cli/src/commands/execution/audit.rs"
---

## Native inference roles (`execution/observation.rs`, `execution/finalization.rs`)

- Native extraction/audit discovery and audit candidate screening return typed semantic
  values through read-only structured hosts. Rust supplies subject/packet identities,
  revalidates current inputs and journals the recipes/decisions. Workers never publish
  their own receipts. Preserve revision-pinned subject reads: title-only selection is
  insufficient. Keep recipes registered during a host read, defer changed packet scope,
  and count only newly recorded recipes/packets as retained progress.
  The host adapter owns file-reading tools: Claude uses Read and Codex uses shell reads
  in its read-only sandbox. Role requests specify scope without prohibiting that read
  path. These readers never invoke Lore commands or publish their own results.
  Discovery replies cover every assigned subject with complete queries or an explicit
  needs-action; audit screening returns complete decisions or needs-action. Prepare a
  fitting prefix from exact input bytes and minimum complete-reply bytes, retaining
  all remaining subjects. This wire feasibility bound supplies no lexical queries.
  Confirmed timeouts/numeric exits and invalid remote responses from read-only hosts
  retain input-bound operational attempts; local transport I/O/kind, signal/cleanup,
  unknown errors and unusable tool-enabled replies stay fatal. Five closed preparation
  owners are extraction discovery, audit discovery, audit screening, whole sections and
  audit assessments; the last holds only a tool-enabled whole or bounded assessment's
  confirmed timeout/numeric exit after completed process-group cleanup. Their
  latest per-parent attempts stay separate from semantic actions and paid decisions;
  unchanged input holds survive instruction-contract changes. Only semantic needs-action
  earns action progress; an all-failed invocation wave ends this run without an operation
  pause. Queue pruning retains pending task/cache attempts; maintenance removes audit
  attempts only for absent subjects under workflow coordination.
  `work retry-preparation` requires its stage, task/path selector and exact reported input
  revision. It removes only that retained attempt under current task/subject guards;
  dry runs write/recover nothing. Clearing metadata never invokes a host, grants
  refinement or replaces the full ordinary admission before a later call.
- Native inspection/reduction and screening are structured inference roles
  (`execution/observation.rs`); bounded single-section finalization uses
  `execution/finalization.rs`. Rust selects applicable shared generation rules through
  typed task/target kinds. Extraction/synthesis use tool-enabled final-schema responses;
  bounded audits return structured judgments with supported review/repair tools. Native Rust assembles
  TaskResult/SynthesisResult/assessment metadata from the exact prepared scope, reserves envelope
  bytes and owns finish/staging. Models return semantic values, needs-action, or
  extraction-only needs-discovery with explicit DiscoveryQuery values.
- Section instructions follow the typed input form: whole-input requests read retained
  resources directly; bounded finalizers compose completed observations and their evidence
  mapping. Neither representation may claim the other is supplied. Both retain complete
  input bounds, scoped review requirements and native publication ownership.
- Discovery recovery journals the legacy recipe and native request-bound sidecar
  together under vault→queue claims. Preserve exact current Source Work; changed
  knowledge scope replans without retagging paid nodes. Recovery is incomplete; exact
  repeated requests/cycles defer before another host. Retention protects pending task
  identities and validates recovery sidecars before removing them. Never interpret old
  reason text as a recovery query. Do not couple output schema to tool prohibition,
  scrape events/prose or mutate saved Work to adopt a reply.
- Selected action receipts bind the actual batch request. Reconsider action-bearing
  tasks individually after request changes, then suppress unchanged singleton actions;
  never substitute a reconstructed singleton for the original batch's identity.
- Tool-free observation, screening and section roles carry the bounded request through
  stdin and decode bounded stdout. Rust records the selected observation or creates the
  guarded section result. In those roles Claude disables tools/customizations; Codex
  uses a read-only sandbox. Never fall back to parsing prose/Markdown when structured
  output is missing or invalid.
- Decode worker objects, Work manifests/finalizer inputs/results and queue rejection
  receipts from their original JSON with unique fields at every nesting level. Reject
  duplicate decoded keys before host status checks, semantic assembly or cleanup;
  converting to a generic value first
  erases that ambiguity. Malformed remote replies never become semantic values;
  tool-free roles may retain an exact operational failure. Corrupt stored state refuses.
- Evidence observers return only a typed observation body or processing-action reason.
  Rust binds native work/node/input and screening selection/batch/input identities from
  the supervised invocation; reject model-supplied identity fields without adopting or
  repairing them. Fixed schemas and exact reply allowances reserve stored identity
  bytes.
- Native screening assigns a short `candidate_ref` to each exact card. Validate
  complete, unique reference coverage before mapping decisions to canonical paths; reply
  order is irrelevant. Canonical requests retain full owner/source revisions while the
  native presentation leaves those opaque guard hashes to Rust. Reserve path expansion
  in the reply allowance. Reference-coverage or blank/oversized decision-reason
  rejection journals an exact request/contract-bound receipt, records no decisions and
  defers only that task; unchanged requests spend no calls. Reasons obey compact UTF-8
  JSON string bytes, including quotes/escapes, not character count. Read existing
  screening-rejections-v1/v2 without rewriting them; new receipts use strict v3 with
  separate semantic dispositions and per-packet operational attempts. Earlier readers
  refuse v3. Semantic rejections match their request/contract; operational failures
  match the canonical packet across contract changes. Confirmed numeric nonzero exits
  and unusable tool-free replies hold the selection at that packet; unknown candidates
  remain incomplete and independent targets continue. Successful current receipts take
  precedence. `work select-retry --task-id ID --batch-id BATCH` clears only its current
  operational attempt, with fresh scope/page guards; `--dry-run` writes nothing. Failures
  earn no semantic progress; failed-only waves end this run without an operation pause.
  Corrupt receipts, signal/cleanup errors and failed fresh scope validation stay fatal.
  Standalone full-path `select-record`,
  selection identities and existing paid decisions remain unchanged.
- Native extraction reuse replies select a task-local `owner_ref` from its complete
  admitted-owner roster. Rust expands it to the unchanged canonical path/file-revision
  contract. Progressive membership is explicit, including an empty selection; legacy
  admission requires Context authority. Review-only and unselected pages grant no reuse.
  Reserve per-occurrence expansion bytes, including repeated references, while retaining
  a fitting empty extraction. Fresh request equality precedes expansion.
  An unknown reference records a typed operational rejection for that exact task/input,
  with actual request identity and bounded reply evidence. It publishes no semantic
  result, earns no action progress and does not pause independent work. Unchanged input
  holds survive instruction changes. The same per-task receipt holds a confirmed
  timeout/numeric exit of a tool-enabled selected, bounded result or bounded section
  finalizer for its exact input. Receipts are strict native-generation-rejection-v2 with a
  typed proposal or invocation cause; v1 proposal receipts are read without rewriting and
  earlier readers refuse v2. `work retry-generation` requires the exact current
  task/input; dry runs never recover or write, and clearing a receipt invokes no host.
  Pending tasks protect receipts during pruning; corrupt state and transport failures
  remain fatal.
- Native new questions contain only semantic text/evidence; Rust supplies IDs and
  retains inherited active questions unchanged unless an exact local question key is
  explicitly resolved. Native child question tuples separate active keys from closed
  resolution history without expanding canonical inputs. Bind action receipts to the
  exact delivered presentation as well as schema/instructions. Keep canonical Work
  requests, node input revisions and standalone record formats unchanged. New node
  records use `work-output-retained-questions-v1` only when needed: preserve exact
  question kinds, IDs, payloads and ordering, and digest the reconstructed full output.
  Older readers refuse the new record; keep legacy bytes, seals and paid observations
  untouched. Retained-question instructions apply only to reductions with active
  inherited questions. Bind node-role instructions to the native contract without
  changing canonical input revisions. Reserve the native retained-position envelope
  before the host call; reject actual assembled overflow.
- Fresh owner/currentness validation remains authoritative. Native range/text mapping is
  already verified; workers can copy assigned ranges without inventing finer offsets.
  Every observation concern still needs evidence. Actions record no observation,
  discharge no concerns and defer only their Work. Bind the receipt to the complete
  request and response/instruction contract; skip unchanged calls, revalidate before
  recording and preserve independent parallel results. A valid decoded evidence output
  above its allowance records a typed exact capacity rejection through the same fresh
  Work/node guards. It supplies no observation, prevents unchanged paid retries and
  preserves independent progress. Transport errors, malformed responses and corrupt
  state remain failures; never truncate the reply.
- Deferred audit subjects remain unassessed; verify receipts only for the other selected
  subjects and retain an incomplete overall result. Corrupt or unknown-scope state
  stops.
  Validate scoped review obligations before sealing a native assessment. A remaining
  needs-reassessment review or follow-up without a current open review becomes an
  exact subject action; independent subjects continue without pausing the operation.
  For existing sealed results, verify completion and staged-result integrity first,
  then derive the same zero-call hold without rewriting retained Work. Missing,
  invalid or unreadable dependencies remain fatal, even alongside a stale review.
- A section `needs-action` response records an exact request-bound action receipt;
  unchanged requests defer before invocation, while independent work continues. Changed
  requests can be reconsidered; valid sealed results restage before checking the
  receipt. Corrupt receipts refuse. Retention validates stored receipt identity without
  requiring historical generation rules to match current ones. Interactive finalizers
  retain their explicit repair and guarded submission capabilities.
- Whole-section action receipts bind exact task/input, captured scope and complete
  request; unchanged actions defer before invocation. Scoped SectionResult values retain
  an optional context revision, revalidated against current reviews at apply. Omission
  preserves the legacy section wire contract. Pending identities protect actions during
  queue pruning.
- Prepared native selections/finalizers supply exact metadata, admitted evidence/root
  and complete shared contracts through supervised stdin, within the complete serialized
  JSON input bound. Ordinary interactive roles retain their argument prompt. Legacy
  prepared requests and standalone consumers may use private files with bounded
  continuation reads; one tool display is not a complete-read requirement. No transport
  authorizes changed scope or certifies a read. Tool-enabled native results are decoded
  only from Claude terminal structured_output or Codex explicit final-message output;
  complete semantic and assembled-result bounds stay separate. Fresh request/root
  comparison precedes sealing/staging. Native results use the shared Markdown body
  validator before staging. Structurally invalid decoded bodies retain exact
  request-bound actions; malformed responses remain failures. Native workers do not run
  work finish/apply. Standalone record/result contracts, inherited prepared finish pins
  and saved Work remain unchanged. Exact request-bound action receipts suppress
  unchanged native calls; unknown/corrupt state refuses and pending identities protect
  receipts from pruning.
