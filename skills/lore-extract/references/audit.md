# Audit coverage and freshness

`/lore-extract audit [project-name]`

Verify extraction quality and coverage.

Start with `lore extract status --json`, which answers the deterministic half for
every project at once: when each manifest was taken, how far the repository has
moved under the paths that scan declared, and how many sources reached a page.
Re-deriving that from `git log` here would be a second implementation of a
question the binary already answers, and the two would disagree the first time
either changed. Read `unmeasured` beside `behind`: a project git could put no
question to — not a repository, or a manifest naming no source — is neither
behind nor current, and reporting it as current is coverage nobody measured.

What is left is what needs a reader:

1. **Per-source staleness** — `lore extract status` answers whether the PROJECT has moved
   since its last scan, which is a different question: a re-scan alone advances
   `git_head_at_scan`, so a project reads current while a page still holds what its source
   said three commits ago. Ask it per entry, with the same anchor and the same content
   question: `git -C <manifest.project.repo_path> diff --name-only <extracted_at_commit> -- ':(literal)<source>'`, plus `git -C <manifest.project.repo_path> ls-files --others --exclude-standard -- ':(literal)<source>'`. `:(literal)` and not `:(glob)` — an `extracted[].source` is
   a concrete path rather than a pattern, and under glob magic a name containing `[`, `*` or
   `?` is read as one: `docs/a[1].md` then answers for `docs/a1.md` and not for the file the
   entry names. Non-empty means the page is behind its own source. An entry
   written before this field existed carries none, and there the question has no anchor: report
   it as un-checked beside the entries that answered, never as current — the next extraction
   run records the field and the check starts working from there. An entry whose
   `coverage_note` says the source is outside git is the same answer for the same reason: both
   commands are silent over a file git ignores, and reading that silence as "current" is what
   the note exists to prevent.

2. **Quality** — per-document: empty sections, missing concept links,
   leaked project identifiers (grep for `strip_patterns` in vault).
3. **Cross-project** — concepts appearing in multiple project
   manifests. Suggest synthesis enrichment.
4. **Unharvested sources** — which declared sources have no `extracted`
   entry, and whether each is worth harvesting. The status command counts
   both sides; deciding that a T1 source was rightly skipped is judgment,
   and a `coverage_note` is where that judgment is recorded so the next
   audit does not re-ask it.

A project the status command reports as moved is re-scanned (`/lore-extract scan`)
before it is re-run: the manifest is what `run` reads, so running against a stale
one harvests the source list as it was.
