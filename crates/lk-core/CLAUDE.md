# lk-core

Domain types and config — no I/O, no async. Depended on by every other crate.

- **`Config::load` validates eagerly**: `validate()` rejects empty/`/`-containing source
  IDs, bad cron, out-of-range thresholds, unknown synthesis/category references, and
  `vault.dirs.*` values that are absolute or contain `..` (path-traversal guard before any
  path is built). A relative `vault.root` is resolved against the config file's parent
  directory. Every config struct — top-level (`Config`, `VaultConfig`, `VaultDirs`,
  `Identity`, `SourceConfig`, `PerformanceConfig`) as well as the nested ones — carries
  `#[serde(deny_unknown_fields)]`, so a typo'd key fails at load instead of being silently
  ignored.
- **`SourceType` is a closed enum**; its static per-variant traits live in one place,
  `SourceType::descriptor() -> SourceDescriptor` (`streaming`, `default_template`,
  `highlight_categories`, `item_kind`). The match is exhaustive with NO catch-all, so adding
  a source type is a compiler-forced complete decision here (+ a `lk-source` adapter/factory
  arm) — no trait can silently default (a new streaming source quietly flagged non-streaming
  would lose scrolled-out items). Set `streaming: true` only if the source CANNOT completely
  re-fetch a past day (a rolling/capped feed like RSS) — that gates the per-date event-log
  accumulation in `lk-pipeline`. Don't replace the enum with a runtime registry —
  exhaustive matching is the point.
- **`SourceConfig.classify`** is a `Vec<ClassifyRule>` (ordered rules, first match
  wins), kept OUT of the free-form `params` so adapter params can use
  `deny_unknown_fields`. `ClassifyRule` itself is `deny_unknown_fields` too, so an
  unknown key in a rule fails at load instead of being silently ignored. Validation
  rejects rules with empty keywords.
- **Two orthogonal taxonomies, one explicit bridge.** `ClassifyRule.category` is a
  daily-page *grouping* bucket (→ `Event::category`); `performance.performance_categories`
  is the *contribution* taxonomy (→ work-log/reviews). They never share a value space.
  A rule's optional `ClassifyRule.performance_category` (→ `Event::performance_category`)
  is the ONLY explicit link between them — validated at load to be a real
  `performance_categories` id.
  `PerformanceConfig::resolve_category` precedence: `source_category_map[id]` →
  `performance_category` (content signal) → `source_type_category_map[type]` (coarse
  fallback). The content signal deliberately OUTRANKS the per-type default so a genuine
  signal beats the "all Jira = project-delivery" blanket. No string-coincidence magic.
- **`EventId::new(source_id, date, content)`** = `source:date:blake3(content)[..16]`.
  In `lk-pipeline::normalize`, `content` is the `external_id` or a JSON array of
  `[title, body]` — never a bare concatenation (that collides).
- **`slugify()`** NFKC-normalizes → lowercases → maps every non-alphanumeric char
  (including spaces, punctuation, and literal `-`) to a separator, collapses runs to a
  single `-`, and trims edges. Concept slugs are always re-normalized through it to
  prevent path injection from LLM output.
- **`wikilink::extract_wikilinks`** skips fenced code blocks and inline code spans
  to prevent false edges in the wiki graph. Closing fence detection requires no info
  string after the marker (per CommonMark). Single source consumed by lk-graph.
- **`fs::write_atomic(path, contents, mode)`** is the single sync atomic file write:
  a per-writer-unique temp (pid + process-global sequence) in the same dir, fsync,
  optional `chmod`, rename, dir-fsync — then temp cleanup on failure. Every sync writer
  (queue files, credentials `0600`, graph cache, event log) goes through it so the
  durability + unique-temp invariant can't drift; `lk_vault::VaultWriter` is the async
  sibling. The temp keeps `path`'s extension before `.tmp` so suffix sweeps still match.
- **`text::collapse_blank_lines`** squeezes 3+ newlines to a paragraph break,
  strips `\r`. Single source consumed by lk-vault, lk-pipeline, lk-source.
- **`frontmatter::field`** single-sources this system's PRIVATE machine-coordination
  protocol keys (`SOURCE_COUNT`, `AUDITED_SOURCES_HASH`, `LLM_INPUTS`) — names invented here
  with no meaning outside the tooling — so an internal rename can't silently break the
  cross-crate agreement; `Frontmatter::source_count()` owns the parse. The criterion is
  *internal protocol that crosses a crate boundary*: e.g. `LLM_INPUTS` — pipeline writes,
  `queue status` in lk-cli reads (its inner per-kind keys are single-sourced by
  `lk_queue::TargetKind::llm_inputs_key`). Standard published vault vocabulary (`created`,
  `updated`, `title`, `id`, `aliases`) is read across crates too but stays a literal on
  purpose: it is anchored to the external Obsidian page format, never the target of a silent
  internal rename, so a constant would add no protection — only noise.
- **`ConceptConfig`** (`concepts:` in YAML): optional `categories` list
  (`Vec<ConceptCategory>`, each with `id` + `label`). Validated: no empty ids, no
  empty labels, no duplicate ids. Empty list = no categorization (concepts get no
  `category` field). `ExtractedConcept` carries an optional `category` assigned by
  the LLM from this list; the pipeline drops unknown category IDs and `tracing::warn`s
  the drop (observable parity with the queue-path `graph lint`).
  `index_split_threshold` (default 100) controls when `lore wiki index` splits
  concepts into per-category sub-pages (`<wiki>/index/{category}.md`).
- **`LlmConfig` defaults to `provider: queue`** (matches docs/example). Uses
  `deny_unknown_fields` so typos in config keys are caught at load time.
- **`VaultDirs` field name == default directory value** for every time period:
  `weekly`/`monthly`/`quarterly`/`annual` each default to a directory of the same
  name. Personal performance paths nest under `dirs.personal`: `<personal>/weekly/`,
  `<personal>/monthly/`, `<personal>/quarterly/`, `<personal>/annual/`. Team
  synthesis lives under `dirs.synthesis`: `<synthesis>/weekly/`. The period names are
  shared as subdirectory names within both `<personal>` and `<synthesis>`.
