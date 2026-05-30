---
name: lore-process
description: Consume Lorekeeper LLM work queue. When `lore ingest` runs in queue mode (config `llm.provider: queue`), the Rust pipeline writes JSONL task files under `<vault>/.lorekeeper/queue/`. This skill drains those queues — running summarize and concept extraction using Claude Code's native LLM (no API key needed) and editing the target vault pages (plain markdown files) in place. Idempotent — partial progress is resumable; processed files move to `.lorekeeper/queue/processed/`. Run after each `lore ingest` (or daily) to enrich pages that were written with empty summary/concept sections.
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

## Materialized-view contract

Daily pages are materialized views with two kinds of fields:

- **Structural** (frontmatter, raw event list, headings) — owned by the Rust
  pipeline, re-rendered on every ingest.
- **Semantic** (summary body, refined event bodies, concept wiki-links) — owned
  by this skill, preserved across re-renders, and invalidated only when the
  underlying LLM input changes.

The pipeline records a BLAKE3-128 hash of each LLM task's cache identity under
the target page's `llm_inputs.<key>` frontmatter at render time. On a re-ingest,
the pipeline compares the new hash against the cached one: if they match and
the section is non-empty, the task is **not** enqueued at all. This is the
designed self-healing path — a successful previous run never triggers redundant
LLM work, regardless of `--force`.

That cache rests on the contract that this skill writes only LLM-produced
content into the target sections, never structural artifacts. The hash key for
each task kind is documented under [Per-kind processing](#per-kind-processing).

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

Each line in a queue file is one task. All `vault_path` values are
vault-relative paths resolved by the pipeline from `vault.dirs.*` —
never construct them manually; use the path patterns from AGENTS.md.

```json
{
  "task_id": "sum-2026-05-23T07-00-00Z-000",
  "kind": "summarize",
  "created_at": "2026-05-23T07:00:00Z",
  "cache_hash": "0123456789abcdef0123456789abcdef",
  "input": {
    "text": "<events concatenated for summarization>",
    "max_sentences": 5,
    "source_type": "rss"
  },
  "target": {
    "vault_path": "<daily>/ai-news/2026-05-23.md",
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
`target.anchor`: the exact section heading (e.g. `"## Summary"`, or its localized
form per AGENTS.md) the pipeline wrote, resolved from i18n at queue time. Always use this as the
locate key — never hardcode headings per `target.kind`.

`cache_hash` is BLAKE3-128 (32 hex chars) of the cache-identity subset of
`input` (excludes registry hints like `existing_concepts`). It equals the
value the pipeline wrote into `target.vault_path`'s `llm_inputs.<key>`
frontmatter at queue time. The skill MUST verify the page's current
frontmatter matches before writing — see "Stale-task guard" below.

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

   b. **Stale-task guard (authoritative).** Run

      ```bash
      lore queue status --json
      ```

      It classifies every pending task as `current`, `stale`, or
      `missing-target` by comparing each `task.cache_hash` against its target
      page's `llm_inputs.<key>` — the same check, computed in tested Rust.
      **Process only `current` tasks; skip `stale` and `missing-target`
      without editing their pages.** Match tasks by `task_id`. This is the
      source of truth for the guard; the per-kind frontmatter detail below
      explains the same logic and the completion signals you still write.

   c. **For each `current` task** (in file order):

      Open `target.vault_path` and read its `llm_inputs.<key>` frontmatter,
      where `<key>` is the task key for this `target.kind`:

      | target.kind                                                            | frontmatter key   |
      |------------------------------------------------------------------------|-------------------|
      | `daily-summary`, `document-summary`                                    | `summary`         |
      | `daily-refine-events`                                                  | `refine_events`   |
      | `daily-concepts`, `document-concepts`                                  | `concepts`        |
      | `work-log-synthesis`                                                   | `topic_summary`   |
      | `weekly-synthesis-narrative`                                           | `themes`          |
      | `weekly-/monthly-/quarterly-/annual-personal-narrative`                | `narrative`       |

      The three outcomes below are keyed on `llm_inputs.<key>` (the stale
      reference point) for EVERY kind. They differ only in the *completion*
      signal: most kinds use a non-empty section body; `daily-refine-events`
      uses a second frontmatter field (see its contract after the outcomes).

      Three outcomes, in order:

      1. **Stale task (page rewrote with a different input):**
         `page.llm_inputs.<key>` is present but ≠ `task.cache_hash`. A later
         ingest superseded this task — writing now would inject content keyed
         to an older input under the newer hash, freezing the mismatch
         forever. **Drop the task as successful without modifying the page.**

      2. **Cache hit (work already done):**
         `page.llm_inputs.<key>` = `task.cache_hash` AND the body under
         `target.anchor` is non-empty. The Rust pipeline normally prevents
         such tasks from being enqueued; if you see one, it usually means the
         page was hand-edited between ingest and process. **Treat as
         successful, no edit.**

      3. **Process normally:**
         `page.llm_inputs.<key>` = `task.cache_hash` AND the section body is
         empty. This is the standard case. Do the LLM work and edit the page.

      A missing `llm_inputs.<key>` field with `task.cache_hash` set means the
      page is in an inconsistent state (template/pipeline drift); fail the
      task and report it.

      **Refine-events completion contract.** `daily-refine-events` rewrites the
      raw event bodies *in place*, so the events section is non-empty from the
      first render (the raw event list is structural) — emptiness can't signal
      "done." The pipeline pre-stamps `llm_inputs.refine_events` with the
      current-input hash (the stale reference point, exactly like every other
      kind), and completion is tracked by a SECOND field,
      `llm_inputs.refine_events_done`, which **you own.** Resolve the three
      outcomes like this:

      - `task.cache_hash` ≠ `page.refine_events` → a newer ingest re-rendered the
        page → **drop** as stale, no edit. *(outcome 1)*
      - `task.cache_hash` = `page.refine_events` AND
        `page.refine_events_done` = `page.refine_events` → already refined for
        this exact input → **skip**, no edit. *(outcome 2)*
      - `task.cache_hash` = `page.refine_events` AND `refine_events_done` is
        absent or ≠ `refine_events` → **process** (see `kind: refine-events`
        below), then set `llm_inputs.refine_events_done` = `task.cache_hash`. *(outcome 3)*

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

        **Source-type-aware synthesis.** Adapt the strategy to
        `input.source_type` (the adapter type verbatim from config; never
        guess it from the vault path). When `source_type` is absent
        (cross-source syntheses such as the work-log), apply the generic
        guidance without a type bias. Per-type strategy:
        see `references/source-types.md`.

        For all types: produce genuine knowledge, not just headlines.
        Not too short (meaningless one-liners) nor too verbose (raw dump).
        Always preserve source URLs/links for traceability.

      - **`kind: refine-events`** — rewrite the raw event bodies under
        `target.anchor` (e.g. `## Key Events` or `## Key Messages`, localized per AGENTS.md)
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

        **After refining, stamp completion** (this kind only): set the page's
        `llm_inputs.refine_events_done` frontmatter to `task.cache_hash`, copied
        verbatim (a 32-char hex string). Leave `llm_inputs.refine_events`
        untouched (the pipeline owns it). The `_done` stamp is what marks the
        refinement complete so the next ingest preserves your rewrite instead of
        re-enqueuing it. Do not compute or alter the value — copy
        `task.cache_hash` exactly.

      - **`kind: identify-themes`** — extract structured themes from
        the combined multi-source text. Identify the top N themes
        (`input.max_themes`). Write each theme as a numbered subsection
        (`### 1. Theme Title\n\nDescription`) under `target.anchor`.
        Write the titles and descriptions in `input.locale` language
        (e.g. `"ko"` → Korean, `"en"` → English); default to Korean if
        absent.

      - **`kind: extract-concepts`** — identify the key named entities,
        topics, and concepts (whatever the source's domain — the focus,
        if present, names it). Output a list of concept names
        (in the source language). Each concept should also produce a
        a concept page (create if missing, merge if exists). Fill the origin
        page's `## Related Concepts` with a `[[concept]]` forward link — the single source
        of truth `backlinks-sync` reads; leave the concept's `## Sources` /
        `source_count` to it. Use the concept path pattern from AGENTS.md.

        **What counts as a concept** (keep the graph high-signal, not noisy):
        extract durable, reusable knowledge nodes — technologies, named methods,
        architectures, patterns, standards, organizations. Do NOT mint concepts for
        transient specifics, generic English words, dates/numbers, one-off phrasings,
        or anything that would never plausibly be cited by a second source. When in
        doubt, prefer the established broader concept over a narrow variant. A good
        rule: a concept earns a page only if a future unrelated source could
        independently link to it. Fewer, load-bearing concepts beat many shallow ones.

        `input.source_type` carries the originating adapter type; use it to
        scope what counts as a concept (per-type scoping:
        see `references/source-types.md`). Never invent it from the path.

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

        **Source references are machine-owned — do NOT hand-write them.** Leave the
        concept's `## Sources` body EMPTY and `source_count: 0`. Record the
        citation instead as a forward `[[wikilink]]` in the ORIGIN page's
        related-concepts section (the `## Related Concepts` of `target.vault_path`, which you
        fill anyway). `lore graph backlinks-sync` re-derives every concept's `## Sources`
        + `source_count` from those forward links — so a concept cited by several
        pages in one batch (e.g. the same topic on two daily pages) is counted
        correctly, whereas hand-writing one ref per task undercounts it. This is
        identical to how lore-capture / lore-wiki / lore-extract treat concept
        sources: the wikilink graph is the single source of truth. Run
        `lore graph backlinks-sync` in Finalize (step 5) — `lore ingest` does NOT
        run it; the `lore-daily-ingest` scheduled task chains it after this skill.

        **Category assignment.** Hard constraint: the `category` value MUST
        be one of the IDs in `input.categories` (verbatim string match) or
        the field MUST be omitted entirely. Never invent a new category, never
        substitute a synonym, never abbreviate. If no listed category fits the
        concept, leave the `category` field off — `lore graph lint` surfaces
        unknown categories as findings, so an invented category is observable
        drift that breaks the index. When `input.categories` is absent or
        empty, omit the field unconditionally.

        Same rule for the `tags` array: when a category is assigned, include
        that category ID as the page's sole tag (`tags: ["{category-id}"]`).
        When no category is assigned, use `tags: ["concept"]`.

        **Concept page format.** Use exactly these frontmatter keys:
        ```yaml
        ---
        id: {slug}
        title: "{Name}"
        aliases: ["{Name}"]
        created: {YYYY-MM-DD}
        updated: {YYYY-MM-DD}
        category: {category-id}
        source_count: 0
        tags: ["{category-id}"]
        ---
        ```
        Do NOT add any keys beyond those listed above. Leave the `## Sources`
        body EMPTY and `source_count: 0` at write time — both are machine-owned and
        re-derived by `lore graph backlinks-sync` from the origin pages' forward
        `[[wikilink]]`s. Never hand-write citations, in the body or in frontmatter.

        **When creating a new concept page**, fill the Synthesis section
        (heading from AGENTS.md) with a 1-2 sentence definition/context of
        the concept based on the source text. Don't leave it empty — even a
        first-appearance concept benefits from a brief grounding. On merge
        (concept already exists), update the synthesis if the new source adds
        meaningful context; otherwise leave it.

   d. **Edit the target page** — the markdown file at `target.vault_path`,
      using the Edit tool (section replace). Every task carries
      `target.anchor` — the exact `## …` heading the pipeline wrote,
      resolved from i18n at queue time (e.g. `"## Summary"`, or its localized
      form per AGENTS.md). Use it as the locate key for all task
      types — never hardcode headings per `target.kind`.

      For each task:
        1. Open the file at `target.vault_path`
        2. Locate the section heading `target.anchor` (literal match)
        3. Replace the body between this heading and the next `## `
           heading (or EOF) with the generated content
        4. Preserve frontmatter and every other section unchanged

      Additional per-kind notes:

      - **`daily-refine-events`** target: in addition to replacing the
        section body, set `llm_inputs.refine_events_done` in the frontmatter to
        `task.cache_hash` (verbatim), leaving `llm_inputs.refine_events` as is.
        This is the one task kind that edits frontmatter — every other kind
        leaves it untouched.

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

   e. **On task failure** (page not found, edit error, malformed task):
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

5. **Finalize** — concept `## Sources` / `source_count` were left empty on purpose;
   reconcile them from the wikilink graph (a concept cited by several pages in the
   batch is counted correctly here, never undercounted), then refresh the catalog:
   ```bash
   lore graph backlinks-sync   # re-derive every concept's ## Sources + source_count
   lore wiki index             # refresh the catalog
   ```
   `lore ingest` does NOT run these — always run them yourself after `/lore-process`.
   (The `lore-daily-ingest` scheduled task chains ingest → /lore-process → these for
   an automated daily run.)

6. **Report** to the user:
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
    `## Sources` body — re-adding the same `- [[ref]]` is a no-op.
  - `source_count` is never written by processing — new pages start at `0`,
    ingest preserves the existing value, and only `lore graph backlinks-sync`
    computes the authoritative count from the wikilink graph.
- **Never partially-commit progress** to the queue file itself
  (no `processed.jsonl` sidecar): the source-of-truth is the vault edits,
  which are themselves idempotent.

## When NOT to invoke

- `lore ingest` writes the queue file atomically (temp + rename) after all
  vault pages are written and before it commits dedup, so a concurrent
  ingest cannot produce a partial `.jsonl` you might consume mid-write.
  There is no append-while-reading hazard. You may still want to wait so
  the user sees the new pages before they get edited.

## Forcing a re-run

To regenerate a section that was already filled, then run `lore ingest --force`
for the same date. The pipeline sees the cache miss, re-queues the task, and
`/lore-process` fills it on the next run. No flag, no skill argument — the page
itself is the cache key. How to invalidate depends on the section's cache shape:

- **Fill-empty sections** (summary, concepts, narratives): delete the body OR
  the `llm_inputs.<key>` line.
- **In-place rewrite** (`daily-refine-events`): delete the
  `llm_inputs.refine_events_done` line. Emptying the event body does NOT force a
  re-run — the event list is structural and the pipeline re-renders it, so
  completion is tracked only by `refine_events_done`.

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

# Today's daily pages have summary content (path depends on vault.dirs.daily)
head -30 "$VAULT/<daily>/ai-news/$(date +%Y-%m-%d).md"
```
