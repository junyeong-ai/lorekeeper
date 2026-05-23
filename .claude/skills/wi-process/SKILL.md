---
name: wi-process
description: Consume wiki-ingest LLM work queue. When `wi ingest` runs in queue mode (config `llm.provider: queue`), the Rust pipeline writes JSONL task files under `<vault>/.wiki-ingest/queue/`. This skill drains those queues — running summarize and concept extraction using Claude Code's native LLM (no API key needed) and editing the target Obsidian pages via Obsidian MCP. Idempotent — partial progress is resumable; processed files move to `.wiki-ingest/queue/processed/`. Run after each `wi ingest` (or daily) to enrich pages that were written with empty summary/concept sections.
when_to_use: |
  wi-process, /wi-process, 큐 처리, queue process, drain queue, 처리 큐,
  wiki 처리, summary 채워, 요약 채우기, 개념 추출 실행, concept extraction run,
  daily 페이지 보강, enrich daily pages, post-ingest, ingest 후 처리
argument-hint: "[--vault path] [--limit N]"
allowed-tools: |
  Bash(ls *)
  Bash(cat *)
  Bash(jq *)
  Bash(mv *)
  Bash(mkdir *)
  mcp__obsidian__*
---

# wi-process — Drain the wiki-ingest LLM queue

The Rust `wi ingest` pipeline, when configured with `llm.provider: queue`, defers
all semantic work (summarization, concept extraction) by writing JSONL task
files into `<vault>/.wiki-ingest/queue/{run-timestamp}.jsonl`. Each task points
at a vault page that was written with an empty section awaiting LLM content.

This skill consumes those tasks: read the queue, perform the LLM work using
your own session, edit target pages via Obsidian MCP, then move the processed
queue file to `.wiki-ingest/queue/processed/`.

## Queue task schema

Each line in a queue file is one task:

```json
{
  "task_id": "sum-2026-05-23T07-00-00Z-000",
  "kind": "summarize",
  "created_at": "2026-05-23T07:00:00Z",
  "input": {
    "text": "<events concatenated for summarization>",
    "max_sentences": 5
  },
  "target": {
    "vault_path": "daily/ai-news/2026-05-23.md",
    "kind": "daily-summary"
  }
}
```

`kind` values: `summarize` | `extract-concepts`
`target.kind` values: `daily-summary` | `daily-concepts` |
`weekly-synthesis-narrative` | `weekly-personal-narrative` |
`monthly-narrative` | `quarterly-narrative` | `annual-narrative`

## Processing protocol

1. **Discover the vault root.** Default: the active Obsidian vault. The user
   may pass `--vault <path>` as an argument.

2. **List unprocessed queue files** in `<vault>/.wiki-ingest/queue/` (top
   level only — `processed/` is the archive):
   ```bash
   ls "$VAULT/.wiki-ingest/queue/"*.jsonl 2>/dev/null
   ```
   Order: oldest filename first (filenames are ISO timestamps).

3. **For each queue file** (one file = one ingest run):

   a. Read all tasks: `cat <file> | jq -c '.'`

   b. **For each task** (in file order):

      - **`kind: summarize`** — synthesize a concise summary in the
        user's preferred language (Korean if `daily-*-narrative`).
        Aim for `input.max_sentences` bullet points. No preamble.

      - **`kind: extract-concepts`** — identify named entities,
        technologies, and key topics. Output a list of concept names
        (in the source language). Each concept should also produce a
        `wiki/concepts/{slug}.md` entry (create if missing, merge if
        exists — increment mention_count, append source ref).

   c. **Edit the target page** via Obsidian MCP:

      - **`daily-summary`** target: open the page, find `## 핵심 요약`
        / `## 요약` section (or insert near top after frontmatter).
        Replace its body with the synthesized summary.

      - **`daily-concepts`** target: locate `## 관련 개념` section.
        Replace its body with `- [[Concept Name 1]]\n- [[Concept Name 2]]\n...`.
        Create each concept page in `wiki/concepts/` if it doesn't exist
        (use frontmatter id, name, first_seen, last_seen, mention_count,
        sources fields — match the format from existing concept pages if
        any).

      - **Synthesis narratives** (`weekly-*`, `monthly-*`, `quarterly-*`,
        `annual-*`) target: replace the `{{ narrative }}` body of the
        page. Preserve all frontmatter and section headings.

   d. **On success**, continue. **On failure** (page not found, MCP
      error), log a warning but keep going — don't abort the run.

4. **After all tasks in a file are processed**, move the file to the
   archive:
   ```bash
   mkdir -p "$VAULT/.wiki-ingest/queue/processed"
   mv "$file" "$VAULT/.wiki-ingest/queue/processed/"
   ```

5. **Report** to the user: number of files processed, tasks completed,
   tasks skipped (with reasons).

## Idempotency

- The queue file is only moved to `processed/` after every task in it has
  been attempted. If a task fails, the queue file stays — re-running
  reprocesses the whole file. Since edits are content-replacements
  (idempotent), this is safe.
- Concept page merging: when re-creating an existing concept page,
  preserve the original `first_seen` and append-but-dedupe `sources`,
  bump `mention_count` only if a genuinely new source reference is added.

## When NOT to invoke

- Don't run while `wi ingest` is currently running — it might be
  appending to a queue file you're trying to process. Wait until ingest
  finishes (or run on a different queue file). Filenames are unique per
  ingest run, so overlap is partial at worst.
- Don't manually edit pages between ingest and process — your edits to
  the target section will be overwritten.

## Example session

```bash
# After cron runs `wi ingest` at 07:00, a queue file exists:
$ ls ~/Documents/Obsidian\ Vault/.wiki-ingest/queue/
2026-05-23T07-00-00Z.jsonl

$ wc -l ~/Documents/Obsidian\ Vault/.wiki-ingest/queue/*.jsonl
14 tasks pending

# Run this skill:
# /wi-process

# Result: 14 tasks processed, daily pages now have summaries and concept
# wiki-links, concept pages created/merged under wiki/concepts/,
# the queue file is in .wiki-ingest/queue/processed/.
```

## Quick verification

After running, the user can check:
```bash
# No pending queue files
ls "$VAULT/.wiki-ingest/queue/"*.jsonl 2>/dev/null
# (empty)

# Today's daily pages have summary content
head -30 "$VAULT/daily/ai-news/$(date +%Y-%m-%d).md"
```
