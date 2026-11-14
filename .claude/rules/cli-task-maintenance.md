---
paths:
  - "crates/lk-cli/src/commands/task.rs"
  - "crates/lk-cli/src/commands/maintenance.rs"
---

## Task intent (`commands/task.rs`, delegated to lk-task)

- Reconcile→requested change→commit; publish exact board/history replacements in one
  mutation journal, then retire reminders best-effort. Acquire mutation claim before
  PlaneLock and recover before deriving task writes. Read-only agenda/brief neither
  mutate nor recover.
- Parse yesterday/today/tomorrow in the vault timezone with command-specific bounds;
  a close cannot target a future day. Board state, completion history, carried_too_long,
  schedules, and reminder status have separate meanings, not one generic “done.”
- JSON preserves unplaced tasks, unwritable diagnostics, timezone and completion
  information. Unavailable stores are null, not empty; incomplete parsing cannot
  prove a task absent. `status` delegates each subsystem's state to its owner.
- Maintenance reads store lifetime declarations. Keep newest collected source
  currency even when old, and preserve raw stream logs. Never prune another live
  worker's tmp directory or treat a disabled source as removed.
  Bounded-work retention captures pending requests, staged results and transitive
  reuse references under workflow→mutation→queue claims. Unrelated valid pending
  work does not block expired published work; unknown references refuse cleanup.
  Validate the historical completion dependency closure from captured observations
  before retirement. Root, frontier and consumed-source proofs retain their distinct
  coverage; expired valid Work does not require current source-page freshness.
