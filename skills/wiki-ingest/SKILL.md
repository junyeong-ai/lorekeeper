---
name: wiki-ingest
description: "Config-driven knowledge ingestion pipeline for Obsidian wikis. Collects daily AI newsletters, team digests, and Slack keyword trends — deduplicates, clusters, labels, extracts concepts, and tracks personal work for performance reviews. Each source is defined in config.yaml with its own schedule, wiki target, and labeling rules."
when_to_use: "/wiki-ingest, 뉴스 수집, 팀 다이제스트 수집, 슬랙 동향, 위키 수집, ingest daily, ingest ai-news, ingest team, 업무 성과 정리, performance review, 브리핑 수집"
allowed-tools:
  - "mcp__obsidian__vault_read"
  - "mcp__obsidian__vault_write"
  - "mcp__obsidian__vault_patch"
  - "mcp__obsidian__vault_append"
  - "mcp__obsidian__vault_list"
  - "mcp__obsidian__vault_move"
  - "mcp__obsidian__search_simple"
  - "mcp__obsidian__search_query"
  - "mcp__obsidian__tag_list"
  - "mcp__claude_ai_Google_Drive__search_files"
  - "mcp__claude_ai_Google_Drive__read_file_content"
  - "mcp__claude_ai_Gmail__search_threads"
  - "mcp__claude_ai_Gmail__get_thread"
  - "mcp__claude_ai_Gmail__list_labels"
  - "Bash(wikigraph *)"
---

# wiki-ingest — Knowledge Ingestion Pipeline

You are a knowledge pipeline operator. Your job: collect daily data streams,
deduplicate, cluster, label, extract concepts, and write structured wiki pages
to an Obsidian vault. You also track the user's personal work for performance reviews.

## Config Loading

On every invocation, read the project config:

```
Read file: ${CLAUDE_SKILL_DIR}/../../config.yaml
```

If not found, fall back to `${CLAUDE_SKILL_DIR}/../../config.example.yaml` and
warn the user to create their own config.yaml.

## Commands

### `/wiki-ingest [source]`

Run the full pipeline for one or all sources.

- No argument: run all enabled sources
- `ai-news`: AI newsletter only
- `team-digest`: team digest only
- `slack-trends`: Slack keyword trends only
- `gmail`: Gmail daily digest only
- `weekly`: weekly synthesis across all sources
- `monthly`: monthly personal work summary
- `quarterly`: quarterly performance review
- `performance`: show personal work category distribution
- `status`: show last ingest time per source
- `health`: check pipeline health

### Source: ai-news

1. Read `sources.ai-newsletter` from config
2. Search Google Drive for today's briefing file matching `file_pattern`
3. Read file content via Google Drive MCP
4. Parse: extract events, themes, key entities
5. Deduplicate: check `wiki/log.md` for already-ingested dates
6. Write to `{vault}/{daily_dir}/ai-briefing/YYYY-MM-DD.md`:

```yaml
---
id: ai-briefing-2026-05-23
title: "AI 브리핑 2026-05-23"
created: 2026-05-23
labels: [ai-industry]
source: google-drive
events_count: 12
---
```

7. Extract concepts: for each significant entity/topic mentioned 2+ times:
   - Check if `wiki/concepts/{slug}.md` exists
   - If exists → `vault_patch` to append this date's reference
   - If new → `vault_write` new concept page with `confidence: extracted`
8. Update `wiki/index.md` and `wiki/log.md`

### Source: team-digest

1. Read `sources.team-digest` from config
2. Use `slack-workspace` skill to read messages from configured channel
3. Parse team digest messages: extract per-member work items
4. **Personal classification**: items mentioning `me.name`, `me.slack_user_id`, or `me.email`:
   - Copy to `{vault}/{personal_dir}/work-log/YYYY-MM-DD.md`
   - Categorize into `performance.work_categories`
   - Frontmatter includes: `categories: [project-delivery, innovation]`
5. Write team summary to `{vault}/{daily_dir}/team-digest/YYYY-MM-DD.md`
6. Extract concepts from team activities

### Source: slack-trends

1. Read `sources.slack-trends` from config
2. For each configured query (channel + keywords):
   - Use `slack-workspace` skill to search
   - Collect matching messages from the last `lookback_hours`
3. Cluster by topic similarity (group related messages)
4. Label each cluster (auto-assign from `labels.categories`)
5. Write to `{vault}/{daily_dir}/slack-trends/YYYY-MM-DD.md`:
   - Sections by cluster
   - Each cluster: representative message, keyword frequency, participants
6. Track keyword frequency trends in concept pages

### Source: gmail

1. Read `sources.gmail` from config
2. For each `include` query, search Gmail via `mcp__claude_ai_Gmail__search_threads`:
   - Use `newer_than:1d` to limit to last 24 hours
   - Combine with configured query filters
3. For each thread found, fetch full content via `mcp__claude_ai_Gmail__get_thread`
4. **Filter out noise**: skip threads matching any `exclude` pattern:
   - `subject_contains` patterns (automated alerts, no-reply)
   - `from_contains` patterns (notification systems, mailer daemons)
   - Promotional/social/update category emails (already filtered by Gmail query)
5. **Classify** each remaining thread using `classify` rules from config:
   - Check subject + body for signal keywords
   - Assign one primary classification: action_required | decisions | project_updates | knowledge_sharing | meeting_followup | other
6. **Extract value**: for each classified thread, generate a concise summary:
   - Who: sender and key participants
   - What: core content/decision/request in 2-3 sentences
   - Action: any action items for me (if applicable)
   - Relevance: why this matters for my work
7. **Write** daily digest to `{vault}/{daily_dir}/gmail-digest/YYYY-MM-DD.md`:

```yaml
---
id: gmail-digest-2026-05-23
title: "Gmail 다이제스트 2026-05-23"
created: 2026-05-23
labels: [personal, team-ops]
total_threads: 15
filtered_threads: 8
action_required: 2
---
```

Page structure:
```markdown
# Gmail 다이제스트 YYYY-MM-DD

## ⚡ 조치 필요 (Action Required)
- **[Subject]** from Sender — 요약 + 필요 조치

## 📋 의사결정/승인 (Decisions)
- **[Subject]** — 결정 내용 요약

## 📊 프로젝트 업데이트
- **[Subject]** — 진행 상황 요약

## 📚 지식 공유
- **[Subject]** — 핵심 내용 + 참고 링크

## 🤝 미팅 후속
- **[Subject]** — 회의 결과 + action items
```

8. **Personal tracking**: all gmail items are personal work signals:
   - `action_required` items → copy to `me/work-log/YYYY-MM-DD.md` as pending tasks
   - `decisions` items → copy as completed decisions
   - `project_updates` I sent → copy as my project contributions
9. Extract concepts from significant email topics (recurring themes across days)

### Weekly Synthesis

1. Read all `daily/` files from the past 7 days
2. Cross-source deduplication (same events in newsletter + Slack)
3. Synthesize themes: what were the top 3-5 topics this week?
4. Write to `{vault}/{weekly_dir}/synthesis/YYYY-Www.md`
5. Include personal work summary if `weekly.include_performance` is true:
   - My work items this week, categorized
   - Write to `{vault}/{weekly_dir}/me/YYYY-Www.md`

### Performance Reviews

**Monthly** (`/wiki-ingest monthly`):
1. Read `weekly/me/` summaries for the month
2. Aggregate work categories: count per category, key achievements
3. Write to `monthly/me/YYYY-MM.md`

**Quarterly** (`/wiki-ingest quarterly`):
1. Read `monthly/me/` for the quarter
2. Structured review:
   - Category distribution (pie chart in text)
   - Top 5 achievements
   - Areas of growth
   - Cross-reference with team trends (my contribution vs team)
3. Write to `quarterly/me/YYYY-Qq.md` using template

**Annual** (`/wiki-ingest annual`):
1. Read `quarterly/me/` for the year
2. Full year narrative + metrics
3. Write to `annually/me/YYYY.md`

## Deduplication Rules

1. **Event ID match**: if two items share `event_id` (canonical_entity:event_type) → merge
2. **URL match**: same primary URL → merge
3. **Title similarity**: > `dedup.similarity_threshold` → merge, keep richer version
4. **Cross-source merge**: mark as "reported by N sources" in frontmatter
5. **Skip already ingested**: check `wiki/log.md` for date+source entries

## Labeling Rules

Auto-assign labels from `labels.categories` based on content:

| Content Signal | Label |
|---|---|
| Vendor announcements, model releases, benchmarks | `ai-industry` |
| Internal Slack discussions about AI/AX tools | `ai-internal` |
| Team member work items, project updates | `team-ops` |
| Roadmap, quarterly planning, strategy docs | `strategy` |
| Items matching `me.*` identity | `personal` |
| Technical articles, tutorials, studies | `learning` |

Labels are written as frontmatter `tags` on each wiki page.

## Concept Extraction

When a named entity or topic appears in 2+ documents (`concepts.min_mentions`):

1. Normalize name (NFKC, lowercase)
2. Check for existing concept with similar name (`concepts.similarity_threshold`)
3. If merge candidate → `vault_patch` to add new source reference
4. If new → create `wiki/concepts/{slug}.md`:

```yaml
---
id: {slug}
title: "Entity/Topic Name"
created: YYYY-MM-DD
sources: ["daily/ai-briefing/2026-05-23"]
confidence: inferred
tags: [ai-industry]
first_seen: 2026-05-23
mention_count: 3
---
```

5. Update index.md

## Output Templates

Follow templates in `${CLAUDE_SKILL_DIR}/../../templates/`:
- Read the template before generating each page type
- Fill in template variables with actual data
- Preserve template structure (headings, sections)

## Interaction with Other Tools

| Tool | How wiki-ingest uses it |
|---|---|
| wiki skill (`/wiki`) | wiki-ingest writes pages; wiki skill queries them |
| wikigraph CLI | Run `wikigraph lint` after batch ingest to verify graph health |
| nodex | Does not touch nodex-managed directories (AI Plan, Meeting, etc.) |
| slack-workspace | Used to read Slack messages and search |
| Google Drive MCP | Used to fetch newsletter briefings |

## Safety

- Never modify existing non-wiki vault notes (AI Plan/, Meeting/, etc.)
- Never expose secrets from Privacy/ directory
- Always append to log.md, never overwrite
- Deduplicate before writing to avoid ballooning vault size
- Respect config.yaml — never hardcode source-specific values
