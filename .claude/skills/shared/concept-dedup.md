# Concept dedup — the convergence algorithm

One concept = one page. Every skill that creates or merges concept pages
(`/lore-process`, `/lore-capture`, `/lore-extract`) follows this exact
algorithm so the wiki converges instead of accumulating variants.

## Algorithm

1. **Load the registry** at the start of the run: `lore wiki concepts`.
   This is the on-disk truth at run start — slugs, names, and aliases.

2. **Maintain a created-this-run set.** Every time you mint a new concept
   page OR register a new alias on an existing one, add that slug + name +
   alias(es) to your in-context set BEFORE processing the next item. The
   run-start registry can't see same-run changes, so two items would
   otherwise independently mint `RAG` and `Retrieval-Augmented-Generation`,
   or a second item would miss the `RAG` alias a first item just added —
   only your running set closes both gaps.

3. **Match each extracted concept** against the union (registry +
   created-this-run) by slug-equivalence OR semantic equivalence. On a
   match, reuse the existing slug + name — do NOT create a variant. When
   in doubt, prefer the established broader concept over a narrow variant.

4. **Register surface forms as aliases.** If the source's surface form
   differs from the canonical name (source says `RAG`, canonical is
   `Retrieval-Augmented-Generation`), append the surface form to that
   concept's `aliases` frontmatter so a future bare `[[RAG]]` resolves to
   the one page. A surface form containing `/` (e.g. `async/await`) can NOT
   be linked bare — `[[async/await]]` resolves as a vault *path*, not via the
   alias — so link it piped: `[[async-await|async/await]]` (slug target,
   surface as display). Registering an alias is a metadata-only edit: it never
   renames the canonical page and is not, by itself, a reason to rewrite
   the body. (Whether to enrich the Synthesis section on merge is a
   separate judgment the consuming skill's own spec governs.) This is safe
   and reversible: the deterministic graph resolves aliases (`lk-graph`)
   and `lore graph lint` surfaces any alias that collides with another
   concept or shadows a real page.

5. **Slug normalization** is `lk_core::slugify`, exactly:
   NFKC → lowercase → non-alphanumeric to hyphen → collapse runs → trim.

## Machine-owned fields — never hand-write

A NEW concept page starts with an empty `## Sources` body and
`source_count: 0`; on an EXISTING page leave both exactly as found —
never write, reset, or "fix" them. Record citations as forward
`[[wikilink]]`s on the ORIGIN page (its related-concepts section).
`lore graph backlinks-sync` re-derives every concept's `## Sources` +
`source_count` from those forward links wholesale — an entry not backed
by a forward link is wiped, and a concept cited by several pages in one
batch is counted correctly where hand-written one-ref-per-item entries
would undercount. Run `backlinks-sync` (then `lore wiki index`) in the
skill's Finalize step.
