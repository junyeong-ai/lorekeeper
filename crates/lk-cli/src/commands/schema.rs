use std::fmt::Write as _;
use std::path::PathBuf;

use lk_core::i18n::{Locale, Strings};
use lk_core::vault_path::{
    CONCEPTS_SUBDIR, DOCUMENTS_SUBDIR, EXPLORATIONS_SUBDIR, WORK_LOG_SUBDIR,
};

/// Section ownership tag.
#[derive(Clone, Copy)]
enum Owner {
    Llm,
    Machine,
}

impl Owner {
    fn label(self) -> &'static str {
        match self {
            Owner::Llm => "LLM",
            Owner::Machine => "machine",
        }
    }
}

/// One section inside a page type.
struct Section {
    /// English semantic name shown in the "Section" column.
    name: &'static str,
    /// Resolves the localized heading from `Strings`. A plain `fn` pointer — every
    /// resolver is a non-capturing closure, so no boxed trait object is needed.
    heading: fn(&Strings) -> String,
    owner: Owner,
}

/// Schema definition for a single page type.
struct PageSchema {
    /// Short type name (e.g. "concept", "daily").
    type_name: &'static str,
    /// Vault path pattern (e.g. "{wiki}/concepts/{slug}.md").
    path_pattern: String,
    /// Frontmatter keys.
    frontmatter: &'static [&'static str],
    sections: Vec<Section>,
    /// The `lore` command that fills this format's `machine` sections, or `None` when it has
    /// none. Required, so a new page format cannot be added without naming its writer — the
    /// ownership legend is generated from these, and a hand-written one went out of date the
    /// moment it named a single command for a column fourteen rows wide.
    machine_writer: Option<&'static str>,
}

fn s(name: &'static str, heading: fn(&Strings) -> String, owner: Owner) -> Section {
    Section {
        name,
        heading,
        owner,
    }
}

/// The `machine` column's writers, each with the page formats it fills, read off the schemas
/// themselves.
///
/// Generated rather than written, because the column is what tells an author whether to fill a
/// section and a legend naming the wrong command sends them to wait for a run that will never
/// touch it. Naming ONE command was false for eleven of the fourteen machine rows; naming
/// three by hand was true only until a format arrived with a fourth. Adding a format now
/// forces its writer to be declared, and this reads the declarations.
fn machine_writers(schemas: &[PageSchema]) -> String {
    let mut grouped: Vec<(&str, Vec<&str>)> = Vec::new();
    for schema in schemas {
        let Some(writer) = schema.machine_writer else {
            continue;
        };
        if !schema
            .sections
            .iter()
            .any(|section| matches!(section.owner, Owner::Machine))
        {
            continue;
        }
        match grouped.iter_mut().find(|(name, _)| *name == writer) {
            Some((_, formats)) => formats.push(schema.type_name),
            None => grouped.push((writer, vec![schema.type_name])),
        }
    }
    grouped
        .iter()
        .map(|(writer, formats)| format!("`{writer}` for {}", formats.join(", ")))
        .collect::<Vec<_>>()
        .join("; ")
}

fn page_schemas(dirs: &lk_core::config::VaultDirs, personal: bool) -> Vec<PageSchema> {
    let mut schemas = vec![
        PageSchema {
            type_name: "concept",
            path_pattern: format!("{}/{CONCEPTS_SUBDIR}/{{slug}}.md", dirs.wiki),
            frontmatter: &[
                "id",
                "type",
                "title",
                "aliases",
                "created",
                "updated",
                "category",
                lk_core::frontmatter::field::SOURCE_COUNT,
                lk_core::frontmatter::field::AUDITED_SOURCES_HASH,
                "tags",
            ],
            sections: vec![
                s("Synthesis", |i| i.concept_synthesis.to_string(), Owner::Llm),
                s("Sources", |i| i.concept_sources.to_string(), Owner::Machine),
                s("Related", |i| i.related.to_string(), Owner::Llm),
            ],
            machine_writer: Some("lore graph backlinks-sync"),
        },
        PageSchema {
            type_name: "daily",
            path_pattern: format!("{}/{{source-id}}/YYYY-MM-DD.md", dirs.daily),
            frontmatter: &[
                "id",
                "type",
                "title",
                "created",
                "labels",
                "source",
                "event_count",
            ],
            sections: vec![
                s("Summary", |i| i.summary.to_string(), Owner::Llm),
                s(
                    "Events / Messages",
                    |i| format!("{} / {}", i.key_events, i.key_messages),
                    Owner::Machine,
                ),
                // Concept wiki-links are EXTRACTED by the LLM (the `concepts` task, gated by
                // its `concepts_done` marker) — the machine emits only the empty heading. So
                // this is LLM-owned, exactly like Summary; only the raw Events list above is
                // machine-owned (the LLM merely refines each event's body in place).
                s("Concepts", |i| i.related_concepts.to_string(), Owner::Llm),
            ],
            machine_writer: Some("lore ingest"),
        },
        PageSchema {
            type_name: "document",
            path_pattern: format!("{}/{DOCUMENTS_SUBDIR}/{{slug}}.md", dirs.wiki),
            frontmatter: &[
                "id",
                "type",
                "title",
                "created",
                "updated",
                "document_type",
                "source_url",
                "source_file",
                "tags",
            ],
            sections: vec![
                s("Summary", |i| i.summary.to_string(), Owner::Llm),
                s(
                    "Content",
                    |i| i.document_content.to_string(),
                    Owner::Machine,
                ),
                // LLM-extracted (the `concepts` task), like the daily Concepts section — the
                // machine emits only the heading.
                s(
                    "Extracted Concepts",
                    |i| i.related_concepts.to_string(),
                    Owner::Llm,
                ),
            ],
            machine_writer: Some("lore ingest"),
        },
        PageSchema {
            type_name: "exploration",
            path_pattern: format!("{}/{EXPLORATIONS_SUBDIR}/{{slug}}.md", dirs.wiki),
            frontmatter: &[
                "id", "type", "title", "aliases", "created", "updated", "tags",
            ],
            sections: vec![
                s(
                    "Question",
                    |i| i.exploration_question.to_string(),
                    Owner::Llm,
                ),
                s(
                    "Synthesis",
                    |i| i.exploration_synthesis.to_string(),
                    Owner::Llm,
                ),
                s(
                    "Grounding",
                    |i| i.exploration_grounding.to_string(),
                    Owner::Llm,
                ),
            ],
            machine_writer: None,
        },
        PageSchema {
            type_name: "weekly-synthesis",
            path_pattern: format!("{}/{}/YYYY-Www.md", dirs.synthesis, dirs.weekly),
            frontmatter: &[
                "id",
                "type",
                "title",
                "created",
                "labels",
                "period",
                "sources_covered",
            ],
            sections: vec![s(
                "Key Themes",
                |i| i.key_themes_this_week.to_string(),
                Owner::Llm,
            )],
            machine_writer: None,
        },
    ];

    // The work-log and the four reviews are the personal module's pages — documented in
    // AGENTS.md only when `personal:` is configured, so a domain-neutral vault's format
    // reference never describes pages it will never produce.
    if personal {
        schemas.extend([
            PageSchema {
                type_name: "work-log",
                path_pattern: format!("{}/{WORK_LOG_SUBDIR}/YYYY-MM-DD.md", dirs.personal),
                frontmatter: &[
                    "id",
                    "type",
                    "title",
                    "created",
                    "labels",
                    "categories",
                    "sources",
                ],
                sections: vec![
                    s("Topic Summary", |i| i.topic_summary.to_string(), Owner::Llm),
                    s("Sources", |i| i.concept_sources.to_string(), Owner::Machine),
                ],
                machine_writer: Some("lore ingest"),
            },
            PageSchema {
                type_name: "weekly-review",
                path_pattern: format!("{}/{}/YYYY-Www.md", dirs.personal, dirs.weekly),
                frontmatter: &[
                    "id",
                    "type",
                    "title",
                    "created",
                    "labels",
                    "period",
                    "days_logged",
                ],
                sections: vec![
                    s("Period", |i| i.period.to_string(), Owner::Machine),
                    s("Summary", |i| i.key_summary.to_string(), Owner::Llm),
                    s(
                        "Category Distribution",
                        |i| i.category_distribution.to_string(),
                        Owner::Machine,
                    ),
                ],
                machine_writer: Some("lore synthesis"),
            },
            PageSchema {
                type_name: "monthly-review",
                path_pattern: format!("{}/{}/YYYY-MM.md", dirs.personal, dirs.monthly),
                frontmatter: &[
                    "id",
                    "type",
                    "title",
                    "created",
                    "labels",
                    "period",
                    "days_logged",
                ],
                sections: vec![
                    s("Period", |i| i.period.to_string(), Owner::Machine),
                    s("Summary", |i| i.key_summary.to_string(), Owner::Llm),
                    s(
                        "Category Distribution",
                        |i| i.category_distribution.to_string(),
                        Owner::Machine,
                    ),
                ],
                machine_writer: Some("lore synthesis"),
            },
            PageSchema {
                type_name: "quarterly-review",
                path_pattern: format!("{}/{}/YYYY-Qq.md", dirs.personal, dirs.quarterly),
                frontmatter: &["id", "type", "title", "created", "labels", "period"],
                sections: vec![
                    s("Period", |i| i.period.to_string(), Owner::Machine),
                    s(
                        "Category Distribution",
                        |i| i.category_distribution.to_string(),
                        Owner::Machine,
                    ),
                    s("Summary", |i| i.key_summary.to_string(), Owner::Llm),
                    s(
                        "Monthly Breakdown",
                        |i| i.monthly_breakdown.to_string(),
                        Owner::Machine,
                    ),
                ],
                machine_writer: Some("lore synthesis"),
            },
            PageSchema {
                type_name: "annual-review",
                path_pattern: format!("{}/{}/YYYY.md", dirs.personal, dirs.annual),
                frontmatter: &["id", "type", "title", "created", "labels", "period"],
                sections: vec![
                    s("Overview", |i| i.overall_summary.to_string(), Owner::Llm),
                    s(
                        "Quarterly Breakdown",
                        |i| i.quarterly_breakdown.to_string(),
                        Owner::Machine,
                    ),
                    s(
                        "Category Distribution",
                        |i| i.category_distribution.to_string(),
                        Owner::Machine,
                    ),
                ],
                machine_writer: Some("lore synthesis"),
            },
        ]);
    }

    schemas
}

/// Render the AGENTS.md content for a given locale and directory layout.
pub fn render_agents_md(
    locale: Locale,
    dirs: &lk_core::config::VaultDirs,
    personal: bool,
) -> String {
    let strings = locale.strings();
    let locale_tag = locale.tag();
    let schemas = page_schemas(dirs, personal);

    let mut out = String::new();
    writeln!(
        out,
        "---\ntype: {}\n---\n",
        lk_core::vault_path::SCHEMA_FORMAT
    )
    .unwrap();
    writeln!(out, "# Lorekeeper Page Formats").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "> Generated by `lore schema` — locale: {locale_tag}").unwrap();
    writeln!(
        out,
        "> Regenerate after changing `vault.locale`: `lore schema`"
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Pages with an LLM-owned section also carry a machine-managed `llm_inputs` \
         frontmatter block (per-section input hashes for the materialized-view cache). \
         It is written by `lore ingest` and `/lore-process`; never hand-author it — a \
         page you create directly simply omits it."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Some listed frontmatter keys are optional and NOT written at page creation: a \
         concept's `audited_sources_hash` is stamped only by `lore graph audit-mark` after \
         the first contradiction audit, and `aliases` appears only when the page actually \
         has synonyms. Omit both when first authoring a page."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Each table's `Owner` column names who fills that section's body: `machine` = `lore` \
         writes it, under whichever command produces the page ({}); `LLM` = an agent writes \
         it, which in the automated pipeline is `/lore-process`. A page you author DIRECTLY \
         has no pipeline behind it, so you fill EVERY section yourself, `machine` ones \
         included — except a concept's `## {}` section and its `{}`, which `lore graph \
         backlinks-sync` re-derives wholesale from the forward links on citing pages every \
         time it runs, so leave those empty and let it.",
        machine_writers(&schemas),
        strings.concept_sources,
        lk_core::frontmatter::field::SOURCE_COUNT,
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Every page's `type` frontmatter is its page-format id — exactly the `## \
         {{type}}` names below (`concept`, `daily`, `document`, …). It is the one \
         REQUIRED key of the Open Knowledge Format, so any OKF consumer can classify \
         the vault's pages without Lorekeeper-specific knowledge."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(out, "## Links").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Every internal reference is an inline markdown link `[Display](relative/path.md)` \
         whose destination is RELATIVE TO THE CONTAINING PAGE'S DIRECTORY and always \
         carries the `.md` extension — the one form Obsidian, GitHub, and OKF consumers \
         all resolve. Never write `[[wikilinks]]`. A concept link from a page is \
         `[{{Name}}]({{concepts-dir}}/{{slug}}.md)`, where `{{concepts-dir}}` is the \
         relative path to `{}/{}` from that page and `{{slug}}` is the slug of the \
         concept name (rule \
         below). Destinations with spaces or parens are percent-encoded (`%20`, `%28`, \
         `%29`); non-ASCII slugs stay verbatim.",
        dirs.wiki, CONCEPTS_SUBDIR
    )
    .unwrap();

    writeln!(out).unwrap();
    writeln!(out, "## Navigating this vault").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Start from a navigation entry point and drill in — navigate, don't scan every file:"
    )
    .unwrap();
    writeln!(
        out,
        "- `{}/map.md` — concepts grouped by citation cluster (the graph's emergent \
         structure); start here to see what relates to a topic.",
        dirs.wiki
    )
    .unwrap();
    writeln!(
        out,
        "- `{}/index.md` — catalog of every page, grouped by category, each with a \
         first-sentence summary. Read it first to locate relevant concepts in one pass \
         without opening each page.",
        dirs.wiki
    )
    .unwrap();
    writeln!(
        out,
        "- `{}/log.md` — reverse-chronological timeline of when each knowledge node entered \
         the vault.",
        dirs.wiki
    )
    .unwrap();
    writeln!(
        out,
        "From an entry point, open the pages it links and follow their links. \
         Regenerate the entry points with `lore wiki map` / `lore wiki index` / `lore wiki log`."
    )
    .unwrap();

    for schema in &schemas {
        writeln!(out).unwrap();
        writeln!(out, "## {} (`{}`)", schema.type_name, schema.path_pattern).unwrap();
        writeln!(out).unwrap();
        writeln!(out, "| Section | Heading | Owner |").unwrap();
        writeln!(out, "|---------|---------|-------|").unwrap();
        for section in &schema.sections {
            let heading = (section.heading)(strings);
            writeln!(
                out,
                "| {} | `## {}` | {} |",
                section.name,
                heading,
                section.owner.label()
            )
            .unwrap();
        }
        writeln!(out).unwrap();
        writeln!(
            out,
            "Frontmatter: {}",
            schema
                .frontmatter
                .iter()
                .map(|k| format!("`{k}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )
        .unwrap();

        if schema.type_name == "document" {
            writeln!(
                out,
                "\n`document_type` values: {} (FORMAT only; subject-matter goes in `tags`).",
                lk_core::document::DOCUMENT_TYPES
                    .iter()
                    .map(|t| format!("`{t}`"))
                    .collect::<Vec<_>>()
                    .join(" | ")
            )
            .unwrap();
        }
    }

    // The convergence contract is schema, not skill lore: it states binary-owned
    // invariants (slugify, backlinks-sync field ownership) and must reference the
    // LOCALIZED headings, so it is generated here rather than shipped as prose.
    let sources_heading = strings.concept_sources;
    let related_concepts_heading = strings.related_concepts;
    writeln!(out).unwrap();
    writeln!(out, "## Concept convergence").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "One concept = one page. Every agent that creates or merges concept pages \
         follows this exact algorithm, so the wiki converges instead of accumulating \
         variants."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "1. **Load the registry** at the start of the run: `lore wiki concepts` — the \
         on-disk truth at run start (slugs, names, aliases)."
    )
    .unwrap();
    writeln!(
        out,
        "2. **Maintain a created-this-run set.** Every minted page or newly registered \
         alias joins your in-context set BEFORE the next item is processed. The \
         run-start registry cannot see same-run changes — without the running set, two \
         items independently mint `RAG` and `Retrieval-Augmented-Generation`."
    )
    .unwrap();
    writeln!(
        out,
        "3. **Match each extracted concept** against the union (registry + \
         created-this-run) by slug-equivalence OR semantic equivalence. On a match, \
         reuse the existing slug + name — never create a variant. When in doubt, prefer \
         the established broader concept over a narrow variant."
    )
    .unwrap();
    writeln!(
        out,
        "4. **Register surface forms as aliases.** When a source's surface form differs \
         from the canonical name, append it to the concept's `aliases` frontmatter — the \
         registry (`lore wiki concepts`) returns aliases, so the next run's dedup match \
         recognizes the synonym instead of minting a variant page. Links are unaffected \
         (they address the slug path; the display text is free-form). An alias edit is \
         metadata-only: it never renames the page and is not, by itself, a reason to \
         rewrite the body (whether a merge also enriches the synthesis body is the \
         consuming workflow's own judgment)."
    )
    .unwrap();
    writeln!(
        out,
        "5. **Slug normalization** is `lore`'s slugify, exactly: NFKC → lowercase → \
         non-alphanumeric to hyphen → collapse runs → trim edges."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Machine-owned citation fields — never hand-write them: a NEW concept page \
         starts with an empty `## {sources_heading}` body and `source_count: 0`; on an \
         EXISTING page leave both exactly as found. Record citations as forward \
         markdown links to the concept page on the ORIGIN page (its \
         `## {related_concepts_heading}` section, link form per `## Links` above); \
         `lore graph backlinks-sync` re-derives every concept's \
         `## {sources_heading}` + `source_count` from those forward links wholesale — \
         an entry not backed by a forward link is wiped, and a concept cited by several \
         pages in one batch is counted correctly where hand-written one-ref-per-item \
         entries would undercount. Finish any batch that created concept pages OR \
         citations with `lore graph backlinks-sync`, then `lore wiki index`, then \
         `lore wiki map`."
    )
    .unwrap();

    out
}

pub async fn run(
    opts: &super::GlobalOptions,
    root_override: Option<PathBuf>,
) -> miette::Result<()> {
    // Single override semantics (shared with `wiki`/`graph`): a present config drives
    // locale/dirs/personal even under `--root`; defaults apply ONLY when no config file exists;
    // a present-but-broken config fails loudly (never silently emit AGENTS.md for the wrong dirs).
    let super::RootConfig {
        root: vault_root,
        config,
    } = super::resolve_root_config(opts, root_override)?;
    let (locale, dirs, personal) = match config {
        Some(config) => (
            config.vault.locale(),
            config.vault.dirs.clone(),
            config.personal.is_some(),
        ),
        None => (
            Locale::default(),
            lk_core::config::VaultDirs::default(),
            false,
        ),
    };

    let content = render_agents_md(locale, &dirs, personal);

    // A page this tool writes into the vault, so it goes through the writer that refuses to
    // replace a page of another format rather than around it.
    let agents_rel = std::path::Path::new(&dirs.wiki).join("AGENTS.md");
    lk_vault::VaultWriter::new(&vault_root)
        .write_generated_page(&agents_rel, &content)
        .await
        .map_err(|e| miette::miette!("write AGENTS.md: {e}"))?;

    eprintln!("Wrote {}", vault_root.join(&agents_rel).display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_md_uses_locale_strings() {
        let ko = render_agents_md(Locale::Ko, &lk_core::config::VaultDirs::default(), true);
        assert!(ko.contains("locale: ko"));
        assert!(ko.contains("`## 핵심`"));
        assert!(ko.contains("`## 출처`"));
        assert!(ko.contains("`## 관련`"));

        let en = render_agents_md(Locale::En, &lk_core::config::VaultDirs::default(), true);
        assert!(en.contains("locale: en"));
        assert!(en.contains("`## Synthesis`"));
        assert!(en.contains("`## Sources`"));
        assert!(en.contains("`## Related`"));
    }

    #[test]
    fn concept_sections_are_advertised_as_llm_owned() {
        // The daily "Concepts" and document "Extracted Concepts" sections are EXTRACTED by the
        // LLM (`concepts` task, `concepts_done` marker), so AGENTS.md — the agent-facing
        // contract — must label them LLM-owned, consistent with Summary. The raw Events list
        // stays machine-owned (the LLM only refines each body in place).
        let md = render_agents_md(Locale::En, &lk_core::config::VaultDirs::default(), false);
        for line in md.lines() {
            if line.starts_with("| Concepts ") || line.starts_with("| Extracted Concepts ") {
                assert!(
                    line.trim_end().ends_with("| LLM |"),
                    "concept section must be advertised LLM-owned: {line}"
                );
            }
        }
        let events = md
            .lines()
            .find(|l| l.starts_with("| Events / Messages "))
            .expect("daily Events row present");
        assert!(
            events.trim_end().ends_with("| machine |"),
            "raw Events list stays machine-owned: {events}"
        );
    }

    #[test]
    fn agents_md_defines_the_ownership_column() {
        // The Owner column decides whether an agent writes a section, so a reader who does
        // not already know the vocabulary cannot act on the tables. Every authoring skill
        // used to restate the rule in its own prose — three copies to drift, and the one
        // skill that lacked it was the one creating the page format with no pipeline at all.
        //
        // `machine` cannot be equated with any ONE command: `lore ingest` writes only the
        // daily/document/work-log rows, while `lore synthesis` writes every machine row on
        // the five periodic pages and `lore graph backlinks-sync` writes a concept's. Naming
        // a single writer would be false for most of the column, and false in the direction
        // that makes an author leave a section for a command that will never touch it. The
        // localized heading reference must come from the i18n bundle like every other one.
        for (locale, sources) in [(Locale::Ko, "## 출처"), (Locale::En, "## Sources")] {
            let md = render_agents_md(locale, &lk_core::config::VaultDirs::default(), true);
            let legend = md
                .lines()
                .find(|l| l.starts_with("Each table's `Owner` column"))
                .unwrap_or_else(|| panic!("{locale:?}: ownership legend present"));
            // Read off the schemas, so a format added with a fourth writer cannot leave the
            // legend naming only the three that existed when it was written.
            let schemas = page_schemas(&lk_core::config::VaultDirs::default(), true);
            let declared: std::collections::BTreeSet<&str> = schemas
                .iter()
                .filter(|schema| {
                    schema
                        .sections
                        .iter()
                        .any(|section| matches!(section.owner, Owner::Machine))
                })
                .filter_map(|schema| schema.machine_writer)
                .collect();
            assert!(
                declared.len() >= 3,
                "expected several writers: {declared:?}"
            );
            for writer in &declared {
                assert!(
                    legend.contains(writer),
                    "{locale:?}: legend omits the writer {writer:?}: {legend}"
                );
            }
            // The legend states one carve-out in prose — a concept's sources section belongs
            // to `backlinks-sync` even on a hand-authored page — so the concept format must
            // declare that same writer. Generated list and prose are two statements about one
            // fact, and nothing else would notice them disagreeing.
            let concept = schemas
                .iter()
                .find(|schema| schema.type_name == "concept")
                .expect("concept format present");
            assert_eq!(
                concept.machine_writer,
                Some("lore graph backlinks-sync"),
                "the generated writer list must agree with the legend's carve-out"
            );

            // And a format whose sections are all agent-written contributes no writer.
            for schema in &schemas {
                let machine = schema
                    .sections
                    .iter()
                    .any(|section| matches!(section.owner, Owner::Machine));
                assert_eq!(
                    machine,
                    schema.machine_writer.is_some(),
                    "{}: a machine section needs a writer and only a machine section has one",
                    schema.type_name
                );
            }
            assert!(legend.contains("`LLM` = an agent writes it"), "{legend}");
            assert!(
                legend.contains(&format!("`{sources}`")),
                "{locale:?}: legend names the localized sources heading: {legend}"
            );
        }
    }

    #[test]
    fn an_exploration_records_its_grounding_once() {
        // `grounded_concepts`/`grounded_documents` restated, as bare slugs, what the Grounding
        // section already holds as links — and links are the form with readers: citations come
        // from them (`backlinks-sync`), edges come from them (`scan`), and a merge repoints
        // them. Nothing ever read the arrays and no rewriter maintained them, so a merged
        // concept left them naming a page that no longer exists. One record, in the form that
        // is checked.
        let md = render_agents_md(Locale::En, &lk_core::config::VaultDirs::default(), false);
        let section = md
            .split("\n## exploration ")
            .nth(1)
            .expect("exploration page format present")
            .split("\n## ")
            .next()
            .unwrap();
        assert!(
            !section.contains("grounded_"),
            "grounding is recorded as links, not as frontmatter slugs: {section}"
        );
    }

    #[test]
    fn exploration_has_no_machine_owned_section() {
        // No `lore` command renders an exploration page — it is authored whole by the
        // knowledge-synthesis skill that answers the question. A `machine` owner would tell
        // that author to leave the section for a pipeline that never runs, and Grounding is
        // where the page's links live: `backlinks-sync` reads exactly those forward links to
        // derive each cited concept's sources and `source_count`, so an empty Grounding costs
        // the page its entire contribution to the graph, silently and with nothing to repair it.
        let md = render_agents_md(Locale::En, &lk_core::config::VaultDirs::default(), false);
        let section = md
            .split("\n## exploration ")
            .nth(1)
            .expect("exploration page format present")
            .split("\n## ")
            .next()
            .unwrap();
        assert!(
            !section.contains("| machine |"),
            "exploration sections have no machine writer: {section}"
        );
    }

    #[test]
    fn agents_md_contains_all_page_types() {
        let content = render_agents_md(Locale::Ko, &lk_core::config::VaultDirs::default(), true);
        for schema in page_schemas(&lk_core::config::VaultDirs::default(), true) {
            assert!(
                content.contains(&format!("## {}", schema.type_name)),
                "missing page type: {}",
                schema.type_name
            );
        }
    }

    /// `PAGE_FORMATS` is what `holds_managed_pages` asks when deciding whether a directory holds
    /// Lorekeeper's output, and it lived in `lk-core` with nothing tying it to the registry those
    /// formats are actually defined by. A `"document"` renamed to `"bogus"` in the array left all
    /// 812 tests passing, because the schema tests assert their own literals. This is the join:
    /// the formats this tool writes are the registry's, plus the two generated meta-pages, whose
    /// `type` values are named in `vault_path` beside the array so the render sites cannot drift.
    #[test]
    fn every_page_format_is_a_format_the_schema_registry_defines() {
        let mut declared: Vec<&str> = page_schemas(&lk_core::config::VaultDirs::default(), true)
            .iter()
            .map(|schema| schema.type_name)
            .chain([
                lk_core::vault_path::MAP_FORMAT,
                lk_core::vault_path::SCHEMA_FORMAT,
            ])
            .collect();
        declared.sort_unstable();
        let mut admitted = lk_core::vault_path::PAGE_FORMATS.to_vec();
        admitted.sort_unstable();
        assert_eq!(
            admitted, declared,
            "PAGE_FORMATS must be exactly the formats `lore schema` publishes plus the generated \
             meta-pages"
        );
    }

    #[test]
    fn agents_md_omits_personal_pages_when_module_absent() {
        // A domain-neutral vault (no `personal:` module) must not document page formats it
        // never produces — `lore schema` passes `personal = false` in that case.
        let content = render_agents_md(Locale::En, &lk_core::config::VaultDirs::default(), false);
        for core in [
            "concept",
            "daily",
            "document",
            "exploration",
            "weekly-synthesis",
        ] {
            assert!(
                content.contains(&format!("## {core}")),
                "core page type must still be documented: {core}"
            );
        }
        for personal in [
            "work-log",
            "weekly-review",
            "monthly-review",
            "quarterly-review",
            "annual-review",
        ] {
            assert!(
                !content.contains(&format!("## {personal}")),
                "personal page type must be omitted when the module is absent: {personal}"
            );
        }
    }

    #[test]
    fn agents_md_carries_concept_convergence() {
        // The convergence contract is part of the schema: agents that create concept
        // pages read it here, and its heading references must be the LOCALIZED
        // machine-owned headings, never hardcoded English.
        let ko = render_agents_md(Locale::Ko, &lk_core::config::VaultDirs::default(), true);
        assert!(ko.contains("## Concept convergence"));
        assert!(ko.contains("created-this-run"));
        assert!(ko.contains("`lore wiki concepts`"));
        assert!(ko.contains("backlinks-sync"));

        let en = render_agents_md(Locale::En, &lk_core::config::VaultDirs::default(), true);
        assert!(en.contains("## Concept convergence"));

        // The section's machine-owned-heading references must be LOCALIZED, never
        // hardcoded English — the same difference-proof as
        // agents_md_headings_never_hardcoded, scoped to the convergence section.
        let ko_section = ko.split("## Concept convergence").nth(1).unwrap();
        assert!(ko_section.contains("`## 출처`"));
        assert!(!ko_section.contains("`## Sources`"));
        let en_section = en.split("## Concept convergence").nth(1).unwrap();
        assert!(en_section.contains("`## Sources`"));
        assert!(!en_section.contains("`## 출처`"));
    }

    #[test]
    fn agents_md_headings_never_hardcoded() {
        // The Ko and En outputs must produce different headings for the same section,
        // proving they come from locale.strings() and not hardcoded strings.
        let ko = render_agents_md(Locale::Ko, &lk_core::config::VaultDirs::default(), true);
        let en = render_agents_md(Locale::En, &lk_core::config::VaultDirs::default(), true);
        // concept Synthesis section differs
        assert!(ko.contains("`## 핵심`"));
        assert!(en.contains("`## Synthesis`"));
        assert!(!ko.contains("`## Synthesis`"));
        assert!(!en.contains("`## 핵심`"));
        // exploration Question section differs
        assert!(ko.contains("`## 질문`"));
        assert!(en.contains("`## Question`"));
        assert!(!ko.contains("`## Question`"));
        assert!(!en.contains("`## 질문`"));
    }
}
