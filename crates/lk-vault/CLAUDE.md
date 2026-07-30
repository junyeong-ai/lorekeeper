# lk-vault

Obsidian vault I/O. All writes go through here so atomicity lives in one place.

- **`VaultWriter::write_page` is atomic + durable**: it delegates to
  `lk_core::fs::write_atomic` (the single atomic-write implementation: per-writer-unique
  temp → fsync → rename → dir-fsync) on the blocking pool via `spawn_blocking`, since file
  I/O is blocking and must fsync. There is no second atomic-write implementation to drift —
  never write the final path directly.
- **Frontmatter parsing is line-based** (`frontmatter::parse_page`): a `---` is a
  delimiter only when it's the standalone first line and a later standalone `---` line.
  A leading BOM is stripped; CRLF is normalized to LF. A substring scan would
  mis-detect `---` inside a YAML value — don't reintroduce one.
- **`TemplateEngine::has_user_override` returns `Ok(false)` only for not-found**; a template
  that exists but fails to parse propagates `Err`. Callers must thread that through so a
  broken user template surfaces instead of silently falling back to the embedded
  renderer.
- **Daily templates inherit from `_daily_base.md.jinja`** (`{% extends %}`): the base owns
  the frontmatter (incl. the `llm_inputs` ladder, single-sourced) + summary, the generic
  `highlights` loop (any source with configured highlight sections), the events/concepts
  skeleton. Each per-type child (gmail/jira/slack-*/google-*/rss/confluence) is a thin override
  of only `{% block title %}`, `{% block items_heading %}` (events vs messages), and
  `{% block events %}` (item rendering). `_`-prefixed templates are partials — never a page
  `default_template`. `template::tests::every_daily_template_renders_with_expected_frontmatter`
  selects the children by partitioning `EMBEDDED` against a list of the NON-daily templates, so
  a new daily template joins it by default — a hand-list of the children omitted `confluence`,
  and a renamed block there left every Confluence page taking the base's generic title with the
  whole suite green. A missing block is caught by comparing the rendered title against that
  default, not by the key being present, because the key is present either way.
- **`concepts` entries arrive FULLY RENDERED, bullet included** — a template emits `{{ c }}`,
  never `- {{ c }}`. That section has a second writer, `lore queue apply`, which replaces it
  without going through any template, so a bullet stated in the template as well would be a
  second answer to what a citation looks like. A `--template-dir` override participates in this
  contract: a user template still carrying the prefix renders `- - [C](…)` on every page it
  owns, and nothing detects it. The daily and document legs are each pinned by a test so the
  embedded copies cannot drift; an override is the user's to keep in step.
- **`IngestLog`** distinguishes `NotFound` (→ empty/None, legitimate "never ingested")
  from real I/O errors (propagated). Malformed JSONL lines are `tracing::warn`-ed and
  skipped, not silently dropped — corruption stays observable without blanking history.
  **`find_last_collection` answers "when was this source last OBSERVED", not "when did it
  last produce something"** — the question `lore status` and `lore health` both actually
  ask. `LogStatus::is_collected` owns the split, exhaustively: `Skipped` is written at
  exactly one site (a fetch that succeeded and yielded no pages), so it carries an
  answer — "nothing happened" — and only `Failed` leaves the window unobserved. Counting
  an empty run as no run is what let a quiet source read as overdue indefinitely while it
  ingested correctly every morning, and a warning that never clears is one its reader
  stops reading.
- Frontmatter values derived from LLM output (e.g. concept `title`) are emitted as JSON
  strings (`serde_json::to_string` / `| tojson`) so quotes/colons can't break the YAML.
- **`section::{replace_section, section_body}`** operate on the body of a
  `## <heading>` section. `replace_section` rewrites it, `section_body` reads it
  as a `&str`. Both share `find_section`, which tracks fenced-code state so `## `
  lines inside ``` blocks are quoted content, not section boundaries, and trims
  trailing whitespace from heading lines. (`lk_pipeline::llm_cache` reads `section_body`
  to splice a preserved LLM-owned body back on a cache hit; completion is marker-signalled,
  never inferred from whether a section is empty.)
- **`index::build_index`** generates `{wiki}/index.md` — a single hierarchical page catalog
  grouped by category (concepts, documents, daily sources, work-log, synthesis), NEVER split
  into sub-pages (like `log.md`/`map.md`). Each entry is `[title](relative-path) — first-sentence summary`,
  the summary extracted from the page's type-specific `## ` section body (concept synthesis,
  daily/document summary — heading resolved from the i18n bundle) and bounded by
  `truncate_summary` (first sentence, `MAX_SUMMARY_BYTES` cap) so a line stays scannable and
  the catalog grows linearly. `write_index` handles the atomic write.
- **`timeline::build_timeline`** generates `{wiki}/log.md` — a reverse-chronological
  knowledge timeline (when each concept/document/exploration first entered the vault).
  A materialized view like the index (regenerate → byte-identical), with two rules:
  anchored on `created` ONLY (never `updated`, which churns on re-mention/machine work
  and would inject fake "knowledge changed" events); and durable knowledge nodes only
  (daily/synthesis excluded — a principled split, not a fuzzy score). Complete like
  `index.md` — every knowledge node ever created appears, never truncated (text-only,
  grows linearly in node count). `log.md` is in `RESERVED_WIKI_FILES`, so the graph
  never flags it as an orphan or index drift.
- **Frontmatter writers are bounded by `frontmatter_block`** — the shared line range between
  the delimiters, recognized by the same first-line rule and `is_delimiter_line` predicate as
  `parse_page`. `set_frontmatter_field` sets a TOP-LEVEL (column-0) key — never an indented
  one nested under a mapping (e.g. `summary:` under `llm_inputs:`) — inserting before the
  closing `---` when absent; `set_llm_input` sets a key one level under the block-style
  `llm_inputs:` mapping, which `set_frontmatter_field` cannot reach. Both preserve a leading
  BOM, copy every other line byte for byte, and take `value` VERBATIM — the caller owns
  serialization (`serde_json::to_string`), so neither hand-rolls quoting. Both return
  `Option`: `None` when the key has no place in the block (no frontmatter at all; for
  `set_llm_input`, no block-style `llm_inputs:` line). That is the whole point of the shared
  range — a writer that cannot place a key writes NOTHING rather than falling through to the
  end of the document and appending it to the body, where `llm_cache` would never read it and
  the task would re-enqueue forever, growing the page each round. A caller turns `None` into a
  named error wherever the key it could not place was needed; `graph normalize` is the one that
  does not, because a page with no frontmatter has no stale `id` for it to rewrite — the graph
  id comes from the path.
- **What belongs to an entry is `continuation_span`'s answer**, and every defect found in
  these writers has been a wrong one. Each rule below fixed a page-corrupting bug, so change
  none of them casually: a continuation is a line indented DEEPER than the entry's own line
  (one function serves a top-level key and an `llm_inputs` child — a sibling ends the latter,
  a column-0 key the former); a BLANK line neither ends an entry nor belongs to it; a COMMENT
  belongs to whatever FOLLOWS it, at ANY indentation — except inside a BLOCK SCALAR
  (`key: |`, `key: >`), the one place a `#` line is value text. Which lines those are is
  tracked PER LINE, from the nearest preceding shallower line that OPENED one — key-bearing
  (`key: |`) or not (`- |`, a list item having no colon to look behind). The entry a span
  belongs to cannot answer for the lines under it: a span over `llm_inputs:` walks its
  children, and `llm_inputs:` is not a block scalar even when a child is. Replacing a key
  takes its whole
  span, so a block-style value never outlives the key it belonged to. And `set_llm_input`
  reads the child indentation from the first REAL child, never from the span's first line,
  which may be a comment indented past them.
- **`VaultWriter::write_page_sync`** calls `lk_core::fs::write_atomic` directly (no tokio
  runtime). Used by graph commands — the same single atomic-write implementation as the
  async path, not a separate one.
