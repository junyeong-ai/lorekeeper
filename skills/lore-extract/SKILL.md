---
name: lore-extract
description: Extracts repository decisions and reusable knowledge into a Lorekeeper
  vault using a persistent scan/run/audit manifest. Uses source documents, code boundaries
  and optional Git history; use lore-wiki capture for an insight from current work.
metadata:
  version: 0.102.1
---

# Repository knowledge extraction

Keep the source's conditions, evidence and limitations while making useful knowledge
available beyond its repository. A manifest makes discovery, extraction and later
freshness checks separate, inspectable operations.

| Mode | Read when selected |
|---|---|
| `scan` | [scan](references/scan.md) and [manifest](references/manifest.md) |
| `run` | [run](references/run.md) and [manifest](references/manifest.md); read the [audit](references/audit.md) per-source check for incremental selection |
| `audit` | [audit](references/audit.md); inspect the [manifest](references/manifest.md) when interpreting coverage |
| `--include-git` | Also read [Git history](references/git-history.md); commit bodies are optional evidence, not one page per commit |

## Shared contract

- Resolve paths with `lore config vault-root` and `lore config schema-path`.
  Read the generated AGENTS.md before page writes; run `lore schema` if missing.
  It owns formats, headings, vocabulary, link syntax and locale. Never guess `wiki/`.
- Start a foreground publishing session under `lore workflow -- claude` or `lore workflow -- codex` before
  reading evidence. Do not nest it in an already guarded pipeline/session. Pause
  other concept/source writers during a wave; external editors do not share this lease.
- These are directly authored COMPLETE pages: fill their owned sections, omit
  pipeline `llm_inputs`, preserve existing addresses and `created`. The manifest
  records per-document progress; rerunning must not delete existing knowledge.
- Resolve EVERY form a source writes with `lore resolve`, check the created-this-run
  set, then use `lore wiki search` on distinguishing terms of each form before
  creating a concept. A lexical difference does not establish a new concept.
- Distinguish source declarations, observed implementation, and verified behavior.
  Carry repository/revision, scope and exceptions into claims. Contradictions are
  findings to investigate; do not publish a contradicted assertion as a fact.
  A decision-relevant unresolved disagreement belongs in a durable knowledge review
  with both direct sources, not an automatic winner chosen by date or citation count.

## Transferability

| Level | Supported scope | Retained result |
|---|---|---|
| T1 | Cross-stack under stated conditions | Document and supported reusable concepts |
| T2 | Depends on a named stack or technology | Document and supported reusable concepts |
| T3 | Reusable method or pattern with rationale and limits | Document and supported reusable concepts |
| T4 | Local detail without a supported reusable finding | Explicit manifest skip and rationale |

Do not generalize to earn inclusion. Project identifiers may be stripped from body
prose using manifest rules; preserve exact source paths, stable document IDs,
technology/API names and evidence that the finding depends on.

## Completion

After publication, run `lore graph backlinks-sync`, `lore wiki refresh`, and
`lore graph lint`. Fix violations on pages written by this run; report unrelated
violations and observations separately. Machine-owned sources and `source_count`
come from reconciliation, never hand edits. Check `lore queue status --json`:
extracted documents and queued concept synthesis have different completion states.
Use `/lore-process` for the latter within the authorized scope; a bounded extraction
is not permission to drain unrelated global work.
