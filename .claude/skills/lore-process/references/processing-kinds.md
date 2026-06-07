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

Identify the key named entities, topics, and concepts (whatever the source's
domain — the focus, if present, names it). Output a list of concept names (in
the source language). Each concept also produces a concept page (create if
missing, merge if exists). Fill the origin page's `## Related Concepts` with
a `[[concept]]` forward link — the single source of truth `backlinks-sync`
reads; leave the concept's `## Sources` / `source_count` to it. Use the
concept path pattern from AGENTS.md.

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

Same rule for the `tags` array: when a category is assigned, include that
category ID as the page's sole tag (`tags: ["{category-id}"]`). When no
category is assigned, use `tags: ["concept"]`.

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

Do NOT add any keys beyond those listed above. Emit all three body sections in
the AGENTS.md concept format: `## Synthesis` (you fill — see below), `## Sources`
(leave EMPTY), and `## Related` (leave EMPTY). `## Sources` and `source_count: 0`
are machine-owned (see the shared dedup reference); `## Related` is human/audit-
curated and never machine-written, so emit the heading with no bullets.

**When creating a new concept page**, fill the Synthesis section (heading
from AGENTS.md) with a 1-2 sentence definition/context of the concept based
on the source text. Don't leave it empty — even a first-appearance concept
benefits from a brief grounding. On merge (concept already exists), update
the synthesis if the new source adds meaningful context; otherwise leave it.

## Per-kind target formatting

- **`daily-concepts`**: replace the section body with
  `- [[Concept Name 1]]\n- [[Concept Name 2]]\n...`. Create each concept page
  (path from AGENTS.md) if it doesn't exist, following the concept page
  format above. Crucially include `aliases: ["Concept Name"]` so the
  `[[Concept Name]]` wikilinks resolve to the slug-named file. If a concept
  name contains `/` (e.g. `async/await`), emit it piped — `[[async-await|async/await]]`
  — because a bare `[[async/await]]` resolves as a vault path, never via the alias.

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
