# `/lore-wiki audit` — five-layer procedure

Surface findings for human review — never auto-resolve. Read AGENTS.md to
resolve section headings before inspecting pages.

1. **Structural** — run `lore graph --json lint`. ONE pass returns ALL of:
   `orphans`, `broken`, `hubs`, `invalid_categories`, `near_duplicate_concepts`,
   `unresolved_conflicts`, and index drift (`missing_from_index`/`_from_disk`).
   Surface every non-empty list. NOTE: a clean lint (`findings: 0`) means no
   structural FAULTS — it does NOT mean the wiki is well-connected. Empty
   `## Related` sections are never a lint finding; only `suggest-links` (layer 2)
   reveals that gap, so never equate "lint clean" with "healthy".
2. **Missing cross-references** — run `lore graph --json suggest-links` (never
   exits non-zero — always inspect the pairs), then confirm topical relatedness
   before proposing a link. Community grounding + LLM confirmation = double gate
   against false positives.
3. **Contradictions** — run `lore graph --json audit-candidates` for the
   worklist: concepts with 2+ sources AND a source set that changed since their last
   audit (the `audited_sources_hash` marker). It exits non-zero when the worklist is
   non-empty — read the JSON `data`, not the exit code (don't wrap it in `set -e`).
   Work it one page at a time
   (avoids combinatorial blow-up). For each, read its cited sources and only flag
   a genuine, unambiguous contradiction (two sources asserting incompatible facts)
   — never a difference in emphasis or a gap; uncertainty means do not flag. When
   one is found, add a callout under the Synthesis section stating both sides and
   citing each source: `> [!conflict] <one-line summary of the disagreement>`.
   Never choose a side. The callout lives in the LLM-owned Synthesis body, so
   ingest re-render preserves it, and `lore graph lint` reports it as an
   unresolved conflict until a human resolves it and deletes the callout.
   AFTER reviewing a candidate — whether or not you flagged a conflict — run
   `lore graph audit-mark <slug>` to record its current source set, so it leaves
   the worklist until its sources change again. This is what keeps the list
   low-noise; skipping the mark makes every multi-source concept resurface.
4. **Frontiers — data gaps + new directions**. Report:
   - Concepts mentioned in daily pages but missing a dedicated wiki page
     (cross-check `[[...]]` wikilinks vs concept files from `lore wiki concepts`).
   - Topics with high cross-source activity (3+ sources in a week) but shallow
     concept coverage (placeholder Synthesis or single source listed).
   - Stale concept syntheses — `updated` recent but Synthesis was written
     long before the recent reference burst.
   This layer is LLM judgment, not a deterministic check. Surface as questions
   for human review, never auto-create pages.
5. **Concept lifecycle** — two parts:
   - **Near-duplicates** from layer 1's `near_duplicate_concepts`: for a true
     variant-spelling pair (`vector-db` ~ `vector-database`), recommend consolidating
     it with `lore graph merge <from> <into>` followed by `lore graph backlinks-sync`
     (the merge rewires every wikilink and deletes the `from` page; it refuses if `from`
     has authored prose unless `--force`, so any authored prose is salvaged into the
     survivor first). Like every audit finding the merge is **surfaced, not run** — it
     deletes a page, so a human makes the call. Leave deliberate model-version siblings
     (`gpt-4`/`gpt-5`) split — they are distinct concepts.
   - **Synonym / abbreviation aliases**: `near_duplicate_concepts` is string-similarity,
     so it cannot see an acronym and its expansion (`rag` ↔ `retrieval-augmented-generation`,
     `k8s` ↔ `kubernetes`) — they share almost no characters. From the `lore wiki concepts`
     registry you already loaded, read down the list ONCE and spot equivalent pairs by
     meaning. Recommend declaring the non-canonical form as an `aliases` entry on the
     canonical concept (`backlinks-sync` then resolves both spellings to one page), or a
     `merge` when one side has no distinct content. Read-only suggestion — a human edits
     frontmatter. This is a registry scan WITHOUT the source text, so an acronym can be
     genuinely ambiguous here (`rag` could be red-amber-green) — surface the candidate for
     human confirmation, don't auto-alias at audit time. (Aliasing AT extraction is the
     same principle, not a contradiction: there the source text disambiguates the term, so
     a concept-creating skill that confidently recognizes the synonym registers the alias
     directly. Confidence comes from context; a context-free registry match doesn't have it.)
   - **Staleness**: run `lore graph stale --days 90`, filter concept entries.
     `stale` already excludes concepts still cited by recent activity (liveness is
     graph-derived), so an entry here is genuinely dormant — no manual recency grep
     needed. For a dormant concept with low `source_count`, suggest
     `status: archived` in frontmatter — do NOT auto-archive.
