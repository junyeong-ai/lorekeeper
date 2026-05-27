# lk-core

Domain types and config — no I/O, no async. Depended on by every other crate.

- **`Config::load` validates eagerly**: `validate()` rejects empty/`/`-containing source
  IDs, bad cron, out-of-range thresholds, empty dedup cascade, unknown synthesis/category
  references, and `vault.dirs.*` values that are absolute or contain `..` (path-traversal
  guard before any path is built). A relative `vault.root` is resolved against the config
  file's parent directory.
- **`SourceType` is a closed enum** with `default_template_name()` co-located on it.
  Adding a source type is a compiler-checked change here + a `lk-source` adapter/factory
  arm. Don't replace it with a runtime registry — exhaustive matching is the point.
- **`SourceConfig.classify`** is a `Vec<ClassifyRule>` (ordered rules, first match
  wins), kept OUT of the free-form `params` so adapter params can use
  `deny_unknown_fields`. Validation rejects rules with empty keywords.
- **`EventId::new(source_id, date, content)`** = `source:date:blake3(content)[..16]`.
  In `lk-pipeline::normalize`, `content` is the `external_id` or a JSON array of
  `[title, body]` — never a bare concatenation (that collides).
- **`slugify()`** lowercases + strips to `[alnum-]`; concept slugs are always
  re-normalized through it to prevent path injection from LLM output.
- **`wikilink::extract_wikilinks`** skips fenced code blocks and inline code spans
  to prevent false edges in the wiki graph. Closing fence detection requires no info
  string after the marker (per CommonMark). Single source consumed by lk-graph.
- **`text::collapse_blank_lines`** squeezes 3+ newlines to a paragraph break,
  strips `\r`. Single source consumed by lk-vault, lk-pipeline, lk-source.
- **`ConceptConfig`** (`concepts:` in YAML): optional `categories` list
  (`Vec<ConceptCategory>`, each with `id` + `label`). Validated: no empty ids, no
  empty labels, no duplicate ids. Empty list = no categorization (concepts get no
  `category` field). `ExtractedConcept` carries an optional `category` assigned by
  the LLM from this list; the pipeline silently drops unknown category IDs.
  `index_split_threshold` (default 100) controls when `lore wiki index` splits
  concepts into per-category sub-pages (`wiki/index/{category}.md`).
- **`LlmConfig` defaults to `provider: queue`** (matches docs/example). Uses
  `deny_unknown_fields` so typos in config keys are caught at load time.
- **`VaultDirs.annual`** is the config key; its default directory value is `"annually"`.
  Personal performance paths nest under `dirs.personal`: `me/weekly/`, `me/monthly/`,
  `me/quarterly/`, `me/annually/`. Team synthesis lives under `dirs.synthesis`:
  `synthesis/weekly/`. Time-period names (`weekly`, `monthly`, etc.) are shared as
  subdirectory names within both `personal` and `synthesis`.
