# `/lore-wiki audit` — five-layer procedure

## Contents

The numbered layers below cover structure, missing citations/relations,
contradictions, evidence gaps and concept identity. Report findings and proposed
repairs; a clean structural pass is not a semantic completeness verdict.

Surface findings and proposed repairs for human review — never auto-resolve or
edit pages during the audit. Execute separately authorized repairs after reporting.
Read AGENTS.md to
resolve section headings before inspecting pages.

1. **Structural** — run `lore graph --json lint`. ONE pass returns ALL of it, in two
   channels: `violations` (`broken`, `invalid_categories`, `duplicate_concepts`,
   `address_collisions` — distinct addresses that would collide under slug normalization — `unnormalized` — a filename that is not its own slug — and index
   drift under `index` when automatic Markdown exports are enabled) and `observations` (`orphans`,
   `hubs`, `unresolved_conflicts`). Surface every non-empty list from both — and read
   `violations.index` as the OBJECT it is: `missing_from_index`/`missing_from_disk` name which
   pages differ, while `stale` is the verdict and can be true with BOTH lists empty (a catalog
   holding every page while stating a title or a summary no page states, which `lore wiki refresh`
   repairs). An agent that surfaces only the lists misses a finding that gates. With default SQLite navigation, `violations.index` is null: optional exports do not promise live catalog currency.
   The split says which ones are FALSE (a violation names its own repair) versus true of a healthy vault
   (an observation), not which ones to read. NOTE: an empty `violations` means no structural
   faults — it does NOT mean the wiki is well-connected. Empty related-concepts sections are
   never a lint finding; only `suggest-links` (layer 2) reveals that gap, so never equate
   "no violations" with "healthy".
2. **Missing cross-references** — two kinds, and they are found differently.

   Between concepts: run `lore graph --json suggest-links`. Inspect the pairs;
   candidates do not gate, but runtime failures must be reported. Clusters generate candidates; they do not establish a
   relation. Read the sources and propose a link only when its explanation states a
   useful, supported relationship, such as a component, prerequisite, alternative,
   or limitation. Sharing a topic or appearing in the same article is insufficient.

   From a source page to a concept — a citation the page owes and does not carry.
   A concept's synthesis is written from its citations and nothing else, so every
   missing citation is evidence the concept's page cannot see, and the next rewrite
   drops whatever only that page supported. Nothing derives these: a page mentioning
   a concept's name is not a citation (thousands of pages mention `Anthropic` in
   passing), so the graph cannot add them and you may not add them from a text match.
   Two signals identify candidates for evidence review:
   - **A page that declares a candidate concept and does not cite it.** A promotion
     marker (`[PROMOTES: <id>]`) or a pattern id in frontmatter identifies an authored
     association. Read the full source context and the resolved concept's synthesis:
     distinguish a named subject from a proposed rule, preserve unestablished conditions,
     and confirm that the evidence belongs to that concept's scope. Only then propose
     a missing citation as an extraction repair. A marker alone establishes neither
     a new concept nor the truth or generality of the proposed pattern.
   - **A material evidence gap recorded during synthesis.** Read open evidence-gap
     reviews and processing findings; older concepts may also carry unsupported-claim
     notes. Look for the page that supports the claim across the WHOLE vault,
     daily pages included: `lore wiki
     search --include-sources` reaches daily observations too; search claim terms and
     search every spelling the concept answers to (an English name inside a Korean
     page is the common miss). A page found is a candidate missing citation: confirm it supports the claim, then
     propose the forward link and an alias where the spelling is equivalent. Nothing found
     means the claim remains unsupported by the pages checked. Keep a material
     unresolved gap in its review; do not reintroduce routine removal history into
     the concept's synthesis.
   Report every proposed citation: a link is a claim of evidence. After authorized
   repairs, run `lore graph backlinks-sync` to queue affected concept rewrites.
   For a mistaken existing citation, compare the complete source context with the
   concept's identity before proposing retraction. Within authorized repair work,
   use `lore graph retract-citation <origin.md> <concept.md> --revision <file_revision>
   --dry-run`, then omit `--dry-run` after reviewing the preview. It archives the
   prior evidence and refuses prose links or touching pending work. Process the
   affected synthesis through `/lore-process` and its full Finalize sequence.
   A rejected alternative can still be useful evidence; disagreement alone is not
   a reason to remove a citation.
3. **Contradictions** — run `lore wiki reviews` first. Review open records and records
   with changed or unreadable evidence; retain resolved records and their reasons. Search
   `lore wiki search "<claim terms>" --include-sources --json` to compare direct evidence,
   including claims never promoted to concepts. Compare subject, scope, time and attribution
   before calling two statements contradictory. Propose exploration reviews for material findings
   under AGENTS.md's Knowledge reviews contract. Then read layer 1's `observations.unresolved_conflicts`: the
   concepts whose Synthesis carries an open `> [!conflict]` callout. These are additional
   candidates, not a complete inventory: `lore graph backlinks-sync` queues a synthesis rewrite
   whenever a concept's source paths or authored revisions change, and the drain that rewrites the
   section is the reader of every source, so it is where a disagreement between
   two of them is seen and stated. What you do here is judge the OPEN ones: read
   each callout with its cited sources and report whether the disagreement still
   stands, has been settled by later evidence, or was never a contradiction at
   all (a difference in emphasis, a gap). Resolve only when direct evidence supports
   the stated scope; never prefer a side merely because its wording or date differs.
   Follow AGENTS.md's reassessment protocol to preserve the prior review before explicitly
   updating the current evidence baseline.

   Another outcome is that the disagreement cannot be settled by the available evidence:
   both sides are REPORTS of
   an external fact — a model's parameter count, a funding round's stage — and the vault
   holds no primary source for it. Record the evidence limit and what would resolve it;
   absence of decisive evidence does not resolve the review. Reuse that assessment while
   its evidence remains unchanged, and revisit it when relevant new evidence arrives.

   A callout is NOT durable, and treating it as a permanent record is the one
   mistake to avoid here. It lives in `## Synthesis`, which is rewritten from the
   sources whenever source paths or authored revisions change, so it survives only by being written
   again — a statement about the last rewrite, not a ledger entry. What that means
   for you: a disagreement worth keeping belongs somewhere the machine does not
   own. Report it and propose an exploration whose Grounding cites both sides.
   Draft the scope and evidence in the audit report; publish the review only as
   separately authorized repair work.
4. **Frontiers — data gaps + new directions**. Report:
   - Durable subjects established by daily pages that have no suitable concept page.
     Check the registry and existing broader pages; incidental mentions alone do not
     justify a new concept.
   - Topics whose sources add distinct claims or decisions that the concept does not
     cover. Repeated mentions or a high citation count alone do not establish a gap.
   - Concepts whose Synthesis is thin against the evidence behind it — several
     citations, a placeholder sentence. Staleness itself is not this layer's:
     `lore graph backlinks-sync` already queues a rewrite for every concept whose
     source paths or authored revisions have changed, so what is left here is judging whether the writing
     is worth the evidence.
   This layer is LLM judgment, not a deterministic check. Surface as questions
   for human review, never auto-create pages.
5. **Concept convergence** — two parts:
   Ask `lore resolve` for EVERY form actually written in the source before proposing changes.
   The two halves split on what is DECIDABLE. Layer 1 owns spelling; you own meaning.
   - **Duplicates** from layer 1's `violations.duplicate_concepts`: two pages answering to one
     declared name. NFC and whitespace normalize; case, compatibility characters, punctuation
     and word boundaries remain significant. Shared names can mean duplicate concepts or distinct senses;
     inspect the sources before recommending a merge. For established equivalence, recommend
     `lore graph merge <from> <into>`
     followed by `lore graph backlinks-sync` (the merge rewires every link and deletes
     the `from` page; it refuses if `from` has authored prose unless `--force`, so
     salvage that prose into the survivor first). Like every audit finding the merge is
     **surfaced, not run** — it deletes a page, so a human makes the call.
     When the two pages turn out to be genuinely DIFFERENT things that merely share a
     name, the fix is the opposite one and it matters more than it looks: **disambiguate
     the name itself** — rename one page (`Go (programming language)` beside `Go (board
     game)`), or drop the alias that reaches across. Nothing downstream can tell two
     things apart while they answer to one name: ingest refuses the ambiguous extraction
     until the declared vocabulary distinguishes the senses. Giving
     them distinct names is what makes every later citation land correctly, and it is why
     an encyclopedia disambiguates rather than tolerating the collision.
   - **Equivalent names missing from aliases**: a title such as `Term (풀이)` may
     contain another name for the same concept. Read the definition and sources before
     treating either part as an alias; punctuation does not establish equivalence.
     Confirm the same meaning and scope, then run `lore resolve` for each candidate.
     Recommend an alias only when it names this concept and does not claim a sibling's
     name. Preserve the stable path. A disambiguating qualifier such as
     `Go (programming language)` does not justify registering the ambiguous bare term.
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
backlinks-sync` detects and queues, keyed on source paths and authored revisions — or a defect in its
structure (layer 1).
