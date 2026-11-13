# Adopt audit candidate screening

Use this Unix procedure for an explicitly requested transition of pending legacy audit
discovery. Native discovery, screening and finalization do not adopt other scopes.
Coordinate through the workflow lease before reading evidence or publishing.

Read `lore wiki audit-plan --path SUBJECT` and copy the selected candidate's exact
guards into a JSON file under `.lorekeeper/`:

```json
{
  "path": "wiki/documents/retention-policy.md",
  "file_revision": "FROM_CURRENT_CANDIDATE",
  "input_revision": "FROM_CURRENT_CANDIDATE"
}
```

Preview with `lore wiki audit-discover --adopt-screening --file FILE --dry-run`.
The result compares the previous context with required direct context and reports
the candidate inventory. It does not estimate saved calls or establish relevance.
Review existing Work and processing actions before choosing a new scope: adoption
changes semantic input identity, so predecessor Work may no longer be current.
Existing plans, paid observations, actions and assessments remain unchanged;
never relabel them or infer that they were reused by the new scope.
Current semantic processing actions refuse adoption; complete their guarded repair
or explicit recovery first. Capacity-only admission limits do not authorize a call
or suppress the preview of a smaller discovery scope.

Publish the reviewed transition by omitting `--dry-run`. Current legacy assessments
refuse transition. A changed subject or dependency refuses the captured request; inspect
current inputs instead of replacing guards on a prior decision. Unchanged screened
scopes are inert. Preview never publishes or recovers pending journals.

Adoption retains the recorded queries, mandatory evidence, direct links and scoped
reviews. It leaves discovery candidates undecided. Continue with the maintenance
[candidate screening procedure](maintenance.md#candidate-screening), retaining
uncertain candidates for inspection, then complete fresh guarded assessment.
