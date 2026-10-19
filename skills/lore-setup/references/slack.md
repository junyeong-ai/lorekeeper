# Looking up Slack values (`slack-cli`)

## Find a channel ID
```bash
slack-cli channels "<name fragment>" --expand name,id,members -j
```
Put the `id` (e.g. `C0123456789`) in `channels`. A name (`#name`) also works, but the id
is stable (unaffected by renames). Repeat the partial search for multiple channels.

## My user id (for the personal split)
```bash
slack-cli auth status            # confirm workspace
slack-cli users "<your name>" -j   # id e.g. U0123456789 → identity.slack_id
```

## Gauge a channel's character (optional)
```bash
slack-cli messages <CHANNEL_ID> --limit 30 --exclude-bots --expand reply_users_count,user_name -j
```
A high thread ratio means `include_threads: true` matters (the real discussion is in replies).

## config block
```yaml
team-slack:
  type: slack-channel
  enabled: true
  params:
    channels: ["C0123456789", "C0123456780"]   # team channels (whole channel = team activity)
    lookback_hours: 24
    include_threads: true     # include thread-reply context
    exclude_bots: true        # drop bots/integrations (default true)
    # watch_users: ["U0123456789"]   # only threads a user authored/was mentioned in; empty = whole channel
  labels: [team-ops, personal]
  extract_concepts: true
  # To split your authored messages (identity.slack_id) into the work-log, add this
  # source id to personal.tracked_sources (the optional personal: module).
```
For keyword trends, add a separate `slack-search` source (below):
```yaml
  type: slack-search   # needs a user token (xoxp) — search.messages rejects bot tokens
  params:
    queries:
      - {channel: "#ai-general", keywords: [AI, LLM, RAG]}
    lookback_hours: 24
```
