# Lorekeeper

Rust CLI (`lore`) for config-driven ingestion into an Obsidian knowledge vault.
Markdown pages are authoritative; SQLite navigation is a rebuildable query view.

## Find the owner before changing a contract

Read the crate's `AGENTS.md` when working there; do not copy its rules into this file.

| Area | Owner |
|---|---|
| Config, names/addresses, links, citations, locale, atomic files | `crates/lk-core/` |
| Page/section I/O, templates, reviews, originals, mutation journal, navigation | `crates/lk-vault/` |
| Adapter identity/auth, fetch windows, normalization, per-item failures | `crates/lk-source/` |
| Render/cache, document identity, concept accumulation, rollups | `crates/lk-pipeline/` |
| Typed semantic requests/results, cache identity, queue persistence | `crates/lk-queue/` |
| Link graph, backlinks/evidence reconciliation, merge/retraction | `crates/lk-graph/` |
| Intent board/state transitions/completed-task observations | `crates/lk-task/` |
| Embedded artifacts, release verification and installation | `crates/lk-dist/` |
| Commands, apply/status/packet guards, workflow lease, scheduling | `crates/lk-cli/src/commands/` |

Templates live in `templates/`. Source descriptors in `lk-core::config` decide
whether an observation becomes a daily aggregate or a document; never special-case
one source to infer that distinction.

## Verify changes

- Run focused tests for the affected behavior; `scripts/check.sh` is the complete CI
  gate runner. `cargo check` checks types; `cargo fmt` formats Rust.
- For paths, aliases and Unicode, verify case-sensitive behavior: macOS may fold
  case and NFC/NFD where Linux does not. Probe legitimate platform differences.
- CLI/config/generated page-format surface changes are at least MINOR; corrections
  to README, schema prose, embedded skills or pipeline text are PATCH. Bump size
  follows the changed contract, not commit type; release gates also check stamps.
- Use CLI help for command syntax. Machine consumers use JSON/bare-value contracts,
  not parsed terminal tables; align terminal columns by display width (`commands::pad`).

## Cross-cutting invariants

- **Use shared representations.** Paths go through `VaultPath`/`vault_path` constants,
  links through `lk_core::link`, names through `ConceptRegistry`, evidence through
  `Citation`/`citation_digest`. A concept slug is its collection-relative address
  without `.md`, not name identity; retain nested addresses and resolve declared
  titles/aliases before creating a page.
- **Keep the vault portable.** Every page declares its format in `type`. Internal
  links are CommonMark `[label](relative/path.md)`, not wikilinks. Exact addresses
  survive rewriting; names are source-written. New prose uses `vault.locale`;
  concept synthesis preserves an established body's language under its per-kind policy.
  Generated `<wiki>/AGENTS.md` spec prose is English; its headings are localized
  and its language policy names the configured locale. Source text is not translated.
- **Publish semantic values, not ad-hoc edits.** Workers stage typed results. The
  applier verifies task/input identity, target revision, all owned headings and
  evidence, then publishes bodies and completion markers together. Empty-but-done
  is valid; a filled body is not completion. See `lk-queue` and `lk-pipeline`.
- **Evidence is explicit.** Backlinks derive source links, count and synthesis input
  together. Normal extraction accumulates established citations, but retention is
  not renewed semantic approval. Correct mistaken links through revision-guarded
  `graph retract-citation`, then reconcile backlinks and synthesize affected concepts.
- **Originals and interpretations are different authorities.** Bound RSS originals
  are immutable pre-classification captures; explicit selectors pin event payload
  revisions. Legacy logs are `legacy_unbound`, not historical derivation proof.
  URLs, matching source/date/config or omitted summary text do not establish that
  proof. Never infer selectors or silently upgrade page-only evidence. See the
  core/vault/queue RSS contracts before changing these paths.
- **Cooperating writers use `MutationSession`.** Recover pending journals before
  deriving writes; journal complete guarded plans before mutation. External editors
  are outside that lock. Read-only queries never write or recover. Publication
  previews never publish or recover; missing-address validation uses disposable
  private directory probes in the destination filesystem, without page payloads.
  Derived document projections use separate per-identity cache claims before
  publication; they are disposable computation, never canonical evidence. Inspection
  and dry runs may reuse them read-only without renewing retention.
- **Long workflows use a separate cooperative lease.** `lore workflow -- COMMAND`
  owns the canonical vault's Unix `workflow.lock` across evidence reading and
  publication; short mutation/queue locks remain inside commands, never across an
  LLM session. Default contention refuses; waiting is explicit. Closing inherited
  descriptors/detaching work breaks the guarantee. No PID/TTL bypass or Windows support.
  Workflow children pin the selected configuration path and byte revision; edits
  invalidate the session rather than redirecting remaining work. This is a content
  guard, never lease ownership or a reentrancy bypass.
- **Preserve observations.** Dates use configured vault timezone. Future events do
  not materialize. Each adapter decides ownership from its structured identity rules, not
  free-text mentions; Calendar includes matching organizers or attendees. Optional `personal` gates personal pages, not weekly knowledge
  synthesis. Failed-all fetches are errors; a genuinely empty observation is success.
- **Retain state, not just history.** Maintenance keeps each source's latest
  collected log entry even beyond retention. Manual inputs retire only after all
  page/request publication succeeds; exact regeneration uses retained input,
  not a potentially changed refetch. Credentials are reported, never scrubbed from
  source text; rotation at the issuer is the repair.
- **Installation and scheduling are explicit.** `lk-dist` owns embedded artifact
  coherence. Native jobs compose guarded CLI stages and never auto-commit user edits.
  Full-source ingest alone rewrites the cross-source work-log; filtered ingest
  cannot replace it with a subset. Navigation refresh updates SQLite discovery and a small entry guide.

## Naming

- `new`: cheap field construction; `from_*`: convert one value; `build_*`/`::build`:
  computed/fallible construction (including test factories); `with_*`: builder mutation.
- `find_*`: optional search; `load_*`: I/O; `resolve_*`: name to value/address;
  `get_*`: present in-memory accessor; `lookup`: keyed cache query.
- Predicates use `is_*`/`has_*`. Types spell words fully (`GraphCommand`, not `Cmd`);
  locals may abbreviate. Public errors have typed domain surfaces.
- `*Result` is a domain outcome. Add `*Report` only when presentation adds structure;
  do not wrap an already display-ready result. Wire enum names use kebab-case.

Config discovery is `./config.yaml` then `~/.config/lorekeeper/config.yaml`;
relative `vault.root` is relative to that config file, not the working directory.
