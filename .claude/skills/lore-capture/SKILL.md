---
name: lore-capture
description: Capture high-value insights from active work into the Lorekeeper vault — troubleshooting discoveries, non-obvious constraints, reusable patterns. Operates in the moment while context is alive.
when_to_use: |
  knowledge capture, lore capture, capture this, record insight,
  save to vault, record this discovery, save this pattern,
  record troubleshooting, knowledge asset, capture finding
argument-hint: "[topic or title]"
allowed-tools: |
  Bash(lore *)
  Bash(ls *)
  Bash(cat *)
  Bash(find *)
  Bash(grep *)
  Bash(wc *)
  Bash(jq *)
  Bash(date *)
  Read
  Edit
  Write
---

# lore-capture — Real-time knowledge capture

Capture high-value insights the moment they surface — during active
troubleshooting, after a discovery, or when a non-obvious constraint
appears. Context fades fast; structured capture now beats perfect
documentation later.

Writes to the Lorekeeper vault. Read `wiki/AGENTS.md` (from
`lore schema`) before creating pages. If missing, ask the user to
run `lore schema`.

## When to capture

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

Use `$ARGUMENTS` as the topic anchor if provided. Otherwise derive
from the conversation's most recent troubleshooting thread.

### 2. Load context

```bash
lore wiki concepts
```

If core concepts exist, this capture MERGES (enriches) rather than
creates duplicate pages.

If an extraction manifest exists for the current project
(`<vault>/.lorekeeper/extracts/<project>/manifest.yaml`), load
`strip_patterns` and `concept_mapping` from it. This keeps captures
consistent with batch extractions from `/lore-extract`.

### 3. Classify transferability

| Level | Criterion | Action |
|-------|-----------|--------|
| T1 | Universal — any project/stack | vault document + concepts |
| T2 | Same technology stack | vault document + concepts |
| T3 | Pattern/methodology | rich concept page |
| T4 | Project-specific only | **skip** — suggest project-local docs instead |

### 4. Write document

Create `wiki/documents/{slug}.md` per AGENTS.md document format.
Use the vault's configured locale for section headings.

**Frontmatter:**
```yaml
id: {slug}
title: "{Generalised Title}"
aliases: ["{Title}"]
created: {today}
updated: {today}
document_type: project-knowledge | engineering-guide
tags: [{technology}]
concepts: [{concept-slugs}]
```

**Sections** (adapt by knowledge type):

- **Summary** — 2-3 sentence standalone synopsis
- **Content** — structured subsections:
  - Background & Constraints (generalise — strip project identifiers)
  - Key Findings (the non-obvious behaviour; cite evidence)
  - Troubleshooting Sequence (hypotheses → outcomes → root cause)
  - Transferable Patterns (the one-sentence rule-of-thumb)
- **Related Concepts** — `[[wikilinks]]`

### 5. Create/merge concepts

For each technology, pattern, or constraint:

1. Slugify: NFKC → lowercase → non-alnum to hyphen → collapse → trim
2. Check existing concepts from step 2
3. New → create with 1-2 sentence synthesis in the Synthesis section (heading from AGENTS.md)
4. Existing → update `updated`, append source, increment `source_count`
5. Assign category from config.yaml `concepts.categories`

### 6. Report

Show created/merged paths, concept count, and transferability level.

## Quality gate

A captured document must have:
- Non-empty summary (≥ 2 sentences)
- At least one of: key findings OR troubleshooting sequence
- At least one transferable pattern
- ≥ 1 concept linked
- No raw project identifiers in generalised sections

## Integration

| Skill | Relationship |
|-------|-------------|
| `/lore-extract` | Reads manifest `strip_patterns` and `concept_mapping` for consistency |
| `/lore-wiki query` | Captured documents appear in query results |
| `/lore-wiki audit` | Covers captured documents in health checks |
| `/lore-ingest` | Daily source ingestion (separate pipeline) |
| `/lore-process` | Not needed (this skill writes complete pages) |
