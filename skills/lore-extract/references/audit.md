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
   said three commits ago. First verify the entry's retained inputs against the
   recorded commit under the manifest's extraction-baseline contract. A bare commit
   field cannot prove that extraction read committed rather than dirty content.
   Without that proof, report freshness as unmeasured and read the source whole.

   For a verified baseline, compare each current source and retained evidence
   dependency byte-for-byte with its captured input; check existence and file kind
   too. Use exact source paths, never glob expansion. Deleted or changed input is
   stale. Git diffs can locate candidates but their silence is not the final check:
   ignored files, filters and index flags can conceal byte changes. An untracked,
   non-Git or otherwise unverified input stays unmeasured, never implicitly current.

2. **Quality** — inspect each document for incomplete sections, unsupported or missing
   concept links, and irrelevant project detail in generalized prose. Scoped searches
   for manifest `strip_patterns` identify review candidates, not leakage findings.
   Preserve source paths, IDs, citations and technology names needed to verify evidence.
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
