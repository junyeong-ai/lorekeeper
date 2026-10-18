# Looking up Google (Calendar / Gmail) values (`gws`)

> Parse JSON from stdout and preserve stderr diagnostics. A failed discovery command
> is an error, not an empty account or an absent calendar.

## Calendar — calendar id
```bash
gws calendar calendarList list
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

Express Gmail query conditions in `include_queries`, including negation such as
`"label:work -category:promotions"`. The optional `exclude` block filters subject,
sender and calendar-invite metadata; its fields are shown in `config.example.yaml`.

When mailbox discovery is needed, inspect the category distribution of unread mail to gauge the noise:
```bash
gws gmail users messages list --params '{"userId":"me","q":"is:unread category:primary","maxResults":1}' | jq -r '.resultSizeEstimate'
gws gmail users messages list --params '{"userId":"me","q":"is:unread category:promotions","maxResults":1}' | jq -r '.resultSizeEstimate'
gws gmail users messages list --params '{"userId":"me","q":"is:unread category:updates","maxResults":1}' | jq -r '.resultSizeEstimate'
gws gmail users messages list --params '{"userId":"me","q":"is:unread category:social","maxResults":1}' | jq -r '.resultSizeEstimate'
gws gmail users messages list --params '{"userId":"me","q":"is:unread category:forums","maxResults":1}' | jq -r '.resultSizeEstimate'
```
Categories are mailbox classifications, not reliable labels for work relevance.
Choose the query scope from the user's needs; the following example collects primary mail
and excludes other categories, including potentially useful updates:
```yaml
email-digest:
  type: gmail
  enabled: true
  params:
    lookback_hours: 24
    include_queries: ["category:primary"]   # primary mail only; include other categories when relevant
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
