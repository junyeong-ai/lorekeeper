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

### `google-drive`, `manual`
- Treat as curated documents: preserve the author's structure, distill to the
  core argument and supporting detail

## Extract-concepts (`kind: extract-concepts`) — per-type scoping

Use `input.source_type` to scope what counts as a concept:
- `slack-channel` / `slack-search` / `gmail` → decisions, projects, people
- `rss` → techniques, products, announcements
- `jira` → issues, epics
- `google-calendar` → recurring meetings, projects and people discussed
- `google-drive` / `manual` → document subject matter
