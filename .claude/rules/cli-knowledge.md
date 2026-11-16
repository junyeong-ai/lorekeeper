---
paths:
  - "crates/lk-cli/src/commands/graph.rs"
  - "crates/lk-cli/src/commands/graph/**"
  - "crates/lk-cli/src/commands/wiki.rs"
  - "crates/lk-cli/src/commands/audit.rs"
  - "crates/lk-cli/src/commands/audit/**"
  - "crates/lk-cli/src/commands/work/audit.rs"
---

## Canonical pages and derived discovery (`commands/wiki.rs`, `graph.rs`)

- Markdown is canonical; SQLite is the prepared derived query snapshot. Refresh
  validates the small owned entry guide, prepares SQLite, then publishes that guide.
  Revalidate the captured configuration and inherited workflow target immediately
  before guide apply, after snapshot preparation and progress output. A late refusal
  retains the derived snapshot and never publishes the guide or redirects its plan.
  There are no Markdown catalog/log/map exporters, archive-replace migration, or
  graph index-currency checks. Existing authored files are not retired by refresh.
- Use captured config to refuse canonical manual-inbox overlap before capture or
  navigation mutation. Guide ownership, symlink, nonregular-file and authored
  collision guards remain strict; never infer ownership from an old export name.
  Map graph capture verification through GraphError::into_vault_error so typed
  drift reaches the command result without becoming a generic I/O string.
  A capture refusal remains incomplete and stops native continuation.
- Browse metadata with query=None; reject query-only/date-sort combinations in the
  library too. Declared-created bounds compose with queries; created sorting is browse-only.
  Exact repository/provider scopes match declared ownership, not concept evidence membership.
  Created means declared authored date, not ingestion time. Preserve
  missing/invalid visibility, stable path ties, scope/filter/facet/pagination meaning.
  Search rejects an empty query and a zero limit. Do not reintroduce full
  corpus metadata decode/sort for SQL browse or load concept vocabulary there.
- Semantic search and discovery share the exact generated-content projection.
  Raw body queries and revision-pinned reads retain complete Markdown.
- Concept filenames are addresses, not necessarily IDs or implicit aliases. Use
  exact public resolution and return the resolved canonical title.
- Evidence/review discovery defaults to read-only JSON. Evidence archive is a writer.
  Open valid reviews and drift needing attention are distinct, not all “resolved.”
  Provenance is metadata-first, bounded and revision-pinned; relation/direction are
  explicit. Discovery must not incidentally prepare a new snapshot.
- `graph repair-link` repairs an explicitly mapped missing concept address in one
  exact concept page. Pin origin and destination file revisions, require canonical
  declared concepts and an absent old address, use shared CommonMark link rewriting,
  and journal target/absence guards with the source edit. Preserve prose, frontmatter
  and completion values; never regenerate semantics or infer mappings from names.
  Typed RSS evidence requires its provenance-aware path. The preview is inert and
  refuses pending journals; the local repair makes no global lint or audit claim.
