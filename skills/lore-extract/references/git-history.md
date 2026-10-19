# Optional Git history mining

Read BODIES rather than subjects — the subject names the change and the body carries the
reasoning:

```bash
git -C <manifest.project.repo_path> log --no-merges --format='%x00%H%x09%ad%x09%s%x01%b' --date=short
```

Read it in windows and extract THEMES. A revert and the commit it reverted are one piece of
knowledge; five commits converging on one shape are another. Never one page per commit — a
commit is a change, and a change is not a unit of knowledge.

The history's whole scope is what the facts cannot carry: an approach tried and abandoned, a
measurement that settled an argument, a reversal and its reason. An invariant that still
holds is read from the code that holds it and checked there; taking it from the commit that
introduced it yields a weaker statement of something already covered, and risks stating a
shape later commits replaced.

It is a separate switch because its cost is a choice rather than a fact about the repository:
under a record-depth discipline the commit bodies rival every doc comment in the project put
together. Body length does not narrow them either — where the discipline holds, nearly every
commit carries a real body, so "has a body" identifies nothing.

Cite as `source_file: ["git:{sha}", …]`, several shas per page, and record the extraction in
`extracted` like any other. Do NOT add the history to `discovered_sources`: staleness there is
measured per declared path, and a source that every commit moves makes the `lore status` row
permanently red — an alarm nobody can clear is one everybody learns to skip. The same holds
for the build manifest and the gate configuration: the facts pass re-reads those on every
scan, so tracking them buys nothing and costs the row.
