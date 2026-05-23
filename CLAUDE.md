# wiki-ingest

Config-driven knowledge ingestion pipeline for Obsidian wikis.
Collects, deduplicates, clusters, labels, and wiki-fies daily data streams.

## Identity

wiki-ingest is a **knowledge pipeline orchestrator**. It bridges data sources
(AI newsletters, team digests, Slack trends) and an Obsidian wiki, turning
raw daily streams into structured, cross-referenced, long-term knowledge.

It is NOT the wiki itself (that's the Obsidian vault + wiki skill),
NOT the graph analyzer (that's wikigraph), and NOT the governance layer (that's nodex).

## Architecture

```
Data Sources                    wiki-ingest                     Obsidian Vault
─────────────                   ───────────                     ──────────────
AI Newsletter ──┐               ┌─ Collect                     daily/ai-briefing/
Team Digest ────┼── config ───► ├─ Deduplicate (event_id)      daily/team-digest/
Slack Trends ───┤    .yaml      ├─ Cluster (topic similarity)  daily/slack-trends/
Manual Add ─────┘               ├─ Label (auto-categorize)     wiki/concepts/
                                ├─ Classify (personal work)    me/work-log/
                                └─ Write (Obsidian MCP)        weekly/ quarterly/
```

## Core Flow

1. **Collect**: Fetch today's data from configured sources
2. **Deduplicate**: Same event across sources → merge, not duplicate
3. **Cluster**: Group related events by topic similarity
4. **Label**: Auto-categorize (ai-industry, team-ops, personal, strategy)
5. **Classify**: Flag personal work items for performance tracking
6. **Compile**: Generate wiki pages (summaries, concepts, cross-refs)
7. **Write**: Push to Obsidian vault via MCP tools

## Config: config.yaml

Defines sources, wiki mapping, personal settings. Each team member
maintains their own config.yaml. The skills and templates are shared.

## Scheduled Execution

Runs as Claude Desktop scheduled tasks. Each source has its own schedule
(daily, twice-daily, weekly). The skill reads config.yaml to determine
what to collect and where to write.

## Project Structure

```
wiki-ingest/
├── CLAUDE.md                 # This file
├── PLAN.md                   # Implementation plan
├── config.yaml               # Source configuration (user-specific, gitignored)
├── config.example.yaml       # Template for new users
├── skills/
│   └── wiki-ingest/
│       └── SKILL.md          # The ingest orchestrator skill
├── templates/                # Wiki page templates
│   ├── daily-briefing.md
│   ├── team-digest.md
│   ├── slack-trends.md
│   ├── weekly-summary.md
│   └── quarterly-review.md
└── scripts/
    └── block-kit-to-md.sh    # Block Kit JSON → markdown converter
```
