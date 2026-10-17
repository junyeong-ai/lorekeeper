# config.yaml schema

The full example is `config.example.yaml` at the repo root, deployed to
`~/.config/lorekeeper/config.example.yaml` for binary installs. It owns the supported
source types and their options; this file summarizes the surrounding structure.

```yaml
vault:
  root: ~/Documents/Obsidian Vault   # relative paths resolve against the config file
  timezone: Asia/Seoul               # IANA name or "system"
  locale: ko                         # generated prose/headings: ko | en; source bodies kept as-is

identity:                            # basis for ownership (the personal split, when enabled)
  name: "..."
  email: "...@company.com"           # Gmail From / Calendar attendee match
  slack_id: "U0..."                  # attributes my Slack posts (with personal.tracked_sources)
                                     # (Jira self-detects from the authenticated account — no config)

ingest:
  schedule: "0 9 * * *"              # ONE daily `lore ingest` over every enabled source
                                     # (never per source: the work-log is a cross-source
                                     # daily aggregate, so one run keeps it complete)

sources:                             # key = source id; document sources use wiki/documents
  <id>:
    type: <source-type>              # from config.example.yaml
    enabled: true
    # instance: cloud               # Jira/Confluence credential instance, if needed
    params: { ... }                  # per-type (see each reference)
    focus: "..."                     # optional natural-language relevance filter (LLM-applied)
    classify:                        # optional ordered rules, first match wins
      - keywords: [deploy, incident]
        category: action_required    # daily-page grouping bucket
        performance_category: project-delivery   # optional → personal taxonomy (needs personal:)
    highlights:                      # optional daily-page highlight sections (per category)
      - { category: action_required, label: "Action Required" }
    labels: [ ... ]
    extract_concepts: true|false     # whether to run LLM concept extraction

personal:                            # OPTIONAL personal-productivity module (work-log +
  tracked_sources: [<id>, ...]       # reviews + contribution taxonomy). OMIT for a pure
  performance_categories: [ ... ]    # domain-neutral knowledge engine. Its presence = ON.
  source_type_category_map: { ... }  # (see config.example for the full shape)
  monthly:   {enabled: true, schedule: "..."}    # quarterly/annual likewise
synthesis: {weekly: {...}}           # cross-source weekly themes (domain-neutral)
concepts:                            # LLM concept-page taxonomy
  categories: [{id: ..., label: ...}] # also orders/labels the `### category` index groups
graph:                               # link graph analysis (lore graph *)
  scope: {dirs: [...]}               # defaults to vault.dirs.wiki
  metrics: {min_hub_degree: 5, orphan_exclude: []}
  cluster: {resolution: 1.0, min_community_size: 1, suggest_min_shared_neighbors: 2}
llm: {provider: queue}
```

## Location / validation
- `./config.yaml` (repo) or `~/.config/lorekeeper/config.yaml` (binary install). Also
  selectable via `--config` / `LORE_CONFIG`. Other locations require that explicit selection;
  `vault.root` resolves against the selected config file, not the working directory.
- After writing, always verify with `lore validate` → `lore ingest --dry-run`.
- If a config already exists, merge only the relevant source block — never overwrite the whole file.

The optional `nodex` adapter requires a current index (`nodex build` after edits).
`params.document_ids` selects exact source IDs within the configured kinds and date
window; every selected document must be present and readable. `reconcile: true`
selects current documents independently of authored dates and requires a stable
`repository_id` namespace. It preserves authored dates rather than treating import
as a new publication. `max_documents` refuses oversized selections, never truncates.
Use temporary configurations for selected batches, not scheduled `document_ids`.

A namespace identifies a repository, not its checkout or source configuration name.
An existing imported document keeps its vault address when its title or path changes.
Legacy pages without verified document identity require explicit migration; a matching
path alone cannot authorize overwriting a curated synthesis. Missing inventory entries
do not imply deletion, archival or false claims.

### Nodex binary selection

The adapter supports nodex `>=0.54.2, <0.55`. Lore's installer and
`lore self update --version` select Lore versions, not nodex versions.
Set a source's optional `params.executable` to a compatible binary's absolute path.
Omitted, it uses
`nodex` on PATH. The value is one executable path, with no shell arguments, `~` or
variable expansion; an explicit failure never retries another binary.

The repository must permit the selected version and already hold an index current for
it. Binary selection does not override a project version pin or rebuild its index.
`lore validate` checks the parameter shape; `lore ingest <source-id> --dry-run` checks
actual source compatibility without vault or queue writes. `doctor` audits page contracts,
`health` reads collection history, and `self status` checks Lore's installation; none
certifies the currently selected nodex executable.

Collection uses nodex's verified metadata, canonical body and outgoing relations.
`source_record.observation_revision` identifies that public response with an explicit
scheme; it is not the raw Markdown file's SHA. Formatting that nodex canonicalizes
can yield the same observation after rebuilding. Earlier `revision` and
`content_revision` evidence keeps its original meaning. Refreshing a source creates
new semantic inputs and may require reassessing reviews that cite its old revision.
