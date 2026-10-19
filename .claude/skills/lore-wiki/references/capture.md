# Capture an insight from active work

## Contents

- [Suitable material](#suitable-material)
- [Capture protocol](#protocol)
- [Evidence quality](#quality-gate)

The evidence is the current work and its observed results. Preserve the concrete
conditions and uncertainty; do not turn a single workaround into a universal rule.

## Suitable material

- Discovered a non-obvious platform/framework constraint
- Completed a multi-hypothesis troubleshooting sequence
- Found a workaround not covered by official docs
- Identified a reusable pattern (fail-open/closed, bounded retry, etc.)
- Hit an SDK/framework limitation requiring empirical discovery
- Resolved a deployment or infrastructure puzzle

## Protocol

### 1. Extract from context

Read the current conversation to identify:
- **Problem** — what was attempted
- **Discovery** — the non-obvious finding
- **Solution** — how it was resolved
- **Pattern** — the generalised lesson

Use the text after `capture` in `$ARGUMENTS` as the topic anchor if provided. Otherwise derive
from the conversation's most recent troubleshooting thread.

### 2. Load context

Ask which page already owns each name this capture will write — `lore resolve` for
EVERY form the material writes, then `lore wiki search` on two or three distinguishing terms
of each for the equivalences no rule about spelling can see. A capture that finds an owner
MERGES into it rather than creating a duplicate page.

If an extraction manifest exists for the current project
(`<vault>/.lorekeeper/extracts/<project>/manifest.yaml`), load
`strip_patterns` and `concept_mapping` from it. This keeps captures
consistent with batch extractions from `/lore-extract`.

### 3. Classify transferability

| Level | Criterion | Action |
|-------|-----------|--------|
| T1 | Cross-stack under supported conditions | vault document + concepts |
| T2 | Same technology stack | vault document + concepts |
| T3 | Pattern/methodology | document + reusable concepts |
| T4 | Project-specific only | **skip** — suggest project-local docs instead |

### 4. Write document

Write to the document path pattern from AGENTS.md. Use the vault's
configured locale for section headings and prose; preserve the schema's exact frontmatter keys —
AGENTS.md is the authoritative source for both.

This skill writes a COMPLETE page directly — no ingest pipeline runs
afterward, so the directly-authored rule in AGENTS.md's ownership legend applies:
you fill every section yourself, and `backlinks-sync` owns the concept sources
section + `source_count` in step 6.
Omit the `llm_inputs` frontmatter block entirely — it belongs to the
pipeline cache, not to directly-authored pages. An in-the-moment capture has
no originating file or URL, so omit `source_file`/`source_url` too (set them
only when the insight came from a concrete file or link you can cite).

Link the relevant concepts as relative markdown links (AGENTS.md § Links) in the document's
related-concepts section (heading from AGENTS.md) — these forward links are
what `backlinks-sync` reads to derive each concept's sources section in step 6.

`document_type` is a FORMAT value, and AGENTS.md lists the vocabulary and what
each value means — read it there rather than from memory —
the insight's nature (troubleshooting, constraint, pattern) belongs in
`tags`, never in `document_type`.

### 5. Create/merge concepts

For each technology, pattern, or constraint, follow the **Concept convergence**
section of the vault's `AGENTS.md` — matching
against what step 2 resolved and searched, with the concepts you already created
in this capture as the created-this-run set. Then:

1. New → create with a 1-2 sentence synthesis in the Synthesis section
   (heading from AGENTS.md).
2. Existing → reuse the page and register equivalent aliases; put new evidence in
   the document and its forward link. Let queued synthesis integrate that evidence.
3. Assign a category from `lore config categories` (one `id\tlabel` per line) — never read
   `config.yaml` yourself, for the same reason `vault.root` is not read there. A category
   outside that vocabulary is a lint violation on the page you just wrote; if none fits, omit
   the field rather than forcing one.

The sources section / `source_count` stay machine-owned (AGENTS.md § Concept convergence) —
`backlinks-sync` re-derives both in step 6 from the document's forward link
(step 4), the source of truth.

### 6. Finalize

Reconcile the graph so machine-owned fields are authoritative and health
checks pass:

```bash
lore graph backlinks-sync   # re-derive every concept's ## Sources + source_count
lore wiki refresh           # refresh SQLite navigation and the entry guide
lore graph lint             # non-zero means the vault contradicts itself somewhere — a link to a page that is not there, a category outside the configured vocabulary, or a name answering to two pages. Resolve every violation naming a page THIS capture wrote; a violation elsewhere in the vault predates you, so report it rather than fixing it silently. Orphans and open conflicts are reported as observations and never gate
```

### 7. Report

Show created/merged paths, concept count, and transferability level. Run
`lore queue status --json` and report synthesis queued by Finalize as pending;
use `/lore-process` to complete it when requested as part of this work. A successful
capture means the authored pages and reconciliation passed; do not call pending
concept rewrites semantically complete. Unrelated queue work does not expand this
capture into a global drain.

## Quality gate

Retain the concrete finding, its supporting observation, conditions and limits.
Separate a tried result from a proposal. Link only concepts that the document
substantially supports; an empty extraction is preferable to a passing-mention
concept. Preserve identifiers required to understand evidence while removing
irrelevant project detail from generalized prose. A capture need not invent a
universal pattern or extra sentences to meet a quota.

The three middle items are `###` sub-sections INSIDE the content section AGENTS.md
defines — they are not `##` sections of their own. A document page has exactly the
`##` sections AGENTS.md lists, and an invented one passes `lore doctor` and
`lore graph lint` silently while no consumer reads it.
