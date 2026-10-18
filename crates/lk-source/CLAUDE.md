# lk-source

Source adapters return observations; the pipeline owns deduplication/rendering.
Follow the root CLAUDE.md for shared IO/config/error contracts. `Source::extract`
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
- `confluence.rs`: validate user CQL without rewriting its meaning; append the date
  window. Re-anchor next links to the authenticated API base and follow `page_step`.
  External identity includes version; `only_my_edits` compares the current author.
- `gmail.rs`: epoch bounds and parenthesized OR terms; queries are required/nonblank.
  An unread-only filter is an explicit user choice, not an implicit default.
- `slack.rs`: form-encode API calls. Cursor pagination is authoritative, not
  `has_more`; distinguish bare channel IDs from `#` lookup. Watch-user author/mention
  semantics, thread/bot filters, and user-token search remain explicit. Split only
  headings, not code, mentions, or URLs.
- `calendar.rs`: construct URL path segments safely. All-day civil dates use the
  vault timezone; skip invalid timestamps diagnostically. `drive.rs` query literals
  escape both backslash and apostrophe.
- `rss.rs`: prefer HTML content then summary; use published/updated dates, never
  acquisition-now as publication. Require a title. Author falls back to feed title
  then feed ID. Full-text fallback keeps the summary if extraction is absent/shorter.

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
  is `>=0.58, <0.59`, checked publicly; an explicit executable applies to every call,
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
  Read entry bytes once for conversion/hash/preservation. Archive=false still
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
- Limits are intentional: 32 MiB entry, 64 MiB member, 2 MiB manifest, depth32/2048 bundle members;
  256 MiB/2048 entries per source, 8192 aggregate member records/256 MiB derived
  documents; CLI aggregate capture cap512 MiB. Closure overflow is an explicit error,
  not silently solved by splitting top-level entries. Binary attachments stay bytes;
  text entry conversion requires supported decoding.
- Retirement requires complete preserved bytes and unchanged entry. Never remove
  attachments. Cross-entry references protect referenced entries even when an event
  is filtered out; any entry capture failure holds retirement for that source.
  Retire only direct inbox entries into a strict descendant of that inbox.
  These checks are not a global inbox snapshot or a lease against external editors.
  Publication order and immutable object/bundle storage belong to vault/CLI owners.
