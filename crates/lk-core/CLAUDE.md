# lk-core

Domain types and config — no I/O, no async. Depended on by every other crate.

- **A dead `Strings` field is a page-format section no page has, and no test can find it.**
  Both locale tables initialize every field, so `dead_code` sees them all used; the fields
  are `pub`, so it would assume downstream readers even if they did not. Two fields had
  outlived their readers before anyone noticed — one a concept heading the template stopped
  emitting, one a title the manual source stopped using — and `lore schema` publishes this
  bundle as the page-format spec, so each described a section of a page that does not exist.
  A gate over it was tried and REMOVED: deciding whether a name is read as `Strings::status`
  or as `resp.status()` needs the receiver's type, and every text-level narrowing either
  whitelists receivers (`resp`, `out`, `entry` are not labels; the list is open) or curates a
  renderer file list (silently stale the first time a renderer is added). It protected the
  distinctively-named fields and not the common ones while reading as uniform, which is worse
  than the sweep it replaced. The sweep, run when a label's last reader might have gone:
  `for f in $(rg -o 'pub (\w+): &.static str' -r '$1' crates/lk-core/src/i18n.rs); do rg -q
  "\.$f\b" --glob '!crates/lk-core/src/i18n.rs' crates templates || echo "$f"; done` — then
  read each hit, since that command has the same ambiguity and only a human resolves it.

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
- **`ConceptRegistry` is the one answer to "which page owns this name".** It maps every name
  a concept page answers to — its own ADDRESS (the file stem), its `title`, and each alias —
  through `identity_key`, and `resolve` returns `Owned` / `Ambiguous` / `Absent`. Pure over
  `(slug, title, aliases)` triples, so each caller feeds it from its own I/O: `lk-pipeline`
  from a `VaultStore` while routing an extraction, `lore resolve` from a directory read while
  answering a skill about to write a page. Two implementations is what lets the write path
  fold `VectorDB` onto `vector-db` while the read path calls the name free and mints a rival.
  A page claims its own address unconditionally and an address outranks any other page's name,
  so a stale alias cannot redirect a concept away from its own page; among equals the earliest
  registration routes, which makes a citation's destination reproducible without knowing how
  many pages claim the name. Ambiguity is REPORTED rather than settled — routing still has to
  pick, and `Resolution::Ambiguous` carries both the pick and every claimant, because a
  citation landing on the page a reader did not expect is otherwise unexplainable.
- **`citation_digest`** is the identity of a concept's EVIDENCE: BLAKE3-128 over the sorted,
  deduplicated set of pages citing it, serialized as a JSON array so no id is confusable with
  a pair of shorter ones. The SET, never the rendered citation list — a source page that is
  retitled changes how a citation reads without changing what it is, and a digest over the
  text would resurface a concept whose material is identical. Single-sourced because
  `lk-graph` records it on the page and `lk-queue` carries it as the task's input; two
  implementations would be a task that can never match the page it names.
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
- **`markdown` holds two contracts over vault text, and they are not the same kind of thing.**
  `scan_defects` is the CLEANLINESS contract the converters uphold at conversion time, so a
  finding means a page predates a tightening and re-rendering repairs it. `scan_credentials`
  is the opposite shape: nothing prevents it, converters must NOT strip it (editing a message
  to remove a key leaves the page asserting something nobody wrote while the key stays live),
  and the repair is at the issuer. It names a SHAPE, never a verdict — AWS publishes
  `AKIAIOSFODNN7EXAMPLE` in its own docs, so a page quoting it matches while holding no key,
  and only the issuer can tell the two apart. It reads only forms whose ISSUER publishes a
  grammar — a reserved prefix followed by a run of that issuer's own alphabet at the width it
  mints — so a hit is a fact about the text rather than a guess, and a clean scan is
  explicitly NOT a statement that no secret is present. Nothing inferred: a 40-character
  base64 run is a key or a hash and the text does not say which, so an entropy rule would fire
  on every commit id in the vault. Four rules carry the precision: the length gate separates a
  credential from prose naming its prefix; the left boundary is ALPHANUMERIC rather than the
  grammar's own alphabet, since several alphabets admit `_` and `-` and testing against them
  hides a key written after one; prefixes are tried LONGEST first, so a nested form
  (`xoxe.xoxb-…`) reads as itself rather than as the shorter one inside it; and private-key
  headers match a closed LABEL set rather than a suffix, which is what admits
  `PGP PRIVATE KEY BLOCK` while refusing `THIS IS NOT A PRIVATE KEY`. Markdown quoting is
  stripped before that match — a key pasted into a mail or a Slack thread reaches the vault
  behind `> `, which is exactly the page this ingests. Every occurrence on a line is reported,
  because an operator rotates what the report lists.
- **`text::collapse_blank_lines`** squeezes 3+ newlines to a paragraph break,
  strips `\r`. Single source consumed by lk-vault, lk-pipeline, lk-source.
- **`frontmatter::field`** single-sources this system's PRIVATE machine-coordination
  protocol keys (`SOURCE_COUNT`, `LLM_INPUTS`, `SYNTHESIS`, `completion`) — names invented here
  with no meaning outside the tooling — so an internal rename can't silently break the
  cross-crate agreement; `Frontmatter::source_count()` owns the parse. The criterion is
  *internal protocol that crosses a crate boundary*: e.g. `LLM_INPUTS` — pipeline writes,
  `queue status` in lk-cli reads (its inner per-kind keys are single-sourced by
  `lk_queue::TargetKind::llm_inputs_key`, which reads `SYNTHESIS` from here because `graph
  backlinks-sync` writes that one key and cannot see the enum; `completion` derives every
  `<key>_done` marker so a writer in one crate and a reader in another cannot spell it
  differently). Standard published vault vocabulary (`created`,
  `updated`, `title`, `id`, `aliases`) is read across crates too but stays a literal on
  purpose: it is anchored to the external Obsidian page format, never the target of a silent
  internal rename, so a constant would add no protection — only noise.
- **`ConceptConfig`** (`concepts:` in YAML): optional `categories` list
  (`Vec<ConceptCategory>`, each with `id` + `label`). Validated: no empty ids, no
  empty labels, no duplicate ids. Empty list = no categorization (concepts get no
  `category` field). `ExtractedConcept` carries an optional `category` assigned by
  the LLM from this list; the pipeline drops unknown category IDs and `tracing::warn`s
  the drop (observable parity with the queue-path `graph lint`). The catalog does NOT read this
  list: `build_index` takes no config, so its `### {category}` groups come from each page's own
  `category` frontmatter, printed verbatim and ordered alphabetically by the `BTreeMap` that
  collects them — a `label` configured here never reaches a heading.
- **`LlmConfig` defaults to `provider: queue`** (matches docs/example). Uses
  `deny_unknown_fields` so typos in config keys are caught at load time.
- **`VaultDirs` field name == default directory value** for every time period:
  `weekly`/`monthly`/`quarterly`/`annual` each default to a directory of the same
  name. The personal reviews (OPTIONAL personal module) nest under `dirs.personal`:
  `<personal>/weekly/`, `<personal>/monthly/`, `<personal>/quarterly/`,
  `<personal>/annual/`. Cross-source weekly themes (core) live under `dirs.synthesis`:
  `<synthesis>/weekly/`. The `weekly` subdir name is shared by both; the rest are
  used only when the personal module is configured.
