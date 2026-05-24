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
    which would fall back to `now`).
  - **Gmail**: uses epoch-second `after:`/`before:` from `day_window` (timezone-exact);
    the `include_queries` OR group is parenthesized so the bounds bind to every term.
  - **Slack** tokens: `SlackCredentials` holds optional `bot_token` (xoxb) and
    `user_token` (xoxp). `conversations.history` (slack-channel) accepts either
    (`history_token`, bot preferred); `search.messages` (slack-search) is user-token-only
    per Slack — `search_token` returns only `user_token` and `create_source` errors if
    it's absent. Channel uses `oldest`/`latest` + `inclusive`; search rounds the hour
    lookback up to whole days for its date-granular operators.
- `Source` has no `source_type()` accessor — the factory selects by the input enum.
- `google/oauth.rs` mints a Google refresh token via an OAuth loopback flow (consent
  URL + ephemeral `127.0.0.1` callback server + code exchange), re-exported as
  `obtain_google_refresh_token` for the `wi init credentials` wizard. URL-building and
  callback parsing are unit-tested; the live token exchange needs a real Google client.
