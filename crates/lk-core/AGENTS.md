# lk-core

Shared domain/config/text contracts. Keep network and async orchestration out;
config loading and the shared filesystem primitives intentionally perform I/O.

## Config and events (`config.rs`, `event.rs`, `i18n.rs`)

- `Config::load` validates eagerly. Reject unknown keys, unsafe source IDs/vault
  directories, invalid cron/thresholds and unknown category references. Relative
  vault roots resolve against the config file. Use `yaml::from_str`: duplicate keys,
  YAML scalar policy, parser budgets and snippet-free errors are shared contracts.
  Config loading also derives semantic Work policy from parsed values in that same read,
  excluding only execution call/parallel/consecutive-call/screening-packet limits and profile timeouts. Preserve
  the separate complete byte revision for live workflow pinning. Direct deserialization
  has no loaded policy revision; it must not fabricate one from resolved paths.
- Filesystem vault roots remain `PathBuf` through resolution and runtime binding.
  YAML roots are strings; runtime binding does not recompute the loaded semantic policy.
- `SourceType::descriptor()` is exhaustive with no fallback. `streaming` means a
  past day cannot be completely refetched; `unit` decides daily versus document
  materialization. Adding a type requires deciding every descriptor trait.
- Classification rules are ordered, first-match, typed config outside adapter
  `params`; empty keywords are invalid. Highlights are additive category sections,
  never a filter hiding events.
- Daily `category` and personal `performance_category` are distinct taxonomies.
  The explicit bridge requires `personal` and a valid category. Resolution order:
  source map → content performance category → source-type fallback. Personal
  periods nest under `dirs.personal`; weekly themes under `dirs.synthesis`.
- `EventId::new` is `source:date:blake3(content)[..16]`. Normalization uses an
  external ID or JSON `[title, body]`, never ambiguous bare concatenation.
- When removing a locale label's last reader, inspect its actual typed usages and
  templates. Both locale tables initializing a public field does not prove it is
  used; a text match such as `.status` does not establish the receiver's type.

## Names, paths and evidence (`concept.rs`, `vault_path.rs`, `rss_evidence.rs`)

- `identity_key` uses NFC/whitespace normalization while preserving case, symbols,
  compatibility characters and word boundaries. `slugify` uses NFKC/lowercase and
  collapsed separators for safe addresses. Never use the lossy `path_slug` to
  identify graph nodes or evidence.
- `ConceptRegistry` indexes declared titles/aliases separately from reserved file
  addresses. Ambiguity refuses writes. `available_slug` disambiguates occupied
  addresses deterministically; that digest does not prove semantic difference.
  Existing addresses remain stable. Put translations/acronyms in `aliases`, not a
  parenthesized gloss appended to the canonical `ExtractedConcept.name`.
- `Citation` pins exact vault-relative paths and authored revisions. `source_revision`
  excludes only `updated`, `source_count`, `llm_inputs`; other frontmatter and body
  participate. All writers use sorted/deduplicated `citation_digest`.
- Explicit RSS selectors pin full immutable payload revisions, not EventIds or
  manifest hashes. Declarations name exact concept addresses and provided-manifest
  event/version pairs. Unknown schemas/fields or malformed selectors fail. With no
  selector, legacy two-field citation JSON and digests stay byte-identical.
- `frontmatter::field` owns private cross-crate protocol keys and completion-key
  derivation. Standard published page keys (`title`, `id`, `aliases`, etc.) remain
  literal vocabulary. Do not introduce a competing marker spelling.
- `Frontmatter` concept-role predicates distinguish an absent legacy `type` from
  an explicit different/null/invalid type. Collection membership is a separate address check.
- Concept categories are optional, validated IDs/labels. Discovery metadata retains
  each page's actual category, not the configured display label. Unknown extracted
  categories are dropped observably by the pipeline.

## Markdown and links (`link.rs`, `markdown.rs`, `text.rs`)

- `markdown::serialize` preserves structured link/image destinations and titles,
  including whitespace, without allowing metadata to introduce Markdown structure.
- `markdown::render_fragment` resolves each independent source before composition,
  contains EOF syntax and namespaces footnotes by its caller-supplied source identity.
  Page embedding supplies its minimum heading level; source assembly retains H1.
- `markdown::root_headings` owns CommonMark root structure. Semantic section bodies
  use `validate_section_body`: reject parent/sibling headings and syntax consuming
  the next section; preserve valid HTML and literal code without rewriting prose.
- Markdown byte ranges and physical line positions recognize LF, CRLF and CR.
  Parse through `markdown::MarkdownInput`: events represent CommonMark content,
  while offsets address the original bytes. Preserve originals separately from
  rendered presentation. Use `markdown::lines_with_endings` when counting or
  slicing original lines; frontmatter delimiters have their own grammar.
- Whole-page link consumers use `frontmatter::body_offset` to exclude metadata while
  preserving original bytes and physical line positions. Body parsers do not infer YAML.
- Use `link` for construction, extraction, rewrite and resolution. Links are
  CommonMark relative `.md` destinations; escape/percent-encode through shared
  helpers. Extraction excludes code/images/external schemes; preserving a link
  requires its label/destination pair. Read destination identity from CommonMark's
  parsed value, not its raw spelling; repoint with the shared label/title renderer.
  Resolution folds dot segments and accepts vault-absolute OKF addresses without
  escaping the root.
- `scan_defects` identifies converter cleanliness; `scan_credentials` reports
  issuer-defined token shapes, not verified live secrets. Never redact source
  content in a converter. A clean scan cannot certify absence of credentials.
- Credential grammars must distinguish prose using issuer field shapes/widths,
  not entropy or broad prefix/length guesses. Preserve longest-prefix matching,
  per-shape fall-through, alphanumeric left boundaries, quoted private-key headers
  and every occurrence per line. Read the grammar comments and positive/negative
  tests in `markdown.rs` before adding/narrowing a form; dropping an unsupported
  grammar is better than false coverage. Another scanner's threshold is not proof.
- Source Markdown and literal code retain their content. Normalize generator-owned
  separators where they are produced, never whole source bodies or rendered pages.

## Semantic ranges (`semantic.rs`)

- Plans bind exact resources, semantic purpose revision and serialized request/result
  bounds. Ranges are half-open Unicode scalar offsets, with Markdown block boundaries
  preferred and oversized blocks explicitly continued. Coverage is not a truth verdict.
- Keep subject, context, retained input, cited pages, immutable events and reviews
  distinguishable. Parts are execution units, never canonical knowledge nodes.
- Reuse requires identical assigned evidence and purpose; changed revisions invalidate
  dependent reductions. Never infer read coverage from an exit code or lexical match.

## Durable files (`fs.rs`)

- `write_atomic` replaces a destination: unique same-directory temp, fsync,
  optional permissions, rename, parent fsync, cleanup on error. Keep the extension
  before `.tmp` for existing suffix sweeps. Use this shared implementation.
  Create temporaries exclusively with requested Unix permissions before writing;
  preserve occupied names and clean up only the temporary this call created.
- `publish_exclusive`/`publish_exclusive_from` preserve originals: private short-name temp then exclusive
  hard-link publication. `Created`/`Reused` succeed; `Occupied` means different bytes
  or a non-regular/symlink entry. Other I/O errors propagate. Every suffix candidate
  must attempt this operation, not preflight existence then overwrite.
- Reuse compares and syncs a pinned regular inode without changing its permissions.
  Hard-link support is required; no overwriting fallback. It does not freeze
  external in-place edits or parent replacement, nor authorize deleting a source
  without separately checking consumed bytes. Long destination names must fit.

## Captured files and manual originals (`captured_file.rs`, `manual_original.rs`)

- `CapturedFile` owns a private RAII temporary file; streaming capture records exact
  length and digest. Streaming publication verifies both before exposing a destination.
  Dry runs may own scratch files but leave neither scratch nor retained evidence.
- Acquisition budgets are configuration, not immutable evidence validation rules.
  New manual-bundle-v2 manifests use the fixed512 MiB storage envelope; existing v1
  manifests keep their original digest domain and 32/64/256 MiB bounds.
- `content.rs` separates native decoding, visual engine configuration and explicit
  PDF scope. With vision configured, an omitted PDF policy selects all physical
  pages; native-only or selected-page processing is an explicit override. Projection
  provenance records packages, model artifacts and recipe independently of paths.
  New engines extend typed configuration and the source execution adapter, not
  document identity, citations or graph rules.

## RSS original serialization (`rss_original.rs`)

Capture v1 pre-classification event identity, adapter timestamp and normalized
article/feed metadata; exclude labels/ownership/classification. Unknown acquisition
time is `captured_at: null`, never mtime. Recursive object-key ordering plus
serde_json scalar encoding is this format, not RFC8785. Exact bytes determine full
BLAKE3 revisions; source/day/event identity alone does not identify content.
`RssEntryIdentity` frames feed/entry strings separately. Its retained metadata is
identity evidence; provenance checks never infer that tuple from title, URL or body.
