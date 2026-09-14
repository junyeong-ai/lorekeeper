# `/lore-wiki audit` — five-layer procedure

Surface findings for human review — never auto-resolve. Read AGENTS.md to
resolve section headings before inspecting pages.

1. **Structural** — run `lore graph --json lint`. ONE pass returns ALL of it, in two
   channels: `violations` (`broken`, `invalid_categories`, `duplicate_concepts`,
   `address_collisions` — two files whose paths slugify to one page id, so one of them silently
   loses its node — `unnormalized` — a filename that is not its own slug — and index
   drift under `index`) and `observations` (`orphans`,
   `hubs`, `unresolved_conflicts`). Surface every non-empty list from both — and read
   `violations.index` as the OBJECT it is: `missing_from_index`/`missing_from_disk` name which
   pages differ, while `stale` is the verdict and can be true with BOTH lists empty (a catalog
   holding every page while stating a title or a summary no page states, which `lore wiki refresh`
   repairs). An agent that surfaces only the lists misses a finding that gates. The split says
   which ones are FALSE (a violation names its own repair) versus true of a healthy vault
   (an observation), not which ones to read. NOTE: an empty `violations` means no structural
   faults — it does NOT mean the wiki is well-connected. Empty related-concepts sections are
   never a lint finding; only `suggest-links` (layer 2) reveals that gap, so never equate
   "no violations" with "healthy".
2. **Missing cross-references** — two kinds, and they are found differently.

   Between concepts: run `lore graph --json suggest-links` (never exits non-zero —
   always inspect the pairs), then confirm topical relatedness before proposing a
   link. Community grounding + LLM confirmation = double gate against false positives.

   From a source page to a concept — a citation the page owes and does not carry.
   A concept's synthesis is written from its citations and nothing else, so every
   missing citation is evidence the concept's page cannot see, and the next rewrite
   drops whatever only that page supported. Nothing derives these: a page mentioning
   a concept's name is not a citation (thousands of pages mention `Anthropic` in
   passing), so the graph cannot add them and you may not add them from a text match.
   Two signals are exact enough to act on:
   - **A page that DECLARES a concept and does not cite it.** A promotion marker
     (`[PROMOTES: <id>]`) or a pattern id in a document's own frontmatter is the
     source asserting that this document establishes that concept. Where the id
     resolves (`lore resolve <id>`) and the page's related-concepts section lacks
     the link, add it — that is a repair of the extraction, not a judgment.
   - **A claim a rewrite dropped.** The drain leaves a `> [!note] 현재 인용 N건에서
     확인되지 않은 기존 주장` on a concept page for each substantive claim the
     current citations did not support. Read the claim and look for the page that
     does support it — across the WHOLE vault, daily pages included: `lore wiki
     search` reads only the wiki, so use `grep -rl` over every page directory, and
     search every spelling the concept answers to (an English name inside a Korean
     page is the common miss). A page found is a missing citation: add the link on
     that page and register the spelling as an alias if it was one. Nothing found
     means the drop stands; leave the note, since the next rewrite re-derives it.
   Report every citation you add: a link is a claim of evidence, and `lore graph
   backlinks-sync` will queue the concept for a rewrite on the strength of it.
3. **Contradictions** — read layer 1's `observations.unresolved_conflicts`: the
   concepts whose Synthesis carries an open `> [!conflict]` callout. Flagging is
   not this skill's job — `lore graph backlinks-sync` queues a synthesis rewrite
   whenever a concept's citation set moves, and the drain that rewrites the
   section is the reader of every source, so it is where a disagreement between
   two of them is seen and stated. What you do here is judge the OPEN ones: read
   each callout with its cited sources and report whether the disagreement still
   stands, has been settled by later evidence, or was never a contradiction at
   all (a difference in emphasis, a gap). Never choose a side.

   There is a fourth answer and it is the commonest one: the disagreement stands and
   nothing this vault will ever observe can settle it, because both sides are REPORTS of
   an external fact — a model's parameter count, a funding round's stage — and the vault
   holds no primary source for it. That is a permanent, correct state rather than an open
   question, and the callout is answered once it says so. Report those separately from the
   ones a later observation could close; asking every audit to re-derive the split is what
   makes this layer read the same 24 findings forever.

   A callout is NOT durable, and treating it as a permanent record is the one
   mistake to avoid here. It lives in `## Synthesis`, which is rewritten from the
   sources whenever the citation set moves, so it survives only by being written
   again — a statement about the last rewrite, not a ledger entry. What that means
   for you: a disagreement worth keeping belongs somewhere the machine does not
   own. Report it, and if it matters beyond this page, file it as an exploration
   page whose Grounding cites both sides.
4. **Frontiers — data gaps + new directions**. Report:
   - Concepts mentioned in daily pages but missing a dedicated wiki page
     (cross-check body links vs concept files from `lore wiki concepts`).
   - Topics with high cross-source activity (3+ sources in a week) but shallow
     concept coverage (placeholder Synthesis or single source listed).
   - Concepts whose Synthesis is thin against the evidence behind it — several
     citations, a placeholder sentence. Staleness itself is not this layer's:
     `lore graph backlinks-sync` already queues a rewrite for every concept whose
     citation set has moved, so what is left here is judging whether the writing
     is worth the evidence.
   This layer is LLM judgment, not a deterministic check. Surface as questions
   for human review, never auto-create pages.
5. **Concept convergence** — two parts:
   The two halves split on what is DECIDABLE. Layer 1 owns spelling; you own meaning.
   - **Duplicates** from layer 1's `violations.duplicate_concepts`: two pages answering to one
     name (`vector-db` ~ `vectordb`, or an alias on one page that is another page's
     title). Layer 1 folds only typography — case, punctuation, and every break except
     one between two numerals (so `claude-35` ~ `claude35` IS a finding, `claude-3-5` ~
     `claude-35` is not) — so this is a fact, not a candidate: one name reaches two pages, so
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
   - **A title holding a name the page does not answer to**: a title of the form
     `Term (풀이)` is two names written as one, and `lore resolve` answers for neither —
     so the page is unreachable by anything a person or a later extraction would type, and
     the next source naming the bare term mints a rival page beside it. Run `lore resolve`
     on both halves to confirm before reporting; a compound title whose halves already
     resolve here is fine.
     The repair is `aliases`, not a rename: the slug is what existing citations address, and
     adding names only makes more of them land. Which half is the TITLE is a separate
     judgment and usually not worth making — the exception is a parenthetical that
     disambiguates rather than glosses (`Go (programming language)`), where the bare term
     must NOT become an alias, because it belongs to whichever sibling page claims it next.
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
What makes a concept due for review is a CHANGE in its evidence — which `lore graph
backlinks-sync` detects and queues, keyed on the citation set — or a defect in its
structure (layer 1).
