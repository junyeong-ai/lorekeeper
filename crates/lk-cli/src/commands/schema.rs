use std::fmt::Write as _;
use std::path::PathBuf;

use lk_core::i18n::{Locale, Strings};
use lk_core::vault_path::{
    CONCEPTS_SUBDIR, DOCUMENTS_SUBDIR, EXPLORATIONS_SUBDIR, WORK_LOG_SUBDIR,
};

/// The build that renders this page, as the page's `generator` frontmatter states it.
///
/// The single spelling of that value: `lore self status` compares the deployed page against
/// it, so a page rendered here and a page found current there cannot disagree about what
/// "current" is.
pub fn generator() -> String {
    format!("lore {}", env!("CARGO_PKG_VERSION"))
}

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

fn page_schemas(
    dirs: &lk_core::config::VaultDirs,
    personal: bool,
    board: Option<&str>,
) -> Vec<PageSchema> {
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
                lk_core::frontmatter::field::LLM_INPUTS,
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
    // Published only where a board is configured, at the name the config gives it. The format
    // is enabled by `personal.tasks` the way the personal formats are enabled by `personal`,
    // and `AGENTS.md` is what a skill reads to locate a page — so a hardcoded `tasks.md` beside
    // a configured `todo.md` would send it to a file that does not exist.
    if let Some(board) = board {
        schemas.push(PageSchema {
            type_name: lk_core::vault_path::TASK_BOARD_FORMAT,
            path_pattern: lk_core::vault_path::VaultPath::task_board(dirs, board).to_string(),
            frontmatter: &["id", "type", "title", "updated"],
            sections: vec![
                s("Today", |i| i.tasks_today.to_string(), Owner::Machine),
                s("Next", |i| i.tasks_next.to_string(), Owner::Machine),
                s("Waiting", |i| i.tasks_waiting.to_string(), Owner::Machine),
                s("Someday", |i| i.tasks_someday.to_string(), Owner::Machine),
            ],
            machine_writer: Some("lore task"),
        });
    }

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

/// The first statement of the naming rule an agent reads, and the one it anchors on: a
/// concept's name is copied from the source rather than chosen. Named beside the two rules
/// of `## Concept convergence` because it is the same rule said earlier, and a check that
/// reached only the section left this sentence free to say the opposite.
fn language_banner(language: &str) -> String {
    format!(
        "**This vault is written in {language}.** Every word added to a page goes in that \
         language — a summary, a theme, a concept's synthesis, an exploration — whatever \
         language the source arrived in. Two things are not translated by it. Source content \
         is quoted as it stands, and a NAME is not prose: a concept's name is the form its \
         own source writes, so a term a source writes in another language keeps its spelling \
         here (§ Concept convergence says what follows from that)."
    )
}

/// The rule a concept's title follows, named so a test asserts on the rule itself. Every
/// boundary a check could slice the rendered paragraph at — a blank line, a bolded lead-in —
/// is a token the rule's own prose may carry, so each one ends the rule early and lets a
/// reworded judgment ship past.
const TITLE_RULE: &str = "**A concept's title is its name and nothing else.** The title is the address and the \
         lookup key, and the lookup is exact — so a title carrying a parenthetical gloss \
         — `Agent Capability (the unit an agent can call)` — answers to neither the term nor \
         the gloss, and the next mention of the bare term \
         mints a rival page beside it. The name is COPIED from the material you just read, \
         never composed: whether a field has settled on a form in one language or another is a \
         judgment with no stable answer for a term the material is introducing, and two \
         answers to it in one batch is exactly how one concept becomes two pages. Where the \
         material writes several forms, the title is the one it uses AS the term and every \
         other — the gloss, the translation, the expanded acronym, the abbreviation — goes in \
         `aliases`, which is what makes a citation written in any of them resolve here. A form \
         the material does not write is a name no later extraction reproduces.";

/// The other half of the naming rule, composed for the same reason the title rule is named.
/// The superseded wording lived here: it told an agent to add a name the field was said to
/// have established, which is a judgment, and two answers to it in one batch is two pages of
/// one concept.
fn alias_rule(language: &str) -> String {
    format!(
        "**This vault is written in {language}, and where the material writes the concept in \
         {language} too that form is not optional in `aliases`.** A reader who does not know \
         the title searches in the language the vault is written in, and without the alias \
         they reach nothing while the page holds every citation on the subject — after which \
         the next extraction writing that form mints a rival page. What bounds this is the \
         same rule the title follows: an alias is a name the material WRITES, so a concept the \
         sources only ever name one way gets one name. A translation nobody writes is a \
         spelling nobody searches for, and inventing one costs a rival page rather than \
         preventing it."
    )
}

/// The body of `## Concept convergence`, one entry per rendered line and `""` a blank one.
///
/// The section is a contract that several skills restate and that `lore self deploy` ships,
/// so it is composed here and emitted whole rather than written line by line into the render.
/// That is what a test can hold it to: the rendered section is these entries and nothing
/// else, so a sentence cannot reach the vault's contract without joining this list.
fn convergence_body(language: &str, strings: &Strings) -> Vec<String> {
    // The contract is schema rather than skill lore: it states binary-owned invariants
    // (slugify, backlinks-sync field ownership) and names the LOCALIZED headings.
    let sources_heading = strings.concept_sources;
    let related_concepts_heading = strings.related_concepts;
    vec![
        String::new(),
        "One concept = one page. Every agent that creates or merges concept pages \
         follows this exact algorithm, so the wiki converges instead of accumulating \
         variants."
            .to_string(),
        String::new(),
        TITLE_RULE.to_string(),
        String::new(),
        alias_rule(language),
        String::new(),
        "1. **Ask which page owns the name**: `lore resolve <name>` answers with the page a \
         citation of it addresses, by the same rule the ingest pipeline routes an extraction \
         by — so the two cannot disagree about what an existing name is. Exit 0 names the \
         page (reuse its slug and title, never a variant), exit 1 means no page answers to \
         it, exit 2 means more than one does and `lore graph lint` already reports the pair. \
         The match is EXACT on identity, which folds spelling and nothing else: `VectorDB` \
         finds `vector-db`, `k8s` does not find `kubernetes`.\n\n   Ask it for EVERY form the \
         material writes, never the title alone. The other forms are what a page written from \
         a source in another language already answers to, and a hit on ANY of them is the \
         owner — reuse that page and register the forms it does not yet carry. Asking only the \
         title is what mints a rival beside the page that already holds the subject: a name an \
         established page answers to is REFUSED as an alias on a new page, so the one form \
         that would have joined them is dropped by the act of creating the rival."
            .to_string(),
        "2. **Keep a created-this-run set, and keep it small.** `lore resolve` reads what is \
         on disk, so a page this run has minted but not yet materialized is one it answers \
         `absent` for — and two items then mint `RAG` and `Retrieval-Augmented-Generation` \
         independently. That set is a second answer to the question `lore resolve` exists to \
         answer, which is why it is kept short-lived rather than accumulated: materialize \
         each batch as it lands, and the set carries only the batch in hand. Written down \
         rather than remembered — a long run is compacted mid-way, and a set held in context \
         goes with it."
            .to_string(),
        "3. **Judge the names `resolve` cannot.** An exit 1 is the answer for a name nothing \
         answers to, not for a concept the vault lacks: an acronym and its expansion, a \
         plural, a team's shorthand are DIFFERENT names for one thing, and no rule about \
         spelling can see it. Ask `lore wiki search` for two or three distinguishing terms of \
         EACH form the material writes — never a whole multi-word name, since every term must \
         appear and a long query narrows past the very page it is looking for, while a \
         one-word form is the only term it has and is asked whole — then read the hits and \
         judge. In `--json`, `format` says whether a hit is even a concept and `matched` how \
         it was reached: one reached at `text` sits behind every name and summary hit and is \
         the one a limit drops. Reuse the established page and register the surface form as an \
         alias when one matches, and when in doubt prefer the established broader concept over \
         a narrow variant. Two things a query cannot reach: a rival sharing no word with any \
         form the material writes, and another inflection of a one-word name — `guardrails` \
         does not find `guardrail` — which is worth asking for explicitly. The registry (`lore \
         wiki concepts`) is where the rest would show, and it is a read whose cost grows with \
         the vault while a query's does not."
            .to_string(),
        "4. **Register surface forms as aliases.** When a source's surface form differs \
         from the canonical name, append it to the concept's `aliases` frontmatter — \
         `lore resolve` answers with this page for an alias, so the next run's first \
         question lands here instead of minting a variant. Links are unaffected \
         (they address the slug path; the display text is free-form). An alias edit is \
         metadata-only: it never renames the page and is not, by itself, a reason to \
         rewrite the body (whether a merge also enriches the synthesis body is the \
         consuming workflow's own judgment)."
            .to_string(),
        "5. **Slug normalization** is `lore`'s slugify, exactly: NFKC → lowercase → \
         non-alphanumeric to hyphen → collapse runs → trim edges."
            .to_string(),
        String::new(),
        format!(
            "Machine-owned evidence fields — never hand-write them: a NEW concept page \
         starts with an empty `## {sources_heading}` body and `source_count: 0`; on an \
         EXISTING page leave both exactly as found. Record citations as forward \
         markdown links to the concept page on the ORIGIN page (its \
         `## {related_concepts_heading}` section, link form per `## Links` above); \
         `lore graph backlinks-sync` re-derives every concept's \
         `## {sources_heading}` + `source_count` from those forward links wholesale — \
         an entry not backed by a forward link is wiped, and a concept cited by several \
         pages in one batch is counted correctly where hand-written one-ref-per-item \
         entries would undercount. Finish any batch that created concept pages OR \
         citations with `lore graph backlinks-sync`, then `lore wiki refresh`."
        ),
    ]
}

/// Render the AGENTS.md content for a given locale and directory layout.
pub fn render_agents_md(
    locale: Locale,
    dirs: &lk_core::config::VaultDirs,
    personal: bool,
    board: Option<&str>,
) -> String {
    let strings = locale.strings();
    let locale_tag = locale.tag();
    let schemas = page_schemas(dirs, personal, board);

    let mut out = String::new();
    writeln!(
        out,
        "---\ntype: {}\n{}: {}\n---\n",
        lk_core::vault_path::SCHEMA_FORMAT,
        lk_core::frontmatter::field::GENERATOR,
        generator()
    )
    .unwrap();
    writeln!(out, "# Lorekeeper Page Formats").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "> Generated by `lore schema` — locale: {locale_tag}").unwrap();
    writeln!(
        out,
        "> Regenerate after changing `vault.locale`: `lore schema`, which REPLACES this file \
         wholesale — nothing added here survives it, and `lore validate` prompts for it on \
         every run. Vault-specific instructions belong in a file this tool does not generate."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(out, "{}", language_banner(locale.english_name())).unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Pages with an LLM-owned section also carry an `llm_inputs` frontmatter block, and it \
         is a two-part contract, not an opaque cache. `lore ingest` and `lore synthesis` record \
         `<key>` — the hash of the input enqueued — and whoever ANSWERS the section records \
         `<key>_done` with \
         that same hash. A section counts as answered only when the two are EQUAL; a non-empty \
         body never signals it, because a section can be legitimately empty (an extraction that \
         found nothing, a focus-filtered summary), and inferring completion from content would \
         re-enqueue every such result forever. So: never touch a `<key>` input hash, and stamp \
         `<key>_done` when you fill the section it belongs to. Stamping is not bookkeeping: a \
         section whose marker does not match its input is ANSWERED AGAIN by the next render, \
         which writes it EMPTY and re-queues it, so an unstamped body is lost the next time the \
         page is rendered — by `lore ingest` for a daily or document page, by `lore synthesis` \
         for a synthesis or review page. `lore doctor` names every page in that state, the \
         queued ones included. The one exception is `concepts_done`: that section and its marker \
         are both written by `lore queue apply`, and stamping it by hand claims an empty \
         section is answered, which loses the extraction permanently. A page you create \
         directly has no pipeline behind it and omits the whole block."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "A page's `llm_inputs` map is machine-coordination state: each key records the input \
         a section is owed against and its `_done` companion the input a section was written \
         from. Never author or edit those values — the writer of a section stamps its own \
         marker in the same edit. A concept page's `aliases` is renderer-written and always \
         present, carrying the title and every other name the page answers to; on a page you \
         author yourself, write `aliases` only where the page really does answer to another name."
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
         time it runs, so leave those empty and let it.\n\nA concept's `## {}` is yours to \
         write when you create the page and is NOT yours after that. It is owed against the \
         set of pages citing the concept, so once that set moves, `lore graph backlinks-sync` \
         queues a rewrite and a drain writes the section from the sources themselves. Write \
         what the page's own material establishes; do not write a synthesis you would not \
         want restated from the evidence.",
        machine_writers(&schemas),
        strings.concept_sources,
        lk_core::frontmatter::field::SOURCE_COUNT,
        strings.concept_synthesis,
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
        "Ask for what you need and drill in from the answer — navigate, don't scan every file:"
    )
    .unwrap();
    writeln!(
        out,
        "- `lore wiki search <query> --json` — the pages a query reaches, best match first. \
         `matched` says WHY each is here: `identity`/`name` is a page the query names, \
         `summary` one that opens by stating it, `text` one that merely mentions it. Start \
         here for any topic you can phrase; it is the only entry point whose cost does not \
         grow with the vault."
    )
    .unwrap();
    writeln!(
        out,
        "- `{}/map.md` — concepts grouped by citation cluster (the graph's emergent \
         structure); read it to see what a topic sits beside once you have found it.",
        dirs.wiki
    )
    .unwrap();
    writeln!(
        out,
        "- `{}/index.md` — catalog of every page, grouped by category, each with a \
         first-sentence summary. It holds every page and grows with the vault, so read a \
         category's section to survey what exists there — not the whole file to find one page.",
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
         Regenerate every entry point with `lore wiki refresh`."
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

    writeln!(out).unwrap();
    writeln!(out, "## Concept convergence").unwrap();
    for line in convergence_body(locale.english_name(), strings) {
        writeln!(out, "{line}").unwrap();
    }

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
    let (locale, dirs, personal, board) = match config {
        Some(config) => {
            let board = config
                .personal
                .as_ref()
                .and_then(|personal| personal.tasks.as_ref())
                .map(|tasks| tasks.board.clone());
            (
                config.vault.locale(),
                config.vault.dirs.clone(),
                config.personal.is_some(),
                board,
            )
        }
        None => (
            Locale::default(),
            lk_core::config::VaultDirs::default(),
            false,
            None,
        ),
    };

    let content = render_agents_md(locale, &dirs, personal, board.as_deref());

    // A page this tool writes into the vault, so it goes through the writer that refuses to
    // replace a page of another format rather than around it.
    let agents_rel = std::path::Path::new(&dirs.wiki).join(lk_core::vault_path::SCHEMA_FILE);
    let full = vault_root.join(&agents_rel);
    // This write replaces the file wholesale, and `lore validate` prompts for it on every run
    // over a copy that differs for any reason — including a hand edit. Which of the two happened
    // is not decidable from the bytes, so the output names what the write DID rather than
    // guessing: an operator who finds an addendum gone can at least see where it went.
    let replacing = tokio::fs::try_exists(&full).await.unwrap_or(false);

    lk_vault::VaultWriter::new(&vault_root)
        .write_page(&agents_rel, &content)
        .await
        .map_err(|e| miette::miette!("write AGENTS.md: {e}"))?;

    let verb = if replacing { "Replaced" } else { "Wrote" };
    eprintln!("{verb} {}", full.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The spec an agent writes a page from must name the language that vault is written in,
    /// and must name only that one. The naming policy read "the established Korean" in every
    /// vault, so an English vault's spec instructed its own agents to prefer Korean names —
    /// the half of `vault.locale` that switched headings and left the writing behind.
    #[test]
    fn agents_md_names_the_vault_language_and_no_other() {
        use strum::IntoEnumIterator;
        for locale in Locale::iter() {
            let md = render_agents_md(
                locale,
                &lk_core::config::VaultDirs::default(),
                true,
                Some("tasks.md"),
            );
            assert!(
                md.contains(&format!(
                    "This vault is written in {}.",
                    locale.english_name()
                )),
                "{locale:?}: AGENTS.md never states the language pages are authored in"
            );
            // The convergence section names the language too, and separately: the header says
            // what pages are written in, the section says which form a page must also answer
            // to. Pinning only the header let the section stop naming a language at all,
            // after which nothing tells an agent which one a reader will search in.
            let convergence = md
                .split_once("## Concept convergence")
                .expect("the spec carries the convergence contract")
                .1;
            assert!(
                convergence.contains(locale.english_name()),
                "{locale:?}: convergence never names the language a reader searches in"
            );
            // A ban on the NAME, so the spec can never carry an example sentence naming
            // another language either — `An English term keeps its spelling on a Korean page`
            // belongs in the skills and cannot be mirrored here. The cost is accepted: an
            // agent told to prefer another vault's language writes that vault's pages in it,
            // and no phrasing rule separates instructing from mentioning.
            for other in Locale::iter().filter(|l| *l != locale) {
                assert!(
                    !md.contains(other.english_name()),
                    "{locale:?}: the spec names {}, which is not this vault's language",
                    other.english_name()
                );
            }
        }
    }

    #[test]
    fn agents_md_uses_locale_strings() {
        let ko = render_agents_md(
            Locale::Ko,
            &lk_core::config::VaultDirs::default(),
            true,
            Some("tasks.md"),
        );
        assert!(ko.contains("locale: ko"));
        assert!(ko.contains("`## 핵심`"));
        assert!(ko.contains("`## 출처`"));
        assert!(ko.contains("`## 관련`"));

        let en = render_agents_md(
            Locale::En,
            &lk_core::config::VaultDirs::default(),
            true,
            Some("tasks.md"),
        );
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
        let md = render_agents_md(
            Locale::En,
            &lk_core::config::VaultDirs::default(),
            false,
            Some("tasks.md"),
        );
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
        // The Owner column decides whether an agent writes a section, so a reader who does not
        // already know the vocabulary cannot act on the tables. Stating it here rather than in
        // each authoring skill is what keeps one definition: a skill that restates it is a copy
        // to drift, and a skill that omits it leaves its reader unable to act at all.
        //
        // `machine` cannot be equated with any ONE command: `lore ingest` writes only the
        // daily/document/work-log rows, while `lore synthesis` writes every machine row on
        // the five periodic pages and `lore graph backlinks-sync` writes a concept's. Naming
        // a single writer would be false for most of the column, and false in the direction
        // that makes an author leave a section for a command that will never touch it. The
        // localized heading reference must come from the i18n bundle like every other one.
        for (locale, sources) in [(Locale::Ko, "## 출처"), (Locale::En, "## Sources")] {
            let md = render_agents_md(
                locale,
                &lk_core::config::VaultDirs::default(),
                true,
                Some("tasks.md"),
            );
            let legend = md
                .lines()
                .find(|l| l.starts_with("Each table's `Owner` column"))
                .unwrap_or_else(|| panic!("{locale:?}: ownership legend present"));
            // Read off the schemas, so a format added with a fourth writer cannot leave the
            // legend naming only the three that existed when it was written.
            let schemas = page_schemas(
                &lk_core::config::VaultDirs::default(),
                true,
                Some("tasks.md"),
            );
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
        let md = render_agents_md(
            Locale::En,
            &lk_core::config::VaultDirs::default(),
            false,
            Some("tasks.md"),
        );
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
        let md = render_agents_md(
            Locale::En,
            &lk_core::config::VaultDirs::default(),
            false,
            Some("tasks.md"),
        );
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
        let content = render_agents_md(
            Locale::Ko,
            &lk_core::config::VaultDirs::default(),
            true,
            Some("tasks.md"),
        );
        for schema in page_schemas(
            &lk_core::config::VaultDirs::default(),
            true,
            Some("tasks.md"),
        ) {
            assert!(
                content.contains(&format!("## {}", schema.type_name)),
                "missing page type: {}",
                schema.type_name
            );
        }
    }

    /// The generated page carries the `document_type` vocabulary, which is what lets every skill
    /// point at it instead of restating the values. A restatement is only detectable by a
    /// windowed word search, since `data` is an ordinary English word.
    #[test]
    fn agents_md_states_the_document_type_vocabulary() {
        let content = render_agents_md(
            Locale::Ko,
            &lk_core::config::VaultDirs::default(),
            true,
            Some("tasks.md"),
        );
        for value in lk_core::document::DOCUMENT_TYPES {
            assert!(
                content.contains(&format!("`{value}`")),
                "AGENTS.md must state the `{value}` document type — every skill is told to read \
                 the vocabulary there rather than carry its own copy"
            );
        }
    }

    /// `AGENTS.md` tells an agent which frontmatter keys a page format carries, and the template
    /// is what actually renders them. Nothing compared the two, so renaming `source_url` to
    /// `source_uri` in the schema left every author instructed to write a key the vault never
    /// reads — and the schema's own tests, which check its output against itself, all passed.
    ///
    /// One format is exempt, for a stated reason rather than because it was inconvenient:
    /// `exploration` has no template at all — the page is authored through `/lore-wiki`, which
    /// is why the template was deleted. One other renders through Rust rather than a template
    /// and is held to the same property against the renderer that actually writes it.
    #[test]
    fn every_frontmatter_key_the_schema_advertises_is_one_a_template_renders() {
        const AUTHORED_NOT_RENDERED: &[&str] = &["exploration"];
        let templates =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../templates");
        let mut checked = 0;
        for schema in page_schemas(
            &lk_core::config::VaultDirs::default(),
            true,
            Some("tasks.md"),
        ) {
            if AUTHORED_NOT_RENDERED.contains(&schema.type_name) {
                continue;
            }
            // The task board is rendered in Rust: its lines carry a machine-readable stamp a
            // template could write and could not read back, so one function owns both
            // directions. Same property, asked of the renderer that writes it.
            if schema.type_name == lk_task::BOARD_FORMAT {
                let page = lk_task::Board::empty()
                    .render(Locale::default(), jiff::civil::date(2026, 1, 1));
                for key in schema.frontmatter {
                    assert!(
                        page.contains(&format!("{key}:")),
                        "AGENTS.md advertises `{key}` on a task-board page, and \
                         `lk_task::Board::render` never writes it"
                    );
                    checked += 1;
                }
                continue;
            }
            // Daily pages render through the shared base; every other format has its own file.
            let file = if schema.type_name == "daily" {
                "_daily_base.md.jinja".to_string()
            } else {
                format!("{}.md.jinja", schema.type_name)
            };
            let path = templates.join(&file);
            let body = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{} renders through {file}: {e}", schema.type_name));
            for key in schema.frontmatter {
                assert!(
                    body.contains(&format!("{key}:")),
                    "AGENTS.md advertises `{key}` on a {} page, and {file} never renders it",
                    schema.type_name
                );
                checked += 1;
            }
        }
        assert!(checked > 0, "no schema frontmatter keys were compared");
    }

    /// `PAGE_FORMATS` is what `holds_managed_pages` asks when deciding whether a directory holds
    /// Lorekeeper's output, and it lived in `lk-core` with nothing tying it to the registry those
    /// formats are actually defined by. A `"document"` renamed to `"bogus"` in the array left all
    /// 812 tests passing, because the schema tests assert their own literals. This is the join:
    /// the formats this tool writes are the registry's, plus the two generated meta-pages, whose
    /// `type` values are named in `vault_path` beside the array so the render sites cannot drift.
    #[test]
    fn every_page_format_is_a_format_the_schema_registry_defines() {
        let mut declared: Vec<&str> = page_schemas(
            &lk_core::config::VaultDirs::default(),
            true,
            Some("tasks.md"),
        )
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
        let content = render_agents_md(
            Locale::En,
            &lk_core::config::VaultDirs::default(),
            false,
            Some("tasks.md"),
        );
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

    /// The wordings that ask which form a field has settled on — the judgment a copied name
    /// exists to avoid, and the shape the superseded rule took.
    ///
    /// A literal set, lowercased, and that is the whole of what it can do: it refuses the
    /// wording that regressed, never a fresh paraphrase of the same idea. What excludes a
    /// paraphrase is review, and saying otherwise here would be the overclaim that lets one
    /// through unread.
    fn superseded_naming_judgments() -> Vec<String> {
        use strum::IntoEnumIterator;
        [
            "the field actually uses".to_string(),
            "established {language}".to_string(),
        ]
        .into_iter()
        .chain(Locale::iter().map(|l| format!("established {}", l.english_name().to_lowercase())))
        .collect()
    }

    /// The two rules an extraction cannot get wrong without splitting a concept in two. A name
    /// chosen by judging what a field has settled on has no stable answer for the term a source
    /// is introducing, and one run answering it twice mints two pages; a rival looked for by
    /// reading the whole registry is a read that outgrows the session, and a convergence step
    /// that stops running fails silently.
    ///
    /// Each check is anchored to the paragraph that carries the rule rather than run over the
    /// document: a ban on a phrase is a proxy for a judgment no check can make, and over a
    /// whole document it fires on prose that merely contains the words — `transform the field`
    /// has nothing to do with which form a field settled on.
    #[test]
    fn the_naming_and_convergence_rules_survive_a_rewording() {
        use strum::IntoEnumIterator;
        for locale in Locale::iter() {
            let md = render_agents_md(
                locale,
                &lk_core::config::VaultDirs::default(),
                true,
                Some("tasks.md"),
            );
            let (_, section) = md
                .split_once("\n## Concept convergence\n")
                .expect("the spec carries the convergence contract");
            let composed = convergence_body(locale.english_name(), locale.strings());
            let banner = language_banner(locale.english_name());

            // The banner states the same rule the section states, earlier and in one
            // sentence, and it is the first naming statement an agent reads. Checking the
            // section alone left it free to say the opposite.
            assert!(
                md.lines().any(|line| line == banner),
                "{locale:?}: no line of the spec IS the language banner, so the first \
                 statement of the naming rule is not the one composed here"
            );

            // The section IS what `convergence_body` composes, not a text those lines appear
            // somewhere inside. Containment answers whether a rule is in there and nothing
            // about what was written beside it, and a clause appended to a rule reads as part
            // of the rule to every agent downstream. What this cannot do is judge the
            // composition: a line added to `convergence_body` satisfies it by construction,
            // and only the refused wordings below stand against that.
            assert_eq!(
                section,
                format!("{}\n", composed.join("\n")),
                "{locale:?}: the rendered section is not what `convergence_body` composes — \
                 prose was written into the render around the composed lines, where a reader \
                 takes it for part of the rule"
            );

            // The named rules are the rules only while the section is composed FROM them: a
            // paragraph written back inline leaves the constant intact for the checks below
            // to read while the vault reads something else.
            for (name, rule) in [
                ("TITLE_RULE", TITLE_RULE.to_string()),
                ("alias_rule", alias_rule(locale.english_name())),
            ] {
                assert!(
                    composed.contains(&rule),
                    "{locale:?}: no line of the contract is {name}, so the rule it states is \
                     not the rule this vault ships"
                );
            }
            assert!(
                TITLE_RULE.contains("COPIED"),
                "{locale:?}: the title rule no longer says a name is copied rather than composed"
            );
            assert!(
                alias_rule(locale.english_name()).contains("the material WRITES"),
                "{locale:?}: the alias rule no longer says an alias is a name the material \
                 writes, so a sentence adding one it does not write contradicts nothing"
            );

            // Over the WHOLE document, not the paragraphs that state the rule. Narrowing
            // this to the rule's own text is what left a style note elsewhere free to say
            // the opposite, and the document has no paragraph where the superseded judgment
            // would be right. Compared lowercased, since a sentence-initial capital is both
            // the only way such a phrase legitimately opens a sentence and the only way past
            // a case-sensitive check.
            //
            // What it costs, which is real: the spec can never write `established <its own
            // language>` in an innocent sense either — `a vault already authored in Korean`
            // rather than `an established Korean vault`; it cannot state its own
            // cross-language case by naming the languages, which is why step 1 says `a page
            // written from a source in another language` instead; and it cannot QUOTE the
            // superseded wording to warn a maintainer off restoring it, so that warning
            // lives in this comment and in the commit history rather than in the document.
            let document = md.to_lowercase();
            for judgment in superseded_naming_judgments() {
                assert!(
                    !document.contains(&judgment),
                    "{locale:?}: the spec says `{judgment}` somewhere, which asks which form \
                     a field settled on — the judgment a copied name exists to avoid"
                );
            }

            let convergence = composed.join("\n");
            assert!(
                convergence.contains("EVERY form"),
                "{locale:?}: convergence no longer asks the resolver for every form the \
                 material writes, so a page written in another language is unreachable from \
                 the one name an extraction happens to carry"
            );
            assert!(
                convergence.contains("`lore wiki search`"),
                "{locale:?}: convergence never names the bounded question, so the only way to \
                 find a rival is a read that grows with the vault"
            );
        }
    }

    /// The contract as an agent reads it, pinned so a change to it is a change someone saw.
    ///
    /// Ten rounds of review each found a reworded naming rule reaching this document, and
    /// what they have in common is not the wording — it is that nobody looked at the RENDERED
    /// text. Every other check here asks whether some sentence is present or absent; this one
    /// asks nothing and states a fact, so it catches the rewording none of those checks was
    /// written for, including the ones nobody has thought of yet. Its cost is the point:
    /// editing the contract fails until `cargo insta accept` records the new text, and that
    /// acceptance lands in the diff as the sentence an agent will read rather than as a change
    /// to a `format!` string.
    ///
    /// It does not replace the refused wordings: an accepted snapshot is silent, and the ban
    /// still fails.
    #[test]
    fn the_rendered_contract_is_the_one_that_was_read() {
        use strum::IntoEnumIterator;
        // Exhaustive because every conditional in `render_agents_md` and `page_schemas` only
        // ADDS — `if personal`, `if let Some(board)` — so each rendering is a superset of the
        // one below it. A future `else`, or prose written for the absent case, is a rendering
        // no case here enters, and it has to add one.
        for locale in Locale::iter() {
            for (scope, personal, board) in [
                ("personal-board", true, Some("tasks.md")),
                ("personal", true, None),
                ("core", false, None),
            ] {
                let md = render_agents_md(
                    locale,
                    &lk_core::config::VaultDirs::default(),
                    personal,
                    board,
                );
                // The generator stamp is the binary's version, which every release changes.
                // Left in, each release would fail every case and be accepted unread — the
                // habit this test exists to prevent. Anchored to the field it belongs to: a
                // substitution matched anywhere would blank whatever prose resembled it and
                // ship that unreviewed.
                let stamp = format!(
                    "{}: {}",
                    lk_core::frontmatter::field::GENERATOR,
                    generator()
                );
                let md = md.replace(
                    &stamp,
                    &format!("{}: lore <version>", lk_core::frontmatter::field::GENERATOR),
                );
                insta::assert_snapshot!(format!("agents-{}-{scope}", locale.tag()), md);
            }
        }
    }

    /// The contract is not the only copy the binary ships, and it is not the copy that names
    /// a concept. `processing-kinds.md` restates the naming rule for the drain session that
    /// writes concept pages, so a wording the spec refuses has to be refused there too — a
    /// judgment the spec cannot state and a skill can is the same regression by a shorter
    /// path to the page.
    #[test]
    fn no_shipped_skill_states_the_superseded_naming_rule() {
        let judgments = superseded_naming_judgments();
        for skill in lk_dist::skill_names() {
            for file in lk_dist::skill_files(skill) {
                let prose = file.contents.to_lowercase();
                for judgment in &judgments {
                    assert!(
                        !prose.contains(judgment),
                        "{skill}/{}: says `{judgment}`, which asks which form a field settled \
                         on — the judgment a copied name exists to avoid, refused in every \
                         artifact this binary ships",
                        file.relative
                    );
                }
            }
        }
    }

    /// The convergence rule lives in the contract and is restated by every skill that runs
    /// it, and three rounds of review found a copy that had been left behind each time. A
    /// reviewer is not a gate.
    ///
    /// Scoped per FILE rather than per paragraph: a skill that names both commands is one
    /// that states the dedup baseline, and it must say which forms it asks. Asking the same
    /// of a paragraph looked tighter and was weaker — splitting a step in two put the copy
    /// out of range while the other copies kept the count up, which is the miss this exists
    /// to prevent. Nothing is banned, so prose that merely shares the words cannot fire it.
    #[test]
    fn every_skill_that_states_the_dedup_baseline_asks_every_written_form() {
        let mut stated = 0;
        for skill in lk_dist::skill_names() {
            for file in lk_dist::skill_files(skill) {
                // A table row names commands; it does not state a rule. `lore-ingest`'s
                // command reference lists `lore resolve`, and one added row would otherwise
                // make it answer for a baseline it never states.
                let mut fenced = false;
                let prose: String = file
                    .contents
                    .lines()
                    .filter(|l| {
                        let l = l.trim_start();
                        if l.starts_with("```") {
                            fenced = !fenced;
                            return false;
                        }
                        // A table row names commands and a fenced block shows how to type
                        // them; neither states a rule for the check below to hold it to.
                        !fenced && !l.starts_with('|')
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if !(prose.contains("lore resolve") && prose.contains("lore wiki search")) {
                    continue;
                }
                stated += 1;
                // The literal phrase, deliberately: the rule lives in one contract and
                // several restatements, and a shared spelling is what makes the set of them
                // findable. A rewording that means the same thing fails here, and the message
                // says so rather than accusing the text of dropping the rule.
                assert!(
                    prose.contains("EVERY form"),
                    "{skill}/{}: states the dedup baseline without the words `EVERY form`. \
                     The copies of this rule are kept together by that spelling, so a \
                     rewording keeps it — and if the rule itself changed, `convergence_body` \
                     is what changes first",
                    file.relative
                );
            }
        }
        assert!(
            stated >= 3,
            "only {stated} skill file(s) state the dedup baseline — the anchor stopped \
             matching and this test now guards nothing"
        );
    }

    #[test]
    fn agents_md_carries_concept_convergence() {
        // The convergence contract is part of the schema: agents that create concept
        // pages read it here, and its heading references must be the LOCALIZED
        // machine-owned headings, never hardcoded English.
        let ko = render_agents_md(
            Locale::Ko,
            &lk_core::config::VaultDirs::default(),
            true,
            Some("tasks.md"),
        );
        assert!(ko.contains("## Concept convergence"));
        assert!(ko.contains("created-this-run"));
        assert!(ko.contains("`lore resolve`"));
        assert!(ko.contains("`lore wiki search`"));
        assert!(ko.contains("backlinks-sync"));

        let en = render_agents_md(
            Locale::En,
            &lk_core::config::VaultDirs::default(),
            true,
            Some("tasks.md"),
        );
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
        let ko = render_agents_md(
            Locale::Ko,
            &lk_core::config::VaultDirs::default(),
            true,
            Some("tasks.md"),
        );
        let en = render_agents_md(
            Locale::En,
            &lk_core::config::VaultDirs::default(),
            true,
            Some("tasks.md"),
        );
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
