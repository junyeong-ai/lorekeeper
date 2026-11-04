# Installation and data preservation

## Contents

- [Install](#install)
- [Interactive and runtime skills](#interactive-and-runtime-skills)
- [Update and repair](#update-and-repair)
- [Back up and restore](#back-up-and-restore)
- [Retention and removal](#retention-and-removal)

## Install

For a new Unix installation, follow the [README quick start](../../../README.en.md#quick-start).
The shell installer accepts `--version`, `--install-dir`, `--data-dir`,
`--skill user|project|none`, `--agent claude-code|codex|both` and `--from-source`;
use its `--help` for the full contract. Source builds use the repository's pinned
Rust toolchain; see the README's [development commands](../../../README.en.md#further-reading-and-development).

For Windows x86_64, run the PowerShell installer:

```powershell
irm https://raw.githubusercontent.com/junyeong-ai/lorekeeper/main/scripts/install.ps1 | iex
```

The Windows CLI does not support Unix workflow leases or native agent jobs.
Optional native binary-document runtimes also have platform-specific support;
read the [preprocessing reference](../../lore-setup/references/config.md#document-preprocessing) before selecting them.

Installation deploys `config.starter.yaml` and `config.example.yaml` without
replacing the user's `config.yaml`. The adjacent `data-dir` and `deployed-skills`
records track installation ownership, not credentials. Config discovery and
credential setup belong to the setup skill; do not reset an existing vault to
make installation checks pass.

## Interactive and runtime skills

Shared procedures live under `skills/`; deployment produces host-specific entry
files from that single source. Claude metadata lives in `agents/claude.json`;
Codex invocation policy uses `agents/openai.yaml` where needed.

```bash
lore self deploy --skills user --agent codex
```

Use `--agent both` for both hosts or `--skills project` for the current project.
Claude discovers `.claude/skills`; Codex discovers `.agents/skills`. Deployment
must account for intentional local edits before replacing carried files.
Unattended jobs use verified copies under `<data>/skills/<engine>/`, independently
of interactive deployment. Missing or modified required runtime files refuse execution.

Custom templates are selected with `--template-dir`. Use `markdown_text` for
plain titles/authors, `markdown_fragment` for source bodies, and
`url | markdown_link(label)` for source links. Fragment parsing keeps source
headings, references and code blocks from taking over later generated sections.

## Update and repair

```bash
lore self status
lore self update
lore self status
```

`self status` compares carried artifact bytes and the selected schema; it does
not establish source access or semantic completion. `self update` replaces the
binary, redeploys its interactive/runtime skills, templates and config examples,
and synchronizes the generated schema. `self deploy` repairs carried copies from
the current binary without downloading another release. Follow the reported repair;
schema generation replaces its generated `AGENTS.md`, so preserve authored additions separately.

Updates use the queue settlement check. Pending work, unreadable or unparseable
input, staged results and interrupted publication block replacement by default.
`--force` permits inspectable unfinished work; it never bypasses configuration or
storage inspection errors, nor active installation claims. An explicit `--version`
can select an older release. Use `--verify-attestations` when attestation verification
is required; a version string alone does not establish artifact provenance.

Unix CLI commands hold shared claims on their selected binary/runtime/skills.
Native runs and participating workflow children retain protection across collection
and inference. Updates reacquire exclusive protection and recheck config, binary
bytes and queue state after downloading. Adjacent `.lorekeeper-*.lock` files are
persistent coordination state; never unlink them during execution or cache cleanup.
Installation paths are resolved without provisioning before admission. Participating
commands must use the updated binary for this guarantee; older binaries retain their
own preparation behavior. Nonregular or symlink lock files refuse access without
automatic removal or repair.
Their creation requires write access to the resource's parent directory.

Bootstrap scripts, `cargo install`, manual binary replacement and older binaries
do not participate in installation locking. Before those paths, pause schedules
and manual sessions for every vault sharing the installation, wait for their
children to exit and require each vault's `queue check` to pass. After replacement,
run `self deploy` and `self status` before resuming. Do not force through active users.
Start a fresh workflow after replacement. Its configuration and physical-root context
belong to that session; saved Work resumes separately through currentness checks.

After a signing-identity change, macOS may require renewed protected-folder consent
for scheduled jobs. Verify actual scheduled vault access using the
[scheduling procedure](scheduling.md), not only signature verification or `--version`.

## Back up and restore

Back up the **entire vault, including hidden files**, separately from Git history.
Original/evidence archives, observations, review receipts and unfinished queues or
journals under `.lorekeeper/` cannot be reconstructed from Markdown alone.
Stop scheduled and interactive writers and external editors, wait for foreground
work to finish. On Unix, run the external backup command under native exclusion:

```bash
lore self snapshot -- /path/to/backup-command /outside/the/vault/backup-destination
```

This foreground boundary excludes native consumers of the same installation,
including acquisition before workflow admission, and holds the selected vault's
workflow, mutation and queue writer claims. It refuses busy state or a pending
mutation journal without running the child or recovering writes. Other vaults
sharing that installation cannot use it during this window. There is no implicit
wait or force option. A workflow lease alone does not freeze acquisition state.

The command receives literal arguments, the caller's working directory and I/O,
the pinned `LORE_CONFIG` and physical `LORE_VAULT_ROOT`; its exit status is preserved.
Keep it in the foreground with inherited descriptors open. It must not invoke Lore
commands while holding this exclusion. External editors and nonparticipating
installations remain outside the guarantee; keep them stopped. The backup command
owns complete inventory, copy/archive and byte verification. A successful child
exit alone does not prove a restorable backup or semantic completion.

Include the selected configuration and custom templates. Protect credential backups separately
or authenticate again; restored Unix credential stores and their lock files require `0600`.

Restore to a separate location with the same CLI version. Adjust the copied
config's `vault.root` and external paths, and reinstall optional document runtimes.
Inspect `lore status`, `lore queue check` and `lore wiki reviews` before resuming writes.
Resolve unfinished work through the recovery protocol instead of deleting state.
Then rebuild derived SQLite with `lore wiki prepare`, run `lore graph lint` and
inspect retained originals. A mutation journal is recovery state, not a historical backup.

Manual inbox receipts bind source ID and canonical path. A changed location refuses
automatic adoption: restore the original binding or plan an explicit ownership
migration before collecting again. A new source ID or deleted state does not
transfer existing document ownership. See [manual inbox batches](ingestion.md#manual-inbox-batches).

Git is optional for reviewed Markdown history. Pipelines never auto-commit user
edits. Exclude `.lorekeeper/`, credentials and private local state from commits;
review and stage only intended files. A public Git repository is not a private backup.

## Retention and removal

```bash
lore maintenance --dry-run
```

Inspect retention eligibility before requesting cleanup. Maintenance preserves
permanent originals and streaming event logs, each source's latest collection
state, unpublished Work and live queue/reuse references. Rebuildable SQLite and
expired verified derived state have different ownership; do not delete hidden
state indiscriminately to reclaim space.

`lore self uninstall` removes owned installation files and leaves the vault intact.
Before confirmation or deletion, it checks the complete removal plan against the
selected vault and active installation coordinator files. A vault inside the installed
`templates` directory refuses removal; a vault inside the configuration directory
refuses `--purge-config`. A normal project skill directory inside a vault can still be
removed. Missing or invalid explicitly selected configuration refuses removal rather
than treating the vault as absent. The selected configuration file and its containing directories are protected from
ordinary removal. `--purge-config` authorizes only the installation configuration
directory; a selected config under another owned tree still refuses removal. Keep
the selected configuration and vault unchanged through confirmation. Uninstall does not relocate data or unregister OS jobs. Unregister schedules explicitly before removal;
see the scheduling reference for the platform procedure.

Use the installed executable from version 0.120.1 or newer for guarded removal.
The standalone removal scripts refuse without deleting files. Their path and
keep-data options do not run; they cannot establish native configuration and
installation protection when the executable is absent.
