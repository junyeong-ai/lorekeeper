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
- **Names and addresses are separate.** `identity_key` normalizes NFC and whitespace while preserving case, compatibility characters, symbols
  and word boundaries. `ConceptRegistry`
  indexes titles and explicit aliases; a shared name is ambiguous and cannot route a write.
  `available_slug` reserves a safe filename, adding a deterministic name digest when the
  base is occupied. Existing addresses never change. A digest is address disambiguation,
  not a semantic equivalence test.
- **`ExtractedConcept` carries the name and the OTHER names separately.** `identity_key` is
  exact, so a `name` holding a gloss (`Agent Capability (에이전트 호출 가능 애플리케이션 단위)`)
  answers to neither the term nor the gloss: the page is unreachable by anything a person or a
  later extraction would write, and the next mention of the bare term mints a rival beside it.
  326 of 2689 pages in the reference vault reached that state because the struct had one field
  for it. `aliases` is where a translation, an expanded acronym or an original-language term
  goes; `lk-pipeline` decides which of them this vault can actually answer with.
- **`Citation`** identifies a source by its vault-relative path and authored revision.
  `source_revision` hashes the body and frontmatter except `updated`, `source_count`, and
  `llm_inputs`. `citation_digest` hashes the sorted, deduplicated citations; graph sync,
  extraction grounding, and queue requests use this same function.
- **`vault_path::path_slug`** normalizes candidate addresses for slug diagnostics.
  It is lossy and never identifies graph nodes or evidence. Graph node keys use
  `lk_graph::scan::page_id`; citations retain actual vault-relative paths.
- **`yaml::from_str`** is the shared YAML policy for configuration, frontmatter and
  extraction manifests. YAML 1.2 scalars, literal merge keys and parser budgets are
  consistent; duplicate keys are errors and diagnostics omit source snippets.
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
  on every commit id in the vault.
  **A length gate is enough only where the alphabet excludes `-`.** For a grammar whose
  alphabet admits it — every Slack form — the body alphabet IS the alphabet of English, so a
  runbook sentence naming a prefix reaches whatever floor a length gate sets. What separates
  them is the issuer's own FIELD SHAPE, and each field rule closed a prose class the previous
  one still admitted: `min_fields` (`xapp-` mints four — version, app id, ticket, secret — so
  `xapp-2-<one long word>` is two and a sentence); `numeric_fields` (the body opens with ids,
  so `xoxb-please-rotate-…` is not one); `numeric_field_digits`, because digit-ness alone
  admits a date — `xoxb-2026-01-tokenrotationrunbookforus` has two numeric fields and a long
  final one; and `min_unbroken_run` on the SECRET, which is the only rule left for the
  rotating forms, whose version field is one digit and so carries no width to require.
  Three more rules carry the rest: the left boundary is ALPHANUMERIC rather than the
  grammar's own alphabet, since several alphabets admit `_` and `-` and testing against them
  hides a key written after one; prefixes are tried LONGEST first, so a nested form
  (`xoxe.xoxb-…`) reads as itself rather than as the shorter one inside it; and private-key
  headers match a closed LABEL set rather than a suffix, which is what admits
  `PGP PRIVATE KEY BLOCK` while refusing `THIS IS NOT A PRIVATE KEY`. Markdown quoting is
  stripped before that match — a key pasted into a mail or a Slack thread reaches the vault
  behind `> `, which is exactly the page this ingests. Every occurrence on a line is reported,
  because an operator rotates what the report lists.
  **Recall is spent freely to keep every finding believable, and another scanner's threshold
  is not evidence for this one.** A push-protection gate on a shared repository and a report
  into one person's vault pay opposite prices for a false positive: there it costs a developer
  a few minutes and catches a real key sometimes, here it costs every finding its credibility
  in a place where the only reader can stop reading. GitHub's scanner flags this module's own
  false-positive EXAMPLES — the `xoxe-` rotation-runbook sentence among them, which this
  file cannot quote because quoting it is itself blocked. That corroborates the shapes being
  reachable from prose and argues nothing about what belongs in these grammars.
  A form whose findings a reader learns to scroll past protects nothing, so a prefix that
  cannot be told from text is
  DROPPED rather than reported loosely: AWS `ASIA` (an English word ahead of sixteen more
  uppercase characters); Anthropic, OpenAI project and admin, PyPI and GitLab keys, whose
  published prefixes are followed by an alphabet admitting `-` with no field shape to require;
  and both of Slack's legacy forms — the WORKSPACE tokens `xoxa-`/`xoxr-`, whose
  eight-to-forty-eight alphanumeric run behind an optional one-digit version is two shapes
  needing two grammars, and the legacy BOT token, one numeric field then a bare run. That last
  is the same shape as the five above with a single id doing the work two do in the modern
  grammar, and no floor closes it: nine digits rules out a date but not the epoch timestamps
  and object ids that actually sit beside `xoxb-` in tooling notes, while the published secret
  range of eighteen to twenty-six is exactly where compound words live — so
  `xoxb-<a ten-digit epoch>-workspacemigrationplan` would be a finding. Stripe's TEST forms are absent
  for the opposite reason — they appear in Stripe's own docs and grant nothing, so every hit is
  a finding with no action behind it; `xoxo-` for a plainer one, being what people sign messages with.
  **A prefix declared on a grammar it cannot match is worse than an absent one**: it reads as
  coverage while matching no token the issuer ever minted, and nothing fails to reveal it —
  `xoxr-` sat in the two-id group, where a legacy token carrying no ids matched nothing, for
  as long as it was declared. The same defect hides behind a prefix that is genuinely covered
  for ANOTHER shape, which is subtler: `xoxe-` matched the 146-character rotating form while
  the three-id user token that may also carry that prefix was rejected. So **a prefix carries
  every shape its issuer mints, each as its own grammar** — `find_map` falls through, so a
  prefix failing one is tried against the next — and each shape is pinned by a test, since a
  later narrowing is exactly what drops one silently. Fall-through is safe rather than lucky
  because every Slack grammar names one `CredentialForm` and they share one body alphabet: the
  order decides which rule answers, never what is consumed or what is reported.
  Widths track what the issuer mints, checked against it rather than assumed: `AKIA` at twenty
  rather than the sixteen-to-128 AWS permits, Slack ids at nine (one under the ten-to-thirteen
  it mints), the rotating secret at sixty (well under the 146 of a refresh token and the 163–166 of a
  configuration access token, so a re-mint at another width is still caught). Each accepts a
  silent miss to buy a believable finding, and each is commented at its grammar with the width
  it was set against. A prefix with NO published widths is dropped rather than carried on
  invented ones — `xoxc-`, for which no issuer or scanner publishes a grammar at all, and
  `xoxs-`, whose one published rule bounds no field.
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
