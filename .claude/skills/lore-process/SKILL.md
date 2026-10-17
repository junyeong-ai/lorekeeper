---
name: lore-process
version: 0.53.1
description: Drain the Lorekeeper LLM work queue after `lore ingest` runs in queue mode — fill the empty LLM-owned sections (summaries, refined events, concepts, themes, reviews) of freshly-written vault pages using the active agent session. Idempotent and resumable. Run after each ingest, or on a daily schedule.
when_to_use: |
  queue process, drain queue, fill summaries,
  concept extraction run, enrich daily pages, post-ingest
allowed-tools: |
  Bash(ls *)
  Bash(cat *)
  Bash(jq *)
  Bash(wc *)
  Bash(head *)
  Bash(mkdir *)
  Bash(mv *)
  Bash(lore *)
  Bash(date *)
  Bash([ *)
  Bash(basename *)
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
work in your own session, stage typed result files, and let `lore queue apply`
publish pages and markers. `lore queue prune` retires completed runs.

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
   still carry EVERY section the task names — its synthesis anchor and, when present,
   `input.related_anchor` for a `synthesize-concept` task; one for the rest. Only a task that passes the
   first, fails the second, and has somewhere to land is work — an anchor the page
   no longer carries means the heading vocabulary changed (a locale switch), which
   no amount of waiting undoes. A task applied earlier in this run
   reads `done` and is skipped. Every staged task remains `current` until apply
   publishes the result and stamps its marker. Apply after each task or a reviewed
   independent batch under step 3d; never loop on classification alone without publishing results.
2. **Locate a section only by a heading the TASK carries** — `target.anchor`, and
   `input.related_anchor` for the second section a `synthesize-concept` task fills. Each is
   the exact `## …` heading the pipeline wrote, resolved from i18n at queue time. Never
   hardcode a heading per `target.kind`, and never derive one from the locale yourself: the
   page may have been written under a different `vault.locale` than the one set now.
3. **Never hand-write a concept's sources-section body or `source_count`** —
   both are machine-owned; `lore graph backlinks-sync` re-derives them from
   forward concept links on origin pages.
4. **Retire a queue file through `lore queue prune` only when `lore queue status` says nothing in it is
   still work.** Step 4 has the sequence; what makes it a rule is that the answer is the
   classifier's and never your own record. On any failure, leave the file in place and stop
   processing that file.
5. **Workers never edit target pages or completion markers.** Every task returns a
   typed result file. Only `lore queue apply` publishes section bodies and the matching
   completion marker after checking the pending task and exact captured target revision.
   A synthesis result's one marker answers for both sections.
6. **Never create or merge a concept page, and never write an ORIGIN page's
   related-concepts section.** `extract-concepts` emits a result file; `lore queue
   apply` materializes both. Concept pages are shared between origin pages and merge
   under rules that already exist as tested Rust — restating them here would be a
   second implementation, and a drain that wrote them could not run two pages at once.
   `synthesize-concept` also stages a result: only `lore queue apply` publishes its
   requested sections and completion marker after validating source and target revisions.

## Materialized-view contract

Daily pages are materialized views with two kinds of fields:

- **Structural** (frontmatter, raw event list, headings) — owned by the Rust
  pipeline, re-rendered on every ingest. This skill never writes them.
- **Semantic** (summary body, refined event bodies) — owned by this skill,
  preserved across re-renders. Concept links are semantic too, but they are
  written by `lore queue apply` from the concepts this skill reports, so that
  their slug and link format match the pages it creates.

The pipeline owns structural fields and input hashes. Workers propose semantic content
in result files; the applier owns all page writes and completion markers. Empty section
results remain valid where the generation specification permits them. Completion depends
on the matching marker, never on whether a section looks filled.

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
   reproduces two resolution rules that already exist in the binary. `lore config schema-path` prints the
   absolute path of the vault's `AGENTS.md` — the wiki dir is configurable, so never assume
   `wiki/`. **READ that file's `## Concept convergence` section before you extract a single
   concept**: it carries the page formats and the naming rule this run answers to, stated
   there and nowhere else, so a drain that works from memory is working from whatever the
   last vault taught it. The path is computed rather than checked — on an install where
   `lore schema` has never run it names a file that is not there, and the answer is to run
   `lore schema` rather than to proceed without the rule. Ask
   `lore resolve "<name>" "<another name>" --json` which page owns each extracted name
   from one registry read, asked for EVERY form the text
   writes: it answers by the same rule the ingest pipeline routes by, so the two cannot
   disagree about an existing name, and a hit on any form is the owner. The queue task
   carries no concept registry, so that answer, plus a created-this-run set, plus
   `lore wiki search` on two or three distinguishing terms of each form for the
   equivalences spelling cannot see, is the dedup baseline
   (see the vault AGENTS.md § Concept convergence).

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

      It classifies every pending task as `current`, `done`, `stale`,
      `missing-target`, or `unreadable` from its target page's `llm_inputs.<key>` and
      `llm_inputs.<key>_done` — computed in tested Rust. **Process only
      `current` tasks; skip the rest without editing their pages.** Match tasks
      by `task_id`.

      An `unreadable` task has an unsafe/unreadable target or source, or a malformed/mismatched request input.
      Leave its queue file in place, report the page and stop. Repairing the page must
      make the task reachable again; moving it to `processed/` prevents that recovery.

   c. **For each `current` task**, capture `target_revision` from status before reading
      the target and drafting. Read the task input and current target; follow the generation
      specification. The applier owns every completion marker in this table:

      | target.kind | input key | completion marker (applier-owned) |
      |-------------|-----------|-------------------------------|
      | `daily-summary`, `document-summary` | `summary` | `summary_done` |
      | `daily-refine-events` | `refine_events` | `refine_events_done` |
      | `daily-concepts`, `document-concepts` | `concepts` | **not yours** — `lore queue apply` stamps `concepts_done` in the same edit that writes the links (safety rule 6). Write the result file and stop. |
      | `concept-synthesis` | `synthesis` | **not yours** — `lore queue apply` stamps `synthesis_done` after validating the staged result. |
      | `work-log-synthesis` | `topic_summary` | `topic_summary_done` |
      | `weekly-synthesis-themes` | `themes` | `themes_done` |
      | `weekly-review-narrative`, `monthly-review-narrative` | `narrative` | `narrative_done` |
      | `quarterly-review-narrative`, `annual-review-narrative` | `narrative` | `narrative_done` |

   d. **Stage and apply the result.** All output goes under
      `<vault>/.lorekeeper/queue/results/`, never directly into a target page:

      - `extract-concepts`: `<task_id>.json`, the extraction contract in
        [processing-kinds.md](references/processing-kinds.md).
      - `synthesize-concept`: `<task_id>.synthesis.json`.
      - Every other task: `<task_id>.section.json`, with `task_id`, `cache_hash`,
        captured `target_revision`, and `body` (the section body without its heading).

      See [queue-format.md](references/queue-format.md) for the exact contracts.
      Write each result atomically through a temporary file and rename. Run
      `lore queue apply` before capturing a subsequent task on the same target:
      a successful earlier publication changes its exact bytes. If another task or
      editor changes a target, reread and regenerate rather than relabeling the old result.

      A coordinator may batch reviewed results of the same kind for distinct targets
      in one apply, provided no result changes another task's evidence. Freeze ingest
      and all other page writers during the wave, reconcile
      names and aliases across the batch, and place only those approved results in
      `results/`: apply consumes the whole directory. Finish the summary wave before
      capturing targets and drafting extraction. Batching extraction shares one concept
      registry load; it does not relax any task, evidence or target revision guard.

      Apply is not all-or-nothing: valid results may be published while others fail.
      On a nonzero exit, stop and inspect status, remaining results, quarantine and
      alias warnings before regenerating affected work. Never assume rollback or
      attach new guards to old drafts.

   e. **On task failure** (page not found, edit error, malformed task):
      record the failed `task_id` and the reason. **Abort processing of
      this queue file** — do not attempt the remaining tasks. The queue
      file stays on disk so the next `/lore-process` run replays every
      task from the top (publication is marker-idempotent).

4. **Finish the file, then let the classifier say it is finished.** Run `lore queue apply`
   while the file is still in the queue, then re-run the step 3b `lore queue status --json`
   and check that no `task_id` from this file reads `current` or `unreadable`, and that
   the file contains no unparseable lines. Retire completed files through the CLI:
   ```bash
   lore queue prune --json
   ```
   Pruning reclassifies tasks while holding the same writer lock as apply. It preserves
   unfinished or unreadable work and chooses archive destinations without overwriting history.
   Your own record of what you wrote is not that answer. A result not yet published with
   its completion marker still reads `current`, and archiving on belief retires a
   run whose bodies the next render erases and whose tasks nothing re-enqueues — invisibly,
   because the pages look filled. The classifier reads the marker and not the body, which is
   the one check that distinguishes work you did from work you recorded. A task that still
   reads `current` leaves the file where it is, and you report it rather than moving on.

   Applying BEFORE the move is also what keeps `lore resolve` authoritative: concept pages
   are what it reads, so a result not yet applied is a concept it cannot see, and a long run
   would otherwise need a second answer to the naming question carried alongside the first.
   Apply makes new concepts visible to the next extraction wave; within an independent
   batch, the coordinator must reconcile names before publication as required by step 3d.
   A failing apply stops the run: inspect what was published and what remains before
   deciding further names. Quarantined results require diagnosis and correction, not
   blind retry.

5. **Finalize (mandatory — do not skip).** The apply here covers whatever the last file
   left pending; `lore graph backlinks-sync` and `lore wiki refresh` are owed once, at the
   end, over everything this run wrote. Concept `## Sources` sections and
   `source_count` are deliberately left empty during processing; they are
   machine-owned and reconciled here from the link graph, then the catalog
   is refreshed. Confirm each command exits 0:
   ```bash
   lore queue apply            # materialize reported concepts into pages + links
   lore graph backlinks-sync   # re-derive every concept's ## Sources + source_count
   ```
   If reconciliation queues current synthesis tasks, resume steps 2–4 for that work,
   then repeat apply and backlinks-sync. Continue until reconciliation leaves no
   current tasks; stop and report any blocked work or failed command. Refresh and
   check completion only after that drain:
   ```bash
   lore wiki refresh           # re-derive the catalog, the timeline and the map
   lore queue check            # pending or blocked work and unpublished results
   lore wiki reviews --check   # evidence drift and invalid assessments; open is valid
   ```
   Run Finalize on every invocation that processed a task. Even when no current
   task was found, run `lore queue check` and `lore wiki reviews --check` before
   reporting completion: zero current tasks alone does not rule out blocked inputs,
   unpublished results, interrupted mutations or evidence drift. Skipping reconciliation
   after publication leaves new concepts with no `## Sources` and a stale
   `source_count` until some later run happens to reconcile. If any command
   exits non-zero, treat the run as failed and surface the error in step 6
   instead of reporting success. `lore ingest` never runs these. (The
   `lore-daily.sh` pipeline chains ingest → /lore-process → these.)

6. **Report** to the user:
   - On full success: number of files processed and tasks completed, and confirm
     the step-5 Finalize ran (every command listed there, all exited 0).
   - On any failure (including a non-zero Finalize command): which file was left
     in place and the failed `task_id`s (or the failed command) with their error
     messages, then stop — leave remaining files for the next run.

## Idempotency contract

Only `lore queue prune` retires queue files after classification. It removes stale
and missing-target tasks, archives a run whose remaining tasks are done, and
preserves unfinished or blocked work. Failure rules:

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
  `processed.jsonl` sidecar): the source-of-truth is the applied vault state, which
  are themselves idempotent.

## Parallel processing

For selected synthesis inputs, use the [read-only evidence packet workflow](references/queue-format.md#read-only-evidence-packets).
Assign each task and target page to one worker. Workers stage typed result files;
one coordinator runs apply, retires queue files through `queue prune`, and performs Finalize. During a
synthesis wave, pause ingest, merges, capture/extract/wiki edits, and other source or
target writers: external editors do not share the applier lock. Workers return
durable-review proposals to the coordinator instead of editing shared exploration
pages. The coordinator reconciles proposals against existing reviews and publishes
them serially before the final synthesis snapshot. Re-run backlinks-sync and status
after review publication; reissue only affected tasks and draft against their new
source revisions and target snapshots. A review proposal is
a coordination artifact, not a field accepted by `lore queue apply`.

## When NOT to invoke

`lore ingest` writes the queue file atomically (temp + rename) after all
vault pages are written, so a concurrent ingest cannot produce a partial
`.jsonl` you might consume mid-write. There is no append-while-reading
hazard. Atomic queue publication does not freeze target pages: serialize ingest
and other page writers with a processing wave so they do not invalidate drafts.

## Regenerating a section

For an established mistaken citation, finish work touching its origin first. Read
the source and the concept's complete synthesis, including its conditions and
boundary cases. A source can establish when a method does not apply; absence of
a positive example does not make that citation mistaken. Judge the evidence
against that scope, not just the concept's title or opening definition. Then preview `lore graph retract-citation
<origin.md> <concept.md> --revision <file_revision from wiki read> --dry-run`.
Omit `--dry-run` to publish the reviewed correction. This archives prior evidence
and removes only standalone links in the existing related-concepts list. It never
rewrites concept bodies. Record consequential reasoning in a knowledge review and
run Finalize; an empty extraction cannot retract accumulated citations.

For a deliberate concept rewrite from its **current citations**, including when its
original task has expired, preview `lore graph resynthesize <exact-concept-path.md> --dry-run`
and run it without `--dry-run`. This preserves the authored bodies and queues one typed
synthesis request; an already-current matching request is reused. Drain and Finalize
normally. Empty or unreadable evidence is refused. This is a new current-evidence
request, not replay of an unavailable historical input.


Use `lore queue regenerate --task-id <retained-task-id> --dry-run` to preview, then
omit `--dry-run` to request that one task again. The task must exist exactly once in
pending or processed queue files. Its complete original typed input, input hash,
target kind and headings must still agree; synthesis source revisions must remain
current. Missing retained input is a refusal, not permission to remotely re-fetch an
expired source or reconstruct raw events from refined prose.

The command preserves authored bodies, clears only completion/provenance for that
section, and publishes a new unique task through the mutation journal. Process it
normally. Never manually clear a marker and re-ingest as a substitute for this operation.

New tasks carry `input.generation_policy`, a per-kind semantic policy identifier.
Only apply writes `llm_inputs.<key>_policy_done` from the matched task. Missing policy
is legacy/unknown; it does not make existing answers stale or trigger a global rewrite.
Unknown requested policy versions require a compatible processor. Policy provenance
is separate from the unchanged data cache hash. Explicit regeneration records a
policy request for this input so an old legacy task cannot satisfy the new request.
