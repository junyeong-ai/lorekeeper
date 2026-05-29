# lk-vault

Obsidian vault I/O. All writes go through here so atomicity lives in one place.

- **`VaultWriter::write_page` is atomic**: writes to a per-process-unique temp file
  (`pid + sequence`) then renames onto the final path. Never write the final path
  directly, and never share a temp name across writers.
- **Frontmatter parsing is line-based** (`frontmatter::parse_page`): a `---` is a
  delimiter only when it's the standalone first line and a later standalone `---` line.
  A leading BOM is stripped; CRLF is normalized to LF. A substring scan would
  mis-detect `---` inside a YAML value — don't reintroduce one.
- **`TemplateEngine::available` returns `Ok(false)` only for not-found**; a template
  that exists but fails to parse propagates `Err`. Callers must thread that through so a
  broken user template surfaces instead of silently falling back to the embedded
  renderer.
- **`IngestLog`** distinguishes `NotFound` (→ empty/None, legitimate "never ingested")
  from real I/O errors (propagated). Malformed JSONL lines are `tracing::warn`-ed and
  skipped, not silently dropped — corruption stays observable without blanking history.
- Frontmatter values derived from LLM output (e.g. concept `title`) are emitted as JSON
  strings (`serde_json::to_string` / `| tojson`) so quotes/colons can't break the YAML.
- **`section::{replace_section, section_body}`** operate on the body of a
  `## <heading>` section. `replace_section` rewrites it, `section_body` reads it
  as a `&str`. Both share `find_section`, which tracks fenced-code state so `## `
  lines inside ``` blocks are quoted content, not section boundaries, and trims
  trailing whitespace from heading lines. (The pipeline's "is this section filled?"
  predicate lives in `lk_pipeline::llm_cache`, built on `section_body`.)
- **`index::build_index`** generates `{wiki}/index.md` — a hierarchical page catalog
  grouped by category (concepts, documents, daily sources, work-log, synthesis).
  One-liner summaries are extracted from each page's primary heading.
  `write_index` handles the atomic write.
- **`VaultWriter::write_page_sync`** is a sync wrapper around the same
  atomic temp+rename flow. Used by graph commands (pure sync, no tokio runtime).
