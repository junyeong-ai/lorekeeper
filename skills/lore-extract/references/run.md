# Extract into the vault

Contents: source selection; transformation; document and concept publication; finalization.

`lore-extract run <repository and options>`

Extract knowledge using the manifest. Requires a prior scan.

1. Load vault config (root, locale, categories).
2. Load manifest from `<vault>/.lorekeeper/extracts/<project>/`.
   Abort if no manifest — tell user to scan first.
   Follow [source retention](manifest.md#source-retention) when choosing a fixed
   revision or retaining local evidence; do not create a new repository copy per run.
3. Follow the vault AGENTS.md § Concept convergence: `lore resolve` for EVERY form the
   source writes, then `lore wiki search` on two or three distinguishing terms of each.
4. Read `strip_patterns` and `concept_mapping` from manifest.

5. For each discovered source (filtered by `--domain` / `--transferability`):

    a. **Skip only if already extracted and unchanged against that entry’s
      `extracted_at_commit`**, using the per-source content check in the audit reference.
      For an entry containing several source paths, check every path.
      `git_head_at_scan` measures discovery, not extraction freshness: a rescan may
      advance it while the extracted page remains old. With no usable per-entry
      baseline (including non-git or ignored sources), read the source whole;
      `--full` also reads every source. A T4 skip is recorded in
      `extracted` with `vault_page: null` and a `coverage_note`, so the Phase 3 coverage
      check stays complete.

   b. Read the full source document.

   c. Final T-level judgment. Skip T4.

   d. **Transform** source → knowledge document. The document's top-level headings are
      ONLY those AGENTS.md defines for the document page type — read them there (they
      are locale-dependent), never invent new `##` sections. The labels below are `###`
      sub-sections nested under the document's content section; map each source part to
      its sub-section:

      | Material | `###` sub-section under the content section |
      |------------|----------------|
      | Structure and boundaries | → Background & Constraints, as what the project is made of and where data crosses between its parts |
      | What the build refuses | → Key Findings. State the declaration at the observed revision, its effective scope, and whether execution was verified; a declaration alone does not prove enforcement |
      | ADR Context | → Background & Constraints |
      | ADR Decision | → Key Findings |
      | ADR Consequences | → Transferable Patterns |
      | Learning Friction | → Troubleshooting Sequence |
      | Learning `[PROMOTES:]` | → Transferable Patterns |
      | Rule invariants | → Key Findings |
      | Spec problem statement | → Background & Constraints |
      | Guide / CLAUDE.md / rule | → context to Background & Constraints, invariants/rules to Key Findings, reusable design to Transferable Patterns |
      | Module doc comment | → what the module is and where its boundary sits to Background & Constraints; the failure mode it was built against to Troubleshooting Sequence; why the boundary is HERE and not one level in to Transferable Patterns |
      | Enforcement verdict | → Key Findings, as the claim with its exceptions and the reach of the check — never the claim alone |
      | Commit body | → a citation under whichever sub-section it supports, never a section of its own |

   e. **Generalise** using manifest `strip_patterns` — applied to the BODY prose only,
      not to `id`/slug (the page id is `{project}-{slug}`, so the project name necessarily
      survives there). Keep technology names, error messages, API surfaces intact.

   f. Extract concepts. Assign categories via `concept_mapping`.

   g. Write to the document path pattern from AGENTS.md.

      The following is **illustrative only** — read AGENTS.md for the
      authoritative path pattern, frontmatter format, and required fields:

      ```yaml
      # Example (illustrative — see AGENTS.md for canonical format)
      id: {project}-{slug}
      type: document
      title: "{Generalised Title}"
      created: {today}
      updated: {today}
      document_type: note          # a FORMAT value AGENTS.md lists; the knowledge's
                                   # nature (project-knowledge, guide, ADR) goes in tags
      source_url: {repo-remote-url}  # the repo's REMOTE url; OMIT for a local-only repo —
                                     # a local/temp path is unstable provenance, worse than none
      source_file:
        - {path-within-repo}
        - {path-within-repo}
      tags: [{technology}, {domain}]
      ```

      Link the extracted concepts as relative markdown links (AGENTS.md § Links) in the document's
      related-concepts body section (heading per AGENTS.md), NOT in a
      frontmatter `concepts` array. These forward links are what the Finalize
      `backlinks-sync` reads to populate each concept's sources section.

   h. Create/merge concept pages per AGENTS.md, following its
      **Concept convergence** section —
      matching against what step 3 resolved and searched, with the concepts already
      created earlier in this run as the created-this-run set (a batch
      processes many documents, so that set is what closes same-run gaps).
      New concept → fill the Synthesis section, emit the related-concepts
      section empty; concept synthesis derives evidence-backed relations. The sources
      section / `source_count`
      stay machine-owned (AGENTS.md § Concept convergence); the Finalize
      `backlinks-sync` re-derives both from the document's forward
      link (step g), the single source of truth. If no configured
      category fits a transferable concept, omit the `category` field (leave it
      uncategorized) rather than forcing a wrong one.

   i. **Update manifest** `extracted` list with the new entry.

6. Persist updated manifest.
7. **Finalize** — reconcile the graph so machine-owned fields are
   authoritative and health checks pass:
   ```bash
   lore graph backlinks-sync   # re-derive every concept's ## Sources + source_count
   lore wiki refresh           # refresh SQLite navigation and the entry guide
   lore graph lint             # repair violations in pages this run wrote; report unrelated findings separately. Observations never gate
   ```
8. Run `lore queue status --json`; report created/merged/skipped counts and any
   pending concept synthesis. Document extraction is complete, but queued concept
   rewrites require `/lore-process` before reporting semantic completion. Report
   unrelated pending work separately; a bounded extraction does not authorize a
   global queue drain.
