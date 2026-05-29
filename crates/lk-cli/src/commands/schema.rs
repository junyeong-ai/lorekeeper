use std::fmt::Write as _;
use std::path::PathBuf;

use lk_core::i18n::{Locale, Strings};

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
    /// Closure that resolves the localized heading from `Strings`.
    heading: Box<dyn Fn(&Strings) -> String>,
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

fn s(
    name: &'static str,
    heading_fn: impl Fn(&Strings) -> String + 'static,
    owner: Owner,
) -> Section {
    Section {
        name,
        heading: Box::new(heading_fn),
        owner,
    }
}

fn page_schemas(dirs: &lk_core::config::VaultDirs) -> Vec<PageSchema> {
    vec![
        PageSchema {
            type_name: "concept",
            path_pattern: format!("{}/concepts/{{slug}}.md", dirs.wiki),
            frontmatter: &[
                "id",
                "title",
                "aliases",
                "created",
                "updated",
                "category",
                "source_count",
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
            frontmatter: &["id", "title", "created", "labels", "source", "events_count"],
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
            type_name: "work-log",
            path_pattern: format!("{}/work-log/YYYY-MM-DD.md", dirs.personal),
            frontmatter: &["id", "title", "created", "labels", "categories", "sources"],
            sections: vec![
                s("Topic Summary", |i| i.topic_summary.to_string(), Owner::Llm),
                s("Sources", |i| i.concept_sources.to_string(), Owner::Machine),
            ],
        },
        PageSchema {
            type_name: "document",
            path_pattern: format!("{}/documents/{{slug}}.md", dirs.wiki),
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
            path_pattern: format!("{}/explorations/{{slug}}.md", dirs.wiki),
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
        PageSchema {
            type_name: "weekly-personal",
            path_pattern: format!("{}/{}/YYYY-Www.md", dirs.personal, dirs.weekly),
            frontmatter: &["id", "title", "created", "labels", "period", "days_logged"],
            sections: vec![
                s("Period", |i| i.period.to_string(), Owner::Machine),
                s("Summary", |i| i.key_summary.to_string(), Owner::Llm),
                s(
                    "Categories",
                    |i| i.work_categories.to_string(),
                    Owner::Machine,
                ),
            ],
        },
        PageSchema {
            type_name: "monthly-summary",
            path_pattern: format!("{}/{}/YYYY-MM.md", dirs.personal, dirs.monthly),
            frontmatter: &["id", "title", "created", "labels", "period", "days_logged"],
            sections: vec![
                s("Period", |i| i.period.to_string(), Owner::Machine),
                s("Summary", |i| i.key_summary.to_string(), Owner::Llm),
                s(
                    "Categories",
                    |i| i.work_categories.to_string(),
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
                    "Categories",
                    |i| i.work_categories.to_string(),
                    Owner::Machine,
                ),
            ],
        },
    ]
}

/// Render the AGENTS.md content for a given locale and directory layout.
pub fn render_agents_md(locale: Locale, dirs: &lk_core::config::VaultDirs) -> String {
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

    for schema in page_schemas(dirs) {
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

    out
}

pub async fn run(opts: &super::GlobalOpts, root_override: Option<PathBuf>) -> miette::Result<()> {
    let (vault_root, locale, dirs) = match root_override {
        Some(r) => {
            let (locale, dirs) = match find_config(opts).and_then(|p| load_config(&p)) {
                Ok(config) => (config.vault.locale(), config.vault.dirs.clone()),
                Err(_) => (Locale::default(), lk_core::config::VaultDirs::default()),
            };
            (r, locale, dirs)
        }
        None => {
            let path = find_config(opts)?;
            let config = load_config(&path)?;
            (
                config.vault.root_path(),
                config.vault.locale(),
                config.vault.dirs.clone(),
            )
        }
    };

    let content = render_agents_md(locale, &dirs);

    let wiki_dir = vault_root.join(&dirs.wiki);
    tokio::fs::create_dir_all(&wiki_dir)
        .await
        .map_err(|e| miette::miette!("create wiki dir: {e}"))?;

    let agents_path = wiki_dir.join("AGENTS.md");
    tokio::fs::write(&agents_path, &content)
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
        let ko = render_agents_md(Locale::Ko, &lk_core::config::VaultDirs::default());
        assert!(ko.contains("locale: ko"));
        assert!(ko.contains("`## 핵심`"));
        assert!(ko.contains("`## 출처`"));
        assert!(ko.contains("`## 관련`"));

        let en = render_agents_md(Locale::En, &lk_core::config::VaultDirs::default());
        assert!(en.contains("locale: en"));
        assert!(en.contains("`## Synthesis`"));
        assert!(en.contains("`## Sources`"));
        assert!(en.contains("`## Related`"));
    }

    #[test]
    fn agents_md_contains_all_page_types() {
        let content = render_agents_md(Locale::Ko, &lk_core::config::VaultDirs::default());
        for type_name in [
            "concept",
            "daily",
            "work-log",
            "document",
            "exploration",
            "weekly-synthesis",
            "weekly-personal",
            "monthly-summary",
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
    fn agents_md_headings_never_hardcoded() {
        // The Ko and En outputs must produce different headings for the same section,
        // proving they come from locale.strings() and not hardcoded strings.
        let ko = render_agents_md(Locale::Ko, &lk_core::config::VaultDirs::default());
        let en = render_agents_md(Locale::En, &lk_core::config::VaultDirs::default());
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
