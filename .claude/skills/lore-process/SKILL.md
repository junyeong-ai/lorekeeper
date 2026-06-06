---
name: lore-process
description: Drain the Lorekeeper LLM work queue after `lore ingest` runs in queue mode — fill the empty summary and concept sections of freshly-written vault pages using Claude Code's own LLM (no API key, no separate billing). Idempotent and resumable. Run after each ingest, or on a daily schedule.
when_to_use: |
  queue process, drain queue, fill summaries,
  concept extraction run, enrich daily pages, post-ingest
argument-hint: "[--vault path]"
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

The Rust `lore ingest` pipeline, when configured with `llm.provider: queue`,
defers all semantic work (summarization, concept extraction) by writing JSONL
task files into `<vault>/.lorekeeper/queue/{run-timestamp}-pid{PID}.jsonl`.
Each task points at a vault page written with an empty section awaiting LLM
content. This skill consumes those tasks: read the queue, perform the LLM
work in your own session, edit the target pages' sections in place, then move
the processed queue file to `.lorekeeper/queue/processed/`.

The protocol below is the integrity contract — follow it exactly. The
generation specs (what good output looks like per task kind) live in
[references/processing-kinds.md](references/processing-kinds.md); read that
file before processing tasks, and re-read it if this session has been
compacted since you last read it.

## Safety rules (never violate, even mid-session)

1. **Process only `current` tasks.** `lore queue status --json` is the
   authoritative classifier — never edit a page for a `stale` or
   `missing-target` task.
2. **Locate sections only by `target.anchor`** (the exact `## …` heading the
   pipeline wrote, resolved from i18n at queue time). Never hardcode a
   heading per `target.kind`.
3. **Never hand-write a concept's `## Sources` body or `source_count`** —
   both are machine-owned; `lore graph backlinks-sync` re-derives them from
   forward `[[wikilink]]`s on origin pages.
4. **Move a queue file to `processed/` only when every task in it
   succeeded.** On any failure, leave the file in place and stop processing
   that file.
5. **The target page's frontmatter is read-only**, with one exception:
   `daily-refine-events` stamps `llm_inputs.refine_events_done`. Concept
   pages the skill creates or merges are not the target page — their
   frontmatter follows the concept page format and the shared dedup
   algorithm (alias appends allowed).

## Materialized-view contract

Daily pages are materialized views with two kinds of fields:

- **Structural** (frontmatter, raw event list, headings) — owned by the Rust
  pipeline, re-rendered on every ingest. This skill never writes them.
- **Semantic** (summary body, refined event bodies, concept wiki-links) —
  owned by this skill, preserved across re-renders.

The pipeline decides what needs work before the queue file exists: a task is
enqueued only when its section is missing or its inputs changed. This skill's
sole obligation to that machinery is to write **only** LLM-produced content
into the target sections, never structural artifacts. Completion is read back
two ways (see Cache shapes below): a fill-empty section (summary, concepts) is
done once its body is non-empty; the in-place event refine instead stamps the
`llm_inputs.refine_events_done` marker, since its section is non-empty from the
first render.

## Queue format & recovery

The queue file lifecycle (`*.jsonl.tmp` → `*.jsonl` → `processed/`), the
crash-recovery path, and the full per-line task JSON schema are read-once
reference — see [references/queue-format.md](references/queue-format.md).
The essentials: a visible `.jsonl` is fully written and every
`target.vault_path` already exists (atomic temp + fsync + rename); only
`.jsonl` files (not `.tmp`) are for consumption.

## Processing protocol

1. **Discover the vault root.** Read `vault.root` from `config.yaml`
   (auto-discovered at `./config.yaml` → `~/.config/lorekeeper/config.yaml`).
   The user may override with `--vault <path>`. Then load the concept registry
   for this run: `lore wiki concepts` (slugs, names, aliases) — the queue task
   carries no concept registry, so this on-disk snapshot plus a created-this-run
   set is the full dedup baseline (see `shared/concept-dedup.md`).

2. **List unprocessed queue files** in `<vault>/.lorekeeper/queue/` (top
   level only — `processed/` is the archive):
   ```bash
   ls "$VAULT/.lorekeeper/queue/"*.jsonl 2>/dev/null
   ```
   Order: oldest filename first (filenames are ISO timestamps).

3. **For each queue file** (one file = one ingest run):

   a. Read all tasks: `cat <file> | jq -c '.'`

   b. **Stale-task guard (authoritative).** Run

      ```bash
      lore queue status --json
      ```

      It classifies every pending task as `current`, `stale`, or
      `missing-target` by comparing each `task.cache_hash` against its
      target page's `llm_inputs.<key>` — computed in tested Rust. **Process
      only `current` tasks; skip `stale` and `missing-target` without
      editing their pages.** Match tasks by `task_id`.

   c. **For each `current` task** (in file order), resolve its state from
      the page's `llm_inputs.<key>` frontmatter, where `<key>` is:

      | target.kind                                           | frontmatter key |
      |-------------------------------------------------------|-----------------|
      | `daily-summary`, `document-summary`                   | `summary`       |
      | `daily-refine-events`                                 | `refine_events` |
      | `daily-concepts`, `document-concepts`                 | `concepts`      |
      | `work-log-synthesis`                                  | `topic_summary` |
      | `weekly-synthesis-themes`                             | `themes`        |
      | `weekly-/monthly-/quarterly-/annual-review-narrative` | `narrative`     |

      Three outcomes, in order:

      1. **Stale (page rewrote with a different input):**
         `page.llm_inputs.<key>` present but ≠ `task.cache_hash`. A later
         ingest superseded this task — writing now would inject content
         keyed to an older input under the newer hash, freezing the
         mismatch forever. **Drop as successful, no edit.**

      2. **Cache hit (work already done):** hashes equal AND the body under
         `target.anchor` is non-empty. **Treat as successful, no edit.**

      3. **Process:** hashes equal AND the section body is empty. Do the
         LLM work per [references/processing-kinds.md](references/processing-kinds.md)
         and edit the page.

      A missing `llm_inputs.<key>` field with `task.cache_hash` set means
      the page is in an inconsistent state (template/pipeline drift); fail
      the task and report it.

      **Refine-events completion contract.** `daily-refine-events` rewrites
      the raw event bodies *in place*, so the events section is non-empty
      from the first render — emptiness can't signal "done." Completion is
      tracked by a SECOND field, `llm_inputs.refine_events_done`, which
      **you own.** Resolve the outcomes like this:

      - `task.cache_hash` ≠ `page.refine_events` → **drop** as stale, no
        edit. *(outcome 1)*
      - `task.cache_hash` = `page.refine_events` AND
        `page.refine_events_done` = `page.refine_events` → already refined
        for this exact input → **skip**, no edit. *(outcome 2)*
      - `task.cache_hash` = `page.refine_events` AND `refine_events_done`
        is absent or ≠ `refine_events` → **process**, then set
        `llm_inputs.refine_events_done` = `task.cache_hash`, copied
        verbatim (a 32-char hex string). Leave `llm_inputs.refine_events`
        untouched — the pipeline owns it. *(outcome 3)*

   d. **Edit the target page** — the markdown file at `target.vault_path`,
      using the Edit tool (section replace):

      1. Open the file at `target.vault_path`
      2. Locate the section heading `target.anchor` (literal match)
      3. Replace the body between this heading and the next `## ` heading
         (or EOF) with the generated content
      4. Preserve frontmatter and every other section unchanged
         (`daily-refine-events` additionally stamps its completion field —
         see above)

      Concept pages created or merged along the way follow the shared
      convergence algorithm in
      `${CLAUDE_SKILL_DIR}/../shared/concept-dedup.md`.

   e. **On task failure** (page not found, edit error, malformed task):
      record the failed `task_id` and the reason. **Abort processing of
      this queue file** — do not attempt the remaining tasks. The queue
      file stays on disk so the next `/lore-process` run replays every
      task from the top (all target edits are idempotent).

4. **Only when every task in the file succeeded**, move the file to the
   archive. If any task failed, leave the file in place:
   ```bash
   mkdir -p "$VAULT/.lorekeeper/queue/processed"
   mv "$file" "$VAULT/.lorekeeper/queue/processed/"
   ```

5. **Finalize** — concept `## Sources` / `source_count` were left empty on
   purpose; reconcile them from the wikilink graph, then refresh the catalog:
   ```bash
   lore graph backlinks-sync   # re-derive every concept's ## Sources + source_count
   lore wiki index             # refresh the catalog
   ```
   `lore ingest` does NOT run these — always run them yourself after
   `/lore-process`. (The `lore-daily-ingest` scheduled task chains
   ingest → /lore-process → these for an automated daily run.)

6. **Report** to the user:
   - On full success: number of files processed and tasks completed.
   - On any failure: which file was left in place and the failed `task_id`s
     with their error messages. Exit non-zero.

## Idempotency contract

The queue file is moved to `processed/` ONLY when every task in it has
succeeded. Failure rules:

- **Any task fails** → leave the queue file in place, report the failed
  `task_id` list to the user, exit non-zero. The next `/lore-process` run
  reattempts the whole file from the start.
- **Re-running on a partially-processed file is safe** because:
  - Section edits replace the body — repeating the edit produces identical
    content. No drift.
  - Concept page merging preserves the original `created` and reuses the
    existing slug; the `## Sources` body is never written during processing
    (it stays empty for `backlinks-sync` to own), so a re-run touches
    nothing there.
  - `source_count` is never written by processing — new pages start at `0`,
    ingest preserves the existing value, and only `lore graph
    backlinks-sync` computes the authoritative count.
- **Never partially-commit progress** to the queue file itself (no
  `processed.jsonl` sidecar): the source-of-truth is the vault edits, which
  are themselves idempotent.

## When NOT to invoke

`lore ingest` writes the queue file atomically (temp + rename) after all
vault pages are written, so a concurrent ingest cannot produce a partial
`.jsonl` you might consume mid-write. There is no append-while-reading
hazard. You may still want to wait so the user sees the new pages before
they get edited.

## Regenerating a section

To regenerate a section that was already filled, invalidate its cache and
re-ingest that day (`lore ingest --date <day>`). The pipeline sees the cache
miss, re-queues the task, and `/lore-process` fills it on the next run. No
flag, no skill argument — the page itself is the cache key. How to invalidate
depends on the section's cache shape:

- **Fill-empty sections** (summary, concepts, narratives): delete the body
  OR the `llm_inputs.<key>` line.
- **In-place rewrite** (`daily-refine-events`): delete the
  `llm_inputs.refine_events_done` line. Emptying the event body does NOT
  force a re-run — the event list is structural and the pipeline re-renders
  it, so completion is tracked only by `refine_events_done`.
