# Add sources and migrate imported identity

## Add a file, folder, pasted text or URL

Manual ad-hoc ingest of a file, **folder**, pasted text, or a URL.

A URL goes through `lore fetch <url>`, which reads the page, keeps its readable core, and
writes it into the `manual` source's inbox with the address as `source_url` — so the document
page cites the original rather than a copy nobody can check. Then run `lore ingest <that
source>`, then `/lore-process` to complete queued semantic work and report where
it landed; do NOT also create the page here, or the same article
exists twice. A page with no article body (a listing, or one the browser assembles) is
refused rather than absorbed: say so and ask for the underlying article's URL, or for the
page saved as HTML into the inbox.

When given a folder path, scan it recursively for `.md`, `.txt`, and `.pdf` files, process
each as an independent source, and report the aggregate results.

1. Read AGENTS.md for the document AND concept page formats.
2. If `<source>` is a folder, list all readable files and process each below.
3. For EACH source, create a **document page** (per AGENTS.md): preserve the
   original content, set `document_type` from the format vocabulary AGENTS.md
   states (the source's nature goes in `tags`), and link the
   extracted concepts as relative markdown links in the related-concepts section, in the
   form AGENTS.md § Links defines — resolved against the CITING page's own directory, which is
   what a template written here gets wrong: a literal `concepts/<slug>.md` on a document page
   resolves to `<wiki>/documents/concepts/<slug>.md`, which `graph lint` reports broken and
   whose citation never reaches the concept's `source_count`.
4. Extract durable, independently reusable concepts. A name's appearance alone does not
   justify a page: the source must establish something worth retrieving about it. Keep
   incidental mentions in the document; an empty extraction is valid. Reuse an existing
   broader concept when it faithfully covers the material. Converge each one through the **Concept convergence**
   section of the vault's `AGENTS.md` — `lore resolve` for EVERY form the source writes,
   since a hit on any of them names the page that already owns the concept, the
   `proposed_path` from `lore resolve --json` for absent names (never derive a filename from the name),
   created-this-run set for pages this add minted, `lore wiki search` on two or three
   distinguishing terms of each form for the equivalences no rule about spelling can
   see, then alias registration and the machine-owned sources
   section/`source_count` fields. Fill the Synthesis section with a 1-2 sentence
   definition for a new concept; the document's forward concept link from step 3
   is what `backlinks-sync` counts as the citation. A concept whose evidence later
   moves is rewritten by the queue, not here — `lore graph backlinks-sync` queues
   it once the citation lands.
5. **Finalize**: `lore graph backlinks-sync`, then `lore wiki refresh` (refresh SQLite navigation and the entry guide), then
   `lore graph lint`. A non-zero exit is the vault contradicting itself somewhere, which
   is not the same as this add having caused it: a broken link or an unknown category
   elsewhere in the vault predates you. Resolve every violation naming a page this add
   wrote — especially a `duplicate_concepts` entry, which means a new page claims a name
   another page already answers to — and REPORT the rest rather than fixing them silently.
   Observations (orphans, open conflicts) are reported without gating.
6. Run `lore queue status --json` and report what was created/updated, grouped by
   source file, with any newly queued synthesis still pending. Drain it through
   `/lore-process` when semantic completion is part of the request. Authored pages
   and pending concept rewrites are separate completion claims; unrelated queue
   work does not expand a bounded add into a global drain.

## Imported document identity migration

For a configured nodex source, set a stable `repository_id` and a sufficient
`max_documents`, then run `lore wiki document-identity plan --source ID --manifest PATH`.
Review the manifest and run `check --manifest PATH` before `apply --manifest PATH`.
These are subcommands of `lore wiki document-identity`; apply also supports `--dry-run`.
Only exact production-projected source copies qualify. Changed bodies, missing sources
and ownership conflicts remain blocked; do not edit the manifest to approve them.
`--allow-blocked` explicitly applies the verified subset while retaining blocked pages.
Apply archives original evidence and adds identity without rewriting bodies or asserting
that historical source metadata is current. After publication, reconcile backlinks,
process resulting synthesis tasks, and refresh navigation.
