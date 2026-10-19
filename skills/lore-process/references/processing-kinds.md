# Per-kind generation specs

## Contents

- [Shared relevance and language](#relevance-focus-all-kinds)
- [Summaries](#kind-summarize)
- [Event refinement](#kind-refine-events)
- [Themes](#kind-identify-themes)
- [Concept synthesis and reviews](#kind-synthesize-concept)
- [Concept extraction](#kind-extract-concepts)
- [Target formatting](#per-kind-target-formatting)
- [Section result contract](#single-section-results)

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

Ground the summary in this task's input. Its cache tracks that input, so claims
borrowed from another document would survive changes to their actual evidence.
Preserve a declared status or successor identity, but do not import the successor's
conclusions from another task or prior reading. Compare versions in a concept
synthesis or knowledge review that records both sources and their revisions.
Preserve the source's modality and attribution: conditions, rules, risks and examples
must not become observed incidents or implemented outcomes unless the input establishes them.

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

1. Replace the raw body with a knowledge summary, usually 2-5 sentences.
2. Cover what it is, why it matters, and key details/decisions. Preserve definitions, methods,
   conditions and limitations needed to interpret a result; a whole article's
   opening paragraph is not a substitute for those findings. Use more space when
   these distinctions would otherwise be lost. For multi-topic digests, retain
   the substantive finding for each relevant topic instead of reducing secondary
   topics to a list of names. Keep named tools, models and methods attached to their
   defining mechanisms, provenance and decision-relevant use conditions. Generic
   prose such as “evaluation” or “optimization” removes evidence later concept
   syntheses need. Compare these claims and conditions with the input before publishing.
3. Keep the original `🔗` source link for traceability
4. Remove noise: HN metadata (Points, Comments, Article URL),
   email signatures, raw thread dumps, Jira checklists
5. If meeting notes are embedded, distill to decisions + action items

The `### heading` lines themselves must be preserved — only replace the body
text between headings. Match each body to its own source link and attribution;
identical titles do not make events interchangeable. A link-only event cannot
establish the linked article's findings, and a truncated excerpt supports only
the content actually retained. Stage the complete replacement section as a `.section.json` result; the applier
checks the event heading sequence and stamps `llm_inputs.refine_events_done`.

## `kind: identify-themes`

Extract structured themes from the combined multi-source text. Identify the
top N themes (`input.max_themes`). Write each theme as a numbered subsection
(`### 1. Theme Title\n\nDescription`) under `target.anchor`. Write the titles
and descriptions in `input.locale`.

## `kind: synthesize-concept`

Rewrite one concept page's synthesis, and its relations, from the pages that currently cite it.

`input.citations` lists each source's vault-relative `path` and `revision`.
Read those exact pages. The revision identifies authored content and provenance;
bookkeeping fields (`updated`, `source_count`, `llm_inputs`) are excluded.
Do not substitute the concept's previous synthesis for a source.

For a typed RSS event citation, the exact selected immutable article is the evidence;
the daily is its navigational origin and a derived interpretation. Follow the packet's
selected-event command, read the supporting article context through `next_offset`, and
keep historical/provider-attributed claims scoped. Current manifest changes do not make
that selected observation current truth. A citation without `rss_event` retains the full
page authored-revision contract. Mixed page/event citations require both kinds of reads.

1. Open `target.vault_path`. Read each page citation's exact source page under the vault
   root; for event citations, read the packet's selected immutable articles. Read both
   when the dependency set mixes page and event evidence.
2. Draft the `synthesis` result body for `target.anchor`: what the vault now knows about this concept,
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
   `## Sources` is re-derived from current forward links by `lore graph backlinks-sync`. Never edit that section here.)
4. Every claim must be traceable to one of the cited pages. Write nothing the
   sources do not support, and nothing from your own background knowledge —
   a concept page is what the vault observed, not what is generally true.

   An unsupported previous claim may be mistaken or may have lost its citation.
   Remove it from factual synthesis without declaring it false. Keep routine
   removals in the processing report, not as recurring editorial notes in the
   concept. If losing a cause, remedy or decision would materially change how
   someone uses the knowledge, return an evidence-gap review proposal under the
   coordinator protocol below: the exact claim, pages checked and their revisions,
   and what evidence is missing. Before restoring a citation, inspect existing reviews
   for a prior retraction or intentional scope decision. A shared term does not establish
   the same concept; recover evidence within its scope rather than silently widening it.
   The audit can then search beyond this citation set.
   For RSS, a refined daily is an interpretation, not the complete collected article.
   Use packet `original` discovery or `lore wiki original <daily-path>`, select the exact
   event with `--revision REV --event EVENT_ID`, and follow `--text-offset` / `next_offset`.
   `page_bound_collection` verifies the page's referenced normalized pre-classification
   collection, not provider HTTP/XML or a live refetch. `legacy_unbound` exposes a retained
   event log but does not prove that the current page was derived from those bytes; do not
   infer that binding from source ID, date or present configuration. Missing originals
   remain unknown. Attribute a claim to the currently retained collection using its exact
   log/event revision, provider URL and actual read scope; do not recast it as proof of the
   past page input or invent an unverified EventID/URL binding. Reading and explicitly
   attributing that retained evidence does not itself require rewriting a daily. Establishing
   a durable page-bound authority reference or correcting the page does require reviewed
   source publication. Do not copy an entire raw inventory into synthesis. Existing task
   input, citation and target guards remain binding; this distinction never expands or
   exempts them. An old concept is not proof of itself.
   An application example does not define the general concept. Losing its defining
   scope is a material evidence gap: recover supporting sources before finalizing
   a rewrite that would reduce the concept to that example.
   Keep uncertainty in the concept only when it affects a current decision.
   If the citations merely name the concept without explaining it, state that
   evidence limit concisely instead of inventing a definition or emitting an empty body.
5. Compare claims under the same subject, environment, definitions and effective time.
   A newer document is not automatically more authoritative; a later revision or a different
   environment may explain the difference. Repeated reports of one source are one evidence
   lineage, not independent corroboration. `source_count` counts citing pages, not confidence.
   Use `lore wiki evidence <source-path>...` to inspect recorded source addresses; absent
   provenance stays unknown. Read actual attributions before claiming independence.

   For a material disagreement, search existing exploration pages and run `lore wiki reviews`.
   The coordinator (or the sole processor in a serial run) creates or updates one
   durable review using AGENTS.md's Knowledge reviews contract, recording
   both claims, their scope and time, direct source links, and what could settle the question.
   Record direct evidence, never the review itself as its own supporting source.
   Derived reviews and summaries are interpretations; follow their original sources rather
   than treating them as independent corroboration. Before reassessing, archive the existing review with `lore wiki evidence --archive` and
   retain its archive path in the dated reassessment. After actually rereading changed evidence,
   archive those sources and explicitly replace `review.evidence` with the revisions supporting
   the current judgment. Do not retain superseded revisions of the same path in that current
   evidence set or use the archived review as independent evidence. Mark resolved only with a
   stated reason supported by evidence; otherwise keep open. A concept's synthesis contains
   a concise `> [!conflict] <what differs>` with a link to the open review. A resolved review
   remains as the explanation; rewriting a synthesis must never erase its record.
   Parallel workers return proposals containing these claims, evidence revisions, and
   an existing review path when found; they never edit shared review pages. The
   coordinator deduplicates and publishes reviews before the final synthesis snapshot,
   then refreshes backlinks and task status and reissues affected work. Draft final
   synthesis against those revisions. Review proposals are not part of the synthesis
   result JSON; do not add fields to that contract.
   Do not create a review for superficial wording differences or already-explained scope.
6. Length follows reusable understanding, not the number of citations. Lead with the
   definition, boundary and decision-relevant rules. Group evidence by the question it
   answers, not by source/date. Preserve conditions, exceptions and consequential changes.
   Keep detailed case histories in their source documents or an existing exploration,
   linked with enough context to find them. Use subheadings when distinct questions warrant
   them. If moving a substantial case would lose a needed distinction, propose a scoped
   exploration to the coordinator before shortening the concept. Workers do not create
   those shared pages. Do not split by character count or mint a concept per case.
7. **Draft the `related` result body for `input.related_anchor`.** The same evidence
   answers both sections, which is why one task writes them: the pages citing this
   concept name others alongside it, and which of those the material actually relates
   to this one is what this section states. One link per line, in the form AGENTS.md
   § Links defines — a concept's relations are its siblings, so the destination is the
   bare `slug.md` and the display text is that page's title. Add a concise explanation
   of the relationship, its applicable conditions, and a link to the supporting source.
   Use ordinary prose rather than inventing relation labels or confidence scores:

   ```
   - [Speculative Decoding](speculative-decoding.md) — accelerates decoding by verifying draft tokens; supported by [the evaluation](../documents/decoding-evaluation.md).
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

   Stage both bodies in `<task_id>.synthesis.json` per `queue-format.md`. Only
   `lore queue apply` publishes them and stamps `synthesis_done`; never edit the
   concept sections or completion marker directly.

A concept with no citations needs an evidence judgment before normal publication.
Report its exact target revision and available history to the coordinator. Recover
supporting evidence for a useful durable subject; a missing captured definition does
not establish falsehood. Otherwise publish a scoped evidence-gap review and retire
normal discovery through guarded `wiki curate` (see lore-wiki's curation reference).
Preserve its address and prior prose. Once that reviewed lifecycle decision is current,
stage the existing semantic bodies unchanged if there is no supported rewrite, with
the captured target revision; only apply records completion. Never mark unsupported
prose as verified merely to empty the queue.

## `kind: extract-concepts`

Identify the key named entities, topics, and concepts (whatever the source's domain — the
focus, if present, names it), and write them to a result file. Do NOT create or edit concept
pages, and do NOT touch the origin page's related-concepts section: `lore queue apply`
materializes both, so that the merge rules (preserved `## Synthesis`, aliases, category,
citation count) and the link/slug format live in one tested place rather than being restated
here. Your output is judgement — which concepts the page names — and nothing else.

Capture `target_revision` from `lore queue status --json` before reading the target.
Read its current content alongside the task input. If the target changes before apply,
read it again and regenerate the extraction; never copy a new revision onto an old answer.
The applier also requires a matching pending task, target and date. A lost task requires
explicit recovery as described in `queue-format.md`; normal draining never opts into recovery.

`input.concept_policy` controls admission. `existing-only` permits links to subjects
already owned by a concept: resolve every name, read its scope, and omit unsupported
or absent owners instead of creating a broader substitute. `supported-new` also permits
new durable subjects with direct support and a useful future retrieval purpose. A
disabled policy never produces an extraction task. This policy participates in the
input hash and is enforced by the native applier, not inferred from source type.

Write one file per task to `<vault>/.lorekeeper/queue/results/{task_id}.json`:

```json
{
  "task_id": "<task.task_id>",
  "cache_hash": "<task.cache_hash>",
  "concept_policy": "<task.input.concept_policy>",
  "target_revision": "<queue status target_revision captured before reading>",
  "target": <task.target verbatim>,
  "date": "<task.input.date>",
  "concepts": [{ "name": "…", "category": "…", "aliases": ["…"], "synthesis": "…" }]
}
```

When `input.rss_original` is present under `concept-extraction-rss-v3`, discover the
bound original and compare discovery `page_revision` to the captured target revision.
Read each article you select through the pinned collection revision and full supporting
context; return `rss_evidence: [{"event_id":"...","revision":"<payload revision>"}]`
inside that concept object. Every returned concept requires at least one selected
article; application refuses a missing selection before any publication. Every pair must occur in the task's exact provided manifest.
Do not copy `concept_evidence` metadata from the page as fresh semantic judgment.
A legacy task cannot acquire event authority by guessing a title, URL, date or EventId.
Existing selections accumulate even if omitted; explicit retraction is a separate reviewed act.

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

What separates that field from `synthesis` is what a long run drifts across: `name` is what
the page is CALLED, `synthesis` is what it SAYS. A finding written into `name` addresses
the wrong subject — later extractions will not reuse a whole finding as a name. Keep the
claim in the synthesis; the registry allocates a bounded filename independently of the name.

An alias an established page already answers to is dropped with a warning rather than taken —
the extraction is one source's reading, and the page that earned the name by being cited under
it keeps it.

`concepts` may be empty — that is a valid answer for a page with nothing durable in it, and
it still records that the task was answered. Copy `target` and `cache_hash` through
unchanged; the applier re-checks the hash against the page and drops the result if the page
moved to a different task input. A page revision mismatch retains the result for regeneration.

**What counts as a concept** (keep the graph high-signal, not noisy): extract
durable, reusable knowledge nodes — technologies, named methods,
architectures, patterns, standards, organizations, and stable subjects that connect
the user's records. Do NOT mint concepts for
transient specifics, generic English words, dates/numbers, one-off phrasings,
or names whose only value is a passing mention. Prefer an established broader
concept when it faithfully covers the evidence. A page should provide a useful
retrieval destination for future evidence or questions; that evidence may come
from the same project or personal context. Do not generalise a local finding to
make it appear reusable.

`input.source_type` carries the originating adapter type; use it to scope
what counts as a concept (per-type scoping: see
[source-types.md](source-types.md)). Never invent it from the path.

**Concept dedup** follows the **Concept convergence** section of the vault's
`AGENTS.md`: `lore resolve` for EVERY form the text writes, read the lexical owner's
complete scope before reuse (an owned homonym can name a different subject), plus the created-this-run set, plus `lore wiki search` on two or three distinguishing
terms of each form for the equivalences no rule about spelling can see. The queue task carries
no concept registry of its own. Reuse the resolved canonical title in `name` and keep the
source's equivalent spelling in `aliases`; a spelling difference alone does not justify a
new page. If the written forms resolve to different owners, resolve that semantic ambiguity
before submitting an extraction. Search matches are candidates, not identity decisions.

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


## Single-section results

For `summarize`, `identify-themes`, and `refine-events`, including work-log and review
narrative targets, return the generated body in `<task_id>.section.json` according to
[queue-format.md](queue-format.md). Do not edit the target or stamp any marker.
An empty body is valid where the generation specification calls for no content.
For refinement preserve every `###` event heading in its original order and keep each
body's own source links and attribution. Apply before capturing another task on the
same page, since publication changes its target revision.
