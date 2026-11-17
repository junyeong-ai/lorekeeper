---
paths:
  - "crates/lk-cli/src/commands/work.rs"
  - "crates/lk-cli/src/commands/work/discovery.rs"
  - "crates/lk-cli/src/commands/work/discovery/**"
  - "crates/lk-cli/src/commands/work/resources.rs"
  - "crates/lk-cli/src/commands/work/selection.rs"
  - "crates/lk-cli/src/commands/work/selection/**"
  - "crates/lk-cli/src/commands/work/publication.rs"
  - "crates/lk-cli/src/commands/queue/rejections.rs"
  - "crates/lk-cli/src/commands/execution/preparation.rs"
  - "crates/lk-cli/src/commands/run/native/preparation.rs"
---

## Progressive extraction and work cost

- Each discovery query rosters its identity matches and at most
  `execution.work.max_query_candidates` other pages through the `Ranked` capture recipe
  (crates/lk-vault/AGENTS.md owns roster membership). That roster is the inventory
  progressive extraction screens. `work cost`, ready `work context` and every
  extraction finalizer request disclose per query how many pages matched and how many
  are rostered; unrostered matches are unexamined, not judged.
  `full-context` extraction keeps the complete match set.
- Progressive extraction separates complete source observations, candidate-card screening
  and admitted full-body evidence. A screening decision belongs to one candidate. Its
  key binds the screening contract (reply schema, instruction and the screening profile's
  model identity: engine, model, effort and the Codex `profile`), the exact source the
  screener reads and the exact row shown: public path, matched queries by value and the
  card without locators.
  Packing, order, byte limits, target and configuration revisions and other candidates
  never enter a key, so an unchanged card under an unchanged source keeps its decision
  while the roster around it changes. A packet is only transport for undecided rows in
  owner order; screeners judge each card independently of the others.
- `work/selection.rs` owns `.lorekeeper/queue/screening/<digest(task_id, cache_hash)>/`:
  immutable `packets/` (`screening-packet-v1`: request and decisions), `ledger.json`
  (`screening-ledger-v1`: key to packet row, including keys that left the roster),
  immutable `rosters/` (`screening-roster-v1`) and `rejections.json`
  (`screening-rejections-v1`); keys hash `screening-contract-v1`. Issuing a packet writes
  nothing. Recording its decisions journals the packet and ledger in one plan guarded by
  the ledger bytes it read and the pending task identity, never by candidate pages;
  sealing journals the roster. Stored state decodes only from canonical bytes; corrupt,
  ambiguous or foreign state remains fatal.
  The retired packet-selection store `.lorekeeper/queue/selection/` is never read or
  imported; maintenance removes it (see `cli-task-maintenance.md`).
- A roster is completely decided when no row is unknown or `defer`; anything less blocks
  completion, and card-only rejection is not a full-body assessment. A decision other
  than `defer` is final for its key. When the only undecided rows are `defer`, that
  target alone is held until `work select --reassess` issues those rows with their
  `previous` reference; fresh validated state must not become a missing-request failure
  or repeat paid screening. Continue undecided rows even when earlier cards defer.
- `rejections.json` keeps the latest semantic hold (rejected reference coverage or
  reasons), matched by request revision, and the latest operational hold (failed
  invocation), matched by the request's contract-independent input revision so it
  outlives contract changes. Either hold records no decisions, defers only that task at
  that packet and spends no call on the unchanged request; undecided candidates remain
  incomplete and independent targets continue. A hold no longer applies once the
  undecided rows change. `work select-retry --task-id ID` clears only the current
  packet's operational hold, guarded by the ledger it read; `--dry-run` writes nothing.
  Standalone `select-record` answers the current request for undecided rows, or for
  deferred rows under reassessment, by its exact request revision.
- Extraction evidence Work binds only what it reads: the source, the selected owners,
  their scoped reviews and an observed source's observations. Its context states
  `extraction_scope.policy` and `selected`; recipes, the roster and unread candidates
  stay outside it. Candidate churn that leaves the selected set unchanged neither
  refuses recorded inspections nor replans the Work, and reading that evidence captures
  no discovery. Rust binds the current completely decided roster where a result is
  assembled, before a finalizer call and again after it, and writes the result's
  `discovery` and `selection` itself. A roster that cannot be formed (screening capacity,
  a blocked or incomplete source), is not completely decided, or admits different
  evidence at either point is changed scope, never a retagged result; other roster churn
  rebinds.
- A result binds one sealed roster: the exact rows of a completely decided roster.
  `work select` seals it before a manual session reads evidence and result completion
  seals it with the result; `work context` is ready only for a sealed current roster.
  Publication verifies every sealed row through the ledger and guards the source, the
  selected owners, their scoped reviews and the stored roster, ledger and packets. It
  captures no discovery: a candidate that was not selected stays bound to the card it
  was judged from. Reused owners must be selected and read in full. Application refuses
  a result under the retired `progressive-evidence-v1` policy (lk-queue still decodes it)
  with a typed request to regenerate. Retention keeps a task's whole screening store
  while the task is pending or has a staged result and removes it together otherwise.
- Source work never finishes extraction independently: canonical extraction publication
  journals its exact supporting-consumption receipt with the knowledge edits. Retention
  validates that receipt and preserves live queue/reuse references before removing
  expired source work.
- `work cost` reports the roster's candidates, the packets its undecided candidates
  still need and compact JSON bytes without persisting anything or reading candidate
  bodies through a host. Optional positive `max_screening_batches` bounds those remaining
  packets and defers a larger demand before new screening calls; a completely decided
  roster remains usable. No clipping, implicit recipe rewrite, token/price claim or
  completion from capacity admission. Unknown packet costs await source observations.
  One candidate that cannot fit a packet alone is a typed byte-capacity failure that
  defers only its target; malformed inputs and corrupt stored state remain integrity
  failures.
- A staged proposal's rejection scope is what deterministic application reads. For a
  result bound to a sealed roster that is the source, the selected owners and the stored
  roster and ledger; a typed refusal keeps the source and that stored state, so the
  proposal stays held until one of them changes. Results without `selection` keep the
  recorded-recipe scope.
