# lk-core

Domain types and config — no I/O, no async. Depended on by every other crate.

- **`Config::load` validates eagerly**: `validate()` rejects empty/`/`-containing source
  IDs, bad cron, out-of-range thresholds, unknown synthesis/category references, and
  `vault.dirs.*` values that are absolute or contain `..` (path-traversal guard before any
  path is built). A relative `vault.root` is resolved against the config file's parent
  directory. Every config struct — top-level (`Config`, `VaultConfig`, `VaultDirs`,
  `Identity`, `SourceConfig`, `PersonalConfig`) as well as the nested ones — carries
  `#[serde(deny_unknown_fields)]`, so a typo'd key fails at load instead of being silently
  ignored.
- **`SourceType` is a closed enum**; its static per-variant traits live in one place,
  `SourceType::descriptor() -> SourceDescriptor` (`streaming`, `default_template`,
  `item_kind`). The match is exhaustive with NO catch-all, so adding
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
- **`SourceConfig.highlights`** (`Vec<HighlightSection { category, label }>`) are
  config-driven daily-page sections: the renderer surfaces events whose `Event::category`
  matches under `label`, ABOVE the full event list (additive — never hides an event). The
  core branches on NO source type; a source declares its own buckets (or none — the empty
  default). Validated: non-blank `category`+`label`, no duplicate `category` per source.
- **Two orthogonal taxonomies, one explicit bridge.** `ClassifyRule.category` is a
  daily-page *grouping* bucket (→ `Event::category`); `personal.performance_categories`
  is the *contribution* taxonomy (→ work-log/reviews), and lives in the OPTIONAL personal
  module. They never share a value space. A rule's optional `ClassifyRule.performance_category`
  (→ `Event::performance_category`) is the ONLY explicit link between them — validated at
  load to require a `personal:` section AND a real `personal.performance_categories` id (a
  bridge with no `personal:` is a rejected contradiction).
  `PersonalConfig::resolve_category` precedence: `source_category_map[id]` →
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
- **`identity_key()` is `slugify` keeping only the separators that MEAN something** — the
  ADDRESS a name is written at vs the IDENTITY it claims. A slug must stay readable as a
  filename so it keeps every break; identity keeps a break only where it changes the name.
  Every break is typography — `Vector DB` / `vector-db` / `vectordb` are one name, as are
  `claude-35` / `claude35` — EXCEPT one between two NUMERALS, which is the name itself:
  positional notation makes `3-5` two numerals and `35` one, so `Claude 3.5` ≠ `Claude 35`,
  `GPT-4.1` ≠ `GPT-41`, `Web 2.0` ≠ `web20`. Dropping every separator folded exactly those
  together, and version-numbered names are the most common shape in a technology vault. A
  break on only ONE side of a numeral is still typography (`GPT-4o` = `gpt4o`,
  `ISO-8601` = `iso8601`). "Numeral" is `char::is_numeric` (Nd ∪ Nl ∪ No), so a digit NFKC
  leaves alone — Arabic-Indic, Devanagari — counts, while `Ⅴ` decomposes to a letter.
  Nothing else is folded: word order and every character are identity (`agent-harness` ≠
  `harness-agent`, `http` ≠ `https`, `doc-hub` ≠ `docs-hub`) — whether two names mean one
  concept is a judgment, and lives in `/lore-wiki audit`, never here.
  Two boundaries stay uncrossed on purpose. A digit break is kept even when it only GROUPS
  one number (`978-0-13-468599-1` ≠ `9780134685991`) — the price of telling `Claude 3.5`
  from `Claude 35`, paid in a vault full of version numbers and empty of ISBNs. And
  separator TYPE is gone before this runs (`slugify` maps `:`/`.`/`/`/space alike, so
  `16:9` = `16-9`); recovering it would cost the slug its filename safety. Single-sourced because
  two consumers must agree EXACTLY: `lk_pipeline`'s alias index routes an extracted name to
  the page owning it, and `lk_graph`'s duplicate lint reports two pages owning one name.
  **The lint only REPORTS; the index ACTS** — it can fold an extraction into an established
  page, and that fold is not reviewable after the fact (only one page ever exists, so the
  lint has no pair to compare and the extraction's synthesis is dropped as a later mention).
  So the fold must be one no reviewer would overturn; that asymmetry, not tidiness, is why
  it stays this narrow.
- **`link`** is the single implementation of the vault's link vocabulary: inline
  markdown links `[Display](relative/path.md)`, destinations relative to the containing
  page and always `.md`-suffixed. Construction (`md_link` + `relative_dest`, CommonMark
  percent-encoding), extraction (`extract_page_links` — fence/inline-code-aware, images and
  external schemes excluded — and `extract_dests`, defined on top of it so the two can never
  disagree about which links a body carries; take the pair form when a link must be KEPT rather
  than merely observed, since its display text is someone else's to preserve, and the escaping
  it resolves is exactly what `md_link` applies), rewriting (`rewrite_links_outside_code`), and lexical
  resolution (`resolve_dest` — folds `.`/`..`, accepts the OKF `/`-absolute form,
  refuses to escape the vault root). Consumed by lk-graph/lk-vault/lk-pipeline —
  never re-derive link syntax elsewhere.
- **`fs::write_atomic(path, contents, mode)`** is the single atomic file write:
  a per-writer-unique temp (pid + process-global sequence) in the same dir, fsync,
  optional `chmod`, rename, dir-fsync — then temp cleanup on failure. Every writer
  (queue files, credentials `0600`, event log) goes through it so the durability +
  unique-temp invariant can't drift; `lk_vault::VaultWriter` delegates here from both its
  sync and its async (tokio `spawn_blocking`) paths — not a second implementation. The
  temp keeps `path`'s extension before `.tmp` so suffix sweeps still match.
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
  the drop (observable parity with the queue-path `graph lint`). The categories also order
  and label the `### {category}` groups in the single-file `lore wiki index` catalog.
- **`LlmConfig` defaults to `provider: queue`** (matches docs/example). Uses
  `deny_unknown_fields` so typos in config keys are caught at load time.
- **`VaultDirs` field name == default directory value** for every time period:
  `weekly`/`monthly`/`quarterly`/`annual` each default to a directory of the same
  name. The personal reviews (OPTIONAL personal module) nest under `dirs.personal`:
  `<personal>/weekly/`, `<personal>/monthly/`, `<personal>/quarterly/`,
  `<personal>/annual/`. Cross-source weekly themes (core) live under `dirs.synthesis`:
  `<synthesis>/weekly/`. The `weekly` subdir name is shared by both; the rest are
  used only when the personal module is configured.
