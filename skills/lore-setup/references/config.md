# config.yaml schema

## Contents

- [Configuration outline](#configuration-outline)
- [Initial setup](#initial-setup)
- [Location and validation](#location-and-validation)
- [Document preprocessing](#document-preprocessing)
- [Nodex binary selection](#nodex-binary-selection)

## Configuration outline

`config.starter.yaml` enables one local inbox without provider credentials or
personal tracking. `config.example.yaml` is the full catalog of optional source
types and modules, with every source disabled. Both ship at the repo root and in
the installed config directory. Start small, then copy only selected blocks into
the user's `config.yaml`; this file summarizes the surrounding structure.
The starter's `ingest.schedule` declares a cadence; it creates no OS registration.

```yaml
vault:
  root: ~/Documents/Obsidian Vault   # relative paths resolve against the config file
  timezone: Asia/Seoul               # IANA name or "system"
  locale: ko                         # generated prose/headings: ko | en; source bodies kept as-is

identity:                            # basis for ownership (the personal split, when enabled)
  name: "..."
  email: "...@company.com"           # Gmail From / Calendar organizer or attendee match
  slack_id: "U0..."                  # attributes my Slack posts (with personal.tracked_sources)
                                     # (Jira self-detects from the authenticated account — no config)

ingest:
  schedule: "0 9 * * *"              # ONE daily `lore ingest` over every enabled source
                                     # (never per source: the work-log is a cross-source
                                     # daily aggregate, so one run keeps it complete)

sources:                             # key = source id; document sources use wiki/documents
  <id>:
    type: <source-type>              # from config.example.yaml
    enabled: true
    # instance: cloud               # Jira/Confluence credential instance, if needed
    params: { ... }                  # per-type (see each reference)
    focus: "..."                     # optional natural-language relevance filter (LLM-applied)
    classify:                        # optional ordered rules, first match wins
      - keywords: [deploy, incident]
        category: action_required    # daily-page grouping bucket
        performance_category: project-delivery   # optional → personal taxonomy (needs personal:)
    highlights:                      # optional daily-page highlight sections (per category)
      - { category: action_required, label: "Action Required" }
    labels: [ ... ]
    concept_policy: supported-new  # disabled | existing-only | supported-new

personal:                            # OPTIONAL personal-productivity module (work-log +
  tracked_sources: [<id>, ...]       # reviews + contribution taxonomy). OMIT for a pure
  performance_categories: [ ... ]    # domain-neutral knowledge engine. Its presence = ON.
  source_type_category_map: { ... }  # (see config.example for the full shape)
  monthly:   {enabled: true, schedule: "..."}    # quarterly/annual likewise
synthesis: {weekly: {...}}           # cross-source weekly themes (domain-neutral)
concepts:                            # LLM concept-page taxonomy
  categories: [{id: ..., label: ...}] # classifies concepts for metadata browsing
graph:                               # link graph analysis and optional semantic maintenance
  audit: {schedule: "0 7 * * 0", batch_size: 5, max_input_bytes: 262144} # uses the configured execution profile
  scope: {dirs: [...]}               # defaults to vault.dirs.wiki
  metrics: {min_hub_degree: 5, orphan_exclude: []}
  cluster: {resolution: 1.0, min_community_size: 1, suggest_min_shared_neighbors: 2}
llm: {provider: queue}
execution:
  default_profile: primary
  profiles:
    primary: {engine: claude-code, timeout_seconds: 3600} # or engine: codex
```

A Drive source uses the exact `folder_id` from its folder URL (or `root`), an
optional `name_prefix`, and `max_files` (default 200). It queries actual modification
time and imports independent documents. A folder name, date filename or glob is not
a selector. Google Docs and Slides have explicit text exports; unsupported formats
are partial coverage. Inventory absence does not authorize retiring existing pages.

Semantic maintenance is optional and separate from weekly synthesis and retention.
Enable `graph.audit.schedule` deliberately: each run uses the LLM to review at most
`batch_size` changed/first-time concepts or declared source documents. `lore run knowledge` uses the configured
execution profile; structural lint alone cannot perform the semantic read.
For an explicitly requested initial campaign, `lore run knowledge --batches N`
repeats bounded batches, releasing the lease between them and resuming from current
receipts. A failed/cancelled batch stops the campaign and preserves unfinished work.
`batch-limit` is a completed execution budget, not full-corpus semantic completion.
`max_input_bytes` includes scoped reviews, their evidence and available prior archives;
it measures distinct source bytes rather than accumulated model usage. Oversized subjects
remain selected with their actual footprint diagnostic. Native execution partitions
their evidence, uses fresh inspection/reduction sessions, then verifies the complete
work before accepting a final assessment. `execution.work` independently bounds
serialized request/result bytes and agent sessions per run; exhaustion retains pending
work and reports failure. Explicit `audit-plan --path` preserves the requested selection.
Each profile's optional `work` fields override `execution.work`; omitted fields inherit.
The effective combination is validated, including the result/input size relationship.
Native jobs pin the selected profile for child commands through `LORE_EXECUTION_PROFILE`;
standalone bounded commands can explicitly set that variable to use the same budget.
Run JSON reports the effective work limits. `review_health`
reports global attention separately from the selected batch's completion.

## Initial setup

For a new installation, establish the vault path, timezone, locale, identity and
whether personal tracking is wanted before selecting sources. Enable only the
requested sources with real IDs; example placeholders are not working accounts.
A manual inbox can prove the local path without provider credentials. Keep it
separate from configured daily/personal/synthesis/wiki directories and
`.lorekeeper`, including symlink targets. Archive=false still preserves originals.
Prefer `inbox_dir: inbox` so changing `vault.root` also moves input discovery.

Discovery helpers (`slack-cli`, `atlassian-cli`, `gws`) have their own installation
and account login. Their success does not authenticate Lore's native adapters.
Use available helpers to find metadata; otherwise ask for the specific IDs or
conditions. Never invent them or install unrelated tooling without need.
Confluence uses a named Atlassian instance and the `confluence` params/CQL example;
use the selected space/page scope, not a Jira issue query.

Credentials belong to `lore init credentials` in an interactive terminal or the
supported credential store/environment, never source params. RSS/manual/tasks and
local nodex sources do not need provider credentials; nodex has its own public
index and executable prerequisites. A config-only request ends after local
validation and reports any outstanding authentication or runtime checks.

By default credentials live at `<vault>/.lorekeeper/credentials.json`. Set optional
`credentials.file` to share one store across vaults using the same accounts;
relative paths resolve against the config file. Cooperating Lore processes lock
refresh and successor persistence together. Do not copy rotating OAuth refresh
tokens into independent stores. Other applications using the same grant do not
participate in this lock and should use their own OAuth app.
On Unix, existing credential and lock files must grant no group or other-user
permissions; use `chmod 600` after a manual copy or restore. Writes use `0600`.
The wizard refuses an unreadable or malformed existing store without resetting it.

For a complete initial setup, distinguish these outcomes:

1. `lore validate` accepts the config; it does not contact providers or prove access.
   For a new vault, generate its writing contract with `lore schema` before processing.
2. A selected-source `lore ingest --dry-run <id>` succeeds with the intended reach.
   It contacts the source without writing pages/queue; an empty result can be valid.
3. The user-invoked `/lore-ingest` and `/lore-process` publish knowledge, then their queue
   and relationship checks pass. Check `lore queue check`, `lore wiki reviews --check`
   and retrieval of the input's topic. Use a full ingest to produce the personal work-log.
4. If automation was requested, follow the existing
   [scheduling procedure](../../lore-ingest/references/scheduling.md), register the
   definitions with the OS, and inspect an actual scheduled run. Definition output
   alone is not registration; registration alone is not successful processing.

Report completed, failed and untested stages separately. `health` is collection
freshness; `self status` is installation coherence. Neither proves semantic quality.

## Location and validation
- `./config.yaml` (repo) or `~/.config/lorekeeper/config.yaml` (binary install). Also
  selectable via `--config` / `LORE_CONFIG`. Other locations require that explicit selection;
  `vault.root` resolves against the selected config file, not the working directory.
- After writing, run `lore validate`. Run source dry-runs when access verification
  or complete onboarding was requested; a config-only change reports these as untested.
- If a config already exists, merge only the relevant source block — never overwrite the whole file.

Scheduled jobs capture the selected config and runtime data directory. Interactive
skill deployment is separate: Claude uses `.claude/skills`, Codex `.agents/skills`.
Choose the authenticated CLI in a named `execution.profiles` entry using
`engine: claude-code` or `engine: codex`, then set `execution.default_profile`
to that profile name. Optional operation profiles and explicit `--profile` override it. A session deadline preserves
unfinished work rather than marking it complete. Regenerate and reload schedules
after changing paths or relevant host/proxy environment.

The optional `nodex` adapter requires a current index (`nodex build` after edits).
`params.document_ids` selects exact source IDs within the configured kinds and date
window; every selected document must be present and readable. `reconcile: true`
selects current documents independently of authored dates and requires a stable
`repository_id` namespace. It preserves authored dates rather than treating import
as a new publication. `max_documents` refuses oversized selections, never truncates.
Use temporary configurations for selected batches, not scheduled `document_ids`.

A namespace identifies a repository, not its checkout or source configuration name.
An existing imported document keeps its vault address when its title or path changes.
Legacy pages without verified document identity require explicit migration; a matching
path alone cannot authorize overwriting a curated synthesis. Missing inventory entries
do not imply deletion, archival or false claims.

## Document preprocessing

Markdown, UTF-8 text and HTML need no extra runtime. For a requested DOCX/XLSX/PDF
workflow, run `lore input install DIRECTORY --python /path/to/python3` with Python
3.11+ and a new directory whose parent exists. It creates a dedicated virtual
environment, installs the embedded hash-locked binary wheels from PyPI and prints
an `ingest.documents.python` block. Merge it into the selected config without
replacing its existing `ingest.schedule`. Missing platform wheels fail explicitly;
the installer does not build native dependencies from source. A failed installation
retains its incomplete directory for inspection and never overwrites existing data.

Then run `lore input check`, `lore input inspect /path/to/file.pdf` and a selected
`lore ingest --dry-run SOURCE`. Inspect emits the projection without publishing or
moving the input. Runtime paths resolve against the config file. Ingestion never
installs packages or downloads models. Input entries are capped at 32 MiB; configured
time, decoded-content and serialized-output bounds refuse overflow without truncation.

OCR requires explicitly installed Tesseract `<language>.traineddata` files:

```yaml
ingest:
  documents:
    python: /path/to/runtime/venv/bin/python
    timeout_seconds: 120
    max_output_bytes: 33554432
    max_decoded_bytes: 134217728
    max_units: 10000
    ocr:
      tessdata_dir: /path/to/tessdata
      languages: [kor, eng]
      layout: auto
      pdf: false
```

Use the official [tessdata_fast](https://github.com/tesseract-ocr/tessdata_fast)
models at a pinned revision; keep model provenance with the installation.
`lore input check` reports installed model SHA-256 digests. `pdf: true` adds OCR
for every PDF page while retaining native text separately. Image OCR recognizes
text; it does not interpret diagrams. Native PDF text follows the decoder's reading
order. DOCX has no reliable physical page labels. XLSX preserves formulas and their
stored cached values without recalculation; drawings, comments and linked workbooks
are outside that projection. `.doc`, `.xls`, audio and video are unsupported.

OCR `layout` is explicit: `auto` for page layout, `single-block` for one text block,
or `sparse-text` for scattered text without a reading-order guarantee. These map
to Tesseract's documented [page segmentation modes](https://tesseract-ocr.github.io/tessdoc/ImproveQuality.html#page-segmentation-method).
An empty result never triggers another mode silently; inspect the original and
select the intended policy. Published projection metadata retains language order,
layout and model hashes, as well as decoder versions.
Images are decoded frame by frame, including multipage TIFF, with EXIF orientation
and white-background transparency composition before OCR. Every frame has a distinct
ordinal inside one source document. The decoded-byte budget bounds aggregate RGBA
frame bytes as well as projected text; `max_units` bounds projected pages/sheets/frames
(native plus additional OCR count separately for PDF). Overflow refuses the projection
instead of dropping later frames/pages. Model files must stay unchanged during OCR.

The inbox defaults to text plus formats enabled by `ingest.documents`/OCR.
Known binary files without that capability stay in the inbox and are counted as
unprocessed for standalone interpretation. Their bytes can still be preserved as
dependencies of a selected note. This does not declare them independently processed.
An explicit `params.extensions` list narrows selection. Unknown extensions remain
untouched; explicitly selected binary input without its decoder fails. Failed/no-text
entries stay in the inbox. Original bytes are preserved
before publication; PDF pages and spreadsheet sheets remain units of one source
document rather than generating independent concepts. Source metadata records format,
projection policy, decoder versions and limitations. Drive still uses its documented
provider text exports; adding an inbox decoder does not expand a remote adapter's reach.

### Nodex binary selection

The adapter supports nodex `>=0.60, <0.61`. Lore's installer and
`lore self update --version` select Lore versions, not nodex versions.
Set a source's optional `params.executable` to a compatible binary's absolute path.
Omitted, it uses
`nodex` on PATH. The value is one executable path, with no shell arguments, `~` or
variable expansion; an explicit failure never retries another binary.

The repository must permit the selected version and already hold an index current for
it. Binary selection does not override a project version pin or rebuild its index.
The repository owner prepares it with the selected binary's `nodex build --full`. Relations
retain every authored `references` location; earlier captured evidence keeps its
original shape and revision rather than being relabeled as a new observation.
`lore validate` checks the parameter shape; `lore ingest --dry-run <source-id>` checks
actual source compatibility without vault or queue writes. `doctor` audits page contracts,
`health` reads collection history, and `self status` checks Lore's installation; none
certifies the currently selected nodex executable.

Collection uses nodex's verified metadata, canonical body and outgoing relations.
`source_record.observation_revision` identifies that public response with an explicit
scheme; it is not the raw Markdown file's SHA. Formatting that nodex canonicalizes
can yield the same observation after rebuilding. Earlier `revision` and
`content_revision` evidence keeps its original meaning. Refreshing a source creates
new semantic inputs and may require reassessing reviews that cite its old revision.
