use std::fmt::Write as _;
use std::path::PathBuf;

use lk_core::i18n::{Locale, Strings};
use lk_core::vault_path::{
    CONCEPTS_SUBDIR, DOCUMENTS_SUBDIR, EXPLORATIONS_SUBDIR, WORK_LOG_SUBDIR,
};

use super::{find_config, load_config};

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
}

fn s(name: &'static str, heading: fn(&Strings) -> String, owner: Owner) -> Section {
    Section {
        name,
        heading,
        owner,
    }
}

fn page_schemas(dirs: &lk_core::config::VaultDirs, personal: bool) -> Vec<PageSchema> {
    let mut schemas = vec![
        PageSchema {
            type_name: "concept",
            path_pattern: format!("{}/{CONCEPTS_SUBDIR}/{{slug}}.md", dirs.wiki),
            frontmatter: &[
                "id",
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
        },
        PageSchema {
            type_name: "daily",
            path_pattern: format!("{}/{{source-id}}/YYYY-MM-DD.md", dirs.daily),
            frontmatter: &["id", "title", "created", "labels", "source", "event_count"],
            sections: vec![
                s("Summary", |i| i.summary.to_string(), Owner::Llm),
                s(
                    "Events / Messages",
                    |i| format!("{} / {}", i.key_events, i.key_messages),
                    Owner::Machine,
                ),
                s(
                    "Concepts",
                    |i| i.related_concepts.to_string(),
                    Owner::Machine,
                ),
            ],
        },
        PageSchema {
            type_name: "document",
            path_pattern: format!("{}/{DOCUMENTS_SUBDIR}/{{slug}}.md", dirs.wiki),
            frontmatter: &[
                "id",
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
                s(
                    "Extracted Concepts",
                    |i| i.related_concepts.to_string(),
                    Owner::Machine,
                ),
            ],
        },
        PageSchema {
            type_name: "exploration",
            path_pattern: format!("{}/{EXPLORATIONS_SUBDIR}/{{slug}}.md", dirs.wiki),
            frontmatter: &[
                "id",
                "title",
                "aliases",
                "created",
                "updated",
                "tags",
                "grounded_concepts",
                "grounded_documents",
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
                    Owner::Machine,
                ),
            ],
        },
        PageSchema {
            type_name: "weekly-synthesis",
            path_pattern: format!("{}/{}/YYYY-Www.md", dirs.synthesis, dirs.weekly),
            frontmatter: &[
                "id",
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
                frontmatter: &["id", "title", "created", "labels", "categories", "sources"],
                sections: vec![
                    s("Topic Summary", |i| i.topic_summary.to_string(), Owner::Llm),
                    s("Sources", |i| i.concept_sources.to_string(), Owner::Machine),
                ],
            },
            PageSchema {
                type_name: "weekly-review",
                path_pattern: format!("{}/{}/YYYY-Www.md", dirs.personal, dirs.weekly),
                frontmatter: &["id", "title", "created", "labels", "period", "days_logged"],
                sections: vec![
                    s("Period", |i| i.period.to_string(), Owner::Machine),
                    s("Summary", |i| i.key_summary.to_string(), Owner::Llm),
                    s(
                        "Category Distribution",
                        |i| i.category_distribution.to_string(),
                        Owner::Machine,
                    ),
                ],
            },
            PageSchema {
                type_name: "monthly-review",
                path_pattern: format!("{}/{}/YYYY-MM.md", dirs.personal, dirs.monthly),
                frontmatter: &["id", "title", "created", "labels", "period", "days_logged"],
                sections: vec![
                    s("Period", |i| i.period.to_string(), Owner::Machine),
                    s("Summary", |i| i.key_summary.to_string(), Owner::Llm),
                    s(
                        "Category Distribution",
                        |i| i.category_distribution.to_string(),
                        Owner::Machine,
                    ),
                ],
            },
            PageSchema {
                type_name: "quarterly-review",
                path_pattern: format!("{}/{}/YYYY-Qq.md", dirs.personal, dirs.quarterly),
                frontmatter: &["id", "title", "created", "labels", "period"],
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
            },
            PageSchema {
                type_name: "annual-review",
                path_pattern: format!("{}/{}/YYYY.md", dirs.personal, dirs.annual),
                frontmatter: &["id", "title", "created", "labels", "period"],
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

    let mut out = String::new();
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
        "From an entry point, open the pages it links and follow their `[[wikilinks]]`. \
         Regenerate the entry points with `lore wiki map` / `lore wiki index` / `lore wiki log`."
    )
    .unwrap();

    for schema in page_schemas(dirs, personal) {
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
         from the canonical name, append it to the concept's `aliases` frontmatter so a \
         bare `[[surface]]` resolves to the one page. A surface form containing `/` \
         cannot be linked bare (`[[async/await]]` resolves as a vault path) — link it \
         piped: `[[async-await|async/await]]`. An alias edit is metadata-only: it never \
         renames the page and is not, by itself, a reason to rewrite the body (whether a \
         merge also enriches the synthesis body is the consuming workflow's own \
         judgment). `lore graph lint` surfaces any alias that collides with another \
         concept or shadows a real page."
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
         `[[wikilink]]`s on the ORIGIN page (its `## {related_concepts_heading}` \
         section); `lore graph backlinks-sync` re-derives every concept's \
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
    let (vault_root, locale, dirs, personal) = match root_override {
        Some(r) => {
            let (locale, dirs, personal) = match find_config(opts).and_then(|p| load_config(&p)) {
                Ok(config) => (
                    config.vault.locale(),
                    config.vault.dirs.clone(),
                    config.personal.is_some(),
                ),
                Err(_) => (
                    Locale::default(),
                    lk_core::config::VaultDirs::default(),
                    false,
                ),
            };
            (r, locale, dirs, personal)
        }
        None => {
            let path = find_config(opts)?;
            let config = load_config(&path)?;
            (
                config.vault.root_path(),
                config.vault.locale(),
                config.vault.dirs.clone(),
                config.personal.is_some(),
            )
        }
    };

    let content = render_agents_md(locale, &dirs, personal);

    let wiki_dir = vault_root.join(&dirs.wiki);
    tokio::fs::create_dir_all(&wiki_dir)
        .await
        .map_err(|e| miette::miette!("create wiki dir: {e}"))?;

    let agents_path = wiki_dir.join("AGENTS.md");
    super::write_atomic(agents_path.clone(), content.into_bytes())
        .await
        .map_err(|e| miette::miette!("write AGENTS.md: {e}"))?;

    eprintln!("Wrote {}", agents_path.display());
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
    fn agents_md_contains_all_page_types() {
        let content = render_agents_md(Locale::Ko, &lk_core::config::VaultDirs::default(), true);
        for type_name in [
            "concept",
            "daily",
            "work-log",
            "document",
            "exploration",
            "weekly-synthesis",
            "weekly-review",
            "monthly-review",
            "quarterly-review",
            "annual-review",
        ] {
            assert!(
                content.contains(&format!("## {type_name}")),
                "missing page type: {type_name}"
            );
        }
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
