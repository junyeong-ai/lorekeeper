---
id: proposal-0001-llm-queue
title: "LLM Work-Queue Architecture: Decouple Deterministic Rust Pipeline from Semantic Claude Code Skill"
status: proposed
created: 2026-05-23
author: junyeong
---

# Proposal 0001 — LLM Work-Queue Architecture

## Summary

Remove the `wi-llm` crate's direct Anthropic API integration. Replace it with a
**work-queue handoff**: the Rust pipeline emits structured JSONL queue files into
the vault, and a Claude Code skill (`/wiki-process`) consumes those queues and
performs all semantic work using Claude's native LLM (no API key, no separate
billing).

This restores the original design intent — **Claude Code is the LLM**, the Rust
binary is the deterministic data plane — while preserving every operational
benefit gained from the Rust rewrite (atomic writes, dedup state, async batching,
cron scheduling).

## Motivation

### The Problem

The current implementation (`crates/wi-llm/src/claude.rs:27-29`) calls Anthropic's
`/v1/messages` endpoint directly using an `ANTHROPIC_API_KEY`:

```rust
let api_key = std::env::var("ANTHROPIC_API_KEY")
    .map_err(|_| LlmError::Api("ANTHROPIC_API_KEY not set".into()))?;
```

This creates four downstream problems:

1. **Billing divergence** — Claude Code subscription already grants LLM access.
   Direct API calls bypass that and bill separately on the Anthropic Console.
2. **Authentication friction** — users must obtain, store, and rotate a separate
   API key alongside their existing Claude Code auth.
3. **Capability regression** — direct API loses Claude Code's tool ecosystem
   (Obsidian MCP, gws CLI, etc.) that the skill could otherwise compose with
   semantic work.
4. **Design drift** — this is exactly the pattern we explicitly rejected when
   evaluating OpenKB. The Rust rewrite was supposed to fix MCP-async limits and
   provide atomic writes — not to take over LLM orchestration.

### Original Design Intent

From the project's earliest commits and the Karpathy LLM Wiki pattern:

> The wiki is a persistent, compounding artifact. **Claude reads sources,
> extracts entities and concepts, updates cross-references.** The Rust binary
> handles collection, deduplication, normalization, and atomic vault writes.

The current architecture violates the "Claude is the LLM" boundary.

## Proposal: Queue-Based Handoff

### Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                       wi (Rust binary)                       │
│                                                              │
│  wi-source  ──→  wi-pipeline (normalize, dedup, classify)   │
│                       │                                      │
│                       ▼                                      │
│              raw events + classification                     │
│                       │                                      │
│                       ▼                                      │
│           wi-vault writes to TWO targets:                    │
│           1. Final pages (rendering-only, no LLM needed)    │
│           2. LLM work queue (.queue/YYYY-MM-DD-*.jsonl)     │
└──────────────────────────────┬──────────────────────────────┘
                               │
                               │ (cron tick completes)
                               ▼
┌─────────────────────────────────────────────────────────────┐
│            Claude Code skill: /wiki-process                  │
│                                                              │
│  1. Read .queue/*.jsonl (oldest first)                      │
│  2. For each queued task:                                   │
│     - summarize   → write summary to vault                   │
│     - extract     → create/update concept page              │
│     - synthesize  → write weekly/monthly/quarterly summary  │
│  3. Move queue file to .queue/processed/                    │
│  4. Update wiki/index.md and wiki/log.md                    │
└─────────────────────────────────────────────────────────────┘
```

### Boundary Contract

| Concern | Owner | Why |
|---|---|---|
| HTTP fetching (Gmail/Drive/Slack/Jira/Calendar) | Rust (wi-source) | tokio async batching, rate limit handling |
| Atomic file writes | Rust (wi-vault) | tmpfile + rename, no MCP partial-state risk |
| Deduplication state (redb cache, 90-day window) | Rust (wi-pipeline) | persistent KV, MCP has no equivalent |
| Frontmatter parsing / template rendering | Rust (wi-vault, wi-pipeline) | deterministic, no LLM judgment needed |
| Cron scheduling | Rust (wi-cli) | native, deterministic |
| **Summarization** | Claude Code skill | semantic, LLM-required |
| **Concept extraction** | Claude Code skill | semantic, requires reasoning |
| **Concept merging across sources** | Claude Code skill | semantic similarity judgment |
| **Weekly/Monthly/Quarterly synthesis** | Claude Code skill | semantic narrative generation |
| **Personal work classification** | Claude Code skill | semantic — distinguishing my work from team |

### Queue Format

Queue files live in `<vault>/.queue/` and use JSONL for append-only semantics
and incremental processing. Filename encodes the work date and queue type:

```
.queue/
├── 2026-05-23-summarize.jsonl       # one task per ingested event
├── 2026-05-23-extract.jsonl         # concept extraction tasks
├── 2026-05-23-synthesize-weekly.jsonl  # weekly synthesis trigger
└── processed/
    └── 2026-05-22-summarize.jsonl   # moved here after skill completes
```

Each line is one queue task:

```json
{
  "task_id": "sum-2026-05-23-001",
  "task_type": "summarize",
  "source_event_id": "ai-newsletter:claude-opus-47-release",
  "source_file": "raw/ai-newsletter/2026-05-23-anthropic-blog.md",
  "target_file": "daily/ai-briefing/2026-05-23.md",
  "target_section": "Anthropic",
  "instructions": "Extract 2-3 sentence summary in Korean. Tag concepts mentioned.",
  "context": {
    "labels": ["ai-industry"],
    "source_type": "blog",
    "language": "en"
  }
}
```

The skill consumes one task at a time, completes it, and appends to a
companion `processed.jsonl` so resumption is possible if the skill is
interrupted mid-batch.

### Skill Responsibility (`/wiki-process`)

A new Claude Code skill (`~/.claude/skills/wiki-process/SKILL.md`) reads queue
files and performs LLM work. Pseudocode of the skill's instructions:

```
1. Find oldest .queue/*.jsonl file in vault
2. For each task in the file:
   a. Read context (source files, related concepts)
   b. Perform semantic work according to task_type:
      - summarize: read source, write summary section via Obsidian MCP
      - extract: scan summary, find candidate concepts, create/update
                 concept pages, add wikilinks
      - synthesize: read past 7/30/90 days, write synthesis page
   c. Update wiki/index.md if new pages were created
   d. Append result to processed.jsonl
3. Move completed queue file to .queue/processed/
4. Report task counts to user
```

The skill uses Obsidian MCP for vault writes (the original I/O path), since
queue tasks are small (<100 per day) and MCP's per-call latency is acceptable
when not in a tight loop.

## Migration Plan

### Phase 1: Remove wi-llm

- Delete `crates/wi-llm/` entirely
- Remove `ANTHROPIC_API_KEY` requirement from README and config
- Update `Cargo.toml` workspace members

### Phase 2: Add wi-pipeline queue emitter

- New module: `wi-pipeline/src/queue.rs`
- Existing classify/normalize/dedup logic stays unchanged
- At pipeline end: instead of calling `LlmClient`, emit JSONL queue entries
- `wi-vault` gains a `write_queue_entry()` helper

### Phase 3: Drop LLM call sites

- `wi-pipeline/src/synthesis.rs:61` (`self.ctx.llm.summarize`) → emit queue task
- `wi-pipeline/src/concepts.rs` LLM merge calls → emit queue task
- All `LlmClient` trait method calls removed from pipeline

### Phase 4: Create /wiki-process skill

- `~/.claude/skills/wiki-process/SKILL.md` with queue-processing instructions
- Reuses Obsidian MCP tools that the existing `wiki` skill already declares
- Idempotent: re-running on a partially-processed queue file skips done tasks

### Phase 5: Update orchestration

- Cron entry sequence (typical day):
  ```
  07:00  wi ingest ai-news        # Rust: collect + queue summarize tasks
  07:05  /wiki-process            # Claude Code: drain ai-news queue
  08:30  wi ingest gmail
  08:35  /wiki-process
  09:00  wi ingest team-digest
  09:05  /wiki-process
  ```
- Or batched: `wi ingest --all` followed by a single `/wiki-process` drain

## Trade-offs

### Benefits

- **Zero additional cost**: Claude Code subscription is the only LLM billing
- **Single auth surface**: no separate Anthropic API key to rotate
- **Composability**: skill can invoke other skills (`wiki`, `nodex`, `wikigraph`)
  during processing — direct API can't
- **Failure isolation**: a failed LLM task leaves the queue entry intact;
  next `/wiki-process` run picks it up
- **Inspectable queue**: humans can read `.queue/*.jsonl` and verify what the
  Rust pipeline asks the skill to do
- **Codebase shrinks**: removing wi-llm removes ~254 lines of LLM HTTP plumbing
  and the reqwest dependency

### Costs

- **Two-step daily flow**: cron must run `wi ingest` and then `/wiki-process`.
  Mitigation: a wrapper script `wi-and-process.sh` chains them.
- **Latency**: Rust pipeline finishes in seconds; LLM step takes longer when
  Claude Code processes 10-50 tasks. Mitigation: skill processes in parallel
  where MCP allows (read-many → analyze → write-once).
- **State split**: dedup state in Rust (`redb`), processing state in queue
  files. Mitigation: queue files are the source of truth; `redb` only tracks
  what's already been *queued*, not what's been *processed*.
- **MCP latency reappears for LLM-driven writes**: but only for the small
  set of pages the skill produces, not for the bulk source ingestion
  (which stays in Rust).

### Non-Risks (claims that sound scary but aren't)

- *"What if the skill never runs?"* — Queue files accumulate, but the Rust
  side keeps working. User runs `/wiki-process` whenever convenient. No data
  loss.
- *"What if a queue task is malformed?"* — Skill skips it, logs a warning,
  continues. Single bad task doesn't poison the queue.
- *"What if two `/wiki-process` runs overlap?"* — File locking on queue
  files prevents double-processing. Implementation detail.

## Alternatives Considered

### Alt A: Remove all LLM work entirely

Skip semantic synthesis; just collect and template-render.

**Rejected**: loses the core value of the system. Karpathy's wiki *is* the
LLM-synthesized concept layer. Without it we have a glorified RSS reader.

### Alt B: Spawn `claude` CLI from wi

`wi-llm` becomes a subprocess invoker of the `claude --print` command.

**Rejected**: tight coupling to a specific Claude Code installation, hard to
test, breaks if Claude Code isn't on PATH. Also feels like a workaround to
avoid restructuring rather than a clean design.

### Alt C: Keep wi-llm as opt-in fallback

Default to queue-based; allow direct API for users who want fully autonomous
overnight runs without invoking the skill.

**Maybe later**: legitimate use case, but adds a second code path to maintain.
Defer until someone actually needs unattended LLM processing.

## Verification

After implementation:

```bash
# Pipeline still works without API key
unset ANTHROPIC_API_KEY
wi ingest ai-news --root ~/Documents/Obsidian\ Vault
# → expect: source pulled, raw written, .queue/2026-05-23-summarize.jsonl created
ls ~/Documents/Obsidian\ Vault/.queue/

# Skill drains the queue
# (in Claude Code) /wiki-process
# → expect: queue file moved to .queue/processed/, summary + concept pages created

# Re-running on empty queue is a no-op
/wiki-process
# → expect: "no queued tasks"

# Skill failure leaves queue intact
# (force a failure mid-batch, e.g., revoke Obsidian REST API key)
/wiki-process
# → expect: partial progress saved, queue file remains in .queue/
# (restore access)
/wiki-process
# → expect: resumes from where it left off
```

## Decision Required

- [ ] Approve queue-based architecture as v0.4 target
- [ ] Approve removal of `wi-llm` crate
- [ ] Approve creation of `/wiki-process` skill in `~/.claude/skills/`
- [ ] Approve cron-then-skill orchestration pattern

If approved, implementation should land in a single PR that:
1. Deletes `crates/wi-llm/`
2. Adds `wi-pipeline/src/queue.rs`
3. Updates `synthesis.rs` and `concepts.rs` to emit queue tasks
4. Creates the `/wiki-process` skill
5. Updates README, config.example.yaml, and CLAUDE.md
