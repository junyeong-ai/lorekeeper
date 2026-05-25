---
paths: ["crates/lk-source/**/*.rs"]
---

- Every Slack API call goes through `slack_post` as `x-www-form-urlencoded` (not JSON — read methods reject it).
- `resolve_channel_id` passes bare ids (`C…/G…/D…`) through; only `#name` triggers a lookup.
- Drive `folder`/`file_pattern` must go through `escape_drive_literal` before interpolation.
- Calendar `calendar_id` is percent-encoded via `Url::path_segments_mut`; all-day events parse `start.date` as a civil date (not `Timestamp::parse`).
- Gmail uses epoch-second `after:`/`before:` from `day_window`; the `include_queries` OR group is parenthesized.
- Jira uses `GET /rest/api/3/search/jql` (the old `/search` returns 410). Search by `updated` only — `duedate`/`customfield` are display fields, never search filters.
- RSS polls many feeds via `feed-rs`; a failed feed is `tracing::warn!`-skipped (never aborts the source). Entries with no `published`/`updated` date, or no title, are skipped — never dated to `now` (that misfiles old posts onto today).
- Individual item failures (thread fetch, file download, message fetch) must be caught with `tracing::warn!` + skip — never abort the entire source.
- All API list endpoints must be cursor-paginated with per-adapter caps and empty-page guards.
- Manual source rejects symlinks (`symlink_metadata`) and uses full filename in `external_id`.
- Source params use `#[serde(deny_unknown_fields)]` — keep new params strict.
