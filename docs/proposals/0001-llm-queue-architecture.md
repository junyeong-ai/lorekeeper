---
id: proposal-0001-llm-queue
title: "LLM Work-Queue Architecture: Decouple Deterministic Rust Pipeline from Semantic Claude Code Skill"
status: accepted
created: 2026-05-23
author: junyeong
---

# Proposal 0001 — LLM Work-Queue Architecture

## Summary

Keep semantic work (summarization, concept extraction, synthesis narratives)
behind a single `LlmClient` trait with three interchangeable providers, selected
by `llm.provider` in config:

- **`queue`** (default) — the Rust pipeline emits structured JSONL task files into
  the vault, and a Claude Code skill (`/wi-process`) drains them using Claude's
  native LLM session (no API key, no separate billing).
- **`anthropic`** — direct Anthropic Messages API for unattended cron where no
  Claude Code session is available (requires `ANTHROPIC_API_KEY`).
- **`noop`** — no semantic work; pages render with empty summary/concept sections.

This keeps **Claude Code as the LLM** for the common interactive case while
preserving an unattended path, and keeps every operational benefit of the Rust
data plane (atomic writes, dedup state, async batching, cron scheduling).

## Motivation

### The Problem

A queue-only design forces every semantic operation through the Claude Code
skill. That is ideal for daily Claude Code users but removes the ability to run
fully unattended (e.g. an overnight cron on a headless server with no Claude Code
session). A direct-API-only design has the opposite problem:

1. **Billing divergence** — a Claude Code subscription already grants LLM access;
   direct API calls bill separately on the Anthropic Console.
2. **Authentication friction** — users must obtain, store, and rotate a separate
   API key alongside their existing Claude Code auth.
3. **Capability regression** — direct API loses Claude Code's tool ecosystem
   (Obsidian MCP, gws CLI) that the skill composes with semantic work.

The shipped design resolves the tension by making the provider a config choice
behind one trait, so neither capability is sacrificed.

### Original Design Intent

From the project's earliest commits and the Karpathy LLM Wiki pattern:

> The wiki is a persistent, compounding artifact. **Claude reads sources,
> extracts entities and concepts, updates cross-references.** The Rust binary
> handles collection, deduplication, normalization, and atomic vault writes.

Queue mode honors the "Claude is the LLM" boundary; anthropic mode is the
explicit, opt-in exception for unattended runs.

## Architecture

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
│           wi-vault writes daily/concept pages, then          │
│           the configured LlmClient handles semantics:        │
│             queue     → buffer tasks, flush JSONL atomically  │
│             anthropic → call Messages API inline             │
│             noop      → return empty results                 │
└──────────────────────────────┬──────────────────────────────┘
                               │ (queue mode only)
                               ▼
┌─────────────────────────────────────────────────────────────┐
│            Claude Code skill: /wi-process                    │
│                                                              │
│  1. Read .wiki-ingest/queue/*.jsonl (oldest first)          │
│  2. For each task:                                          │
│     - summarize        → replace target page section         │
│     - extract-concepts → create/update concept pages         │
│  3. On full success, move file to queue/processed/           │
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
| **Summarization** | LlmClient (queue skill / anthropic API) | semantic, LLM-required |
| **Concept extraction** | LlmClient (queue skill / anthropic API) | semantic, requires reasoning |
| **Weekly/Monthly/Quarterly synthesis** | LlmClient (queue skill / anthropic API) | semantic narrative generation |

### Queue Format

Queue files live in `<vault>/.wiki-ingest/queue/` and use JSONL. Each ingest run
writes one file named `{run-timestamp}-pid{PID}.jsonl`, published atomically
(temp file + fsync + rename) only after every target page has been written — so
once a `.jsonl` file is visible, every task in it points at a page that exists.

```
.wiki-ingest/queue/
├── 2026-05-23T07-00-00Z-pid12345.jsonl   # pending, one ingest run
└── processed/
    └── 2026-05-22T07-00-00Z-pid11122.jsonl  # moved here after the skill drains it
```

Each line is one queue task:

```json
{
  "task_id": "sum-2026-05-23T07-00-00Z-pid12345-000",
  "kind": "summarize",
  "created_at": "2026-05-23T07:00:00Z",
  "input": { "text": "<events concatenated>", "max_sentences": 5 },
  "target": { "vault_path": "daily/ai-news/2026-05-23.md", "kind": "daily-summary" }
}
```

`kind`: `summarize` | `extract-concepts`. `target.kind` maps each task to the
exact section heading the skill must replace (e.g. `daily-summary` → `## 요약`).

There is **no `processed.jsonl` sidecar**. The vault edits are the source of
truth, and every edit is idempotent (section-body replacement, dedupe-aware
concept merging), so re-running on a partially-drained file is safe.

### Skill Responsibility (`/wi-process`)

`~/.claude/skills/wi-process/SKILL.md` reads queue files and performs LLM work:

```
1. List .wiki-ingest/queue/*.jsonl (oldest filename first).
2. For each file, for each task in order:
   a. summarize        → replace the target section body via Obsidian MCP.
   b. extract-concepts → write concept wikilinks; create/merge concept pages.
   c. On task failure → record the task_id, abort this file, leave it in place.
3. Only when every task succeeded, move the file to queue/processed/.
4. Report processed/failed counts; exit non-zero on any failure.
```

The skill uses Obsidian MCP for vault writes. Queue tasks are small (<100/day),
so per-call MCP latency is acceptable.

## Trade-offs

### Benefits

- **Zero additional cost** in queue mode: the Claude Code subscription is the
  only LLM billing.
- **Single auth surface** in queue mode: no separate Anthropic API key.
- **Unattended capability retained**: anthropic mode covers headless cron.
- **Composability**: the skill can invoke other skills during processing.
- **Failure isolation**: a failed queue task leaves the file in place; the next
  `/wi-process` run replays it. Every edit is idempotent.
- **Inspectable queue**: humans can read `.wiki-ingest/queue/*.jsonl`.

### Costs

- **Two-step daily flow** in queue mode: cron runs `wi ingest`, then the user (or
  a scheduled session) runs `/wi-process`. Mitigation: a chained wrapper script.
- **Latency**: the Rust pipeline finishes in seconds; the LLM step takes longer.
- **State split**: dedup state in redb, pending semantic work in queue files.
  Mitigation: the queue flush runs *before* the dedup commit, so a crash between
  them re-queues on the next run rather than losing work.

### Non-Risks

- *"What if the skill never runs?"* — Queue files accumulate; the Rust side keeps
  working. No data loss.
- *"What if a queue task is malformed?"* — The skill records it, leaves the file
  in place, and exits non-zero; a re-run retries.
- *"What if two `/wi-process` runs overlap?"* — Files are published atomically and
  are filename-unique per run (timestamp + PID); a drain either sees a complete
  file or not at all.

## Alternatives Considered

### Alt A: Remove all LLM work entirely

Skip semantic synthesis; just collect and template-render.

**Rejected**: loses the core value. The wiki *is* the LLM-synthesized concept
layer. (This survives as the `noop` provider for development/CI.)

### Alt B: Spawn the `claude` CLI from wi

`wi-llm` becomes a subprocess invoker of `claude --print`.

**Rejected**: tight coupling to a specific Claude Code installation, hard to
test, breaks if Claude Code isn't on PATH.

### Alt C: Queue-only, delete the direct-API path

Default to queue and remove anthropic entirely.

**Rejected**: removes the only unattended path. Kept as the `anthropic` provider
behind the same trait, with graceful degradation to `noop` when the API key is
absent.

## Decision

Accepted as the dual-mode (`queue` / `anthropic` / `noop`) architecture:

- `LlmClient` trait in `wi-llm` with three implementations.
- Queue provider emits `.wiki-ingest/queue/*.jsonl` and is the default.
- `/wi-process` skill drains the queue with an abort-on-first-failure,
  idempotent-replay contract.
- Flush-before-dedup-commit ordering guarantees no semantic work is lost on
  partial failure.
