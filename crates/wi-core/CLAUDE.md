# wi-core

Domain types and config — no I/O, no async. Depended on by every other crate.

- **`Config::load` validates eagerly**: `validate()` rejects empty/`/`-containing source
  IDs, bad cron, out-of-range thresholds, unknown synthesis/category references, and —
  importantly — `vault.dirs.*` values that are absolute or contain `..` (path-traversal
  guard before any path is built). A relative `vault.root` is resolved against the config
  file's parent directory.
- **`SourceType` is a closed enum** with `default_template_name()` co-located on it.
  Adding a source type is a compiler-checked change here + a `wi-source` adapter/factory
  arm. Don't replace it with a runtime registry — exhaustive matching is the point.
- **`SourceConfig.classify`** is a typed `BTreeMap<String, Vec<String>>` (keyword →
  category), kept OUT of the free-form `params` so adapter params can use
  `deny_unknown_fields`.
- **`EventId::new(source_id, date, content)`** = `source:date:blake3(content)[..12]`.
  In `wi-pipeline::normalize`, `content` is the `external_id` or a JSON array of
  `[title, body]` — never a bare concatenation (that collides).
- **`slugify()`** lowercases + strips to `[alnum-]`; concept slugs are always
  re-normalized through it to prevent path injection from LLM output.
- **`LlmConfig` defaults to `provider: queue`** (matches docs/example).
- **`VaultDirs.annual`** is the config key; its default directory value is `"annually"`
  (the on-disk folder series stays daily/weekly/monthly/quarterly/annually).
