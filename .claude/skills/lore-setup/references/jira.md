# Looking up Jira values (`atlassian-cli`)

> Note: `atlassian-cli` uses its own OAuth account. Lorekeeper uses the Jira account
> (email+token) in `credentials.json`, so if the two point at different instances the
> results can differ. Always confirm with `lore ingest --dry-run`, which validates
> against Lorekeeper's account.

## My issues / project key
```bash
atlassian-cli jira search "assignee = currentUser() ORDER BY updated DESC" \
  --limit 5 --fields summary,project,duedate
```
Use the `project.key` (e.g. `OYAI`) in the JQL.

## Find the start-date custom-field id
Jira's "Start date" is a per-instance custom field (commonly `customfield_10015`). Fetch
one issue and inspect its date fields:
```bash
atlassian-cli jira get <ISSUE-KEY> | grep -i -A2 "start date"
```
(`jira get` returns all fields by default — there is no `--fields` flag on `get`.)
Put the id you find in `start_date_field`. Omit it if there is none (the start date just
won't be shown).

## config block
```yaml
my-tasks:
  type: jira
  enabled: true
  schedule: "0 9 * * 1-5"
  params:
    # Only issues changed that day (= worked on) = a work-history snapshot. Do NOT search by due/start date.
    jql: >
      project = OYAI AND assignee = currentUser()
      AND updated >= -1d
      ORDER BY updated DESC
    max_results: 50
    start_date_field: customfield_10015   # optional: show the start date
  labels: [personal]
  extract_concepts: false
  track_personal: true
```
The description is rendered ADF→Markdown, and status/period render as an as-of-that-day
snapshot header.
