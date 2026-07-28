# Per-kind generation specs

How to produce the content for each task kind. The SKILL.md protocol decides
WHETHER a task runs (stale-task guard) and WHERE the output lands
(`target.anchor`); this reference defines WHAT good output looks like per kind.

## Relevance focus (all kinds)

If `input.focus` is present, it is the source's natural-language relevance
criterion. Treat everything outside that focus as off-topic and exclude it:
for `summarize`, cover only matching content; for `extract-concepts`, extract
concepts ONLY from items matching the focus and skip off-topic items entirely
(create no concepts for them). This is how a broad source (e.g. a news
aggregator that also surfaces human-interest or politics) contributes focused
knowledge without polluting the graph. No `focus` → no filtering.

## `kind: summarize`

Synthesize a knowledge-rich summary. Use the language specified in
`input.locale` (e.g. `"ko"` → Korean, `"en"` → English); default to Korean if
absent. Aim for `input.max_sentences` substantive points. No preamble.

**Source-type-aware synthesis.** Adapt the strategy to `input.source_type`
(the adapter type verbatim from config; never guess it from the vault path).
When `source_type` is absent (cross-source syntheses such as the work-log),
apply the generic guidance without a type bias. Per-type strategy:
see [source-types.md](source-types.md).

For all types: produce genuine knowledge, not just headlines. Not too short
(meaningless one-liners) nor too verbose (raw dump). Always preserve source
URLs/links for traceability.

## `kind: refine-events`

Rewrite the raw event bodies under `target.anchor` (e.g. `## Key Events` or
`## Key Messages`, localized per AGENTS.md) into refined knowledge in
`input.locale` language.

For EACH `### event heading` in the section:

1. Replace the raw body with a knowledge summary (2-5 sentences)
2. Cover: what it is, why it matters, key details/decisions
3. Keep the original `🔗` source link for traceability
4. Remove noise: HN metadata (Points, Comments, Article URL),
   email signatures, raw thread dumps, Jira checklists
5. If meeting notes are embedded, distill to decisions + action items

The `### heading` lines themselves must be preserved — only replace the body
text between headings. Completion stamping (`llm_inputs.refine_events_done`)
is part of the protocol contract in SKILL.md.

## `kind: identify-themes`

Extract structured themes from the combined multi-source text. Identify the
top N themes (`input.max_themes`). Write each theme as a numbered subsection
(`### 1. Theme Title\n\nDescription`) under `target.anchor`. Write the titles
and descriptions in `input.locale` language; default to Korean if absent.

## `kind: extract-concepts`

Identify the key named entities, topics, and concepts (whatever the source's domain — the
focus, if present, names it), and write them to a result file. Do NOT create or edit concept
pages, and do NOT touch the origin page's related-concepts section: `lore queue apply`
materializes both, so that the merge rules (preserved `## Synthesis`, aliases, category,
citation count) and the link/slug format live in one tested place rather than being restated
here. Your output is judgement — which concepts the page names — and nothing else.

Write one file per task to `<vault>/.lorekeeper/queue/results/{task_id}.json`:

```json
{
  "task_id": "<task.task_id>",
  "cache_hash": "<task.cache_hash>",
  "target": <task.target verbatim>,
  "date": "<task.input.date>",
  "concepts": [{ "name": "…", "category": "…", "synthesis": "…" }]
}
```

`synthesis` is one or two sentences grounding the concept, and is used only when the page
is being CREATED — an established page's synthesis is its accumulated meaning across every
source that cited it, so a single mention never overwrites it. Omit it and a new page is an
empty heading.

`concepts` may be empty — that is a valid answer for a page with nothing durable in it, and
it still records that the task was answered. Copy `target` and `cache_hash` through
unchanged; the applier re-checks the hash against the page and drops the result if the page
moved on while you were working.

**What counts as a concept** (keep the graph high-signal, not noisy): extract
durable, reusable knowledge nodes — technologies, named methods,
architectures, patterns, standards, organizations. Do NOT mint concepts for
transient specifics, generic English words, dates/numbers, one-off phrasings,
or anything that would never plausibly be cited by a second source. When in
doubt, prefer the established broader concept over a narrow variant. A good
rule: a concept earns a page only if a future unrelated source could
independently link to it. Fewer, load-bearing concepts beat many shallow ones.

`input.source_type` carries the originating adapter type; use it to scope
what counts as a concept (per-type scoping: see
[source-types.md](source-types.md)). Never invent it from the path.

**Concept dedup** follows the **Concept convergence** section of the vault's
`AGENTS.md`: the run-start registry
(`lore wiki concepts`) plus the created-this-run set is the full dedup
context — the queue task carries no concept registry of its own.

**Category assignment.** Hard constraint: the `category` value MUST be one of
the IDs in `input.categories` (verbatim string match) or the field MUST be
omitted entirely. Never invent a new category, never substitute a synonym,
never abbreviate. If no listed category fits the concept, leave the
`category` field off — `lore graph lint` surfaces unknown categories as
findings, so an invented category is observable drift that breaks the index.
When `input.categories` is absent or empty, omit the field unconditionally.


## Per-kind target formatting

- **`daily-concepts`**: replace the section body with one link per line —
  `- [Concept Name]({target.concepts_dir}/{slug}.md)`. `target.concepts_dir` is
  carried on every task (the precomputed relative path from the target page to
  the concepts directory — concatenate, never compute paths yourself); `{slug}`
  is the concept name slugified per AGENTS.md § Concept convergence. The display
  text is the concept's name verbatim (any character is fine — the slug lives
  only in the destination). Create each concept page (path from AGENTS.md) if it
  doesn't exist, following the concept page format above, and include
  `aliases: ["Concept Name"]` so the registry recognizes the surface form.

- **`work-log-synthesis`**: the input text contains personal events from
  multiple sources, each prefixed with `[source_id]`. Instead of a plain
  summary, **group the events by topic/project** across sources. Format as:

  ```
  ### Topic Name
  - 📅 calendar event *(my-schedule)*
  - 💬 slack discussion *(team-slack)*
  - 📧 email follow-up *(email-digest)*
  ```

  Use source-type emoji indicators: 📅 calendar, 💬 slack, 📧 gmail,
  📄 google-drive, 🎫 jira. Correlate events that share the same project,
  topic, or concept across different sources. A single event may appear in
  multiple topic groups if it spans topics. Aim for concise topic names.
  Include 1-2 sentences of context per topic (not just the event title).
  Note decisions made, blockers encountered, and next steps. Skip trivial
  notifications (calendar accepts, read receipts, approvals). Preserve
  source links for traceability.

- **Synthesis narratives** (`weekly-*`, `monthly-*`, etc.): these pages
  contain multiple `## ` sections (period, categories, etc.) — only the one
  matching `target.anchor` is replaced. Leave all other headings untouched.
