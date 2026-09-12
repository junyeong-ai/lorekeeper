# Queue file lifecycle, recovery, and task schema

Read-once reference for `/lore-process`. The skill body carries the processing
protocol; this file carries the format details you consult only when needed.

## Queue file lifecycle

```
<run>.jsonl.tmp        ←  ingest is mid-flush (transient, sub-second; not for consumption)
<run>.jsonl            ←  pending, ready to be drained by this skill
processed/<run>.jsonl  ←  drained successfully, retained 90 days
(deleted)              ←  pruned by `lore maintenance` after retention expires
```

`lore ingest` sweeps `*.jsonl.tmp` files older than 1 hour at startup (crash debris
from previous runs); `lore maintenance` does NOT touch tmp files, so a concurrent
maintenance run cannot race an active flush. Only `.jsonl` files matter to this skill.

**Known limitation:** the tmp sweep is mtime-based, not PID-aware. If an ingest process
is paused (SIGSTOP) for more than 1 hour, a later-starting ingest could delete its tmp;
the paused ingest's flush then fails with ENOENT and that run's LLM tasks are lost.
Ingest is idempotent for the CURRENT day — the same window returns the same events — so
recovery is re-running it:

```bash
lore ingest              # re-renders the same pages, re-queues their tasks
/lore-process            # drains the new queue file
```

In practice, cron-scheduled ingests never hit this case.

## Queue task schema

Each line in a queue file is one task. All `vault_path` values are vault-relative paths
resolved by the pipeline from `vault.dirs.*` — never construct them manually; use the
path patterns from AGENTS.md.

```json
{
  "task_id": "sum-2026-05-23T07-00-00Z-pid4211-0-000",
  "kind": "summarize",
  "created_at": "2026-05-23T07:00:00Z",
  "cache_hash": "0123456789abcdef0123456789abcdef",
  "input": {
    "text": "<events concatenated for summarization>",
    "max_sentences": 5,
    "locale": "ko",
    "source_type": "rss"
  },
  "target": {
    "vault_path": "<daily>/ai-news/2026-05-23.md",
    "kind": "daily-summary",
    "anchor": "## Summary"
  }
}
```

`kind` values: `summarize` | `extract-concepts` | `identify-themes` | `refine-events` |
`synthesize-concept`

`target.kind` values: `daily-summary` | `daily-refine-events` | `daily-concepts` |
`document-summary` | `document-concepts` | `weekly-synthesis-themes` |
`weekly-review-narrative` | `monthly-review-narrative` |
`quarterly-review-narrative` | `annual-review-narrative` | `work-log-synthesis` |
`concept-synthesis`

`input.locale` is on EVERY task, whatever its kind: it is `vault.locale`, the language the
vault is authored in, and it is what every word the drain adds gets written in. See
[processing-kinds.md](processing-kinds.md) § Output language.

`target.anchor`: the exact section heading (e.g. `"## Summary"`, or its localized form per
AGENTS.md) the pipeline wrote, resolved from i18n at queue time. Always use this as the
locate key — never hardcode headings per `target.kind`.

`input.related_anchor` is on a `synthesize-concept` task only, and holds the concept page's
related-concepts heading as THAT page spells it (absent where the page carries no such
section). One citation set is what both sections answer to, so one task fills both and one
`synthesis_done` answers for the pair. It is payload rather than cache identity: a heading
says where an answer lands, not what the answer would be.

`cache_hash` is BLAKE3-128 (32 hex chars) of the cache-identity subset of `input` — the
fields that decide whether the output would differ, which is narrower than the payload. It
excludes `source_type`, which scopes extraction without shaping its answer, and it includes
`locale` for the kinds writing a section that is re-derived from its input (a summary, a
week's themes) while excluding it for the two that write onto a concept page, whose authored
body a locale switch deliberately leaves standing. It equals the value the pipeline wrote
into `target.vault_path`'s `llm_inputs.<key>` frontmatter at queue time. The skill MUST
verify the page's current frontmatter matches before writing — see the Stale-task guard in
the skill body.
