# Looking up Google (Calendar / Gmail) values (`gws`)

> `gws` prints output to stdout (JSON) and diagnostics to stderr — use `2>/dev/null` when parsing.

## Calendar — calendar id
```bash
gws calendar calendarList list 2>/dev/null
```
Usually `primary` (your own default calendar). For a shared/team calendar, use its id.
```yaml
my-schedule:
  type: google-calendar
  enabled: true
  params: {calendar_id: primary, lookback_hours: 24, lookahead_hours: 24}
  labels: [personal]
  extract_concepts: false
  # add this source id to personal.tracked_sources to feed your work-log
```
Event descriptions are converted HTML→Markdown automatically.

## Gmail — which mail to collect
Inspect the category distribution of unread mail to gauge the noise:
```bash
gws gmail users messages list --params '{"userId":"me","q":"is:unread category:primary","maxResults":1}' | jq -r '.resultSizeEstimate'
gws gmail users messages list --params '{"userId":"me","q":"is:unread category:promotions","maxResults":1}' | jq -r '.resultSizeEstimate'
gws gmail users messages list --params '{"userId":"me","q":"is:unread category:updates","maxResults":1}' | jq -r '.resultSizeEstimate'
gws gmail users messages list --params '{"userId":"me","q":"is:unread category:social","maxResults":1}' | jq -r '.resultSizeEstimate'
gws gmail users messages list --params '{"userId":"me","q":"is:unread category:forums","maxResults":1}' | jq -r '.resultSizeEstimate'
```
Usually only `category:primary` is work mail; the rest is bots/marketing. Recommended:
```yaml
email-digest:
  type: gmail
  enabled: true
  params:
    lookback_hours: 24
    include_queries: ["category:primary"]   # exclude bots/notifications (GitHub etc.), work mail only
  classify:                                   # optional: ordered keyword→category rules (first match wins)
    # keyword values may be in any language — they match the source body verbatim
    - category: action_required
      keywords: ["검토 요청", "확인 부탁", "please review"]
    - category: decisions
      keywords: ["승인", "결재 완료", "approved"]
  labels: [personal]
  extract_concepts: true
  # add this source id to personal.tracked_sources to feed your work-log
```

## When there is no refresh token
Use `lore init credentials` for browser OAuth issuance (Desktop-app client, read-only scopes).
