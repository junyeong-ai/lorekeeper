---
paths:
  - "crates/lk-cli/src/commands/work.rs"
  - "crates/lk-cli/src/commands/work/discovery.rs"
  - "crates/lk-cli/src/commands/work/discovery/**"
  - "crates/lk-cli/src/commands/work/resources.rs"
  - "crates/lk-cli/src/commands/work/selection.rs"
  - "crates/lk-cli/src/commands/work/selection/**"
  - "crates/lk-cli/src/commands/execution/preparation.rs"
  - "crates/lk-cli/src/commands/run/native/preparation.rs"
---

## Progressive extraction and work cost

- Each discovery query contributes its identity matches and at most
  `execution.work.max_query_candidates` other pages in search order; the task's target
  is never its own candidate. That ranked roster is the inventory progressive extraction
  screens. Ranks never enter cards, revisions or keys. `work cost`, ready `work context`
  and every extraction finalizer request disclose per query how many pages matched and
  how many are rostered; unrostered matches are unexamined, not judged.
  `full-context` extraction keeps the complete match set.
- Progressive extraction separates complete source observations, candidate-card screening
  and admitted full-body evidence. A screening decision belongs to one candidate. Its
  key binds the screening contract (reply schema, instruction and the screening
  profile's engine, model and effort), the exact source the screener reads and the exact
  row shown: public path, matched queries by value and the card without locators.
  Packing, order, byte limits, target and configuration revisions and other candidates
  never enter a key, so an unchanged card under an unchanged source keeps its decision
  while the roster around it changes. A packet is only transport for undecided rows in
  owner order; screeners judge each card independently of the others.
- `.lorekeeper/queue/screening/<digest(task_id, cache_hash)>/` holds immutable `packets/`
  (request and decisions), `ledger.json` (key to packet row, including keys that left
  the roster), immutable `rosters/` and `rejections.json`. Nothing is written before a
  host call. Recording journals the packet and ledger in one plan guarded by the ledger
  bytes it read and the pending task identity, never by candidate pages. Stored state
  decodes only from canonical bytes; corrupt, ambiguous or foreign state remains fatal.
  The earlier `.lorekeeper/queue/selection/` store is never read or imported;
  maintenance retires it (see `cli-task-maintenance.md`).
- Unknown or deferred decisions block completion, and card-only rejection is not a
  full-body assessment. A decision other than `defer` is final for its key. A fully
  decided roster with deferred cards holds only that target until `work select
  --reassess` issues those rows with their `previous` reference; fresh validated state
  must not become a missing-request failure or repeat paid screening. Continue undecided
  rows even when earlier cards defer.
- Extraction evidence Work binds only what it reads: the source, the selected owners,
  their scoped reviews and an observed source's observations. Its context states
  `extraction_scope.policy` and `selected`; recipes, the roster and unread candidates
  stay outside it. Candidate churn that leaves the selected set unchanged neither
  refuses recorded inspections nor replans the Work, and reading that evidence captures
  no discovery. Rust binds the current completely decided roster where a result is
  assembled, before a finalizer call and again after it, and writes the result's
  `discovery` and `selection` itself. A roster that is undecided or admits other
  evidence at either point is changed scope, never a retagged result.
- A result binds one sealed roster: the exact rows of a completely decided roster.
  `work select` seals it before a manual session reads evidence and result completion
  seals it with the result; `work context` is ready only for a sealed current roster.
  Publication verifies every sealed row through the ledger and guards the source, the
  selected owners, their scoped reviews and the stored roster, ledger and packets. It
  captures no discovery: a candidate that was not selected stays bound to the card it
  was judged from. Reused owners must be selected and read in full. A result under the
  retired `progressive-evidence-v1` policy is recognized and refused with a typed
  request to regenerate. Retention keeps a task's whole screening store while the task
  is pending or has a staged result and removes it together otherwise.
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
