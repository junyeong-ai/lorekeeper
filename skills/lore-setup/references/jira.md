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
Jira's "Start date" is a per-instance custom field. Fetch all fields on a known issue:
```bash
atlassian-cli jira get <ISSUE-KEY> --fields '*all'
```
The response uses keys such as `customfield_10015`, not display labels. Confirm the
exact field ID through the site's field metadata or administration UI; a date value
alone does not identify its meaning. Set `start_date_field` only after confirming it.
Omit this optional setting if the ID is unknown; the start date will not be shown.

## config block
```yaml
my-tasks:
  type: jira
  enabled: true
  params:
    # Select currently assigned issues by updated time; this does not identify who changed them.
    jql: >
      project = PROJ AND assignee = currentUser()
      AND updated >= -1d
      ORDER BY updated DESC
    # max_issues: 200                     # optional per-run cap; the fetch paginates the
                                          # whole JQL result and warns if it drops issues
    start_date_field: customfield_10015   # optional: show the start date
  labels: [personal]
  concept_policy: disabled
  # add this source id to personal.tracked_sources to feed your work-log
```
The description is rendered ADF→Markdown. Status, assignee and planned dates are
current values at acquisition, not reconstructed values for a historical target day.
The configured JQL determines selection; `updated` does not prove personal work or
complete date coverage, and a later relative query can omit previously collected issues.
