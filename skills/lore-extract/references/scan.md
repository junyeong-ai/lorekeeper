# Scan repository knowledge

Contents: baseline and discovery; implementation evidence; source claims; manifest publication.

`/lore-extract scan <repo-path>` discovers sources and persists a manifest.
Read the manifest reference from SKILL.md for its schema and address.

## Baseline and discovery

1. Detect Git with `git -C <path> rev-parse --short HEAD`. Record the selected
   revision as `git_head_at_scan`, or null for a non-Git project. Without a usable
   Git baseline, freshness is **not measured**; mtimes are not content evidence.
2. Read any existing manifest. Preserve user overrides, `strip_patterns` and
   `concept_mapping`. Compare discovery against its `git_head_at_scan`:

   ```bash
   git -C <path> diff --name-only <baseline> -- ':(glob)<pattern>'
   git -C <path> ls-files --others --exclude-standard -- ':(glob)<pattern>'
   ```

   Compare the baseline to the working tree, not `<baseline>..HEAD`: this includes
   uncommitted changes and rollback checkouts. Author dates cannot replace a lost
   baseline. The untracked listing is required; `git diff` alone misses new files.
3. Record repository-relative, verbatim source paths/globs. Never derive paths from
   titles or use absolute/home-relative paths. Patterns follow shell-glob semantics:
   `*` stays within a directory, `**` recurses. Use `:(glob)` for Git patterns so
   root-level matches and recursive matches agree with discovery. Record the actual
   matched count, not a guess from the directory's size.
4. In Git projects, use this population for measured discovery:

   ```bash
   git -C <path> ls-files --cached --others --exclude-standard -- ':(glob)<pattern>'
   ```

   Do not declare an empty population as a measured source. Ignored files are
   outside this check, whether authored or generated. They can still contain useful
   knowledge: inspect them when requested, record their exact paths and a
   `coverage_note` that Git freshness is unmeasured. An ignored file matching a
   declared glob is not silently part of that glob's measurable population.

## Implementation evidence

Read the implementation at the selected revision/working tree before treating its
prose as current behavior. Record dirty-worktree limitations when relevant.

| Question | Evidence |
|---|---|
| Components | Build manifest members/packages and module boundaries |
| Dependencies | Direct declarations, separate from transitive/test-only packages |
| Boundaries | Exported interfaces and values crossing them |
| Constraints | Declared lints, types, CI gate commands and tests |
| Coupled changes | Paths changed together in Git; bodies only with `--include-git` |

Use the format's parser or ecosystem tool, such as `cargo metadata --no-deps` for
Rust manifests or a YAML parser for workflows. If unavailable or unreadable, record
that limitation; a failed read is not absence. For a script/task-runner gate, follow
one hop and state that reach; do not claim deeper enforcement without inspecting it.

A declaration is evidence of an intended constraint at this cutoff. Check whether
it reaches the files in question: workspace inheritance can be declined, tests can
be ignored, and jobs can be excluded. A declaration, an inspected implementation
and a successful execution are different observations. Do not call an unexecuted
gate verified or timeless.

Re-read these facts on every scan. Do not add build/gate configuration or all Git
history to `discovered_sources` merely because it changes often: the manifest
tracks selected knowledge-source populations, not every repository change.

## Source claims

Read ADRs, learnings, guides, README, CLAUDE.md, rules and module documentation.
Compare claims to the implementation evidence above.

- Search locates candidates; read surviving hits. Use declared boundaries to separate
  shipped/test code and direct/transitive dependencies. A count is not a verdict.
- For a negative assertion, report the inspected population and limits. A targeted
  search cannot establish a universal absence outside its reach.
- **Confirmed:** retain the claim, exceptions and the check's scope.
- **Contradicted:** report the finding. Do not publish the false assertion as current
  fact. If conflicting direct evidence leaves a material decision unresolved, retain
  the disagreement in a knowledge review under AGENTS.md's review contract.
- **Intended but unverified:** label the intention and what remains unchecked.
- **Undocumented invariant:** establish its scope before stating it; do not infer a
  system-wide guarantee from a sample.

Code shows current shape; optional history can show abandoned approaches,
measurements and reversals. Scan freshness and extraction freshness differ:
`git_head_at_scan` anchors discovery; an `extracted_at_commit` with verified
retained inputs anchors that page's input. A rescan alone never refreshes an extracted page.

## Manifest publication

Classify supported transferability using SKILL.md's T1–T4 criteria; group by content,
not keywords. Infer body-only `strip_patterns` from actual project identifiers and
map domains using `lore config categories` (tab-separated ID/label). Omit a category
when none fits rather than forcing an unrelated one.

Persist the manifest under `<vault>/.lorekeeper/extracts/<project>/manifest.yaml`.
Report new/changed sources, unreadable or unmeasured populations and retained
user overrides. Explain that `run` consumes this manifest; do not silently treat
scan as extraction or overwrite user choices.
