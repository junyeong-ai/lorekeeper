# lk-source

Source adapters. Each implements `Source::extract(params, ctx) -> Result<Vec<RawItem>, SourceError>`;
`build_source(source_type, ..)` is the factory. No dedup/render here — just fetch +
map to `RawItem`.

- **Params are validated BY CONSTRUCTION, at consumption.** `parse_validated::<P>()` does
  the two-step `parse_params` (deserialize into the `#[serde(deny_unknown_fields)]` struct)
  then `P::validate()` (the `ValidatedParams` trait — semantic checks: caps `> 0`, required
  non-empty fields, value formats; pure, no I/O). BOTH the offline `validate_params(source_type,
  params)` dispatch (`lore validate`) AND every adapter's `extract` route through
  `parse_validated`, so an invariant can never be enforced in the config check yet skipped at
  runtime — a direct `extract` call can't reach the body with unvalidated params (e.g. gmail
  could otherwise assemble `() after:…`; manual could scan/archive the vault root). A new
  adapter inherits the guarantee the moment it `impl ValidatedParams for XParams` and parses
  via `parse_validated` — never call `parse_params` directly from an adapter. `classify` is
  NOT a param — it's a top-level `SourceConfig` field.
- **`ExtractContext::day_window(lookback, lookahead)`** anchors a query window to the
  target day's bounds in the configured timezone — never `now`. Time-windowed adapters
  must build their windows from it so `lore ingest --date <past>` backfills the right day
  (the pipeline still date-filters afterward).
- **Per-item isolation stops where NOTHING was reached.** RSS skips an unreachable feed and
  Drive skips an undownloadable file so one broken item doesn't cost the others their day.
  Both then go through `require_any_observation`: if EVERY attempt failed the adapter returns
  `SourceError::NothingObserved` instead of an empty success. **The unit is the thing fetched, the guard takes what was OBSERVED (never what failed, and
  never a subtraction — see `require_any_observation` for the `usize` underflow that would
  silently disarm it in release), and "read" covers usable as well as reachable** — a Drive file whose metadata will not
  parse yielded nothing just as surely as one that would not download, and an RSS feed that
  answered with entries none of which can become an observation was not read either. RSS
  therefore separates MAPPING from WINDOWING (`map_entry` decides only whether an entry can
  become an item; the caller decides whether it belongs to this day), because an entry dated
  for another day is a perfectly good observation — conflating the two would make every quiet
  feed look broken, the false positive this must never produce. The window was never observed,
  and downstream cannot tell that apart from a quiet day — `lore ingest` logs `Skipped`
  either way, and `lore health` reads that log as its only evidence the source is alive. An
  empty listing (nothing attempted) is genuinely empty and stays a success. **A PARTIAL
  outage stays invisible to the ingest log by design** — four of five feeds dead still
  reports a collection, because the source WAS observed and the log records only that one
  bit. Surfacing degradation rather than absence needs a per-source expectation and a
  threshold, which is a finding-style check (`doctor`/`lint`), not a freshness signal; the
  individual failures are `tracing::warn`ed meanwhile. A new
  many-item adapter inherits the guarantee by counting its failures and calling the rule.
- **Confluence storage format is XHTML, and three of its constructs break the HTML rule.**
  `markdown::html_to_markdown` is loss-averse — an unmapped construct degrades to its text —
  which is right until the text is not prose — and what the text IS, not where it came from,
  is the test: a `<style>` or `<script>` block out of an email or a feed is machine state on the
  same grounds as any `ac:` construct, and one real vault page carries an RSS feed's CSS rules
  mid-article because it was not. (1) `ac:parameter` and a task's
  `ac:task-id`/`-uuid`/`-status` carry MACHINE STATE, and degraded they weld onto the
  surrounding words unseparated (`170e6f1a-9cincompleteShip the thing`); they are dropped,
  at the documented cost that a body-less macro loses its visible label. (2) Some constructs
  carry what the reader saw in an ATTRIBUTE, so degrading found nothing to degrade and the
  thing vanished silently — every Confluence→Confluence cross-reference, and the same for an
  external reference, an inline date and an emoji. `ATTRIBUTE_BORNE_TEXT` pairs each element
  with the attribute that carries its value, and the handler prefers the element's own text
  where it has any (only `time` does), so one rule covers all of them; `ri:user` is
  deliberately absent, since an opaque account id is machine state. (3) CDATA is not HTML: an
  HTML5 parser reads `<![CDATA[…]]>` as a bogus COMMENT and drops it, so every code macro
  arrived EMPTY — one real page grew 4.6x once `normalize_storage_format` unwrapped it ahead
  of the converter. Only a section that CLOSES is one: every HTML source shares this converter,
  so an unterminated `<![CDATA[` is likelier prose about XML than a truncated Confluence body,
  and reading the document's remainder as its content escaped the rest of a newsletter into
  literal markup. Having judged it TEXT it is escaped as text, the same as an unwrapped
  section: left as live bytes it was still markup to the parser — `<!…` opens a bogus comment
  running to the next `>`, which inside the `<pre><code>` a code macro becomes is the one in
  `</code>`, so the body disappeared and the element never closed. Recovering that text is only half the job: an unknown `ac:plain-text-body`
  degrades to a PARAGRAPH, which the converter then markdown-escapes (`[1, 2]` → `\[1, 2\]`,
  backslashes injected into JSON), so it is mapped to `<pre><code>` — the one form that both
  fences and stops escaping. All of this was invisible until a real page was drained.
- **An XHTML empty element must be made empty before anything reads it.** HTML has no
  self-closing syntax for a non-void element, so a parser reads `<ac:parameter ac:name="icon"/>`
  as an OPEN tag and hands it every following sibling as a CHILD. Both rules above answer from
  the element ALONE — one drops it, the other replaces it with an attribute — so those adopted
  siblings are discarded with it: an unset parameter (which Confluence writes exactly this way,
  BEFORE the body) deletes the macro's entire content, and a self-closed `<ac:task-id/>` deletes
  the task list. `normalize_storage_format` therefore gives every non-void self-closed tag an
  explicit end tag, which is what makes ignoring a handled element's children correct BY
  CONSTRUCTION rather than by luck — and it holds for constructs nobody has enumerated yet,
  where a per-handler patch would not. It follows the tokenizer's tag states rather than
  matching `/>`, so a slash inside a quoted attribute value stays value text; void elements are
  left alone (`<br></br>` is TWO breaks), and so is anything inside a comment or a RAW-TEXT
  element (`script`/`style`/`textarea`/`title`/…), whose content is text — rewriting there
  injected a `</div>` into a JavaScript string, the pre-parse pass corrupting the document it
  exists to leave alone. `noscript` is one of them — scripting is a parser's default — and it is
  ordinary in email and fetched articles. What ENDS such a span is an end tag whose NAME matches,
  which HTML decides on the character after it, **ASCII** whitespace / `/` / `>`: matching the
  name as a mere prefix ended the span at a person typing `</textareas>`, and `char::is_whitespace`
  ends it at `</textarea\u{3000}>` where a parser would not — the same early ending twice over,
  and everything after it gets rewritten. A text box is the one raw-text element whose content is
  KEPT rather than dropped, so nothing else masks the bug there. **One gap here is deliberate**:
  `unwrap_cdata` does not share the skips, so CDATA inside a RAWTEXT element keeps literal
  entities — pinned by a test, argued in `normalize_storage_format`, and left because closing it
  costs either a second scanner that can diverge or a merged walk whose ordering invariant is
  harder to audit than the call order it replaces. The same pass renames the storage-format containers that HAVE an exact
  HTML counterpart (`REWRITTEN_ELEMENTS`), because structure has to reach the converter as
  structure: an `ac:task-list` must arrive as `<ul>` to come out a list, and unmapped its items
  ran together into one string. Both halves are matched BY NAME — testing for a bare open token
  while replacing every close tag left an attributed one unmatched on open and matched on close,
  closing an enclosing block early. `ac:task-status` is NOT machine state: the reader sees a
  ticked box, so it is translated to one, and only its `complete`/`incomplete` spelling is the
  machine's. An `ac:adf-extension`'s children are ALTERNATIVE renderings of one thing (Cloud
  emits both for every bodied extension, so both printed), and exactly one is emitted: the
  `ac:adf-node`, falling back to `ac:adf-fallback` only where the node renders to nothing — asked
  BY NAME, because a rule keyed to position carries the premise that the stand-in comes second,
  and a premise that fails does so silently. The mirror case is a link carrying BOTH a target label
  and the display text its author typed: emitted as siblings they weld (`Design Notesthe
  notes`), so the body contributes a separator and `ac:link` trims it back off, confining the
  space to the gap between them.
- **Ownership (root invariant) — the per-adapter exact-match fields**: each adapter sets
  `RawItem::is_self` by EXACT-matching its structured authorship field to
  `ExtractContext::identity` — Gmail `From` vs `identity.email`, Slack author id vs
  `identity.slack_id`, Jira assignee vs the authenticated account, Calendar
  organizer/attendee vs `identity.email`. No-authorship sources (Drive/RSS/Manual) set `false`.
- Adapter contracts (invariants that define correctness):
  - **Drive**: `folder`/`file_pattern` go through `escape_drive_literal` (`\` and `'`)
    before interpolation into the query string.
  - **Calendar**: `calendar_id` is percent-encoded via `Url::path_segments_mut`; all-day
    events parse `start.date` as a civil date in the vault tz. Events with unparseable
    timestamps are skipped. `description` is HTML → `markdown::html_to_markdown`.
  - **Gmail**: uses epoch-second `after:`/`before:` from `day_window` (timezone-exact);
    the `include_queries` OR group is parenthesized so the bounds bind to every term.
    `include_queries` is REQUIRED and each entry must be non-blank — `validate_params`
    rejects an empty list AND any blank-after-trim entry (which would assemble an empty `()`
    clause). With no stable filter the window would fall to mailbox read-state and the daily
    page would drift between runs, breaking complete-refetch. Using `is:unread` *inside* an
    explicit query is the operator's visible choice — only the silent empty default is forbidden.
  - **Atlassian (`atlassian/`)**: the shared auth + routing facade both Atlassian adapters
    use. `AtlassianAuth` answers three questions — `api_base(product)`, `header()`,
    `deployment()` — so no adapter branches on the credential form. `Deployment` is DERIVED
    from the auth method (oauth/api-token ⇒ Cloud, pat ⇒ DataCenter) and owns every
    Cloud/Server dialect difference in one place: search path, pagination style
    (`JiraPaging::Token` vs `Offset`), and the user-identity key (`accountId` vs
    `name`/`username`). OAuth routes through `api.atlassian.com/ex/{product}/{cloud_id}`;
    everything else talks to `site_url` (Confluence Cloud adds `/wiki`, Data Center already
    has its context path). A PAT authenticates as **Bearer, not Basic** — sending it as a
    Basic password is the classic Data Center misconfiguration. Refresh tokens ROTATE, so
    the refresh is the one request deliberately NOT wrapped in `send_with_retry` (replaying
    a committed rotation strands the grant), the successor is persisted before returning,
    and the ONE `AtlassianAuth` per instance must be shared across adapters.
  - **Jira**: on Cloud, `GET /rest/api/3/search/jql` (the old `/search` was removed, returns
    410) paginated via `nextPageToken`; on Data Center, v2 `/search` paginated by `startAt`
    against the reported `total`. The dialect comes from `Deployment`, never from sniffing. `description` is ADF
    JSON → `markdown::adf_to_markdown`. The user supplies a
    `jql` query string directly in config; convention is to search by `updated` for daily
    work snapshots. `duedate` + a configurable `start_date_field` (the Jira start-date
    custom field, e.g. `customfield_10015`) render as a status/period header. The authenticated `accountId` (the exact ownership key) is fetched from
    `/myself` ONCE and cached on the source (`OnceCell`); a fetch failure is PROPAGATED,
    never degraded to "no owner" — silently marking every assigned issue not-self would
    erase a batch's personal contribution with no signal.
  - **Slack**: every call goes through `slack_post` as `x-www-form-urlencoded` (JSON is
    rejected by read methods like `search.messages`). Cursor pagination is single-sourced
    in `slack::paginate` (collect-all up to a cap; `tracing::warn!`s when it truncates) and
    the `next_cursor` envelope is one `ResponseMetadata`; `resolve_channel_id` keeps its own
    find-by-name loop for early exit. `resolve_channel_id` passes bare ids
    (`C…/G…/D…`) through and only name-resolves `#name`. `slack-channel` reads whole channels
    (= team activity); `watch_users` (author or `<@id>` mention) narrows to focus people,
    `include_threads` pulls `conversations.replies` for context, `exclude_bots` (default
    true) drops bot posts from root + replies. Text → `markdown::slack_to_markdown`, then
    `split_first_line` derives the title (promoting+stripping the first line only when it
    reads like a heading — never a code fence/mention/URL, which would corrupt the body).
    `search.messages` (slack-search) is user-token-only per Slack. Tokens: `history_token`
    prefers bot (xoxb), `search_token` requires user (xoxp).
  - **Confluence** (`confluence.rs`): CQL search over `/rest/api/content/search`, following
    the server-supplied `_links.next` (re-anchored on the API base, which is a different
    host than the one that produced it) under the shared `page_step` termination rule. The
    user's `cql` selects WHICH pages and must carry NO time clause — `validate_params`
    rejects one — because the adapter appends the `day_window` itself; a hand-written
    `lastModified` would anchor the query to the wall clock and break `--date` backfill.
    `external_id` is `confluence:{id}:v{version}`, making an edit a new event and an
    unchanged re-fetch a dedup no-op. `is_self` = the CURRENT version's author (Cloud
    `accountId` / DC `username`) matching the authenticated account, and `only_my_edits`
    (default true) drops pages last touched by someone else. Storage format is XHTML → the
    shared `markdown::html_to_markdown`.
  - **RSS** (`rss.rs`): one source polls many public feeds (`feeds: [{id, url}]`) via
    `feed-rs` (RSS/Atom/JSON Feed) — no credentials. A feed that 404s or fails to parse is
    `tracing::warn!`-skipped, never aborting the source. `body` is `content`→`summary` HTML
    run through `markdown::html_to_markdown`. An entry with no `published`/`updated` date is
    skipped (NOT dated to `now` — that would misfile old posts onto today); likewise a
    title-less entry. Provenance: entry author → feed title → configured feed id.
    `fetch_full_text` feeds fetch the article and run it through
    `markdown::readable_html_to_markdown`, which returns `None` when readability finds no
    article core; on `None` (or a result shorter than the summary) the known-clean feed
    summary is kept, so boilerplate never overwrites it.
  - **Error isolation**: individual item failures (thread fetch, file download, timestamp
    parse) are caught with `tracing::warn!` and skipped — one inaccessible thread or file
    does not abort the entire source.
  - **No silent truncation (complete-refetch contract)**: every windowed/listing adapter
    paginates to the END of its window or listing — a single-page fetch would silently
    lose knowledge the daily page is re-rendered from. Termination is single-sourced in
    `paging::page_step`: a listing ends ONLY when the continuation signal is absent — a
    page may legitimately arrive EMPTY with a token still present (server-side
    filtering), so an empty page never terminates; a hard page budget (`paging::MAX_PAGES`)
    keeps an unattended ingest finite against a server streaming tokens without progress
    (loud warn when it trips). Caps are guards against pathological volume, all
    config-exposed and validated `> 0`: `slack-channel`
    `max_messages_per_channel`/`max_thread_messages`, `slack-search`
    `max_matches_per_query`, `gmail` `max_messages`, `google-calendar`
    `max_events`, `google-drive` `max_files`, `jira` `max_issues`.
    Every adapter `tracing::warn!`s ONLY when the cap may have dropped items (overshoot
    or a pending next page — never a false alarm at an exact-cap fetch; "may have been
    dropped" because a pending page can turn out empty), so truncation is always
    observable and the operator can raise the cap. slack-channel routes through the
    shared `paginate` helper (cursor continuation; a non-empty `next_cursor` is the one
    authoritative signal — `has_more` is advisory and not consulted); slack-search
    paginates `search.messages` by page number (`messages.paging.page` of `.pages`);
    Gmail/Calendar/Drive/Jira each loop with `page_step` directly.
  - **Transient-failure retry**: Slack retries inside `slack_post`; Google (token refresh)
    and Jira (`/myself`, search) wrap idempotent requests in `retry::send_with_retry`
    (bounded retries on 429/5xx + connect/timeout, honoring numeric `Retry-After`), so a
    single provider hiccup doesn't abort an unattended ingest. Per-item Google fetches keep
    their warn-and-skip isolation. Extend new idempotent calls with the same helper.
- **Manual source** (`manual.rs`): watches an inbox directory for user-dropped files
  (`.md`, `.txt`, `.markdown`, `.html`, `.htm` by default — NOT `.json`: there is no
  JSON→Markdown renderer, and storing raw JSON as a document is meaningless).
  `inbox_dir` is resolved by `resolve_inbox_dir` — `~`/`~/…` expands to home, a relative
  path anchors at `ExtractContext::vault_root` (never the process CWD, so cron and
  interactive runs read the same inbox); extract and archive share the one resolution.
  `validate_params` rejects an empty or `.`/`..` `inbox_dir` (it would resolve to the vault
  root and the adapter would scan/archive vault pages). Files are
  read into `RawItem` with `external_id = "manual:{filename}:{blake3(body)[..8]}"` — the
  content fingerprint means a same-name file re-dropped with EDITED content on the same
  day is a distinct event; an unchanged re-drop keeps a stable id. Symlinks are rejected.
  Archive-after-ingest defaults to **true**, but archival is deferred to
  `archive_consumed_files` (run only after every vault write and the queue flush
  succeeded), so a write/flush failure leaves the inbox intact for retry. It archives
  every scanned file (one event per file), so nothing lingers in the inbox to be
  re-scanned next run.
- `Source` has no `source_type()` accessor — the factory selects by the input enum.
- `markdown` module normalizes rich text to Markdown, loss-aversely (unmapped constructs
  degrade to their text): `adf_to_markdown`, `html_to_markdown` (via `htmd`),
  `slack_to_markdown`, and `readable_html_to_markdown` (readability article extraction
  returning `None` + `tracing::warn!` when no article core is found — the caller owns the
  fallback: RSS keeps the feed summary, manual converts the whole user-chosen page). Keeps
  LLM/vault input clean instead of ADF/HTML/token soup.
- `google/oauth.rs` mints a Google refresh token via an OAuth loopback flow (consent
  URL + ephemeral `127.0.0.1` callback server + code exchange), re-exported as
  `build_google_refresh_token` for the `lore init credentials` wizard. URL-building and
  callback parsing are unit-tested; the live token exchange needs a real Google client.
