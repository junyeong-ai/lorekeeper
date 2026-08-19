# Source-type-aware synthesis & extraction

`input.source_type` carries the adapter type verbatim from config (the source's
`type:` field) — **never guess it from the vault path**. Adapt the synthesis /
extraction strategy to it. When `source_type` is absent (cross-source tasks such as
the work-log), apply the generic guidance without a type bias.

## Summarize (`kind: summarize`) — per-type strategy

### `slack-channel`, `slack-search`
- Extract key decisions, action items with owners
- Ignore repetitive agreement messages (ok, +1, sounds good)
- Structure as: decision/outcome → action items → context
- Preserve technical details, project names, and links

### `gmail`
- Extract the core ask or decision from each email
- Identify action items with owners and deadlines
- Skip signatures, disclaimers, forwarded-chain noise
- For email chains, focus on the most recent exchange

### `rss`
- Focus on key findings, announcements, techniques
- For technical articles: what it is, why it matters, key numbers
- Skip author bios, CTAs, navigation artifacts

### `google-calendar`
- Highlight meeting outcomes and decisions if notes are present

### `jira`
- Summarize key task status changes and deliverables

### `confluence`
- A wiki page the user WROTE or last edited — treat it as their own authored
  reference, not as news: preserve its argument and structure, distil to what a
  reader would need to act on
- An edit re-enters the pipeline as a new event (the page version is part of its
  identity), so summarize the page AS IT NOW READS; never describe the change
- Long pages carry headings, tables and code — keep the load-bearing specifics
  (names, values, thresholds, endpoints) and drop navigation scaffolding

### `google-drive`, `manual`
- Treat as curated documents: preserve the author's structure, distill to the
  core argument and supporting detail

### `tasks`
- The user's OWN completed work, one item per task they closed. The title is what
  they set out to do; the body is the note they closed it with, and is often the
  only record that the work happened at all
- Summarize what was DONE and what it established — never restate the task list.
  A day of six closed tasks is a day's work, not six headlines
- A body naming a finding, a constraint or a decision is the valuable part: keep
  its specifics (names, values, thresholds) verbatim rather than paraphrasing

## Extract-concepts (`kind: extract-concepts`) — per-type scoping

Use `input.source_type` to scope what counts as a concept:
- `slack-channel` / `slack-search` / `gmail` → decisions, projects, people
- `rss` → techniques, products, announcements
- `jira` → issues, epics
- `google-calendar` → recurring meetings, projects and people discussed
- `confluence` → the systems, contracts and decisions the page documents; a page
  the user authored is usually the definitive statement of them, so prefer its
  naming over a synonym seen elsewhere
- `google-drive` / `manual` → document subject matter
- `tasks` → what the closing note established: the systems touched, the
  constraint found, the decision taken. The task's title names an intention and
  is rarely a concept; the note is where the knowledge is
