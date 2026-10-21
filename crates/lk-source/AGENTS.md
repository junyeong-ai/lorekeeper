# lk-source

Source adapters return observations; the pipeline owns deduplication/rendering.
Follow the root AGENTS.md for shared IO/config/error contracts. `Source::extract`
uses validated params and context; do not add per-adapter output pipelines.

## Shared adapter contracts (`lib.rs`, `paging.rs`, `fetch.rs`)

- Runtime and offline validation share `parse_validated`: reject unknown fields and
  invalid semantic values. Diagnose top-level source fields separately from params.
- Use the requested date's `day_window` in the vault timezone, not today's clock.
- `require_any_observation` counts reachable usable observations before window
  filtering. A genuinely empty listing succeeds; all attempted observations failing
  is `NothingObserved`. Partial success warns, not a fabricated quality percentage.
- `Source::observe` declares actual query reach and incomplete pagination/readings.
  A configured query is not proof of date coverage. Optional enrichment warnings
  do not turn a complete source listing into missing observations. Adapters without
  an observation contract remain explicitly unverified.
- Pagination follows the provider's continuation even after an empty page. Require
  positive caps; warn when overshoot/pending continuation proves truncation, not
  merely when the result happens to equal the cap. Keep the `MAX_PAGES` guard.
- Retry idempotent requests for 429/5xx/connect/timeout with numeric Retry-After;
  rotating-token refresh is not an idempotent retry. Report incomplete reach.
- Shared readable fetching streams a bounded body and refuses over-limit responses,
  rather than truncating. Timeout is not a memory cap. Decode the HTTP Content-Type
  charset once via encoding_rs; do not reinterpret an already decoded meta charset.
  Private redirect targets are intentionally supported for intranet sources.
- Remote readable fetch requires a supported HTTP media type. HTML uses readability;
  Markdown keeps its structure and rebases parsed Link/Image destinations against the
  final response URL. A `text/plain` URL ending in `.md`/`.markdown` uses that file
  format; other plain text is escaped as literal prose. Raw HTML inside Markdown stays
  unchanged and is subject to manual capture's dependency checks. Unknown media/charset,
  malformed text and oversized bodies are errors; RSS retains its feed content.
- `is_self` uses structured identity: Gmail From email, Slack user ID, Jira assignee,
  Calendar organizer/attendee. Drive/RSS/manual are false; nodex's authored kind is
  not a Git author identity.

## Provider-specific seams

- `atlassian/`: auth method determines deployment routing. OAuth/scoped token uses
  api.atlassian.com product/cloud ID; API token/PAT uses the site (PAT Bearer).
  Keep Cloud/Server pagination and user identity separate. Persist a rotated refresh
  token before use and share one auth owner; do not retry refresh blindly.
- `credentials.rs`: validate regular-file and Unix owner-only permissions on the
  opened credential descriptor before reading content, and on the companion lock
  before acquiring it. Missing stores are empty; unreadable or malformed stores
  fail without reset. Parse diagnostics expose location, never credential values.
- `jira.rs`: Cloud v3 search/jql uses tokens; Server/DC v2 uses offsets/total. Cache
  `/myself` with OnceCell and fail on identity failure. Preserve ADF and due/start
  date headers rather than losing work semantics in text conversion.
- `confluence.rs`: treat CQL as a provider-validated filter, intersect it with the date
  window and add ordering. Never infer query semantics from substrings. Relative
  provider functions evaluate at acquisition, not the requested historical date.
  Re-anchor next links to the authenticated API base and follow `page_step`.
  External identity preserves the supplied version; unavailable version/body makes
  the observation partial. `only_my_edits` compares the current author.
- `gmail.rs`: epoch bounds and parenthesized OR terms; queries are required/nonblank.
  An unread-only filter is an explicit user choice, not an implicit default.
- `slack.rs`: form-encode API calls. Cursor pagination is authoritative, not
  `has_more`; distinguish bare channel IDs from `#` lookup. Watch-user author/mention
  semantics, thread/bot filters, and user-token search remain explicit. Split only
  headings, not code, mentions, or URLs.
- `calendar.rs`: construct URL path segments safely. All-day civil dates use the
  vault timezone; skip invalid timestamps diagnostically.
- `drive.rs`: select direct children by explicit `folder_id`, never a folder name.
  Query the actual modified-time day window: `lookback_days` defaults to1 prior civil
  day plus the target day; explicit0 selects only the target day. Query and reported
  observation bounds agree, including historical targets. This overlap does not
  establish automatic outage recovery or revision-history coverage. `name_prefix` uses Drive's literal
  prefix matching, not a date template, glob or arbitrary substring. Files use
  provider identity independent of folder/title and become document pages.
  Recheck selected metadata/version
  after bounded, strictly decoded text capture; changed or unsupported files stay
  unobserved. Report provider gaps, unfinished pagination and per-file failures as
  partial reach. Docs use Markdown export, Slides plain-text export; neither claims
  embedded media, comments or revision-history preservation. A received-byte digest
  identifies the captured text payload, not an archived provider original.
- `rss.rs`: prefer HTML content then summary; use published/updated dates, never
  acquisition-now as publication. Require a title. Author falls back to feed title
  then feed ID. Full-text fallback keeps the summary if extraction is absent/shorter.
  Feed HTTP bodies are bounded at 16 MiB before parsing, including chunked responses;
  overflow refuses that feed rather than publishing a parseable truncated prefix.

## HTML/storage normalization (`markdown.rs`)

Use parser/token-state behavior, not regex replacement of arbitrary markup.

- Remove machine-only storage parameters, task IDs/UUIDs/ADF attributes, script and
  style; translate task status into a checkbox. For attribute-borne text prefer
  existing text/time; never expose opaque `ri:user` IDs as readable identity.
- Unwrap only closed CDATA with correct escaping. Unterminated CDATA stays literal;
  plain-text-body becomes pre/code. Self-closing non-void storage tags require
  explicit close tags; keep HTML void elements alone. Respect comments, raw-text
  elements, and HTML ASCII end-tag boundaries while rewriting known storage tags.
- Select a usable ADF representation by node identity, with named fallback when
  empty, not positional duplication. Preserve spacing between link text and target.
- Do not claim fully lossless HTML normalization: the CDATA prepass has a documented
  raw-text interaction. Change that seam deliberately with a reproducing fixture,
  not an unrelated broad sanitizer rewrite.

## Public nodex boundary (`nodex.rs`)

- Read the declared public graph, NEVER substitute filesystem discovery. Require
  nodex.toml and a current compatible public index. Current subprocess compatibility
  is `>=0.60, <0.61`, checked publicly; an explicit executable applies to every call,
  without fallback or changing the repository's own version pin.
- Query created and updated separately, then fold selected documents by identity.
  Grow query reach past the window's first day; one below-bound observation proves
  completion. A reach/window cap fails the source instead of returning a partial
  inventory as complete. Invalid public paths likewise fail, not skip.
- Require complete current inventory before kind/explicit-ID filtering. Missing,
  unbuildable, or unavailable coverage is not permission to rebuild or approximate.
  Every requested document ID must resolve; selection happens before document caps.
- Repository ID is portable identity; document identity is distinct from location and
  lifecycle. `source_file` remains the legacy absolute ownership reference. Optional
  base URLs are configured, never inferred from a remote. Undated observed records
  do not acquire invented authored timestamps; pipeline upper bounds still apply.
- Consume one verified public response for node, canonical body, and relations.
  Preserve all authored `references` and their typed body/frontmatter locations.
  Preserve nonempty nested `attributes` as `source_record.attributes`; omit empty
  attributes to avoid input noise. They are neither identity nor lifecycle state.
  `observation_revision` is a semantic observation digest, not raw-byte hash or a
  replacement for the provider's content revision.
- Reobserve selected documents and compare the full projection before returning;
  inventory/config checks are mandatory too. A snapshot fingerprint is not a
  per-document cache key. Reproject only actual contained canonical links with
  case preserved; reject escaping paths rather than guessing a destination.

## Manual original bundles (`manual.rs`, `manual_bundle.rs`)

- Capture is a sidechannel, not blob fields on `Source`/`RawItem`. Keep `source_file`
  ownership distinct from typed `manual_original` and preserved `manual_frontmatter`.
  Capture entry bytes once for conversion/hash/preservation. Archive=false still
  preserves originals; it only disables retirement.
- The configured inbox is the explicit boundary, not an implicit whole-vault copy.
  Reject a vault root/ancestor inbox and overlap with the actual configured managed
  roots or `.lorekeeper`; compare canonical prefixes, including uncreated paths.
  Select entry extensions case-insensitively; refuse symlink/nonregular/missing members, escapes, absolute paths, Windows drive
  and UNC syntax before URI classification. Do not fetch remote dependencies.
- Markdown uses pulldown-cmark Link/Image/WikiLink events and a serializer. Support
  explicit relative wiki filenames, not Obsidian basename/implicit-extension lookup
  or embed options. Reject HTML base and raw HTML local links in Markdown rather
  than rewrite against a wrong base. HTML URL surfaces not fully supported, external
  CSS/SVG dependency closure, and similar unsupported constructs yield partial
  coverage/no retirement. Preserving their bytes is not proving full closure.
- `ingest.originals` defaults to 32 MiB entry, 64 MiB member and 256 MiB per source;
  configured entry/member budgets fit the source budget, at most512 MiB. Keep the
  2 MiB manifest, depth32/2048 bundle members, 2048 entries per source,
  8192 aggregate member records/256 MiB derived documents and CLI aggregate capture
  cap512 MiB. Closure overflow is an explicit error,
  not silently solved by splitting top-level entries. Binary attachments stay bytes;
  text entry conversion requires supported decoding.
- `CapturedFile` owns private temporary acquisition files, hashes while streaming,
  and removes them on drop/error. Inspection and dry runs may use these files but
  never retained stores or vault writes. Preserve owned captures through publication
  and retirement; never substitute a mutable source path for captured bytes.
- Default standalone selection follows configured decoder capabilities. Keep other
  known binary inputs explicitly unprocessed without failing a note's byte-preserving
  attachment capture. An explicit extension selection requires its decoder. Do not
  infer dependency ownership from filename folding, equal hashes or shared inodes.
- Retirement requires complete preserved bytes and unchanged entry. Never remove
  attachments. Cross-entry references protect referenced entries even when an event
  is filtered out; any entry capture failure holds retirement for that source.
  Retire only direct inbox entries into a strict descendant of that inbox.
  These checks are not a global inbox snapshot or a lease against external editors.
  Publication order and immutable object/bundle storage belong to vault/CLI owners.

## Document projection (`content.rs`, `content/`)

- Format decoding is separate from source acquisition and semantic work. The optional
  pinned Python runtime reads captured private file paths; Rust bounds time/output,
  validates its typed response and owns publication. No runtime installs or OCR model
  downloads during ingestion. Decoder stderr may contain source text; never forward it.
- File-backed acquisition avoids full raw-byte copies, not native decoder allocations.
  Projected output remains bounded and materialized; physical pages/sheets/frames are
  format units, never model split parts or canonical document identities.
- Rust owns foreground decoder children in the caller's process group, including
  separate Paddle layout and MLX recognition workers. Rust brokers their bounded
  protocol; Python workers must not spawn helpers, detach or run an HTTP service.
  Keep timeout, cancellation and direct-child reaping explicit.
- XLSX preserves sheet state, cell coordinates, formulas and stored cached values without
  calculation. Typed sheet structure retains merged rectangles, default row visibility
  and declared row/column visibility intervals without expanding sparse geometry or
  excluding hidden values. The locked openpyxl sparse parser is part of this boundary;
  decoder upgrades must replay the optional runtime tests. PDF physical page numbers differ
  from DOCX flow content. Legacy Tesseract OCR is literal text with dictionary-based
  line deletion and table reconstruction disabled. Visual recognition retains typed
  block geometry, order, table markup and formula content as an interpretation,
  separate from native Markdown. Empty extraction is unprocessed, never knowledge.
  PDF vision scope is explicit native/all/selected physical pages, not a text-length
  or keyword quality heuristic. Render pending visual pages at 150 DPI with a
  per-page RGBA cap and aggregate text/output bounds; legacy PDF OCR also retains
  its aggregate RGBA cap. Never reuse native page text as recognition evidence.
  Kreuzberg v4's [best-effort support](https://docs.kreuzberg.dev/lts/) ends in 2026.
  A decoder replacement must retain supported platform wheels and pass native/OCR
  page/frame, resource-bound and private-diagnostic contracts. Replay the real-runtime
  invalid-model cases: a successful envelope, an OCR method label or complete page
  inventory does not prove OCR succeeded. Never publish native text as OCR evidence
  after recognition failed, or infer success by parsing diagnostic prose.
- Decode image frames through Pillow, preserving ordinals, EXIF orientation and
  alpha compositing on white. Visual block coordinates follow EXIF and the recorded
  counterclockwise document correction. Verify physical inventory and exact selected
  unit coverage; missing later frames/pages, nonterminal token generation or an
  unsuccessful worker exit cannot complete a document.
  Image inventory uses encoded canvas sizes without decoding PNG/APNG pixels;
  EXIF can transpose those sizes during selected-frame rendering. Library metadata
  readers may still allocate buffers, notably WebP's canvas, so the pending raster
  limit is not a whole-process memory cap.
- `vision.rs` owns the engine execution boundary; `vision_common.py`, `paddle.py`
  and `mlx.py` isolate the pinned Paddle adapter. A replacement must preserve typed
  units, provenance, coverage and resource/failure contracts without changing graph
  semantics. Package locks, model revisions, artifact digests and recipe determine
  cache identity; paths, selection and resource ceilings do not imply new meaning.
  Inference is offline; model downloads belong only to explicit installation.
- `cache.rs` checkpoints verified units and native projections before canonical
  publication under separate per-identity claims. Partial units are reusable work,
  never a completed source. Read-only inspection/dry run neither create cache files
  nor refresh their lifetime. Successful normal reuse renews operational mtime;
  maintenance previews and prunes expired payloads under the same claims without
  removing originals. A stable namespace lock serializes claim opening and empty
  namespace/lock removal; never unlink a claim outside that guard. Current
  output/decoded limits apply before renewing cache hits.
- Decoder-generated links are original references, not canonical wiki edges. Render
  them as literal destinations. Preserve raw input before optional inbox retirement.
- Update the embedded requirements/hash lock and package protocol together. The ignored
  real-runtime test is opt-in because Markdown-only CI needs no Python package installation;
  run it explicitly when changing the decoder or its dependencies.
