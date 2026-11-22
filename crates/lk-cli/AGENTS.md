# lk-cli

Clap front door and orchestration (`main.rs`, `commands/`). Follow the root
AGENTS.md for shared architecture, output, locks, and mutation rules. Delegate
meaning to the owning crate; do not recreate its parser/status model in a command.

Build the Clap command tree before inspecting all arguments or nested commands;
deferred subcommands expose their names eagerly for name-only discovery.

## Configuration and workflow sessions

- `commands/mod.rs` resolves flag/environment config, working-directory config, then XDG
  config; never silently use config.example.yaml. Resolve the selected config
  absolutely; relative vault roots are relative to that config.
- A mutation session's canonical root owns every page and queue path derived for
  publication. Ingestion pins its physical root before building acquisition clients.
- Workflow configuration is loaded and revisioned from one read. Pass its selected path
  and `LORE_CONFIG_REVISION` to children; every CLI config load rejects changed bytes or
  another path. A workflow also pins its original root selector, working path and
  directory identity. Bind parent and child I/O to that working path; validate selection
  before and after waiting/recovery and before host/publication; mutation sessions carry
  their acquisition check as the publication precondition, and every writer passes the
  pin of the configuration it loaded (`Absent` without one). Same-directory aliases
  remain valid; another directory or incomplete inherited context refuses. Root context
  validates scope, never lease ownership or reentrancy. Native jobs stop after drift and
  retain unfinished work.
- Semantic Work identities bind the semantic Work policy `Config::load` derives
  (lk-core names the operational keys it excludes); input/result byte limits, extraction
  policy, model/effort and all other configuration remain semantic inputs. The candidate
  bound acts only through the candidate roster. Screening decisions bind their own
  contract, not a configuration revision (cli-extraction.md). The live workflow still
  pins every configuration byte. Work whose subject revision hashes the complete
  configuration bytes resumes only through an exact current-configuration
  reconstruction; never retag it after an operational change or guess historical settings.
- Optional typed stage overrides choose only host profiles. Keep the originating profile
  selected for Work policy, budgets and child LORE_EXECUTION_PROFILE. Resolve the actual
  node/task kind before each invocation; never route from prompts or paths. Independent
  read waves retain per-request host/timeout/decoder association; generation batches
  require the same resolved profile name. Reuse the shared engine adapters and
  both-engine installation claims. Host success alone is neither retained semantic
  progress nor publication proof.
- Revalidate pinned configuration/root and cancellation after awaited host-version
  preflight, before reserving a turn or counting/invoking a semantic call.
- `commands/workflow.rs` is a Unix foreground coordinator for long LLM consumers, not a
  vault mutation lock. It locks canonical-vault `.lorekeeper/workflow.lock`, clears only
  FD_CLOEXEC, and `exec`s literal argv. Busy defaults to nonzero with no child; `--wait`
  explicitly waits. Pass selected absolute LORE_CONFIG and optional LORE_TEMPLATE_DIR to
  the child via Command.env, not global environment mutation.
- Acquire persistent `workflow-admission.lock` before `workflow.lock`. A waiting
  contender retains admission until it obtains the workflow lease, preventing a
  new campaign batch from barging ahead. Release admission before working/exec;
  only the workflow descriptor is inherited. Cancellation/refusal releases held
  claims by descriptor lifetime. Maintenance claims retention through the same
  admission order: it skips held work stores unless `--wait-for-vault` makes it wait,
  so scheduled campaigns yield to it. Neither file is unlinked, and legacy binaries still
  share mutual exclusion through `workflow.lock` without the admission guarantee.
- The lease lasts only while a participating process retains the descriptor. Detached
  programs or programs closing inherited descriptors are outside that guarantee. No
  PID/TTL cleanup, env-bool bypass, or reentrancy guess. The lease has no Windows
  implementation. Ordinary short CLI writers do not reacquire the lease; they retain their
  own locks/revision guards. Workflow does not recover journals.
- `self snapshot` holds existing exclusive installation claims, including the binary,
  and the selected physical vault's workflow, mutation and queue claims through a
  foreground external command. Refuse pending journals without recovery; validate
  config/root selection again before exec. No Lore child command, implicit wait,
  archive implementation or exemption for acquisition before publication. External
  editors and nonparticipating installations still require separate coordination.
- Child PATH starts with the selected executable's directory and LORE_BIN names that
  executable. Its canonical `lore` sibling must identify the same file; a renamed binary
  cannot silently delegate skill commands to another installation. Claude grants exact
  quoted executable spellings and, only for plain ASCII shell path characters, the same
  unquoted path. Never use a wildcard executable prefix, wrapper permission or
  unrestricted Bash to accommodate another spelling.

## Contract rules

Read the matching rules before changing these areas. Each file is the canonical
contract for its scope and also loads through Claude Code path rules. Read the
related owner when a change crosses boundaries.

| Area | Rules |
|---|---|
| Native sequencing, host supervision, budgets, pauses, invocation-failure holds and audit admission | [cli-native-runs.md](../../.claude/rules/cli-native-runs.md) |
| Saved Work, coverage, evidence and finalization | [cli-semantic-work.md](../../.claude/rules/cli-semantic-work.md) |
| Extraction roster, screening decisions, store and holds, sealed results and work cost | [cli-extraction.md](../../.claude/rules/cli-extraction.md) |
| Native role requests and replies (`candidate_ref`, `owner_ref`), preparation, generation and action receipts, publication | [cli-native-results.md](../../.claude/rules/cli-native-results.md) |
| Queue retirement, fair scheduling, readiness and application | [cli-queue.md](../../.claude/rules/cli-queue.md) |
| Installation, job definitions, claims and credentials | [cli-installation.md](../../.claude/rules/cli-installation.md) |
| Ingestion, originals and provenance queries | [cli-ingestion.md](../../.claude/rules/cli-ingestion.md) |
| Canonical pages, graph and derived discovery | [cli-knowledge.md](../../.claude/rules/cli-knowledge.md) |
| Task intent and maintenance lifetime | [cli-task-maintenance.md](../../.claude/rules/cli-task-maintenance.md) |
