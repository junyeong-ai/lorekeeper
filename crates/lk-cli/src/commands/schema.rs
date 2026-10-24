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
                "knowledge_status",
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
                "source_record",
                "source_identity",
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
                "id", "type", "title", "aliases", "created", "updated", "tags", "review",
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

/// Shared language and naming policy, including source-grounded homonym qualification.
fn language_banner(language: &str) -> String {
    format!(
        "**This vault is written in {language}.** New prose uses this language. \
         Concept synthesis preserves an established body's language under its generation \
         policy. Quoted source text and source-written concept names retain their spelling. \
         Distinct subjects sharing a name need a source-grounded scope qualifier \
         (§ Concept convergence)."
    )
}

/// The rule a concept's title follows, named so a test asserts on the rule itself. Every
/// boundary a check could slice the rendered paragraph at — a blank line, a bolded lead-in —
/// is a token the rule's own prose may carry, so each one ends the rule early and lets a
/// reworded judgment ship past.
const TITLE_RULE: &str = "**A concept's title names one subject.** Its base name is COPIED from the \
         material, preserving its spelling and language; do not invent a translation or add \
         an explanatory gloss to an otherwise unambiguous name. When that name already \
         belongs to a different subject, qualify it with scope established by the source. \
         For example, a source describing property contracts may support `Lease — property \
         contracts` when `Lease` already names distributed coordination. This narrow \
         disambiguation exception does not require the source to write the combined title \
         verbatim. Explain that scope in the grounded synthesis; if it is uncertain, retain \
         the uncertainty instead of inventing a distinction. Other source-written forms may \
         be aliases only when they denote the same subject and do not claim another owner. \
         Never add the ambiguous bare homonym as an alias of the new qualified subject.";

/// Shared alias contract for every generated page format.
fn alias_rule(language: &str) -> String {
    format!(
        "**This vault is written in {language}.** Include the material's equivalent \
         {language} form in `aliases` when it is unowned or belongs to the selected same \
         subject; an already-owned homonym is excluded. Propose a new alias only for \
         a name the material WRITES for the same concept in the same scope. A method, \
         component, broader category, narrower application or distinct version is a \
         relationship, not an alternative name. Co-occurrence or a conditional claim does not \
         establish identity: an input, precondition or outcome is not automatically another \
         name for the operation or observation. Omit uncertain new aliases and preserve the \
         source wording until equivalence is established. Existing declared aliases are \
         lookup keys: absence from the currently read citations does not establish that an \
         alias is wrong. Retain that mapping and record uncertainty when its basis cannot be \
         verified. Remove or qualify an existing alias through reviewed curation only when \
         read evidence establishes a different subject, scope or ambiguous identity; pin \
         that evidence and reason. Retention is not renewed semantic approval."
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
         page whose name is registered, exit 1 means no page answers to \
         it, exit 2 means more than one does and `lore graph lint` already reports the pair. \
         Identity preserves case, compatibility characters, punctuation and word boundaries after NFC and whitespace \
         normalization. Different spellings require explicit aliases. Ambiguous names must be \
         resolved before writing. For an absent name, `lore resolve --json` provides an unoccupied \
         `proposed_path`; use that address rather than deriving a filename from the name.\n\n   Ask it for EVERY form the \
         material writes, never the title alone. The other forms are what a page written from \
         a source in another language already answers to, and a hit on ANY of them is the \
         lexical owner. Read that page and verify the same subject and scope before reuse; an owned \
         homonym is not semantic equivalence. Use a source-grounded qualified name when the senses differ, \
         and remove an ambiguous bare alias through reviewed curation rather than assigning it to another sense. \
         Reuse the exact address/title only after that check. Asking only the \
         title can miss a different declared form. A new extraction proposal may not claim \
         an owned name or alias: select the verified exact owner or a source-grounded qualified \
         name for a distinct subject."
            .to_string(),
        "2. **Keep a created-this-run set, and keep it small.** Track proposed subjects and \
         declared names for the current batch. Equal titles alone do not establish equal meaning: \
         separate new proposals never coalesce automatically, even before publication. Consolidate \
         one intended subject only after comparing its scope. Queue extraction uses explicit new or \
         revision-pinned reuse admission. Publish a selected new owner, then rediscover and capture \
         the current context before another result reuses it. Keep the set in a bounded artifact \
         rather than relying on session memory."
            .to_string(),
        "3. **Judge the names `resolve` cannot.** An exit 1 is the answer for a name nothing \
         answers to, not for a concept the vault lacks: an acronym and its expansion, a \
         plural, a team's shorthand are DIFFERENT names for one thing, and no rule about \
         spelling can see it. Ask `lore wiki search` for distinguishing source terms of \
         EACH form the material writes, and also ask the complete declared name so its \
         registered aliases participate in retrieval. Other multi-term queries require \
         every term to appear, so split them when they narrow past useful candidates. Read the hits and \
         judge. In `--json`, `format` says whether a hit is even a concept and `matched` how \
         it was reached: one reached at `text` sits behind every name and summary hit and is \
         the one a limit drops. Reuse the established page and register the surface form as an \
         alias only when they denote the same concept. A broader page may receive a citation \
         when it faithfully covers the claim, but the narrower method or application must not \
         become its alias. Two things a query cannot reach: a rival sharing no word with any \
         form the material writes, and another inflection of a one-word name — `guardrails` \
         does not find `guardrail` — which is worth asking for explicitly. The registry (`lore \
         wiki concepts`) is where the rest would show, and it is a read whose cost grows with \
         the vault. Live search bounds the returned context but scans its scope; an explicit prepared snapshot reuses its lexical index."
            .to_string(),
        "4. **Register equivalent surface forms as aliases.** When the source establishes \
         that a different form names the same concept, append it to the concept's `aliases` frontmatter — \
         `lore resolve` answers with this page for an alias, so the next run's first \
         question lands here instead of minting a variant. Links are unaffected \
         (they address the slug path; the display text is free-form). An alias edit is \
         metadata-only: it never renames the page and is not, by itself, a reason to \
         rewrite the body (whether a merge also enriches the synthesis body is the \
         consuming workflow's own judgment). Use `lore wiki curate --file PLAN --dry-run` \
         to validate guarded metadata and review changes, then publish the reviewed plan without `--dry-run`."
            .to_string(),
        "5. **Slug normalization** is `lore`'s slugify, exactly: NFKC → lowercase → \
         non-alphanumeric to hyphen → collapse runs → trim edges."
            .to_string(),
        String::new(),
        "**Authored synthesis adoption is a baseline, not semantic verification.** \
         Backlink reconciliation can preserve written prose and record its current citation \
         input without an LLM rewrite. Inspect its adoption report, especially when several \
         sources are involved. Before claiming this operation integrated the evidence, read \
         those sources against the complete synthesis. When a rewrite is needed within the \
         authorized scope, use `lore graph resynthesize <exact-concept-path>` and process the \
         typed request; do not hand-write completion markers or rewrite unrelated concepts. \
         A zero queue count alone does not verify adopted prose."
            .to_string(),
        String::new(),
        format!(
            "Machine-owned evidence fields — never hand-write them: a NEW concept page \
         starts with an empty `## {sources_heading}` body and `source_count: 0`; on an \
         EXISTING page leave both exactly as found. Record citations as forward \
         markdown links to the concept page on the ORIGIN page (its \
         `## {related_concepts_heading}` section, link form per `## Links` above); \
         `lore graph backlinks-sync` derives citations and `source_count` from source links, \
         and writes a bounded discovery instruction under `## {sources_heading}`. \
         Complete evidence is queried through `wiki prepare` then `wiki citations \
         EXACT_CONCEPT_PATH --snapshot ID`; substitute the exact current concept address. \
         Custom Sources prose is preserved and reported, never treated as the complete \
         citation authority. Finish any batch that created concept pages OR \
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
        "Pipeline-managed semantic sections carry `llm_inputs`: `<key>` identifies the \
         queued input and `<key>_done` identifies the input whose answer was published. \
         Equality signals completion; a non-empty body does not. For every queued kind, \
         `/lore-process` stages a typed result and `lore queue apply` validates and publishes \
         the body and completion marker together. Never edit a queued target or stamp its \
         markers directly. `lore doctor` reports unanswered inputs, including pending work. \
         Directly authored pages omit this block until a machine writer establishes it."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "The applier owns completion and generation-policy \
         provenance; `lore queue regenerate --task-id ID` requests regeneration from retained \
         input when its guards still match. A concept page's `aliases` is renderer-written and always \
         present, carrying the title and every other name the page answers to; on a page you \
         author yourself, write `aliases` only where the page really does answer to another name."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Each table's `Owner` column names who fills that section's body: `machine` = `lore` \
         writes it, under whichever command produces the page ({}); `LLM` = an agent generates \
         its content; queued results are published by `lore queue apply`. A page you author DIRECTLY \
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
         relative path to `{}/{}` from that page and `{{slug}}` is the exact \
         collection-relative address without `.md`, including nested paths. Resolve \
         names with `lore resolve --json` and use the returned path; never recalculate \
         an existing address from its title. Destinations with spaces or parens are percent-encoded (`%20`, `%28`, \
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
         here for a named topic. Output is bounded by `--limit`; live calls scan their scope; `--snapshot` reuses a prepared lexical index. \
         Query meaningful terms separately when a full question has no results."
    )
    .unwrap();
    writeln!(
        out,
        "- `lore wiki browse --json` — browse by title with category/type counts. \
         Both browse and search accept `--category`, `--type`, `--source-status`, `--limit` and `--offset`. \
         Facet counts describe the query before metadata filters; `total` counts filtered \
         matches and `next_offset` locates the next page. Counts include empty metadata under \
         an empty key. Live pagination reflects current files; `--snapshot` paginates captured revisions."
    )
    .unwrap();
    writeln!(out, "- `lore wiki search <literal> --body --json` — exact raw Markdown body lines, including code and link destinations. Case-sensitive by default; select --regex or --ignore-case explicitly. --context 0..5 bounds surrounding lines; each result supplies physical line/column, clipping flags and file_revision. Totals count matching lines/pages, not semantic claims. The default scans live files; --snapshot searches captured bodies without reopening each file. Both read the full selected scope even with a small --limit. A snapshot is historical evidence, not a guarantee that current files are unchanged.").unwrap();
    writeln!(out, "- `lore wiki prepare` — capture a reusable SQLite lexical search snapshot. Pass its \
        `snapshot` to search/browse with `--snapshot`. It contains captured revisions, not live changes; \
        prepare again after edits or before a new investigation. Missing or mismatched snapshots fail explicitly.").unwrap();
    writeln!(out, "- `lore wiki read <page.md>` — paginated CommonMark outline (20 entries by default). \
        Select `--section ID` for up to 4000 Unicode characters including nested sections. Follow \
        `next_offset` with `--offset`; pass the returned `file_revision` as `--revision` on every \
        subsequent read. Search hits also carry this revision. A mismatch requires fresh retrieval, \
        not reuse of the old section id. Source provenance is returned alongside the content. For declared RSS concept evidence, discover selected articles with `wiki original ORIGIN --evidence-concept CONCEPT`, then read `--event ID --revision PAYLOAD_REV`; the inventory revision pins pagination, not the selected article. Mixed page/event evidence requires both reads.").unwrap();
    writeln!(out, "- `lore wiki provenance <document.md>` — follow declared relations by exact \
        repository/document identity, including superseded_by. Missing and ambiguous owners stay explicit. \
        Use --direction incoming|both or --relation to narrow, then --limit/--offset with the returned \
        collection --revision. Target candidates carry file revisions for wiki read. Recorded status \
        and relations are source declarations, not current truth or inferred support/contradiction.").unwrap();
    writeln!(
        out,
        "- `lore graph --json neighbors <vault-relative.md>` — optional live graph diagnostics for direct incoming/outgoing \
        links across managed pages, including daily evidence. Narrow with `--type concept` or \
        `--direction incoming|outgoing|mutual` before paginating with `--limit` and `--offset`. \
        Types match declared metadata exactly; an empty type selects undeclared pages. \
        `formats` counts all neighbors before filters. Incoming/outgoing include mutual links; \
        a link's direction does not establish its semantic meaning."
    )
    .unwrap();
    writeln!(
        out,
        "- `lore wiki links <page.md> --snapshot ID --direction both` — follow captured incoming/outgoing links in a prepared retrieval session. Paginate with --offset in the same snapshot, then confirm selected sources with revision-checked wiki read. For a single live connection, `lore wiki links <source.md> --target <target.md>` inspects explicit link \
         occurrences as JSON with source line numbers, literal context and file_revision. \
         Omit --target to see all internal links. Supports --limit and --offset; pass \
         --revision from the previous response to reject pagination across source edits. \
         For incoming neighbors inspect the neighbor as source. A link does not establish \
         support or contradiction; read its context."
    )
    .unwrap();
    writeln!(out, "- `{}/index.md` — a small permanent navigation guide after `lore wiki refresh`. The refresh captures SQLite discovery first and prints its snapshot ID. Start a retrieval session with `lore wiki prepare`, then reuse its ID with browse/search/links `--snapshot ID` (links supports `--direction incoming|both`). Confirm current originals with wiki read and the captured file_revision as `--revision`. Markdown knowledge pages remain the source of truth.", dirs.wiki).unwrap();
    writeln!(out, "Run `lore wiki refresh` after knowledge changes to capture SQLite discovery and update the entry guide. Search/browse/links expose the prepared corpus without generated catalog, timeline or map files. An unmarked entry file is preserved and reported as a collision. Structural graph lint checks the knowledge pages and links.").unwrap();

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

        if schema.type_name == "daily" {
            out.push_str("\nOptional `source_record.rss_original` binds a newly rendered RSS daily to an immutable manifest of adapter-collected, pre-classification text. `lore wiki original <daily-path>` returns a bounded inventory; pin its `revision` and select `--event EVENT_ID`, then follow `--text-offset` and `next_offset`. This is normalized collection text, not provider HTTP/XML. `page_bound_collection` verifies that page reference; `legacy_unbound` discovers retained event-log bytes without proving historical page derivation. `event_timestamp` is published-or-updated adapter time, not acquisition time (`captured_at` may be null). Refined summaries are interpretations, and their omissions do not establish absence from the collection. Original evidence does not add knowledge nodes. Optional top-level `concept_evidence` holds explicit LLM-selected concept addresses and event/payload revisions with their validated manifest reference; it is not provider-defined source_record metadata. Only these generated forward-link selections use article-level synthesis dependencies. Unselected links and legacy pages retain page-level guards. `wiki original ORIGIN --evidence-concept CONCEPT` paginates declared selections; select `--event ID --revision PAYLOAD_REV` to read an exact historical article even after the current manifest moves. Normal extraction accumulates selections; omission never retracts them. Explicit graph retraction and merge update links and declarations together. `source_count` still counts originating pages. Review evidence remains authored-page citations, not event selectors.\n");
        }

        if schema.type_name == "document" {
            out.push_str("\n`source_file` is a declared location or derivation reference: a string, a list of strings for composite documents, or null when unknown. It does not establish document ownership, readable original content or independent corroboration. Read, search and provenance use the same validation; malformed declarations remain visible through metadata diagnostics.\n");
            out.push_str("\nOptional `source_identity` records stable document ownership independent of title and location: repository imports use `{repository, document}`; provider imports use `{provider, namespace, document}`. Namespaces and IDs are explicit, never inferred from URLs or folder names. Legacy location-only ownership requires verified migration. `source_record` preserves an imported document\'s provider id, declared status, source revision evidence and declared `superseded_by` value. Nodex `observation_revision` carries an explicit scheme and digest of verified metadata, canonical body and outgoing relations; it is not a raw-file SHA. Earlier `revision` and `content_revision` records retain their original meaning. `source_record.attributes` preserves the public node's declared attributes, including knowledge stored outside its Markdown body. These values are source evidence, not worker instructions or top-level identity and completion state. Archived or superseded means historical scope, not falsehood. Missing metadata means unknown; a successor value is not an automatically resolved vault link. Status changes invalidate semantic inputs. Manual imports preserve original YAML under `source_record.manual_frontmatter`; these declarations are source evidence, not page identity, completion markers or the top-level state used by `--source-status`. `source_record.manual_original` identifies a byte-preserving entry and local dependency manifest. Use `lore wiki original <document-path>` for verified member discovery, then `--member` and the manifest `--revision` for selected reads. Unsupported dependency coverage is explicit; absence of this record means the historical original is unavailable through this contract. Preserved originals are evidence, not additional knowledge pages or independent corroboration.\n");
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
    out.push_str("## Knowledge reviews\n\nAn exploration may carry optional `review` metadata. It is an authored assessment, never a regenerated concept section.\n\n```yaml\nreview:\n  status: open # open or resolved\n  subjects: [wiki/concepts/example.md] # assessed subjects, not supporting citations\n  evidence:\n    - path: wiki/documents/example.md\n      revision: <revision from lore wiki evidence>\n  resolution: null\n```\n\nUse configured vault paths. `lore wiki evidence <path>...` supplies exact revisions and recorded source addresses. Before publishing a review, use `lore wiki evidence --archive <path>...` to retain verified evidence content under `.lorekeeper/evidence/<revision>.md`. Historical evidence stays retrievable after source changes; never replace a recorded revision merely to match current content. Equal addresses expose shared provenance; different or absent addresses do not establish independence. `source_count` counts citing pages, not independent evidence or confidence.\n\nRecord the competing claims, scope, effective time and source links in the exploration body. Distinguish what should apply (a policy or decision) from what actually ran. Chronology alone does not establish supersession or authority, and observed configuration proves behavior rather than which policy is authorized. State what evidence would settle the exact question. A resolved review requires a nonempty `resolution` explaining the evidence and reasoning. Before reassessing, archive the existing review with `lore wiki evidence --archive` and record its archive path in a dated reassessment. After actually rereading changed sources, archive them and explicitly replace `review.evidence` with the revisions supporting the current judgment; historical revisions remain in the archived prior review, not alongside newer revisions of the same path in the current evidence set. An archived review is judgment history, not independent evidence. `lore wiki reviews` reports the recorded status and its effective state without rewriting history. `lore wiki reviews --check` fails for evidence drift or invalid records, but accepts open reviews. Link a concept's conflict callout to the corresponding review.\n\n");
    out.push_str("## Knowledge lifecycle and retrieval\n\n`knowledge_status` is `active` (default when omitted) or `retired`; it is independent of the origin's `source_record.status`. A retired page retains its address, content, citations and history but is excluded from normal discovery. Use `--knowledge-status retired|all` for explicit retrieval. Neither age nor one citation determines retirement. Judge whether the node answers a useful future question and whether its claims have adequate evidence. A missing definition in one captured source is not proof of falsehood: recover primary evidence for a durable subject, narrow unsupported scope, or record an unresolved evidence gap. A reviewed source-only episode can stay in its original document without an active standalone concept. Status changes require a durable `knowledge_review` explicitly naming the subject; new citations never reactivate a retired page automatically. Use `wiki curate` for guarded metadata/review publication and `graph resynthesize` plus the typed queue for semantic sections.\n\nSearch/browse hits expose `knowledge_status`, tags and scoped review previews. An explicit review subject is not a source citation and does not raise `source_count`. Read the review and supporting revisions before interpreting an open or changed assessment. Tags filter themes; aliases identify the same subject. Do not invent aliases merely to improve search or treat generic tags as evidence. `--tag` is repeatable AND filtering, case-insensitive, including preserved manual-source tags.\n\nName retrieval ignores spacing when suggesting a whole declared name (`matched: name-variant`), without changing exact name ownership. For other visible prose use `--phrase` for one contiguous phrase and `--spacing` to ignore whitespace within a visible Markdown block. These operators may be combined, remain lexical candidate retrieval, and never join separate paragraphs/list items/table cells. Exact identity/name and contiguous phrase hits precede scattered terms. Raw `--body` evidence uses literal or explicit regex matching and does not accept these operators. Prepared search, browse, links and provenance share the same `--snapshot`; read selected live pages with their returned file revisions.\n\n");
    out.push_str("Use `wiki citations EXACT_CONCEPT_PATH --snapshot ID` for complete graph-derived evidence, following `next_offset` within the same snapshot. This set differs from ordinary link occurrences; RSS entries preserve event selectors and immutable payload revisions. `total` counts citation entries while `source_count` counts origin pages. Capture does not certify current source bytes or factual confidence.\n\n");
    out.push_str("Incremental `wiki audit-plan` includes reviews scoped by declared subjects, the concept's `knowledge_review` and prior assessment dependencies. Read their current pages, recorded evidence and available historical archives at the returned revisions. Missing, invalid or stale scoped reviews prevent a completed assessment even when omitted from the submitted review list; reassess them explicitly rather than replacing hashes. Valid open findings remain durable follow-up. `run knowledge` reports whole-vault review health separately: unrelated reassessment backlog does not complete this batch or expand its scope.\n\n");
    out.push_str("Guarded `wiki curate` may correct concept categories and tags. Omit `category` to retain it, use a configured category ID to replace it, or null to clear it explicitly through later extraction. Omit `tags` to retain them, provide the complete replacement list, or use [] to clear them. Tags are unique nonblank single lines and describe the concept itself; source tags are not inherited automatically. Evidence and target file revisions must still match at publication.\n\n");
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
    run_with_wait(opts, root_override, false).await
}

pub async fn run_with_wait(
    opts: &super::GlobalOptions,
    root_override: Option<PathBuf>,
    wait_for_vault: bool,
) -> miette::Result<()> {
    // Single override semantics (shared with `wiki`/`graph`): a present config drives
    // locale/dirs/personal even under `--root`; defaults apply ONLY when no config file exists;
    // a present-but-broken config fails loudly (never silently emit AGENTS.md for the wrong dirs).
    let super::RootConfig {
        root: vault_root,
        config,
        pin,
    } = super::resolve_root_config(opts, root_override)?;
    super::wiki::refuse_navigation_inbox_overlap(&vault_root, config.as_ref())?;
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

    std::fs::create_dir_all(&vault_root).map_err(|e| miette::miette!("create vault: {e}"))?;
    let _mutation = super::publication::acquire_mutation(&vault_root, wait_for_vault, &pin).await?;

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
            assert!(legend.contains("`LLM` = an agent generates"), "{legend}");
            assert!(
                legend.contains("published by `lore queue apply`"),
                "{legend}"
            );
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

    /// Managed formats match the published page registry plus the generated schema guide.
    #[test]
    fn every_page_format_is_a_format_the_schema_registry_defines() {
        let mut declared: Vec<&str> = page_schemas(
            &lk_core::config::VaultDirs::default(),
            true,
            Some("tasks.md"),
        )
        .iter()
        .map(|schema| schema.type_name)
        .chain([lk_core::vault_path::SCHEMA_FORMAT])
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
