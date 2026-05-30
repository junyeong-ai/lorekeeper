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
  schedule: "0 8 * * 1-5"
  params: {calendar_id: primary, lookback_hours: 24, lookahead_hours: 24}
  labels: [personal]
  extract_concepts: false
  track_personal: true
```
Event descriptions are converted HTML→Markdown automatically.

## Gmail — which mail to collect
Inspect the category distribution of unread mail to gauge the noise:
```bash
for c in primary promotions updates social forums; do
  n=$(gws gmail users messages list --params "{\"userId\":\"me\",\"q\":\"is:unread category:$c\",\"maxResults\":1}" 2>/dev/null \
       | python3 -c "import sys,json;print(json.load(sys.stdin).get('resultSizeEstimate','?'))" 2>/dev/null)
  echo "$c: $n"
done
```
Usually only `category:primary` is work mail; the rest is bots/marketing. Recommended:
```yaml
email-digest:
  type: gmail
  enabled: true
  schedule: "30 8 * * 1-5"
  params:
    lookback_hours: 24
    include_queries: ["category:primary"]   # exclude bots/notifications (GitHub etc.), work mail only
  classify:                                   # optional: keyword→category section
    # keyword values may be in any language — they match the source body verbatim
    action_required: ["검토 요청", "확인 부탁", "please review"]
    decisions: ["승인", "결재 완료", "approved"]
  labels: [personal]
  extract_concepts: true
  track_personal: true
```

## When there is no refresh token
Use `lore init credentials` for browser OAuth issuance (Desktop-app client, read-only scopes).
