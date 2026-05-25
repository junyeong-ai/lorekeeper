---
paths: ["crates/lk-source/**/*.rs"]
---

- Every Slack API call goes through `slack_post` as `x-www-form-urlencoded` (not JSON — read methods reject it).
- `resolve_channel_id` passes bare ids (`C…/G…/D…`) through; only `#name` triggers a lookup.
- Drive `folder`/`file_pattern` must go through `escape_drive_literal` before interpolation.
- Calendar `calendar_id` is percent-encoded via `Url::path_segments_mut`; all-day events parse `start.date` as a civil date in the vault timezone.
- Gmail uses epoch-second `after:`/`before:` from `day_window`; the `include_queries` OR group is parenthesized.
- Jira uses `GET /rest/api/3/search/jql` (the old `/search` returns 410). The user supplies a `jql` query directly; `duedate`/`customfield` render as status headers.
- RSS polls many feeds via `feed-rs`; a failed feed is `tracing::warn!`-skipped (never aborts the source). Entries with no `published`/`updated` date, or no title, are skipped — never dated to `now` (that misfiles old posts onto today).
- Individual item failures (thread fetch, file download, timestamp parse) must be caught with `tracing::warn!` + skip — never abort the entire source. Events with unparseable timestamps are skipped (never dated to `now`).
- Gmail and Slack history use cursor pagination with per-adapter caps; other adapters issue bounded single requests.
- Manual source rejects symlinks (`symlink_metadata`) and uses full filename in `external_id`.
- Source params use `#[serde(deny_unknown_fields)]` — keep new params strict.
