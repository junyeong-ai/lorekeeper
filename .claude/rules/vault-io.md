---
paths: ["crates/lk-vault/**/*.rs"]
---

- All vault writes go through `VaultWriter` (atomic temp+rename). Never write the final path directly.
- Frontmatter parsing is line-based (`---` delimiter). A substring scan mis-detects `---` inside YAML values.
- `replace_section` tracks fenced-code state — `## ` lines inside ``` blocks are not section boundaries. Heading lines are trimmed of trailing whitespace before matching.
- Template errors must propagate, not silently fall back. `TemplateEngine::available` returns `Ok(false)` only for not-found.
- Frontmatter values from LLM output must be JSON-escaped (`| tojson`) to prevent YAML injection.
- `IngestLog` distinguishes `NotFound` (legitimate "never ingested") from I/O errors.
