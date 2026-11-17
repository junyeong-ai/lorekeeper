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
  is never its own candidate. That ranked roster is the screened and guarded inventory.
  Ranks never enter cards, revisions or keys. `work cost` discloses per query how many
  pages matched and how many are rostered; unrostered matches are unexamined, not judged.
  `full-context` extraction keeps the complete match set.
- Progressive extraction separates complete source observations, bounded candidate-card
  screening and admitted full-body evidence. All candidate and scoped-review guards
  remain pinned; unknown or deferred decisions block completion, and card-only rejection
  is not a full-body assessment. Continue unrecorded packets even when earlier cards
  defer.
- A fully recorded ledger with deferred cards holds only that target for explicit
  reassessment; fresh validated state must not become a missing-request failure or
  repeat paid screening. Corrupt ledgers remain fatal. Reused owners must be selected
  and read in full. New computed selections may reuse only an entire equal semantic
  packet under fresh complete inventory/ownership/review guards. Bind ordered cards,
  source context, queries, limits and resolved screening contract; opaque closure
  identities are not semantic input. Preserve saved legacy layout/receipts/contracts.
- Reserve full direct-origin provenance inside the result bound. Validate and guard
  direct same-owner origins at every current use; known reassessment holds dependent
  packets without calls, while missing/corrupt/indirect origins refuse. The latest
  computed-selection head is only a bounded lookup hint. Retention protects all proofs
  for pending/staged owners and removes completed owners together.
- Source work never finishes extraction independently: canonical extraction publication
  journals its exact supporting-consumption receipt with the knowledge edits. Retention
  validates that receipt and preserves live queue/reuse references before removing
  expired source work.
- `work cost` reports complete candidate/packet counts and compact JSON bytes without
  persisting plans or reading candidate bodies through a host. Optional positive
  `max_screening_batches` defers oversized incomplete ledgers before new screening
  calls; complete ledgers remain usable. No clipping, implicit recipe rewrite,
  token/price claim or completion from capacity admission. Unknown packet costs await
  source observations. An indivisible packet's typed byte-capacity failure defers only
  its target; malformed inputs and corrupt saved manifests remain integrity failures.
- Capture capacity and typed source-observation blockers with their complete inventory
  and source guards for staged proposal rejection too; known incomplete scope defers
  only its work.
