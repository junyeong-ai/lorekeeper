---
name: lore-process
version: 0.12.1
description: Drain the Lorekeeper LLM work queue after `lore ingest` runs in queue mode — fill the empty LLM-owned sections (summaries, refined events, concepts, themes, reviews) of freshly-written vault pages using Claude Code's own LLM (no API key, no separate billing). Idempotent and resumable. Run after each ingest, or on a daily schedule.
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
defers all semantic work (summaries, refined events, concepts, themes, reviews)
by writing JSONL task files into
`<vault>/.lorekeeper/queue/{run-id}.jsonl` (run-id = `{timestamp}-pid{PID}-{seq}`).
Each task points at a vault page written with an empty section awaiting LLM
content. This skill consumes those tasks: read the queue, perform the LLM
work in your own session, edit the target pages' sections in place, then move
the processed queue file to `.lorekeeper/queue/processed/`.

The protocol below is the integrity contract — follow it exactly. The
generation specs (what good output looks like per task kind) live in
[references/processing-kinds.md](references/processing-kinds.md), and the
per-source-type synthesis/extraction strategy (keyed on each task's
`input.source_type`) lives in
[references/source-types.md](references/source-types.md); read both before
processing tasks, and re-read them if this session has been compacted since
you last read them.

## Safety rules (never violate, even mid-session)

1. **Process only `current` tasks.** `lore queue status --json` is the
   authoritative classifier — never edit a page for a `done`, `stale`, or
   `missing-target` task. It answers three questions per task: is the page still
   the one this task was made for (`cache_hash` vs `llm_inputs.<key>`), has this
   exact input already been answered (`llm_inputs.<key>_done`), and does the page
   still carry the section the task's `anchor` names. Only a task that passes the
   first, fails the second, and has somewhere to land is work — an anchor the page
   no longer carries means the heading vocabulary changed (a locale switch), which
   no amount of waiting undoes. A task you filled and
   stamped earlier in this run therefore reads `done` if you re-classify — that
   is correct, and it is skipped, not failed. The "this run is finished" signal
   is the queue file moving to `processed/`, not `queue status` reaching zero.
2. **Locate sections only by `target.anchor`** (the exact `## …` heading the
   pipeline wrote, resolved from i18n at queue time). Never hardcode a
   heading per `target.kind`.
3. **Never hand-write a concept's sources-section body or `source_count`** —
   both are machine-owned; `lore graph backlinks-sync` re-derives them from
   forward concept links on origin pages.
4. **Move a queue file to `processed/` only when every task in it
   succeeded.** On any failure, leave the file in place and stop processing
   that file.
5. **The target page's frontmatter is read-only**, except for the one
   `llm_inputs.<key>_done` completion marker you own and MUST stamp when a task
   finishes (the per-kind key is in the step 3c table).
6. **Never write a concept page, and never write a related-concepts section.**
   `extract-concepts` emits a result file; `lore queue apply` materializes both.
   Concept pages are shared between origin pages and merge under rules that
   already exist as tested Rust — restating them here would be a second
   implementation, and a drain that wrote them could not run two pages at once.

## Materialized-view contract

Daily pages are materialized views with two kinds of fields:

- **Structural** (frontmatter, raw event list, headings) — owned by the Rust
  pipeline, re-rendered on every ingest. This skill never writes them.
- **Semantic** (summary body, refined event bodies) — owned by this skill,
  preserved across re-renders. Concept links are semantic too, but they are
  written by `lore queue apply` from the concepts this skill reports, so that
  their slug and link format match the pages it creates.

The pipeline decides what needs work before the queue file exists: a task is
enqueued only when its section is missing or its inputs changed. This skill's
sole obligation to that machinery is to write **only** LLM-produced content
into the target sections, never structural artifacts, and to stamp the section's
`llm_inputs.<key>_done` completion marker when finished (step 3c). Completion is
uniformly marker-signalled — a non-empty body never signals done — so a section
that is legitimately empty (a focus-filtered summary, an extraction that found
nothing, a trivial-only work-log, an empty-period review) stays done instead of
re-enqueueing forever.

## Queue format & recovery

The queue file lifecycle (`*.jsonl.tmp` → `*.jsonl` → `processed/`), the
crash-recovery path, and the full per-line task JSON schema are read-once
reference — see [references/queue-format.md](references/queue-format.md).
The essentials: a visible `.jsonl` is fully written and every
`target.vault_path` already exists (atomic temp + fsync + rename); only
`.jsonl` files (not `.tmp`) are for consumption.

## Processing protocol

1. **Discover the vault root.** Run `lore config vault-root` — it prints the
   resolved absolute path and nothing else. Never read `vault.root` out of
   `config.yaml` yourself: a relative value there resolves against the config
   file's directory (not the CWD), and the config file itself is auto-discovered
   (`./config.yaml` → `~/.config/lorekeeper/config.yaml`), so parsing the YAML
   reproduces two resolution rules that already exist in the binary. The wiki dir is
   `vault.dirs.wiki` (default `wiki`); its `AGENTS.md` carries the page formats
   and the Concept convergence contract. Then load the concept registry
   for this run: `lore wiki concepts` (slugs, names, aliases) — the queue task
   carries no concept registry, so this on-disk snapshot plus a created-this-run
   set is the full dedup baseline (see the vault AGENTS.md § Concept convergence).

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

      It classifies every pending task as `current`, `done`, `stale`, or
      `missing-target` from its target page's `llm_inputs.<key>` and
      `llm_inputs.<key>_done` — computed in tested Rust. **Process only
      `current` tasks; skip the rest without editing their pages.** Match tasks
      by `task_id`.

   c. **For each `current` task** (in file order), resolve its state from the
      page's frontmatter. Completion is **uniformly marker-signalled**: every kind
      has an `llm_inputs.<key>` (pipeline-owned input hash) and a companion
      `llm_inputs.<key>_done` marker **you own and MUST stamp** — even when the
      result is empty. Body emptiness NEVER signals done, because many sections can
      be legitimately empty (a focus-filtered summary that matched nothing, an
      extraction that found nothing, a trivial-only work-log, an empty-period
      review); inferring completion from a non-empty body would re-enqueue every
      such result forever. Find the row for `target.kind`:

      | target.kind | input key | completion marker (you stamp) |
      |-------------|-----------|-------------------------------|
      | `daily-summary`, `document-summary` | `summary` | `summary_done` |
      | `daily-refine-events` | `refine_events` | `refine_events_done` |
      | `daily-concepts`, `document-concepts` | `concepts` | `concepts_done` |
      | `work-log-synthesis` | `topic_summary` | `topic_summary_done` |
      | `weekly-synthesis-themes` | `themes` | `themes_done` |
      | `weekly-review-narrative`, `monthly-review-narrative` | `narrative` | `narrative_done` |
      | `quarterly-review-narrative`, `annual-review-narrative` | `narrative` | `narrative_done` |

      Resolve with the input key and marker for that row, in order:

      1. **Stale:** `task.cache_hash` ≠ `page.<input key>` (or the input key is
         absent). A later ingest superseded this task — writing now would inject
         content keyed to an older input under the newer hash, freezing the
         mismatch forever. **Drop as successful, no edit** — exactly how `lore
         queue status` classifies it; never invent a different outcome.
      2. **Cache hit:** `task.cache_hash` = `page.<input key>` AND
         `page.<marker>` = `page.<input key>` → already done for this exact
         input. **Skip, no edit.**
      3. **Process:** `task.cache_hash` = `page.<input key>` AND `<marker>` is
         absent or ≠ the input key. Do the LLM work per
         [references/processing-kinds.md](references/processing-kinds.md) — a
         genuinely empty result is fine where the kind allows it (write nothing
         rather than invent low-value content) — then set `llm_inputs.<marker>` =
         `task.cache_hash`, copied verbatim (a 32-char hex string). Leave the
         pipeline-owned input key untouched.

   d. **Edit the target page** — the markdown file at `target.vault_path`,
      using the Edit tool (section replace):

      1. Open the file at `target.vault_path`
      2. Locate the section heading `target.anchor` (literal match)
      3. Replace the body between this heading and the next `## ` heading
         (or EOF) with the generated content
      4. Preserve frontmatter and every other section unchanged, then stamp the
         task's `llm_inputs.<key>_done` completion marker (the table under 3c)

      Concept pages created or merged along the way follow the
      **Concept convergence** section of the vault's `AGENTS.md`.

   e. **On task failure** (page not found, edit error, malformed task):
      record the failed `task_id` and the reason. **Abort processing of
      this queue file** — do not attempt the remaining tasks. The queue
      file stays on disk so the next `/lore-process` run replays every
      task from the top (all target edits are idempotent).

4. **Only when every task in the file succeeded**, move the file to the
   archive. If any task failed, leave the file in place:
   ```bash
   mkdir -p "$VAULT/.lorekeeper/queue/processed"
   mv "$file" "$VAULT/.lorekeeper/queue/processed/" 2>/dev/null \
     || [ -f "$VAULT/.lorekeeper/queue/processed/$(basename "$file")" ]
   ```
   The fallback is not error-swallowing: `lore queue prune` runs on its own
   schedule and retires a run whose every task is already answered, so it can
   archive this file while you are working through it (you read every task in
   step 3a, so nothing is lost). A file already sitting in `processed/` is the
   outcome this step wanted — treat it as success. Any other `mv` failure leaves
   neither file present and fails the check, which is a real failure to report.

5. **Finalize (mandatory — do not skip).** Concept `## Sources` sections and
   `source_count` are deliberately left empty during processing; they are
   machine-owned and reconciled here from the link graph, then the catalog
   is refreshed. Run both and confirm each exits 0:
   ```bash
   lore queue apply            # materialize reported concepts into pages + links
   lore graph backlinks-sync   # re-derive every concept's ## Sources + source_count
   lore wiki index             # refresh the catalog
   lore wiki map               # refresh the citation-cluster navigation map
   ```
   Run these on EVERY `/lore-process` invocation that processed at least one
   task — skipping them leaves new concepts with no `## Sources` and a stale
   `source_count` until some later run happens to reconcile. If any command
   exits non-zero, treat the run as failed and surface the error in step 6
   instead of reporting success. `lore ingest` never runs these. (The
   `lore-daily.sh` pipeline chains ingest → /lore-process → these.)

6. **Report** to the user:
   - On full success: number of files processed and tasks completed, and confirm
     the step-5 Finalize ran (`queue apply` + `backlinks-sync` + `index` + `map`, all exited 0).
   - On any failure (including a non-zero Finalize command): which file was left
     in place and the failed `task_id`s (or the failed command) with their error
     messages, then stop — leave remaining files for the next run.

## Idempotency contract

The queue file is moved to `processed/` ONLY when every task in it has
succeeded. Failure rules:

- **Any task fails** → leave the queue file in place, report the failed
  `task_id` list to the user, and stop. The next `/lore-process` run
  reattempts the whole file from the start.
- **Re-running on a partially-processed file is safe** because:
  - Section edits replace the body — repeating the edit produces identical
    content. No drift.
  - Concept page merging preserves the original `created` and reuses the
    existing slug; the sources-section body is never written during processing
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
flag, no skill argument — the page itself is the cache key. To invalidate,
delete the section's `llm_inputs.<key>_done` marker line (e.g. `summary_done`,
`concepts_done`, `narrative_done` — the per-kind key is in the step 3c table).
Emptying the body does NOT force a re-run: completion is tracked only by the
marker, so an empty-but-done result stays cached.
