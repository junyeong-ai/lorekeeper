# `/lore-wiki audit` — five-layer procedure

Surface findings for human review — never auto-resolve. Read AGENTS.md to
resolve section headings before inspecting pages.

1. **Structural** — run `lore graph --json lint`. ONE pass returns ALL of:
   `orphans`, `broken`, `hubs`, `invalid_categories`, `duplicate_concepts`,
   `unresolved_conflicts`, and index drift
   (`missing_from_index`/`_from_disk`).
   Surface every non-empty list. NOTE: a clean lint (`findings: 0`) means no
   structural FAULTS — it does NOT mean the wiki is well-connected. Empty
   related-concepts sections are never a lint finding; only `suggest-links` (layer 2)
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
     (cross-check body links vs concept files from `lore wiki concepts`).
   - Topics with high cross-source activity (3+ sources in a week) but shallow
     concept coverage (placeholder Synthesis or single source listed).
   - Stale concept syntheses — `updated` recent but Synthesis was written
     long before the recent reference burst.
   This layer is LLM judgment, not a deterministic check. Surface as questions
   for human review, never auto-create pages.
5. **Concept convergence** — two parts:
   The two halves split on what is DECIDABLE. Layer 1 owns spelling; you own meaning.
   - **Duplicates** from layer 1's `duplicate_concepts`: two pages answering to one
     name (`vector-db` ~ `vectordb`, or an alias on one page that is another page's
     title). Layer 1 folds only what carries no identity — case, punctuation, and a break
     between letters — so this is a fact, not a candidate: one name reaches two pages, so
     citations fragment by spelling, and every entry needs resolving. Recommend
     `lore graph merge <from> <into>`
     followed by `lore graph backlinks-sync` (the merge rewires every link and deletes
     the `from` page; it refuses if `from` has authored prose unless `--force`, so
     salvage that prose into the survivor first). Like every audit finding the merge is
     **surfaced, not run** — it deletes a page, so a human makes the call.
     When the two pages turn out to be genuinely DIFFERENT things that merely share a
     name, the fix is the opposite one and it matters more than it looks: **disambiguate
     the name itself** — rename one page (`Go (programming language)` beside `Go (board
     game)`), or drop the alias that reaches across. Nothing downstream can tell two
     things apart while they answer to one name: ingest routes an extraction to whichever
     page owns it, and the loser silently accumulates none of its own mentions. Giving
     them distinct names is what makes every later citation land correctly, and it is why
     an encyclopedia disambiguates rather than tolerating the collision.
   - **Synonyms, abbreviations, plurals, and short forms**: layer 1 compares names, so by
     construction it cannot see two DIFFERENT names for one thing — an acronym and its
     expansion (`rag` ↔ `retrieval-augmented-generation`, `k8s` ↔ `kubernetes`), a plural
     (`doc-hub` ↔ `docs-hub`), or a team's shorthand and the full id (`enterprise-prd` ↔
     `oy-gemini-enterprise-prd`).
     That judgment is yours and it is where this layer earns its keep. From the
     `lore wiki concepts` registry you already loaded, read down the list ONCE and spot
     equivalent pairs by meaning; check the cited daily pages when a short form might
     just be how the team writes the long one. Recommend declaring the non-canonical
     form as an `aliases` entry on the canonical concept (the registry returns aliases,
     so future extractions converge on the one page instead of minting a variant), or a
     `merge` when one side has no distinct content. Leave deliberate siblings split —
     model versions (`gpt-4`/`gpt-5`), namespace members (`agentcore-memory` ~
     `agentcore-identity`), and a qualifier that narrows a concept (`agent-harness` ~
     `code-as-agent-harness`) are distinct concepts. Read-only suggestion — a human edits
     frontmatter. This is a registry scan WITHOUT the source text, so an acronym can be
     genuinely ambiguous here (`rag` could be red-amber-green) — surface the candidate for
     human confirmation, don't auto-alias at audit time. (Aliasing AT extraction is the
     same principle, not a contradiction: there the source text disambiguates the term, so
     a concept-creating skill that confidently recognizes the synonym registers the alias
     directly. Confidence comes from context; a context-free registry match doesn't have it.)

A page's age or citation recency is never an audit signal: reference knowledge does
not expire by going unmentioned, so "old and uncited" identifies nothing actionable.
What makes a concept due for review is a CHANGE in its evidence (layer 3's source-set
hash) or a defect in its structure (layer 1) — both already covered above.
