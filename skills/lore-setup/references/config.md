# config.yaml schema

## Contents

- [Configuration outline](#configuration-outline)
- [Initial setup](#initial-setup)
- [Location and validation](#location-and-validation)
- [Execution profiles](#execution-profiles)
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
  schedule: "0 9 * * *"              # one native `lore run daily` over enabled sources
                                     # personal work-logs aggregate across sources;
                                     # failures remain explicit and may block reviews
  inputs:
    max_source_bytes: 67108864        # serialized normalized/projected input per source
    max_run_bytes: 268435456          # cumulative run input; must be >= max_source_bytes

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
time over the target civil day plus `lookback_days` earlier days (default 1; 0 means
only the target day) and imports independent documents. The overlap includes edits
after the previous daily run; longer outages need a wider window or explicit date
backfill. It does not reconstruct provider revision history. A folder name, date filename or glob is not
a selector. Google Docs and Slides have explicit text exports; unsupported formats
are partial coverage. Inventory absence does not authorize retiring existing pages.

Drive plain text stays literal, while declared Markdown and HTML retain structure.
Structured documents with unresolved relative or root-local links/images are not
captured; use absolute source URLs or a self-contained manual capture rather than
relying on a Drive filename or viewer URL as a dependency base. Google Workspace
[text exports are limited to 10 MB by the provider](https://developers.google.com/workspace/drive/api/reference/rest/v3/files/export);
the adapter's 16 MiB response reader does not raise that provider limit.

`ingest.inputs` bounds cumulative serialized retained provider records, normalized
observations and projected planning inputs. Collection and planning apply these limits
independently; `ingest.originals` raw-file preservation and `execution.work` model
requests have separate limits.
Both limits are positive bytes, with the per-source limit no larger than the run limit.
Over-budget input is reported as incomplete rather than clipped. Increase these values
deliberately or use smaller self-contained input batches. They do not cap total process
memory, native decoder allocations, or provider usage.

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
Run JSON's `review_health` reports global attention separately from the selected
batch's completion. Execution budgets are configured below.

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
use the selected space/page scope, not a Jira issue query. Its `cql` is a filter
without `ORDER BY`; the adapter intersects it with the requested modification
window and adds ordering. Confluence validates the grammar at fetch time.
Provider-relative functions such as `now()` use acquisition time; a past `--date`
does not restore their old evaluation or historical page versions.

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
3. Run `lore run daily` with the configured execution profile for the authorized
   first collection, semantic publication and controller checks. Verify retrieval
   of the input's topic and its evidence. For explicitly staged manual processing,
   `/lore-ingest` collects and `/lore-process` publishes and finalizes; use a full
   ingest to produce the personal work-log. Neither configuration nor dry run
   establishes these stages' completion.
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

## Execution profiles

Choose the authenticated CLI in a named `execution.profiles` entry using
`engine: claude-code` or `engine: codex`, then set `execution.default_profile`
to that profile name. Selection is explicit `--profile`, operation-specific
`execution.operations`, then default profile. Failure never switches hosts silently.
Optional `executable` and `model` select the CLI and model. Relative executable paths
and Claude's `config_directory` resolve against the config file; Codex's `profile`
names an existing profile in that CLI. Credentials remain with the host.

`timeout_seconds` is a per-session wall-clock budget. `execution.work` defaults to
262144 request bytes, 32768 result bytes and 64 semantic calls per native run or
knowledge batch. These bound serialized JSON and invocation attempts, not total
context, tokens or billing. Large requests are inspected/reduced in fresh sessions;
exhaustion preserves verified work and reports incompletion.
`execution.work.extraction_context` defaults to `progressive`: examine bounded cards
for the complete candidate inventory, then read selected bodies and their required
reviews/evidence. Card-only exclusions do not establish whole-body findings; deferred
decisions block completion. `full-context` reads every retrieved candidate body.
Both retain complete source reads and exact candidate/review guards. See
[extraction discovery](../../lore-process/references/discovery.md) for the staged protocol.
Input must be at least 4096 bytes; results at least 1024 bytes and at most one quarter
of the input limit; calls must be positive. A profile's optional `work` overrides
only specified fields, and the effective combination is validated. Native children
inherit the pinned profile through `LORE_EXECUTION_PROFILE`; standalone bounded
commands can set that variable explicitly. Run JSON reports effective limits.
Regenerate and reload schedules after changing paths or host/proxy environment.

## Document preprocessing

Markdown, UTF-8 text and HTML need no extra runtime. For a requested DOCX/XLSX/PDF
workflow, run `lore input install DIRECTORY --python /path/to/python3` with Python
3.11+ and a new directory whose parent exists. It creates a dedicated virtual
environment, installs the embedded hash-locked binary wheels from PyPI and prints
an `ingest.documents.python` block. Merge it into the selected config without
replacing its existing `ingest.schedule`. Missing platform wheels fail explicitly;
the installer does not build native dependencies from source. A failed installation
retains its incomplete directory for inspection and never overwrites existing data.

The native runtime uses pinned Xberg for DOCX/PDF and openpyxl for XLSX. The pinned
Xberg release provides macOS/Linux wheels; Windows native-runtime installation is
currently unavailable. Literal PDF OCR renders each physical page through pypdfium2
before image recognition; native text stays separate from failed OCR results.
After a CLI upgrade, run `lore input check`. If dependency versions differ,
install into a new directory and update `ingest.documents.python` from its printed
configuration, then check again. Dependency identity invalidates derived cache
reuse; preserved originals and document identities remain valid.

For visual PDF and image recognition on Apple Silicon macOS 14+, also run
`lore input install-vision VISION_DIRECTORY --python python3.12`. This explicitly
installs separate hash-locked Paddle layout and MLX recognition environments and
verified PaddleOCR-VL 1.6 model artifacts. Merge its printed `vision` block into
`ingest.documents`, retaining the native runtime's `python`:

```yaml
ingest:
  documents:
    python: /path/to/runtime/venv/bin/python
    timeout_seconds: 120
    max_output_bytes: 33554432
    max_decoded_bytes: 134217728
    max_raster_bytes: 67108864
    max_units: 10000
    vision:
      engine: paddle-ocr-vl
      python: /path/to/vision/layout/venv/bin/python
      vlm_python: /path/to/vision/vlm/venv/bin/python
      models_dir: /path/to/vision/models
```

With `vision` configured, an omitted `documents.pdf` means native PDF text plus visual
recognition of every physical page. Select `pdf: {mode: native}` for native text only,
or `pdf: {mode: visual, pages: [1, 3]}` to recognize selected pages while retaining
native text for all pages. Page numbers are physical, 1-based and strictly increasing.
Without `vision`, native text is the default; explicit `ocr.pdf` below can add literal
OCR. A manual source's `params.pdf`
overrides the global PDF policy for that source; visual mode requires `vision`.
DOCX/XLSX stay native, while supported image formats use visual recognition.

```sh
lore input check
lore input inspect /path/to/file.pdf --pdf-mode native
lore input inspect /path/to/file.pdf --pdf-mode visual --visual-pages 1,3
lore ingest --dry-run SOURCE
```

Inspection emits bounded JSON without publishing or moving the input. PDF flags
override only that inspection, not a source's configuration. Runtime paths resolve
against the config file. Ingestion never installs packages or downloads models.
PDF visual pages render at 150 DPI. Output keeps physical page/frame locations,
recognized blocks and their coordinates, rotation, runtime/model identity and selected
scope. Native text remains distinct. Image frames, including multipage TIFF, retain
stored order with EXIF orientation and white-background transparency composition.
There is no cross-page merging, document unwarping or chart/seal captioning. Recognition
is a derived interpretation, not semantic approval; verify consequential identifiers,
quantities, formulas and operators against the preserved original.

For literal-text OCR, configure `ocr` instead of `vision`; they are mutually exclusive:

```yaml
ocr:  # within ingest.documents
  tessdata_dir: /path/to/tessdata
  languages: [kor, eng]
  layout: auto
  pdf: false
```

Install official [tessdata_fast](https://github.com/tesseract-ocr/tessdata_fast) models
at a pinned revision. `lore input check` reports model digests. `ocr.pdf: true` adds
literal OCR for every PDF page while retaining native text. Explicit `documents.pdf`
or source `params.pdf` takes precedence over `ocr.pdf`. Layout is
`auto`, `single-block` or `sparse-text`, matching Tesseract's
[page segmentation modes](https://tesseract-ocr.github.io/tessdoc/ImproveQuality.html#page-segmentation-method).
This path does not reconstruct tables, interpret diagrams or delete lines through
dictionary filtering. Empty results never trigger another engine or layout silently.

`max_raster_bytes` defaults to 64 MiB per pending page/frame. `max_decoded_bytes`
bounds aggregate projected text/structure; literal OCR also bounds aggregate RGBA
frame bytes. `max_output_bytes` bounds serialized output, and `max_units` counts
native plus visual/OCR units separately for PDFs. Overflow refuses the projection
without dropping later pages. `timeout_seconds` covers the whole projection; the
source acquisition timeout separately covers its collection of entries. Measure a
representative input before raising both budgets. Metadata readers can allocate
codec buffers before pending-unit preflight, including WebP's canvas. These bounds
do not cap the decoder/model process's total memory.

Real ingestion checkpoints validated native components and completed visual units in
the private, derived `.lorekeeper/cache/projections/` store. Exact source bytes,
decoder/worker policy, dependency and model identity govern reuse; current output
and unit limits still apply. Cached units skip rendering and recognition. Inspection and dry
runs read caches without writing or renewing retention. Successful read/write reuse
renews last use; maintenance prunes expired derived entries, preserving active claims
and malformed data. Cache removal does not remove originals or completed knowledge.

Original acquisition defaults are 32 MiB per entry, 64 MiB per referenced member and
256 MiB per manual source. Raise `ingest.originals` budgets explicitly for larger
files; entry/member limits must fit `max_source_bytes`, and one ingest's captures
must fit 512 MiB. Private acquisition files are automatically removed, including
after inspection and dry runs; real ingestion publishes preserved originals before
knowledge pages or inbox retirement.

The inbox defaults to text plus formats enabled by `ingest.documents`/OCR/vision.
Known binary files without that capability stay in the inbox and are counted as
unprocessed for standalone interpretation. Their bytes can still be preserved as
dependencies of a selected note. This does not declare them independently processed.
An explicit `params.extensions` list narrows selection. Unknown extensions remain
untouched; explicitly selected binary input without its decoder fails. Failed/no-text
entries stay in the inbox. Pages/sheets/frames remain units of one source document.
DOCX has no reliable physical page labels. XLSX retains formulas, stored cached values,
merged ranges and declared visibility without recalculation or excluding hidden values;
drawings, comments and linked workbooks are outside that projection. `.doc`, `.xls`,
audio and video are unsupported. Drive retains its provider text exports; configuring
an inbox decoder does not expand a remote adapter's reach.

## Nodex binary selection

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
