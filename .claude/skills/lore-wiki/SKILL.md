---
name: lore-wiki
description: Semantic wiki operations for the Lorekeeper vault. Add sources, query with compounding, audit structural and semantic health. Reads wiki/AGENTS.md for page formats and section vocabulary — never hardcodes headings. Pairs with `lore` (the deterministic binary) for graph analysis and queue processing.
when_to_use: |
  wiki query, wiki 질문, 지식 검색, 지식 조회, knowledge search, ask wiki,
  wiki add, 지식 추가, wiki에 추가, add to wiki, ingest document,
  wiki audit, 감사, wiki 검사, lint wiki, check wiki health,
  wiki status, wiki 상태
argument-hint: "<command> [args]"
allowed-tools: |
  Bash(lore *)
  Bash(ls *)
  Bash(cat *)
  Bash(wc *)
  Bash(head *)
  Bash(jq *)
  Bash(find *)
  Bash(grep *)
  mcp__obsidian__*
---

# lore-wiki — Semantic wiki operations for Lorekeeper

Semantic layer for the Lorekeeper vault. Pairs with the `lore` binary
(deterministic plane) but operates in the semantic plane (Claude Code LLM).

Before any page creation or section editing, read `wiki/AGENTS.md` (generated
by `lore schema`). It defines every page type's frontmatter keys, sections,
headings, and ownership (machine vs LLM). Never embed a format spec or heading
in this skill — derive everything from AGENTS.md.

If AGENTS.md is missing, tell the user to run `lore schema` first.

## Commands

### `/lore-wiki add <source>`

Manual ad-hoc ingest of a URL, file, **folder**, or pasted text. When given a
folder path, scan it recursively for `.md`, `.txt`, and `.pdf` files, process
each as an independent source, and report the aggregate results.

1. Read `wiki/AGENTS.md` for the concept page format.
2. If `<source>` is a folder, list all readable files and process each below.
3. For each source: extract every named entity, technology, and topic as
   concepts (not a lone summary — typically several per source).
4. For each concept: create or merge a concept page following AGENTS.md's
   concept format exactly (frontmatter + four sections). Machine sections are
   filled; LLM sections (synthesis, related) are filled by the model.
   **When creating a new concept**, fill the `## 핵심` section with a 1-2
   sentence definition rather than leaving it empty.
5. Report what was created/updated, grouped by source file.

### `/lore-wiki query <question>`

Answer a question grounded in vault content, with compounding.

1. Search the vault (concepts, daily pages, explorations) for relevant pages.
2. Synthesize an answer grounded in vault content.
3. **Compounding judgment** — after answering, judge reusability:
   - **Reusable** (synthesis, comparison, multi-source analysis) → write to
     `wiki/explorations/{slug}.md`, wikilink cited concepts/sources, tell the
     user where it landed.
   - **Ephemeral** (single-fact, navigational lookup) → do not file.

   The judgment is per-answer by the model. No frequency rule, no
   file-everything default.

### `/lore-wiki audit`

Three-layer health check. Surface findings for human review — never
auto-resolve.

1. **Structural** — run `lore graph --json lint`, report findings (orphans,
   broken links, hubs).
2. **Missing cross-references** — run `lore graph --json suggest-links`, then
   confirm topical relatedness before proposing a link. Community grounding +
   LLM confirmation = double gate against false positives.
3. **Contradictions** — scoped to one concept page at a time whose `sources`
   cite conflicting claims. Flag with `confidence: ambiguous` and a review
   note. Never choose a side. One page at a time to avoid combinatorial
   blow-up.

### `/lore-wiki status`

Quick vault stats.

- Total concept pages, explorations, daily pages.
- Last ingest time (read `.lorekeeper/ingest.jsonl`).
- Any pending queue files.

## Natural-language trigger mapping

- "이 문서 위키에 넣어줘" / "add this to wiki" → `/lore-wiki add`
- "위키에서 찾아봐" / "search the wiki" → `/lore-wiki query`
- "위키 건강 검사" / "audit the wiki" → `/lore-wiki audit`
- "위키 상태" / "wiki status" → `/lore-wiki status`

## When NOT to invoke

- For deterministic ingest (daily cron, source fetching) use `/lorekeeper`
  (the `lore ingest` skill).
- For draining the LLM queue after ingest, use `/lore-process`.
- This skill is for semantic operations: ad-hoc knowledge ingest, vault
  queries, and health audits.
