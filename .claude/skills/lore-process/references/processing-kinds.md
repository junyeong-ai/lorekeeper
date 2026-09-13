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

## Output language (all kinds)

`input.locale` is the language the vault is authored in — `vault.locale`, carried on every
task — and everything you WRITE goes in it: a summary, a refined event body, a theme title
and its description, a concept's grounding sentence, a synthesis. It is always present. A
task arriving without one is a defect to report, not a case to guess from the pages around
the target: a vault that stays in its language by inference stays in it only while there is
something to infer from, and the first page a new vault writes has nothing.

Two things it does not reach. Source content is never translated — a quoted line, a title
you preserve, a link stay as they are. And a NAME is not prose: a concept's name is the form
the source writes, so an English term keeps its spelling on a page written in Korean and a
Korean product name keeps its on one written in English (see `kind: extract-concepts`).

## `kind: summarize`

Synthesize a knowledge-rich summary in `input.locale`. Aim for
`input.max_sentences` substantive points. No preamble.

A daily page's summary answers a different question from the events below it.
Each refined body says what its own event is; the summary says what the day's
events establish together, naming the ones it draws on. A sentence that would
be true of a single event belongs to that event's body rather than here — where
one event is the whole day, say so in a sentence instead of repeating its body.

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
and descriptions in `input.locale`.

## `kind: synthesize-concept`

Rewrite one concept page's synthesis, and its relations, from the pages that currently cite it.

`input.citations` lists those pages by id, and the concept page's own sources
section links every one of them. Read the sources — that is the whole input;
the task carries no text.

1. Open `target.vault_path` and follow every link in its sources section.
2. Write, under `target.anchor`, what the vault now knows about this concept:
   what it is, and what the citing material establishes about it. Write it in the
   language the page is already written in — a `vault.locale` switch renames
   headings and leaves authored bodies alone, so an established page keeps the
   language it was written in rather than being retranslated section by section.
   A page carrying no prose yet answers nothing, and there `input.locale` is the
   language: inferring one from the sources would write the vault in whichever
   language its inputs happened to arrive in.
3. **REWRITE the section, never append.** The synthesis states current
   understanding, so an older reading that the evidence has moved past is
   replaced, not kept beside the new one. (Its citations are the opposite —
   `## Sources` only ever accumulates, and `lore graph backlinks-sync` owns
   it. Never edit that section here.)
4. Every claim must be traceable to one of the cited pages. Write nothing the
   sources do not support, and nothing from your own background knowledge —
   a concept page is what the vault observed, not what is generally true.
5. When two cited sources disagree on a fact about the concept, state the
   disagreement in a `> [!conflict]` callout naming both sides, inside the
   section. `lore graph lint` surfaces open callouts. You are the only writer of
   this section, so a callout exists only where THIS rewrite puts one: if the
   sources you just read still disagree, write it again. A page that carried one
   before is not evidence either way — read the sources, not the previous body.

   Three things decide whether a callout is worth the reader's time:
   - **Put the statement on the callout's own line** (`> [!conflict] <what differs>`).
     That line is what `lore graph lint` reports, so it is where a reader who has not
     opened the page meets the disagreement.
   - **Do not record what the evidence settles.** A figure two sources give differently
     is a disagreement; a figure one source computes wrongly from a number both of them
     carry is an error, and there the synthesis states the supported value and writes no
     callout. Check before recording — `261 of 367` is 71%, so a source calling it "over
     80%" is not a second reading of the evidence.
   - **Say which kind it is when the answer is not in the vault.** Where both sides are
     REPORTS of an external fact — a model's parameter count, a funding round's stage —
     no observation this vault will ever hold settles it, and a callout that says so is
     answered. One that does not has every later reader, and every audit, re-derive the
     same judgment from nothing.
6. Length follows the evidence: one or two sentences for a concept with a
   single citation, a short paragraph for one with many. Never pad.
7. **Write the concept's relations under `input.related_anchor`.** The same evidence
   answers both sections, which is why one task writes them: the pages citing this
   concept name others alongside it, and which of those the material actually relates
   to this one is what this section states. One link per line, in the form AGENTS.md
   § Links defines — a concept's relations are its siblings, so the destination is the
   bare `slug.md` and the display text is that page's title:

   ```
   - [Speculative Decoding](speculative-decoding.md)
   ```

   Four rules, and the first two are what keep this section worth reading:

   - **A relation is a claim the evidence supports, never a co-occurrence.** A day's
     news names twenty concepts and relates almost none of them; a document about one
     subject relates the three it names. Write nothing rather than fill the section —
     an empty relations section is a true statement about thin evidence, and a padded
     one costs every later reader the work of telling the two apart.
   - **Every destination must be a page that exists**, confirmed with
     `lore resolve <name>`. A link to a page nothing answers to is the broken link
     `lore graph lint` reports, and this is the one section a rewrite could mint one in.
     Resolve by NAME rather than guessing a slug: a renamed or merged concept keeps its
     original address.
   - **REWRITE it, like the synthesis** — it states the relations the current evidence
     supports, so one the evidence has moved past is replaced rather than kept beside.
   - **A task carrying no `related_anchor` has no section to write.** The page does not
     have one; writing anyway would edit nothing and report success.

   `synthesis_done` answers for BOTH sections — one input, one act, one marker — so
   stamp it once, after writing both.

A concept with no citations left keeps whatever the page says — write the
section from the sources you have; if there are none, leave the existing body
alone and stamp the marker. The concept became uncited because its sources
were deleted, which is not a reason to erase what was known about it.

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
  "concepts": [{ "name": "…", "category": "…", "aliases": ["…"], "synthesis": "…" }]
}
```

`synthesis` is one or two sentences grounding the concept, and is used only when the page
is being CREATED — an established page's synthesis is its accumulated meaning across every
source that cited it, so a single mention never overwrites it. Omit it and a new page is an
empty heading.

**`name` and `aliases` follow § Concept convergence in the vault's `AGENTS.md`**
(`lore config schema-path`), which states what a name may be, which form is the name where the
text writes several, and what belongs in `aliases` instead. It is stated there and not here:
a second copy is how the two come to disagree, and step 1 has you read it before this one.
Read its rule about the vault's language against `input.locale`, which this payload carries:
the contract names the language it was RENDERED with, so between a `vault.locale` switch and
the `lore schema` that follows it, the task in your hand is the current answer and the file is
not.

An alias an established page already answers to is dropped with a warning rather than taken —
the extraction is one source's reading, and the page that earned the name by being cited under
it keeps it.

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
`AGENTS.md`: `lore resolve` for EVERY form the text writes — a hit on any of them is the
owner — plus the created-this-run set, plus `lore wiki search` on two or three distinguishing
terms of each form for the equivalences no rule about spelling can see. The queue task carries
no concept registry of its own.

**Category assignment.** Hard constraint: the `category` value MUST be one of
the IDs in `input.categories` (verbatim string match) or the field MUST be
omitted entirely. Never invent a new category, never substitute a synonym,
never abbreviate. If no listed category fits the concept, leave the
`category` field off — `lore graph lint` counts an unknown category as a
violation, so an invented one makes it exit non-zero and breaks the index.
When `input.categories` is absent or empty, omit the field unconditionally.


## Per-kind target formatting

- **`daily-concepts`, `document-concepts`**: nothing to format. These are the
  `extract-concepts` kinds, and they write a RESULT FILE rather than editing the
  target — `lore queue apply` writes both the concept pages and the origin
  page's links, through the same merge the synchronous ingest path uses. Report
  the concepts; the link format and slug rule are not yours to restate.

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
