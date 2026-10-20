# Manual semantic lifecycle scenario

Copy this directory to a private temporary directory before execution. Use an
installed `lore` and an authenticated agent profile; no credentials belong in this
fixture. Keep deployment data separate from your normal installation. This is a
small paid model exercise, not a CI truth oracle.

From the copied directory, set `LORE_CONFIG` to its absolute `config.yaml`,
`LORE_INSTALL_DATA_DIR` to its `data` directory and `XDG_CONFIG_HOME` to its
`config` directory. Then run:

```sh
lore validate
lore self deploy --skills none
lore ingest
lore run process --profile codex --json
lore run knowledge --profile codex --json
lore graph lint
lore queue check
lore wiki reviews
```

Use a fresh copy and `--profile claude` for Claude Code. Explicit profile `model`
and `executable` values may be set for reproducible local runs. Native byte/call
budgets are real limits: exhaustion must report incomplete work, never success.

Inspect the resulting pages, not just exit codes:

- Five source documents and one existing concept retain exact original evidence.
- Existing declared aliases remain lookup keys when the selected citations do not
  attest them; removal requires evidence of wrong or ambiguous identity.
- Production's 30-day and 7-day requirements share an effective date and conflict.
- Development and historical production scopes do not establish that conflict.
- The copied 30-day statement supplies no independent corroboration.
- Approval and enforcement remain unknown; synthesis states these limits.
- The material conflict has an open, revision-pinned review with resolution
  criteria. An assessed unresolved conflict is `follow-up`, not resolved.
- Queue completion and audit receipts match published values. Re-ingest creates
  no duplicate documents; source count alone does not certify semantic quality.

Check navigation separately from those semantic judgments. Live search reads current
pages; a prepared snapshot pins captured pages and relations without rereading them:

```sh
lore wiki browse --json
lore wiki search '보존 정책' --json
time lore wiki search '보존정책' --json
lore wiki search 'production 7 days' --json
time lore wiki prepare
```

Replace the placeholders below with returned values. The declared Korean
alias and its spacing variant should reach the established concept before its source
documents. Spacing matches remain candidates. `production 7 days` can also reach the
development document because it explicitly disclaims a production requirement; read
the environment, effective period and negation before judging support or conflict.

```sh
time lore wiki search '보존정책' --snapshot '<snapshot>' --json
lore wiki context wiki/concepts/data-retention-policy.md --snapshot '<snapshot>'
lore wiki citations wiki/concepts/data-retention-policy.md --snapshot '<snapshot>'
lore graph neighbors wiki/concepts/data-retention-policy.md
lore wiki read wiki/documents/production-retention-requirement.md
lore wiki read wiki/documents/production-retention-requirement.md --section '<id>' --revision '<file_revision>'
lore wiki evidence wiki/documents/production-retention-requirement.md
lore wiki original wiki/documents/production-retention-requirement.md
lore wiki original wiki/documents/production-retention-requirement.md --member '<member-path>' --revision '<manifest_revision.value>'
```

Compare repeated-query times separately from preparation; fixture timings do not
predict a larger corpus. Use the outline's section ID and `file_revision` for the
selected read. Context and citations describe the capture; `read` and graph neighbors
inspect current files. Explicit links, citation
counts and copied statements do not establish independent corroboration. After editing
a source, compare live and captured search before preparing again; a stale capture is
expected to retain its earlier result, while a stale revision-pinned read must refuse.
