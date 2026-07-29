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
  `SourceError::NothingObserved` instead of an empty success. The window was never observed,
  and downstream cannot tell that apart from a quiet day — `lore ingest` logs `Skipped`
  either way, and `lore health` reads that log as its only evidence the source is alive. An
  empty listing (nothing attempted) is genuinely empty and stays a success. A new
  many-item adapter inherits the guarantee by counting its failures and calling the rule.
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
