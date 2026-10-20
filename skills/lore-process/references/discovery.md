# Extraction discovery

Use for `discover <absolute-task-ids.json>` before a native oversized extraction is
partitioned, or before an ordinary extraction needs its related-knowledge context.
This operation chooses retrieval scope; it does not create knowledge or certify reads.

Read the exact selected task-ID array, then `lore queue status --json`. Process only
its current `extract-concepts` tasks. Capture each target's `target_revision` before
reading its outline and bounded relevant sections with `wiki read --revision`.
Inspect source-written subjects, definitions and scope. Use those to choose up to
16 lexical queries, including declared names or distinguishing claim terms. These
retrieve complete candidates; a match does not establish identity or contradiction.
Do not load the full concept registry or copy a large corpus into the prompt.

Record one strict JSON file per selected task:

```json
{
  "target_revision": "<queue status revision captured before reading>",
  "queries": [{"query":"source-written subject", "phrase":true, "spacing":false}]
}
```

Run `lore work discover --task-id ID --file FILE`. A changed target requires rereading
and choosing the recipes again. Empty recipes are valid for an eventual empty extraction;
publishing any concepts requires at least one query. Never invent generic queries merely
to satisfy that count.

Return after recording all selected recipes. Rust plans the complete retained input
and retrieved candidate pages together. Native selected workers also record recipes
before calling `lore work context --task-id ID`. Check its `requires_bounded_work`
before reading candidate bodies. When true, execute the returned `plan_command`,
stage no extraction result, finish the other selected tasks and return. The native
controller verifies the current recipe and expanded plan before scheduling bounded
inspections; exit zero alone cannot excuse an unfinished task.

Manual ordinary workers can use `lore work context
--task-id ID --discovery-file FILE` with a strict `{"queries":[...]}` file immediately
before drafting, or record a recipe first. For bounded manual work, pass that same file
to `lore work plan --task-id ID --discovery-file FILE`. Stored current recipes are reused
when the flag is omitted. The final result returns the exact queries and captured context
revision; scope changes require rereading and regenerating.

Context reports `resource_count` and `part_count`. Multi-part context has
`resources: null`, rather than an incomplete roster; its `plan_command` retains an
explicit discovery file when supplied. Complete the resulting bounded work before
publication. Selected review dependencies and their current or verified archived
evidence remain part of that same scope, even when the review text does not match a query.
