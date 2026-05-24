---
id: proposal-0002-karpathy-alignment
title: "Karpathy Alignment: Generated Schema, Compounding Query, Semantic Audit"
status: proposed
created: 2026-05-24
author: junyeong
---

# Proposal 0002 — Karpathy Alignment

## Summary

Bring the system into full fidelity with Karpathy's LLM Wiki pattern by
establishing a **single generated source of truth** for every page format and
every localized label, then filling the two semantic capabilities the pattern
requires but our skills lack.

1. **Generated schema (keystone)** — `lore schema` emits `<vault>/wiki/AGENTS.md`
   from the Rust `i18n` tables. It is the one document that defines page
   formats and section vocabulary for the active locale. Templates,
   `/lore-process`, and `/wiki` all bind to it; none restate a format or a
   heading.
2. **Locale-proof handoff** — every queue task carries the resolved section
   anchor the Rust pipeline actually wrote, so the semantic skills never
   hardcode a heading and never break when `vault.locale` changes.
3. **Compounding query** — `/wiki query` files reusable answers into
   `wiki/explorations/`. This is the one behavior that separates a wiki from
   stateless RAG.
4. **Semantic audit** — `/wiki audit` adds contradiction surfacing and
   cluster-grounded missing-link suggestions on top of `wikigraph`'s
   structural lint.

## Principle: one source of truth per concern

| Concern | Single source | Consumers bind by |
|---|---|---|
| Localized labels & headings | `lk-core::i18n` (Rust) | generated `wiki/AGENTS.md` + queue `target.anchor` |
| Page formats (frontmatter, sections) | `wiki/AGENTS.md` (generated) | reference, never restate |
| Rust↔skill task contract | queue JSONL (`target.kind` + `target.anchor`) | verbatim use |
| Source IDs ↔ daily directories | `config.yaml` `sources.*` keys | derive, never hardcode |

Anything currently restating one of these is a drift site and is rewritten to
bind instead.

## Defects this resolves

**D1 — Page format restated in three places.** The concept schema lives in
`templates/concept.md.jinja`, again as prose in `/lore-process`, and a third,
incompatible shape in `/wiki` (fewer frontmatter keys, English headings). Three
writers cannot stay consistent; the concept pages already differ between
`anthropic` mode (jinja) and `queue` mode (skill prose).

**D2 — Localized headings hardcoded in the semantic plane.** `lk-core::i18n`
is fully locale-driven and templates emit `{{ i18n.* }}` correctly, but
`/lore-process` carries a hardcoded Korean anchor table and `/wiki` assumes
Korean. Set `vault.locale: "en"` and the templates emit English headings while
the skills still search for Korean ones — the section lookup fails. The locale
contract is honored on one side of the handoff only.

**D3 — Stale directory names.** `/wiki` references `daily/ai-briefing/`,
`daily/team-digest/`, `weekly/ai-newsletter/`; the real source IDs (which *are*
the directory names) are `ai-news`, `team-slack`, `slack-trends`, `gmail`.

**D4 — Query does not compound; semantic lint absent.** `/wiki query`
synthesizes and stops. No actor detects contradictions or suggests missing
cross-references. The semantic half of the `lint` operation does not exist.

## Architecture

```
Deterministic plane (Rust, no LLM)            Semantic plane (Claude Code, LLM)
──────────────────────────────────            ─────────────────────────────────
lk-core::i18n  ── single label source         /lore-process → drain queue:
lore schema    → generate wiki/AGENTS.md                       summarize + extract,
lore ingest    → render structural sections                    fill ## 핵심 / ## 관련,
               → queue tasks (+ resolved anchor)             edit task.target.anchor
wikigraph      → structural lint               /wiki add   → manual ad-hoc ingest
               → suggest-links                 /wiki query → answer + compound
                                               /wiki audit → semantic lint
                                               /wiki status
```

The deterministic/semantic boundary from Proposal 0001 is unchanged. i18n stays
wholly in the Rust plane; the skills receive locale through generated artifacts
and task fields, never by embedding language strings.

### Change 1 — `lore schema` generates `wiki/AGENTS.md`

A new read-only subcommand renders the schema document from `config.locale()`
and the `i18n::Strings` table:

```
lore schema [--root <vault>]   → writes <vault>/wiki/AGENTS.md
```

`AGENTS.md` declares, for the active locale: the page types, their frontmatter
key sets, and their section headings (taken verbatim from `i18n`). Because it
is generated, it cannot drift from the templates — both derive from the same
`Strings`. `lore init` calls `lore schema`; `lore validate` fails if the on-disk
`AGENTS.md` differs from a fresh render (drift gate).

Canonical concept page (the keystone format AGENTS.md describes; headings shown
for `ko`, substituted per locale):

```markdown
---
id: {slug}
title: "{name}"
aliases: ["{name}"]
created: {first_seen}      # YYYY-MM-DD
updated: {last_seen}       # YYYY-MM-DD
confidence: extracted      # extracted | inferred | ambiguous
reference_count: {n}
sources: ["daily/ai-news/2026-05-23", ...]
tags: [...]
---

# {name}

## {i18n.concept_synthesis}     # ko: 핵심 — LLM-owned cross-source synthesis
{2-4 sentence synthesis. The compounding value.}

## {i18n.sources}               # ko: 출처 — machine-written backlinks
- [[daily/ai-news/2026-05-23]]

## {i18n.meta}                  # ko: 메타 — machine-written
- ...

## {i18n.related}               # ko: 관련 — LLM-owned related concepts
- [[concepts/related-1]]
```

Section ownership, stated once in AGENTS.md: **machine sections
(`sources`, `meta`) are render-owned and idempotently rewritten; semantic
sections (`concept_synthesis`, `related`) are LLM-owned and merge-updated.**
This removes the double-write ambiguity between `anthropic` and `queue` modes —
both write identical structural sections via the same template, and the LLM
fills the same two semantic sections in either mode.

New `i18n::Strings` fields required: `concept_synthesis`, `sources`, `meta`,
`related` (the concept template currently hardcodes `## 출처` / `## 메타`;
those move into `Strings` so the template uses `{{ i18n.* }}` like every other
template).

### Change 2 — Queue tasks carry the resolved anchor

The pipeline knows the locale and the exact heading it wrote. It puts that
heading in the task:

```jsonc
{
  "kind": "summarize",
  "created_at": "2026-05-24T07:00:00Z",
  "input": { "text": "...", "max_sentences": 5 },
  "target": {
    "vault_path": "weekly/synthesis/2026-W21.md",
    "kind": "weekly-synthesis-narrative",
    "anchor": "## Key Themes This Week"   // resolved from i18n at queue time
  }
}
```

`TaskTarget` gains an `anchor: String` field, populated in the Rust call sites
that already build `TaskTarget` (`lk-pipeline/src/lib.rs`, `synthesis.rs`).
`/lore-process` locates the section by `target.anchor` verbatim and deletes its
hardcoded `target.kind → heading` table entirely. `target.kind` remains for
classification/logging; `target.anchor` is the locate key.

### Change 3 — `/wiki` skill (rewritten clean)

Command set:

| Command | Role | Karpathy op |
|---|---|---|
| `/wiki add <source>` | manual ad-hoc ingest of a URL/file/text | ingest |
| `/wiki query <q>` | answer + compound reusable answers | query |
| `/wiki audit` | semantic lint (Change 5) | lint (semantic) |
| `/wiki status` | stats | — |

The skill reads `wiki/AGENTS.md` for every format and heading. It does not
contain a frontmatter spec, a section list, a language assumption, or a
directory name — those come from AGENTS.md (formats/headings) and `config.yaml`
(source IDs). `/wiki add` extracts every named entity, technology, and topic as
a concept (typically several per source), not a lone summary.

### Change 4 — Compounding query

`/wiki query` ends with an LLM-judged file-back step:

```
After answering, judge reusability:
- reusable (synthesis / comparison / multi-source analysis)
    → write wiki/explorations/{slug}.md, wikilink cited concepts & sources,
      update index + log; tell the user where it landed.
- ephemeral (single-fact / navigational lookup)
    → do not file.
```

The judgment is per-answer and made by the model — there is no frequency rule
and no file-everything default, so `explorations/` accumulates only durable
analysis.

### Change 5 — `/wiki audit` and `wikigraph suggest-links`

`/wiki audit` runs three checks, each **surfacing for human review, never
auto-resolving**, each **grounded** rather than heuristic:

1. **Structural** — delegates to `wikigraph lint` (orphans, broken links,
   index drift). Deterministic.
2. **Missing cross-references** — `wikigraph suggest-links` (new, read-only)
   emits pairs in the same Louvain community with no edge between them, ranked
   by shared-neighbor count; the LLM then confirms topical relatedness before
   proposing a link. Community grounding plus LLM confirmation is the
   double gate against false positives. Output is a suggestion list, never an
   edit.
3. **Contradictions** — scoped to a single concept page whose `sources` cite
   conflicting claims. The LLM reads the cited sources, flags genuine conflicts
   with `confidence: ambiguous` and a review note, and never chooses a side.
   One page at a time, so no cross-page combinatorial blow-up.

`wikigraph suggest-links`:

```
wikigraph --root <vault> suggest-links [--min-community-size N] [--json]
→ { pairs: [{a, b, shared_neighbors}] }   # same community, no edge
```

Pure graph, deterministic, reuses existing community assignments. Verb style
matches `build`/`hubs`/`cluster`/`export`.

Deliberately excluded as flimsy: body-frequency "data gap" detection, and any
automated contradiction resolution.

## Migration

1. `lk-core::i18n` — add `concept_synthesis`, `sources`, `meta`, `related` to
   `Strings` (both `KO` and `EN`).
2. `templates/concept.md.jinja` — replace hardcoded `## 출처` / `## 메타` with
   `{{ i18n.* }}`; add empty `{{ i18n.concept_synthesis }}` and
   `{{ i18n.related }}` sections.
3. `TaskTarget` — add `anchor: String`; populate at every construction site
   from the locale-resolved heading.
4. `lore schema` subcommand — render `wiki/AGENTS.md`; wire into `lore init` and add
   the drift gate to `lore validate`.
5. `/lore-process` SKILL.md — delete the hardcoded anchor table; locate by
   `target.anchor`; fill the two semantic concept sections; reference
   `wiki/AGENTS.md` for concept format.
6. `/wiki` SKILL.md — rewrite to four commands binding to AGENTS.md and
   `config.yaml`; add compounding `query`; add `audit`.
7. `wikigraph` — implement `suggest-links` in `cluster.rs` with `--json` and
   tests.

The vault holds only a handful of pages today, so existing concept pages are
brought to the canonical shape by one `lore schema` + a single `/wiki audit`
pass; no migration framework.

## Verification

```bash
# Single source: changing locale flips headings everywhere, no skill edits
# config.yaml: vault.locale: en
lore schema && grep -q "## Sources" "$VAULT/wiki/AGENTS.md"      # generated EN
lore ingest --provider queue ai-news
jq -r '.target.anchor' "$VAULT/.lorekeeper/queue/"*.jsonl     # English anchors
# /lore-process drains using task.target.anchor — no Korean assumption

# Format parity across modes
lore ingest --provider anthropic ai-news    # concept via jinja
lore ingest --provider queue ai-news && /lore-process   # concept via skill
# diff the two concept pages' frontmatter key sets → identical

# Drift gate
echo "tampered" >> "$VAULT/wiki/AGENTS.md" && lore validate    # → fails

# Compounding
# /wiki query "compare X and Y across sources"   → explorations/ gains a page
# /wiki query "what's today's date"              → nothing filed

# Audit surfaces, never mutates
wikigraph --root "$VAULT" suggest-links --json | jq '.data.pairs'
# /wiki audit   → checklist only; edits require approval

cd ~/workspace/wikigraph && cargo test && cargo clippy --all-targets -- -D warnings
```

## Decision Required

- [ ] `lk-core::i18n` as the single label/heading source; `lore schema` generates `AGENTS.md`
- [ ] `TaskTarget.anchor` resolved at queue time; `/lore-process` drops its heading table
- [ ] `/wiki` four-command set, binding to AGENTS.md + config
- [ ] Compounding `query` (LLM-judged file-back)
- [ ] `/wiki audit` + `wikigraph suggest-links`, surface-only

## Non-Goals

- Moving LLM work into the Rust plane (Proposal 0001 boundary holds).
- Body-frequency data-gap detection; automated contradiction resolution.
- A concept-page migration framework (vault is small; one regenerate pass).
