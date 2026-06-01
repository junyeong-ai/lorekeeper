# config.yaml schema

The full example is `config.example.yaml` at the repo root (per-source-type blocks +
comments). This file summarizes only the top-level structure around the source blocks.

```yaml
vault:
  root: ~/Documents/Obsidian Vault   # relative paths resolve against the config file
  timezone: Asia/Seoul               # IANA name or "system"
  locale: ko                         # label language: ko | en (source bodies kept as-is)

identity:                            # basis for personal split + performance tracking
  name: "..."
  email: "...@company.com"           # Gmail From / Calendar attendee match
  slack_id: "U0..."                  # my Slack messages → work-log
                                     # (Jira self-detects from the authenticated account — no config)

sources:                             # key = source id = <daily>/{id}/ directory name
  <id>:
    type: gmail | google-calendar | google-drive | slack-channel | slack-search | jira | rss | manual
    enabled: true
    schedule: "0 9 * * 1-5"          # cron (used by `lore schedule`)
    params: { ... }                  # per-type (see each reference)
    focus: "..."                     # optional natural-language relevance filter (LLM-applied)
    classify:                        # optional ordered rules, first match wins
      - keywords: [deploy, incident]
        category: action_required    # daily-page grouping bucket
        work_category: project-delivery   # optional → performance taxonomy
    labels: [ ... ]
    extract_concepts: true|false     # whether to run LLM concept extraction
    track_personal: true|false       # whether to count toward the work-log

dedup: {cascade: [event-id, content-hash, url], extra_tracking_params: [...]}  # exact-match stages; lossless
labels: {categories: [...]}
performance: {...}                   # performance-category mapping (see config.example)
synthesis: {weekly: {...}, monthly: {...}, quarterly: {...}, annual: {...}}
concepts:                            # LLM concept-page taxonomy
  categories: [{id: ..., label: ...}]
  index_split_threshold: 100         # split index.md into per-category pages above this
graph:                               # wikilink graph analysis (lore graph *)
  scope: {dirs: [...]}               # defaults to vault.dirs.wiki
  metrics: {min_hub_degree: 5, orphan_exclude: [], concept_near_duplicate_threshold: 0.6}
  cluster: {resolution: 1.0, min_community_size: 1, suggest_min_shared_neighbors: 2}
llm: {provider: queue}
```

## Location / validation
- `./config.yaml` (repo) or `~/.config/lorekeeper/config.yaml` (binary install). Also
  selectable via `--config` / `LORE_CONFIG`. It cannot live inside the vault (the vault
  path is defined inside the config — that would be circular).
- After writing, always verify with `lore validate` → `lore ingest --dry-run`.
- If a config already exists, merge only the relevant source block — never overwrite the whole file.
