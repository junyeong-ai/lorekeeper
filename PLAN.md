# wiki-ingest — Implementation Plan

## Context

Daily data streams (AI newsletters, team digests, Slack trends) produce
ephemeral content that loses value unless systematically accumulated into
structured knowledge. This project turns those streams into a persistent,
cross-referenced Obsidian wiki — with special attention to personal work
tracking for quarterly/annual performance reviews.

The system is config-driven so team members can share the project and
each maintain their own source configuration.

## Architecture Decision: Skill-First, Not Code-First

wiki-ingest is a **Claude Code skill project**, not a traditional software
project. The "code" is the SKILL.md instructions; the "runtime" is Claude
itself. Scripts exist only for format conversion (Block Kit → markdown).

Why:
- Collecting from Drive/Slack requires LLM understanding of content
- Concept extraction IS an LLM task
- Deduplication by meaning (not just ID) needs LLM judgment
- Obsidian MCP tools are Claude's native I/O
- Zero deployment: clone repo, copy config, schedule tasks

## Vault Structure (target state)

```
Obsidian Vault/
├── daily/
│   ├── ai-briefing/
│   │   ├── 2026-05-23.md            # AI news for this date
│   │   └── _events/                 # structured event data (optional)
│   ├── team-digest/
│   │   ├── 2026-05-23.md            # team activity summary
│   │   └── _members/                # per-member breakdown
│   └── slack-trends/
│       └── 2026-05-23.md            # keyword trend summary
├── weekly/
│   ├── synthesis/
│   │   └── 2026-W21.md              # cross-source weekly synthesis
│   └── me/
│       └── 2026-W21.md              # my weekly work summary
├── monthly/
│   └── me/
│       └── 2026-05.md               # my monthly work summary
├── quarterly/
│   └── me/
│       └── 2026-Q2.md               # my quarterly performance review
├── annually/
│   └── me/
│       └── 2026.md                  # annual performance review
├── me/
│   └── work-log/
│       └── 2026-05-23.md            # daily personal work items
├── wiki/
│   ├── concepts/                    # cross-source concept pages
│   ├── sources/                     # ingested source summaries
│   ├── index.md                     # catalog
│   └── log.md                       # operation log
└── nodex.toml                       # governance for non-wiki notes
```

## Phase 0: Gmail Daily Digest (quick win, immediate value)

Gmail은 가장 즉각적인 가치를 제공하는 소스 — 별도 인프라 없이 Gmail MCP로 바로 수집 가능.

### 0.1 Gmail Source Handler

- [ ] Gmail MCP 도구 연동 (search_threads, get_thread)
- [ ] Include 쿼리 구성: `is:unread -category:promotions -category:social -category:updates`
- [ ] Exclude 필터: automated/noreply/notification 패턴
- [ ] 5가지 분류: action_required / decisions / project_updates / knowledge_sharing / meeting_followup
- [ ] 노이즈 필터링: 가치 없는 메일 자동 제외

### 0.2 Gmail → Wiki 페이지 생성

- [ ] `daily/gmail-digest/YYYY-MM-DD.md` 생성 (templates/gmail-digest.md 기반)
- [ ] 섹션별 분류: ⚡조치필요 / 📋의사결정 / 📊프로젝트 / 📚지식공유 / 🤝미팅후속
- [ ] 각 메일: 발신자 + 핵심 요약 2-3문장 + 필요 액션
- [ ] 통계 섹션: 전체/필터링/조치필요 건수

### 0.3 Personal Work Tracking Integration

- [ ] action_required → me/work-log/ 에 pending task로 기록
- [ ] decisions → me/work-log/ 에 completed decision으로 기록
- [ ] 내가 보낸 project_updates → 내 프로젝트 기여로 기록
- [ ] 개념 추출: 반복되는 메일 주제 → wiki/concepts/

## Phase 1: Core Skill + AI Newsletter Ingest

### 1.1 Skill Creation

- [ ] Write `skills/wiki-ingest/SKILL.md` with full pipeline instructions
- [ ] Define `/wiki-ingest` command: reads config.yaml, executes appropriate source handler
- [ ] Define `/wiki-ingest ai-news` command: fetch today's AI briefing → wiki
- [ ] Define `/wiki-ingest status` command: show last ingest times per source
- [ ] Support `$ARGUMENTS` for source selection: `/wiki-ingest team-digest`

### 1.2 Config System

- [ ] Write `config.example.yaml` (done)
- [ ] Add `.gitignore` with `config.yaml` excluded
- [ ] Validate config structure in SKILL.md instructions (Claude reads YAML natively)
- [ ] Document all config fields in README.md

### 1.3 AI Newsletter Ingest

- [ ] Read from Google Drive (via Google Drive MCP or WebFetch)
- [ ] Parse briefing markdown: extract events, themes, entities
- [ ] Write to `daily/ai-briefing/YYYY-MM-DD.md` via Obsidian MCP
- [ ] Extract concepts: new entities → `wiki/concepts/` pages
- [ ] Update `wiki/index.md`
- [ ] Append to `wiki/log.md`

### 1.4 Templates

- [ ] `templates/daily-briefing.md` — frontmatter + sections for AI news
- [ ] `templates/concept-page.md` — standard concept page structure

## Phase 2: Team Digest + Slack Trends

### 2.1 Team Digest Ingest

- [ ] Read from Slack channel (via Slack MCP `slack-workspace` skill)
- [ ] Parse Block Kit messages → extract work items per team member
- [ ] Write to `daily/team-digest/YYYY-MM-DD.md`
- [ ] **Personal classification**: items matching `me.*` identifiers → `me/work-log/YYYY-MM-DD.md`
- [ ] Concept extraction from team activities

### 2.2 Slack Keyword Trends

- [ ] Search configured channels for configured keywords (via Slack MCP)
- [ ] Cluster by topic similarity
- [ ] Write to `daily/slack-trends/YYYY-MM-DD.md`
- [ ] Track keyword frequency over time (append to `wiki/concepts/` trend data)

### 2.3 Deduplication Engine

- [ ] Event ID matching: same `canonical_entity:event_type` across sources → merge
- [ ] Title similarity: fuzzy match for events without formal IDs
- [ ] Cross-source flag: "seen in N sources" metadata on wiki pages

### 2.4 Auto-Labeling

- [ ] Read `labels.categories` from config
- [ ] Claude auto-assigns 1-3 labels per ingested item based on content
- [ ] Labels become frontmatter tags on wiki pages
- [ ] Labels drive directory routing (ai-industry → daily/ai-briefing, etc.)

## Phase 3: Performance Tracking

### 3.1 Personal Work Log

- [ ] Daily: extract my work items from team digest + gmail → `me/work-log/YYYY-MM-DD.md`
- [ ] Categorize each item into `performance.work_categories`
- [ ] Frontmatter: `categories: [project-delivery, innovation]`

### 3.2 Periodic Summaries

- [ ] Weekly: synthesize `me/work-log/` for the week → `weekly/me/YYYY-Www.md`
- [ ] Monthly: synthesize weekly summaries → `monthly/me/YYYY-MM.md`
- [ ] Quarterly: synthesize monthly summaries → `quarterly/me/YYYY-Qq.md`
  - Include: category breakdown, key achievements, metrics
  - Format: performance review template (structured sections)
- [ ] Annual: synthesize quarterly reviews → `annually/me/YYYY.md`

### 3.3 Performance Dashboard

- [ ] `/wiki-ingest performance` command: show work category distribution
- [ ] Cross-reference personal work with team activity (my contribution %)
- [ ] Trend analysis: which categories am I spending most time on?

## Phase 4: Scale + Clustering

### 4.1 Concept Clustering

- [ ] When wiki/concepts/ exceeds 100 pages: run `wikigraph cluster`
- [ ] Auto-generate topic group pages: `wiki/topics/{cluster-label}.md`
- [ ] Link concepts to their topic group

### 4.2 Incremental Dedup Cache

- [ ] Cache seen event IDs in `.wiki-ingest/seen-events.json`
- [ ] Skip already-ingested events on re-run
- [ ] Expire cache entries after 90 days

### 4.3 Multi-User Support

- [ ] Each user's `config.yaml` defines their own `me.*` identity
- [ ] Shared templates and skills via git
- [ ] Personal data (config.yaml, cache) gitignored

## Phase 5: Scheduled Tasks

### 5.1 Claude Desktop Schedules

Register these as Claude Desktop scheduled tasks:

```
# Daily AI briefing (weekdays 07:00)
/wiki-ingest ai-news

# Gmail daily digest (weekdays 08:30)
/wiki-ingest gmail

# Team digest (weekdays 09:00)
/wiki-ingest team-digest

# Slack trends (weekdays 10:00)
/wiki-ingest slack-trends

# Weekly synthesis (Monday 08:00)
/wiki-ingest weekly

# Monthly summary (1st of month 09:00)
/wiki-ingest monthly

# Quarterly review (Q end + 1 week)
/wiki-ingest quarterly
```

### 5.2 Health Monitoring

- [ ] `/wiki-ingest health` command: check last successful ingest per source
- [ ] Alert if a source hasn't been ingested in 48+ hours
- [ ] Log failures to `wiki/log.md`

## Non-Goals

- Building a web UI (Obsidian IS the UI)
- Real-time streaming (daily batch is sufficient)
- Multi-tenant server deployment (each user runs locally)
- Custom LLM fine-tuning (Claude's general capability is sufficient)
- Replacing aix-platform's Cloud Run jobs (those continue running; we consume their output)

## Verification

```bash
# Phase 1: AI newsletter ingest works
/wiki-ingest ai-news
# → check daily/ai-briefing/ for today's file
# → check wiki/concepts/ for new concept pages

# Phase 2: Team digest works
/wiki-ingest team-digest
# → check daily/team-digest/ for today's file
# → check me/work-log/ for personal items

# Phase 3: Performance tracking
/wiki-ingest performance
# → quarterly/me/ has structured review

# Phase 4: Scale
wikigraph --root ~/Documents/Obsidian\ Vault lint
# → no orphans, no broken links, hubs identified

# Phase 5: Scheduled
# Check Claude Desktop scheduled tasks are firing
```
