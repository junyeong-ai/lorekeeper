---
name: lore-process
description: Consume Lorekeeper LLM work queue. When `lore ingest` runs in queue mode (config `llm.provider: queue`), the Rust pipeline writes JSONL task files under `<vault>/.lorekeeper/queue/`. This skill drains those queues — running summarize and concept extraction using Claude Code's native LLM (no API key needed) and editing the target vault pages (plain markdown files) in place. Idempotent — partial progress is resumable; processed files move to `.lorekeeper/queue/processed/`. Run after each `lore ingest` (or daily) to enrich pages that were written with empty summary/concept sections.
when_to_use: |
  lore-process, queue process, drain queue, fill summaries,
  concept extraction run, enrich daily pages, post-ingest
argument-hint: "[--vault path] [--limit N]"
allowed-tools: |
  Bash(ls *)
  Bash(cat *)
  Bash(jq *)
  Bash(wc *)
  Bash(head *)
  Bash(mv *)
  Bash(mkdir *)
  Bash(lore *)
  Bash(date *)
  Read
  Edit
  Write
---

# lore-process — Drain the Lorekeeper LLM queue

The Rust `lore ingest` pipeline, when configured with `llm.provider: queue`, defers
all semantic work (summarization, concept extraction) by writing JSONL task
files into `<vault>/.lorekeeper/queue/{run-timestamp}-pid{PID}.jsonl`. Each
task points at a vault page that was written with an empty section awaiting LLM
content. Files appear atomically (temp + fsync + rename) — once a `.jsonl`
file is visible, every task in it is fully written and points at a page that
already exists on disk.

This skill consumes those tasks: read the queue, perform the LLM work using
your own session, edit the target pages' sections in place (each page is a
plain markdown file at `target.vault_path`), then move the processed queue file
to `.lorekeeper/queue/processed/`.

### Queue file lifecycle

```
<run>.jsonl.tmp        ←  ingest is mid-flush (transient, sub-second; not for consumption)
<run>.jsonl            ←  pending, ready to be drained by this skill
processed/<run>.jsonl  ←  drained successfully, retained 90 days
(deleted)              ←  pruned by `lore maintenance` after retention expires
```

`lore ingest` sweeps `*.jsonl.tmp` files older than 1 hour at startup
(crash debris from previous runs); `lore maintenance` does NOT touch tmp
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
lore ingest --force      # re-extracts the same events, re-queues their tasks
/lore-process              # drains the new queue file
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
    "kind": "daily-summary",
    "anchor": "## Summary"
  }
}
```

`kind` values: `summarize` | `extract-concepts` | `identify-themes` | `refine-events`
`target.kind` values: `daily-summary` | `daily-refine-events` | `daily-concepts` |
`document-summary` | `document-concepts` |
`weekly-synthesis-narrative` | `weekly-personal-narrative` |
`monthly-personal-narrative` | `quarterly-personal-narrative` |
`annual-personal-narrative` | `work-log-synthesis`
`target.anchor`: the exact section heading (e.g. `"## Summary"` or `"## 요약"`)
the pipeline wrote, resolved from i18n at queue time. Always use this as the
locate key — never hardcode headings per `target.kind`.

## Processing protocol

1. **Discover the vault root.** Read `vault.root` from `config.yaml`
   (auto-discovered at `./config.yaml` → `~/.config/lorekeeper/config.yaml`).
   The user may override with `--vault <path>`.

2. **List unprocessed queue files** in `<vault>/.lorekeeper/queue/` (top
   level only — `processed/` is the archive):
   ```bash
   ls "$VAULT/.lorekeeper/queue/"*.jsonl 2>/dev/null
   ```
   Order: oldest filename first (filenames are ISO timestamps).

3. **For each queue file** (one file = one ingest run):

   a. Read all tasks: `cat <file> | jq -c '.'`

   b. **For each task** (in file order):

      **Relevance focus.** If `input.focus` is present, it is the source's
      natural-language relevance criterion. Treat everything outside that focus
      as off-topic and exclude it: for `summarize`, cover only matching content;
      for `extract-concepts`, extract concepts ONLY from items matching the
      focus and skip off-topic items entirely (create no concepts for them).
      This is how a broad source (e.g. a news aggregator that also surfaces
      human-interest or politics) contributes focused knowledge without
      polluting the graph. No `focus` → no filtering.

      - **`kind: summarize`** — synthesize a knowledge-rich summary.
        Use the language specified in `input.locale` (e.g. `"ko"` → Korean,
        `"en"` → English). If absent, default to Korean.
        Aim for `input.max_sentences` substantive points. No preamble.

        **Source-type-aware synthesis.** Infer the source type from
        `target.vault_path` (e.g. `daily/team-slack/`, `daily/ai-news/`)
        and adapt the synthesis strategy:

        **Slack sources** (`team-slack`, `*-slack`):
        - Extract key decisions, action items with owners
        - Ignore repetitive agreement messages (ok, +1, sounds good)
        - Structure as: decision/outcome → action items → context
        - Preserve technical details, project names, and links

        **Email sources** (`email-digest`, `*-email`):
        - Extract the core ask or decision from each email
        - Identify action items with owners and deadlines
        - Skip signatures, disclaimers, forwarded-chain noise
        - For email chains, focus on the most recent exchange

        **RSS/news sources** (`ai-news`, `tech-news`):
        - Focus on key findings, announcements, techniques
        - For technical articles: what it is, why it matters, key numbers
        - Skip author bios, CTAs, navigation artifacts

        **Calendar sources** (`my-schedule`):
        - Highlight meeting outcomes and decisions if notes are present

        **Jira sources** (`my-tasks`):
        - Summarize key task status changes and deliverables

        For all types: produce genuine knowledge, not just headlines.
        Not too short (meaningless one-liners) nor too verbose (raw dump).
        Always preserve source URLs/links for traceability.

      - **`kind: refine-events`** — rewrite the raw event bodies under
        `target.anchor` (e.g. `## 주요 이벤트` or `## 주요 메시지`)
        into refined knowledge in `input.locale` language.

        For EACH `### event heading` in the section:
        1. Replace the raw body with a knowledge summary (2-5 sentences)
        2. Cover: what it is, why it matters, key details/decisions
        3. Keep the original `🔗` source link for traceability
        4. Remove noise: HN metadata (Points, Comments, Article URL),
           email signatures, raw thread dumps, Jira checklists
        5. If meeting notes are embedded, distill to decisions + action items

        The `### heading` lines themselves must be preserved — only
        replace the body text between headings.

      - **`kind: identify-themes`** — extract structured themes from
        the combined multi-source text. Identify the top N themes
        (`input.max_themes`). Write each theme as a numbered subsection
        (`### 1. Theme Title\n\nDescription`) under `target.anchor`.

      - **`kind: extract-concepts`** — identify the key named entities,
        topics, and concepts (whatever the source's domain — the focus,
        if present, names it). Output a list of concept names
        (in the source language). Each concept should also produce a
        a concept page (create if missing, merge if exists — increment
        source_count, append source ref). Use the concept path pattern
        from AGENTS.md.

        **Concept dedup.** Before creating any concept page, check for
        duplicates against the concept registry:

        1. At the start of processing a queue file, run `lore wiki concepts`
           to load the current concept registry into context.
        2. If `input.existing_concepts` is present, use it as the
           authoritative registry (it was snapshot at ingest time).
        3. For each extracted concept, check if a slug-equivalent or
           semantically equivalent concept already exists. Use existing
           slug + name when matched — do NOT create a variant.
        4. Slug normalization: NFKC → lowercase → non-alphanumeric to
           hyphen → collapse runs → trim. Same as `lk_core::slugify`.

        **Source reference format.** The `sources` array entries MUST use
        the vault-relative path pattern: `daily/{source_id}/{date}`.
        Derive from the task: `daily/{input.source_id}/{input.date}`.
        NEVER use bare source IDs like `"email-digest"`.

        **Category assignment.** If `input.categories` is present, assign
        exactly one category ID from that list to each concept. Include
        `category: {id}` in frontmatter. If absent, omit the field.

        **Concept page format.** Use exactly these frontmatter keys:
        ```yaml
        ---
        id: {slug}
        title: "{Name}"
        aliases: ["{Name}"]
        created: {YYYY-MM-DD}
        updated: {YYYY-MM-DD}
        category: {category-id}
        source_count: {N}
        sources: ["daily/{source-id}/{date}", ...]
        tags: ["{category-id}"]
        ---
        ```
        Do NOT add any keys beyond those listed above.

        **When creating a new concept page**, fill the Synthesis section
        (heading from AGENTS.md) with a 1-2 sentence definition/context of
        the concept based on the source text. Don't leave it empty — even a
        first-appearance concept benefits from a brief grounding. On merge
        (concept already exists), update the synthesis if the new source adds
        meaningful context; otherwise leave it.

   c. **Edit the target page** — the markdown file at `target.vault_path`,
      using the Edit tool (section replace). Every task carries
      `target.anchor` — the exact `## …` heading the pipeline wrote,
      resolved from i18n at queue time (e.g. `"## Summary"` for English
      or `"## 요약"` for Korean). Use it as the locate key for all task
      types — never hardcode headings per `target.kind`.

      For each task:
        1. Open the file at `target.vault_path`
        2. Locate the section heading `target.anchor` (literal match)
        3. Replace the body between this heading and the next `## `
           heading (or EOF) with the generated content
        4. Preserve frontmatter and every other section unchanged

      Additional per-kind notes:

      - **`daily-concepts`** target: replace the section body with
        `- [[Concept Name 1]]\n- [[Concept Name 2]]\n...`.
        Create each concept page (path from AGENTS.md) if it doesn't
        exist, following the concept page format above.
        Crucially include `aliases: ["Concept Name"]` so the
        `[[Concept Name]]` wikilinks resolve to the slug-named file.

      - **Work-log synthesis** (`work-log-synthesis`): the input text
        contains personal events from multiple sources, each prefixed
        with `[source_id]`. Instead of a plain summary, **group the
        events by topic/project** across sources. Format as:
        ```
        ### Topic Name
        - 📅 calendar event *(my-schedule)*
        - 💬 slack discussion *(team-slack)*
        - 📧 email follow-up *(email-digest)*
        ```
        Use source-type emoji indicators: 📅 calendar, 💬 slack,
        📧 gmail, 📄 google-drive, 🎫 jira.
        Correlate events that share the same project, topic, or concept
        across different sources. A single event may appear in multiple
        topic groups if it spans topics. Aim for concise topic names.
        Include 1-2 sentences of context per topic (not just the event title).
        Note decisions made, blockers encountered, and next steps.
        Skip trivial notifications (calendar accepts, read receipts, approvals).
        Preserve source links for traceability.

      - **Synthesis narratives** (`weekly-*`, `monthly-*`, etc.): these
        pages contain multiple `## ` sections (period, categories,
        etc.) — only the one matching `target.anchor` is replaced.
        Leave all other headings untouched.

   d. **On task failure** (page not found, edit error, malformed task):
      record the failed `task_id` and the reason. **Abort processing
      of this queue file** — do not attempt the remaining tasks. The
      queue file stays on disk so the next `/lore-process` run replays
      every task from the top (all target edits are idempotent).

4. **Only when every task in the file succeeded**, move the file to the
   archive. If any task failed, leave the file in place:
   ```bash
   mkdir -p "$VAULT/.lorekeeper/queue/processed"
   mv "$file" "$VAULT/.lorekeeper/queue/processed/"
   ```

5. **Report** to the user:
   - On full success: number of files processed and tasks completed.
   - On any failure: which file was left in place and the failed
     `task_id`s with their error messages. Exit non-zero.

## Idempotency contract

The queue file is moved to `processed/` ONLY when every task in it has
succeeded. Failure rules:

- **Any task fails** → leave the queue file in place, report the failed
  `task_id` list to the user, exit non-zero. The next `/lore-process` run
  reattempts the whole file from the start.
- **Re-running on a partially-processed file is safe** because:
  - Daily summary/concept edits replace the section body — repeating the
    edit produces identical content. No drift.
  - Concept page merging preserves original `created` and dedupes the
    `sources` array — re-adding the same source ref is a no-op.
  - `source_count` is only incremented when a genuinely new source
    ref is appended.
- **Never partially-commit progress** to the queue file itself
  (no `processed.jsonl` sidecar): the source-of-truth is the vault edits,
  which are themselves idempotent.

## When NOT to invoke

- `lore ingest` writes the queue file atomically (temp + rename) after all
  vault pages are written and before it commits dedup, so a concurrent
  ingest cannot produce a partial `.jsonl` you might consume mid-write.
  There is no append-while-reading hazard. You may still want to wait so
  the user
  sees the new pages before they get edited.
- Don't manually edit the queue-targeted sections between ingest and
  process — your edits will be overwritten.

## Example session

```bash
# After cron runs `lore ingest` at 07:00, a queue file exists:
$ ls ~/Documents/Obsidian\ Vault/.lorekeeper/queue/
2026-05-23T07-00-00Z-pid12345.jsonl

$ wc -l ~/Documents/Obsidian\ Vault/.lorekeeper/queue/*.jsonl
14 tasks pending

# Run this skill:
# /lore-process

# Result: 14 tasks processed, daily pages now have summaries and concept
# wiki-links, concept pages created/merged, queue file archived.
```

## Quick verification

After running, the user can check:
```bash
# No pending queue files
ls "$VAULT/.lorekeeper/queue/"*.jsonl 2>/dev/null
# (empty)

# Today's daily pages have summary content
head -30 "$VAULT/daily/ai-news/$(date +%Y-%m-%d).md"
```
