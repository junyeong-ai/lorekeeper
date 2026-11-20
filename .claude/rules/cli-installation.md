---
paths:
  - "crates/lk-cli/src/commands/installation.rs"
  - "crates/lk-cli/src/commands/installation/**"
  - "crates/lk-cli/src/commands/input.rs"
  - "crates/lk-cli/src/commands/init.rs"
  - "crates/lk-cli/src/commands/schedule.rs"
  - "crates/lk-cli/src/commands/workflow.rs"
---

## Job definitions (`commands/schedule.rs`)

- `commands/schedule.rs` renders native jobs with stable scheduling-slot labels.
  Queue-mode semantic jobs need explicit execution profiles. Capture selected absolute
  config/data paths, anchored PATH and relevant existing host directories; never capture
  credentials. Rendering never authenticates or registers jobs.
- Launchd `--namespace` constructs labels and plist names (default `com.lorekeeper`);
  custom namespaces use separate default logs. `--log-dir` is an explicit absolute path.
  These options only render definitions: never register jobs or create log directories
  here. Reject unsafe namespaces before any output.
- Preserve explicitly set standard proxy variables in both formats and direct jobs,
  including casing and empty values. Refuse credential-bearing proxy endpoints without
  printing their values; do not capture arbitrary environment variables.

## Installation and credentials

`commands/installation.rs` performs artifact/schema synchronization using the NEW
binary and refuses unsafe update over pending queue work. Uninstall never removes
user vaults. Prepare and validate the whole uninstall removal plan before confirmation
or deletion. Resolve native directory identity and selected-path ancestry; protect
vault containers, selected configuration ancestors and held coordinator files, while allowing owned project skills below
the vault. Strict optional config discovery distinguishes absence from invalid selection;
recheck config and vault identity after confirmation. Explicit config-directory purge
authorizes only that removal; other owned trees still protect the selected config. Delegate byte coherence/target/release validation to lk-dist. Preflight all selected
artifact trees under the acquired claims before payload writes or pruning. Coherence
keeps unavailable inspection and invalid directory layout distinct from changed bytes;
neither is advice to force a redeploy.
Unix dispatch retains shared installation claims on absolute selected destinations;
schedule rendering creates no files and does not acquire installation claims.
Mutators exclusively claim changed resources and the selected vault workflow before
writing. Resolve coordinator paths without provisioning. Acquire existing claims
before creating missing coordinators; containing resource scopes precede descendants,
and the selected binary precedes unrelated resources. Keep descriptor handoff in
canonical path order independently of acquisition order. Native runs retain claims
across collection and batch gaps. Pin the same
Installation and data directory in children rather than rediscovering mutable records.
Standalone mutators also pin the physical vault. Schema synchronization carries the
original configuration revision and root identity through acquisition and publication;
delegation preserves that context. Selector or directory replacement invalidates it.
Native project-skill claims use the vault CWD, not the invoking directory. Reacquire
that scope before the first host invocation so a vault created by Daily collection
is protected too; retain it through the rest of that run.
Downloads occur without exclusive claims; recheck selection, binary bytes and queue
under protection before replacement. The final mutation claim excludes writers but
never recovers a pending journal. Drop it and the queue claim before delegated schema
publication. New-binary deployment receives actual exclusive descriptors through a
Unix socket, verifies their lock-file identities and never unlocks inherited claims.
An environment marker alone grants no ownership. `--force` never bypasses active
claims. Bootstrap/manual replacement requires all shared users to be quiescent.
Create Unix private workspaces without group/other access at mkdir, before payloads;
restore owner permissions afterward when a restrictive umask removed them.
`input install` owns the optional native decoder environment. `input install-vision`
owns separate Paddle layout and MLX recognition environments plus verified models;
their dependencies must not share an environment. Installation is explicit, never
an ingest fallback. Package installation children inherit set standard proxy
variables (`lk_core::network`) unchanged. Engine platform/version checks belong to
the source adapter.
Credential wizard requires a TTY/dialoguer flow; persist shared credentials through
atomic mode0600 writes. Avoid command-specific alternate credential stores.
