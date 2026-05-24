# wi-source

Source adapters. Each implements `Source::extract(params, ctx) -> Vec<RawItem>`;
`create_source(source_type, ..)` is the factory. No dedup/render here — just fetch +
map to `RawItem`.

- **`validate_params(source_type, params)`** deserializes into each adapter's typed
  params struct, which carry `#[serde(deny_unknown_fields)]` (including nested structs).
  `wi validate` calls this for every enabled source, so config typos fail before any
  network call. Keep new params strict. `classify` is NOT a param — it's a top-level
  `SourceConfig` field.
- **`ExtractContext::day_window(lookback, lookahead)`** anchors a query window to the
  target day's bounds in the configured timezone — never `now`. Time-windowed adapters
  must build their windows from it so `wi ingest --date <past>` backfills the right day
  (the pipeline still date-filters afterward).
- Adapter gotchas (don't regress these):
  - **Drive**: `folder`/`file_pattern` go through `escape_drive_literal` (`\` and `'`)
    before interpolation into the query string.
  - **Calendar**: `calendar_id` is percent-encoded via `Url::path_segments_mut`; all-day
    events parse `start.date` as a civil date in the vault tz (not `Timestamp::parse`,
    which would fall back to `now`). `description` is HTML → `markdown::html_to_markdown`.
  - **Gmail**: uses epoch-second `after:`/`before:` from `day_window` (timezone-exact);
    the `include_queries` OR group is parenthesized so the bounds bind to every term.
  - **Jira**: current `GET /rest/api/3/search/jql` (the old `/search` was removed, returns
    410). `description` is ADF JSON → `markdown::adf_to_markdown`. Search by `updated` only
    (a daily *work snapshot*); `duedate` + `customfield_10015` (start date) render as a
    status/period header but are never search filters — they change and would corrupt past
    snapshots.
  - **Slack**: every call goes through `slack_post` as `x-www-form-urlencoded` (JSON is
    rejected by read methods like `search.messages`). `resolve_channel_id` passes bare ids
    (`C…/G…/D…`) through and only name-resolves `#name`. `slack-channel` reads whole channels
    (= team activity); `watch_users` (author or `<@id>` mention) narrows to focus people,
    `include_threads` pulls `conversations.replies` for context, `exclude_bots` (default
    true) drops bot posts from root + replies. Text → `markdown::slack_to_markdown`.
    `search.messages` (slack-search) is user-token-only per Slack. Tokens: `history_token`
    prefers bot (xoxb), `search_token` requires user (xoxp).
- `Source` has no `source_type()` accessor — the factory selects by the input enum.
- `markdown` module normalizes rich text to Markdown, loss-aversely (unmapped constructs
  degrade to their text): `adf_to_markdown`, `html_to_markdown` (via `htmd`),
  `slack_to_markdown`. Keeps LLM/vault input clean instead of ADF/HTML/token soup.
- `google/oauth.rs` mints a Google refresh token via an OAuth loopback flow (consent
  URL + ephemeral `127.0.0.1` callback server + code exchange), re-exported as
  `obtain_google_refresh_token` for the `wi init credentials` wizard. URL-building and
  callback parsing are unit-tested; the live token exchange needs a real Google client.
