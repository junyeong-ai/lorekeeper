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
- Production's 30-day and 7-day requirements share an effective date and conflict.
- Development and historical production scopes do not establish that conflict.
- The copied 30-day statement supplies no independent corroboration.
- Approval and enforcement remain unknown; synthesis states these limits.
- The material conflict has an open, revision-pinned review with resolution
  criteria. An assessed unresolved conflict is `follow-up`, not resolved.
- Queue completion and audit receipts match published values. Re-ingest creates
  no duplicate documents; source count alone does not certify semantic quality.
