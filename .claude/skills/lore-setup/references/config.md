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
    type: gmail | google-calendar | google-drive | slack-channel | slack-search | jira
    enabled: true
    schedule: "0 9 * * 1-5"          # cron (used by `lore schedule`)
    params: { ... }                  # per-type (see each reference)
    classify: { category: [keywords] }   # optional, gmail etc.
    labels: [ ... ]
    extract_concepts: true|false     # whether to run LLM concept extraction
    track_personal: true|false       # whether to count toward the work-log

dedup: {cascade: [event-id, content-hash, url], title_threshold: 0.85}  # `title` is opt-in (recurring titles); add only when titles are unique ids
labels: {categories: [...]}
performance: {...}                   # performance-category mapping (see config.example)
synthesis: {weekly: {...}, monthly: {...}, quarterly: {...}, annual: {...}}
llm: {provider: queue}
```

## Location / validation
- `./config.yaml` (repo) or `~/.config/lorekeeper/config.yaml` (binary install). Also
  selectable via `--config` / `LORE_CONFIG`. It cannot live inside the vault (the vault
  path is defined inside the config — that would be circular).
- After writing, always verify with `lore validate` → `lore ingest --dry-run`.
- If a config already exists, merge only the relevant source block — never overwrite the whole file.
