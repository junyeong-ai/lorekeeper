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
  Bash(wc *)
  Bash(head *)
  Bash(mv *)
  Bash(mkdir *)
  Bash(wi *)
  Bash(date *)
  mcp__obsidian__*
---

# wi-process — Drain the wiki-ingest LLM queue

The Rust `wi ingest` pipeline, when configured with `llm.provider: queue`, defers
all semantic work (summarization, concept extraction) by writing JSONL task
files into `<vault>/.wiki-ingest/queue/{run-timestamp}-pid{PID}.jsonl`. Each
task points at a vault page that was written with an empty section awaiting LLM
content. Files appear atomically (temp + fsync + rename) — once a `.jsonl`
file is visible, every task in it is fully written and points at a page that
already exists on disk.

This skill consumes those tasks: read the queue, perform the LLM work using
your own session, edit target pages via Obsidian MCP, then move the processed
queue file to `.wiki-ingest/queue/processed/`.

### Queue file lifecycle

```
<run>.jsonl.tmp        ←  ingest is mid-flush (transient, sub-second; not for consumption)
<run>.jsonl            ←  pending, ready to be drained by this skill
processed/<run>.jsonl  ←  drained successfully, retained 90 days
(deleted)              ←  pruned by `wi maintenance` after retention expires
```

`wi ingest` sweeps `*.jsonl.tmp` files older than 1 hour at startup
(crash debris from previous runs); `wi maintenance` does NOT touch tmp
files, so a concurrent maintenance run cannot race an active flush.
Only `.jsonl` files matter to this skill.

**Known limitation:** the tmp sweep is mtime-based, not PID-aware.
If an ingest process is paused (SIGSTOP) for more than 1 hour, a
later-starting ingest could delete its tmp; the paused ingest's flush
will then fail with ENOENT and that run's LLM tasks are lost. Pages
written before the failure remain on disk and the affected events
are NOT marked seen in dedup (the flush failure aborts before the
dedup commit), so the recovery path is simply:

```bash
wi ingest --force        # re-extracts the same events, re-queues their tasks
/wi-process              # drains the new queue file
```

In practice, cron-scheduled ingests never hit this case.

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
        exists — increment reference_count, append source ref).

   c. **Edit the target page** via Obsidian MCP. All daily and synthesis
      templates emit stable section anchors, so the section is always
      present (with an empty body when queued by `wi ingest`):

      - **`daily-summary`** target: locate `## 요약` and replace its
        body (everything between this heading and the next `## ` heading)
        with the synthesized summary.

      - **`daily-concepts`** target: locate `## 관련 개념` and replace
        its body with `- [[Concept Name 1]]\n- [[Concept Name 2]]\n...`.
        Create each concept page in `wiki/concepts/` if it doesn't
        exist (use frontmatter id, name, first_seen, last_seen,
        reference_count, sources fields — match the format from existing
        concept pages).

      - **Synthesis narratives** — each `target.kind` maps to one
        specific anchor heading. Search the page for the EXACT heading
        text from this table (not "the first `##`" — these pages also
        contain `## 기간`, `## 업무 카테고리`, etc. that must be left
        alone). Both the bundled Jinja template and the fallback
        renderer guarantee the listed heading exists.

        | `target.kind` | Anchor heading |
        |---|---|
        | `weekly-synthesis-narrative` | `## 이번 주 핵심 주제` |
        | `weekly-personal-narrative`  | `## 핵심 요약` |
        | `monthly-narrative`          | `## 핵심 요약` |
        | `quarterly-narrative`        | `## 주요 성과 Top 5` |
        | `annual-narrative`           | `## 종합 요약` |

        Replace the body of the anchor (everything between this heading
        and the next `## ` heading, or EOF) with the generated
        narrative. Preserve frontmatter and every other section heading
        unchanged.

   d. **On task failure** (page not found, MCP error, malformed task):
      record the failed `task_id` and the reason. **Abort processing
      of this queue file** — do not attempt the remaining tasks. The
      queue file stays on disk so the next `/wi-process` run replays
      every task from the top (all target edits are idempotent).

4. **Only when every task in the file succeeded**, move the file to the
   archive. If any task failed, leave the file in place:
   ```bash
   mkdir -p "$VAULT/.wiki-ingest/queue/processed"
   mv "$file" "$VAULT/.wiki-ingest/queue/processed/"
   ```

5. **Report** to the user:
   - On full success: number of files processed and tasks completed.
   - On any failure: which file was left in place and the failed
     `task_id`s with their error messages. Exit non-zero.

## Idempotency contract

The queue file is moved to `processed/` ONLY when every task in it has
succeeded. Failure rules:

- **Any task fails** → leave the queue file in place, report the failed
  `task_id` list to the user, exit non-zero. The next `/wi-process` run
  reattempts the whole file from the start.
- **Re-running on a partially-processed file is safe** because:
  - Daily summary/concept edits replace the section body — repeating the
    edit produces identical content. No drift.
  - Concept page merging preserves original `first_seen` and dedupes the
    `sources` array — re-adding the same source ref is a no-op.
  - `reference_count` is only incremented when a genuinely new source
    ref is appended.
- **Never partially-commit progress** to the queue file itself
  (no `processed.jsonl` sidecar): the source-of-truth is the vault edits,
  which are themselves idempotent.

## When NOT to invoke

- `wi ingest` writes the queue file atomically (temp + rename) after all
  vault pages are written and before it commits dedup, so a concurrent
  ingest cannot produce a partial `.jsonl` you might consume mid-write.
  There is no append-while-reading hazard. You may still want to wait so
  the user
  sees the new pages before they get edited.
- Don't manually edit the queue-targeted sections between ingest and
  process — your edits will be overwritten.

## Example session

```bash
# After cron runs `wi ingest` at 07:00, a queue file exists:
$ ls ~/Documents/Obsidian\ Vault/.wiki-ingest/queue/
2026-05-23T07-00-00Z-pid12345.jsonl

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
