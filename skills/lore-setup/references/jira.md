# Looking up Jira values (`atlassian-cli`)

> Discovery uses `atlassian-cli`'s configured account. Lorekeeper separately selects a
> named credential instance using the source's `instance` field; it supports OAuth,
> scoped token, API token or PAT authentication. Confirm the intended site/account with
> `lore ingest --dry-run` before relying on discovered IDs.

## My issues / project key
```bash
atlassian-cli jira search "assignee = currentUser() ORDER BY updated DESC" \
  --limit 5 --fields summary,project,duedate
```
Use the `project.key` (e.g. `PROJ`) in the JQL.

## Find the start-date custom-field id
Jira's "Start date" is a per-instance custom field (commonly `customfield_10015`). Fetch
one issue and inspect its date fields:
```bash
atlassian-cli jira get <ISSUE-KEY> | grep -i -A2 "start date"
```
(`jira get` takes `--fields`, and its DEFAULT is an essential subset plus whatever custom
fields are configured — so a custom field nobody configured is absent unless you pass
`--fields '*all'`. Note the response names keys as `customfield_NNNNN`, not by label, so grepping
for a human name may find nothing even then; `--expand names` is what maps them.)
Put the id you find in `start_date_field`. Omit it if there is none (the start date just
won't be shown).

## config block
```yaml
my-tasks:
  type: jira
  enabled: true
  params:
    # Only issues changed that day (= worked on) = a work-history snapshot. Do NOT search by due/start date.
    jql: >
      project = PROJ AND assignee = currentUser()
      AND updated >= -1d
      ORDER BY updated DESC
    # max_issues: 200                     # optional per-run cap; the fetch paginates the
                                          # whole JQL result and warns if it drops issues
    start_date_field: customfield_10015   # optional: show the start date
  labels: [personal]
  extract_concepts: false
  # add this source id to personal.tracked_sources to feed your work-log
```
The description is rendered ADF→Markdown, and status/period render as an as-of-that-day
snapshot header.
