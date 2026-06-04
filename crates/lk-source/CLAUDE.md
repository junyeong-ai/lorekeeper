# lk-source

Source adapters. Each implements `Source::extract(params, ctx) -> Result<Vec<RawItem>, SourceError>`;
`build_source(source_type, ..)` is the factory. No dedup/render here — just fetch +
map to `RawItem`.

- **`validate_params(source_type, params)`** deserializes into each adapter's typed
  params struct, which carry `#[serde(deny_unknown_fields)]` (including nested structs).
  `lore validate` calls this for every enabled source, so config typos fail before any
  network call. Keep new params strict. `classify` is NOT a param — it's a top-level
  `SourceConfig` field.
- **`ExtractContext::day_window(lookback, lookahead)`** anchors a query window to the
  target day's bounds in the configured timezone — never `now`. Time-windowed adapters
  must build their windows from it so `lore ingest --date <past>` backfills the right day
  (the pipeline still date-filters afterward).
- **Ownership is the adapter's job.** Each adapter sets `RawItem::is_self` by comparing
  its structured authorship field to `ExtractContext::identity` with an EXACT match —
  Gmail `From` address vs `identity.email`, Slack message author id vs `identity.slack_id`,
  Jira assignee account vs the authenticated account, Calendar organizer/attendee email
  vs `identity.email`. Sources with no authorship notion (Drive/RSS/Manual) set `false`.
  The pipeline never re-derives ownership from free-form text, so a recipient/CC/mention
  is never mistaken for the user's own work.
- Adapter gotchas (don't regress these):
  - **Drive**: `folder`/`file_pattern` go through `escape_drive_literal` (`\` and `'`)
    before interpolation into the query string.
  - **Calendar**: `calendar_id` is percent-encoded via `Url::path_segments_mut`; all-day
    events parse `start.date` as a civil date in the vault tz. Events with unparseable
    timestamps are skipped. `description` is HTML → `markdown::html_to_markdown`.
  - **Gmail**: uses epoch-second `after:`/`before:` from `day_window` (timezone-exact);
    the `include_queries` OR group is parenthesized so the bounds bind to every term.
  - **Jira**: current `GET /rest/api/3/search/jql` (the old `/search` was removed, returns
    410). `description` is ADF JSON → `markdown::adf_to_markdown`. The user supplies a
    `jql` query string directly in config; convention is to search by `updated` for daily
    work snapshots. `duedate` + `customfield_10015` (start date) render as a status/period
    header. The authenticated `accountId` (the exact ownership key) is fetched from
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
    does not abort the entire source. Gmail and Slack history use cursor pagination with
    configurable caps (`slack-channel` exposes `max_messages_per_channel`/`max_thread_messages`,
    defaults 500/200; `gmail` exposes `max_messages`, default 200 — all validated `> 0`).
    slack-channel enforces its cap in the shared `paginate` helper and Gmail in its own
    fetch loop; both `tracing::warn!` when they actually drop messages, so a very busy
    source is observable and the operator can raise the cap; other adapters issue bounded
    single requests.
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
  `obtain_google_refresh_token` for the `lore init credentials` wizard. URL-building and
  callback parsing are unit-tested; the live token exchange needs a real Google client.
