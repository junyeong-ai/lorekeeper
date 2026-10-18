# lk-dist

Embedded artifact layout, deployment coherence, and release verification. Shared
safety/testing rules live in the root CLAUDE.md; CLI install/update orchestration
lives in `lk-cli/commands/installation.rs`.

## Carried artifacts and ownership (`embedded.rs`, `layout.rs`, `build.rs`)

- Embedded bytes are the deployment authority, not a version label. `build.rs`
  watches both files and directories so additions/removals invalidate embedding.
  Vault templates have one embedding owner; do not introduce another template copy.
- Write carried files exactly, then prune individual uncarried paths only where
  ownership permits. Never recursively replace a user config directory or skill
  tree. Config deployment preserves user config files.
- Prune by actual same-file identity and `symlink_metadata`, not case-folded names
  or canonicalization guesses. A symlink must not survive as a carried regular file;
  hardlink/case-insensitive aliases must not delete the file just deployed.
- Retired skills are owned only if recorded in `deployed-skills` for that level.
  A `lore-` prefix is not provenance; no record grants no deletion authority.
  Report failed removals too, retaining enough ownership information to retry.
- Project-level deployment writes carried files but NEVER deletes extras or retired
  skills. Its checks ignore extras while still comparing carried bytes. An update
  cannot bypass this policy by calling a lower-level pruning helper.
- Data directory selection is environment override, recorded `data-dir`, then
  default. Uninstall removes the record as well. Optional deployment locations are
  used only when already present unless the user explicitly selects a level.
- Installer hints also serve explicitly pinned older binaries. Select a deployed
  starting example without assuming that every release carries the same files.

## Coherence (`coherence.rs`)

Compare artifact bytes and required executable bits, not whole Unix modes or release
strings. Platforms without Unix executable bits do not fabricate a mismatch.
`ArtifactState` distinguishes Current/Modified/Absent/Retired; declined optional
groups are not drift. `SchemaState` receives the actual selected config and generated
schema expectation. The installation report owns repair guidance: a schema-only
mismatch needs schema repair, not a reinstall.

Deploy owned files before best-effort schema synchronization. A user vault/config
being unavailable must not turn a successful artifact install into failure. The CLI
must invoke the NEW binary for its schema/skill contract after replacement.

## Release boundary (`target.rs`, `release.rs`, `archive.rs`, `verify.rs`, `update.rs`)

- Keep the single supported-target table coherent with carried archives, release
  matrix, and installer target cases. Unknown target parsing fails closed. Windows
  ZIP/install.ps1 is distinct from Unix self-swap behavior.
- Prefer the releases API; preserve provenance when using the web fallback. Compare
  semantic precedence without build metadata. Refuse an older default-channel
  release; an explicitly requested version may intentionally roll back.
- Extract only the caller-named archive member, skipping all others. Never accept a
  server-selected destination path. Verify SHA-256 in process, including the
  sidecar's expected asset name, before installation.
- Attestation staging uses a private created directory and exclusive files, not a
  predictable shared path that can be replaced by a symlink.
- `install_binary` atomically replaces bytes, preserves the old executable mode,
  runs the replacement to check its expected version, and restores old bytes on
  failure. Passing a version check alone does not prove carried artifacts coherent.

Use `tests/carried.rs` and the layout/coherence/update module tests for focused
changes to these contracts; do not add network-dependent release tests.
