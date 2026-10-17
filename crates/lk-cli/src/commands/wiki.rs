//! `lore wiki <subcommand>` — vault-wide markdown utilities that live outside the
//! ingest/synthesis hot path.

use std::path::PathBuf;

use lk_core::frontmatter;
use lk_core::i18n::Locale;

use super::{find_config, load_config};

#[derive(clap::Subcommand)]
pub enum WikiCommand {
    /// Verify and migrate legacy imported document ownership
    DocumentIdentity {
        #[command(subcommand)]
        command: super::document_identity::IdentityCommand,
    },
    /// Prepare a reusable lexical search snapshot from current pages (JSON)
    Prepare,
    /// Read a page outline or a revision-checked section in bounded Unicode characters (JSON)
    Read {
        /// Exact vault-relative Markdown path
        path: PathBuf,
        /// Section id returned by the outline; omit to list sections
        #[arg(long)]
        section: Option<usize>,
        /// Outline entry offset, or Unicode character offset within a selected section
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Maximum outline entries (default 20), or section characters (default 4000)
        #[arg(long)]
        limit: Option<usize>,
        /// Require the exact file_revision returned by the previous read
        #[arg(long)]
        revision: Option<String>,
    },
    /// List durable knowledge reviews, including changed or unreadable evidence (JSON)
    Reviews {
        /// Fail for changed evidence or invalid reviews; open judgments remain valid
        #[arg(long)]
        check: bool,
    },
    /// Snapshot evidence revisions and recorded source addresses (JSON)
    Evidence {
        /// Exact vault-relative Markdown paths
        #[arg(required = true)]
        paths: Vec<String>,
        /// Preserve the exact evidence content for later historical review
        #[arg(long)]
        archive: bool,
    },
    /// Inspect explicit internal links with exact source lines and revision (JSON)
    Links {
        /// Exact vault-relative Markdown source path
        path: PathBuf,
        /// Filter by vault-relative target address; existing filesystem aliases are verified
        #[arg(long)]
        target: Option<PathBuf>,
        /// Maximum link occurrences to return
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Skip this many matching occurrences in source order
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Require the file_revision from a previous page of results
        #[arg(long)]
        revision: Option<String>,
    },
    /// Generate `<vault>/<wiki>/index.md` — hierarchical catalog of every vault page
    Index {
        /// Vault root override (default: vault.root from config)
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Generate `<vault>/<wiki>/log.md` — reverse-chronological knowledge timeline
    /// (when each concept/document/exploration first entered the vault, by `created`)
    Log {
        /// Vault root override (default: vault.root from config)
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Generate `<vault>/<wiki>/map.md` — a navigable knowledge map: concepts grouped by
    /// citation cluster (Louvain), hub-first, for agent/human traversal without embeddings
    Map {
        /// Vault root override (default: vault.root from config)
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Regenerate every page derived from the vault: the catalog, the timeline and the map
    Refresh {
        /// Vault root override (default: vault.root from config)
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// List all concept pages in the vault
    Concepts {
        /// Output as JSON array
        #[arg(long)]
        json: bool,
    },
    /// Find the knowledge pages a query reaches — pages the query NAMES first, then pages
    /// whose prose holds it. The lookup `index.md` stopped being able to answer once the
    /// vault outgrew one readable file
    Search {
        /// What to look for. Every whitespace-separated term must appear on a page — or, when
        /// the query is a concept's name, any other name that concept answers to
        query: Vec<String>,
        #[command(flatten)]
        options: DiscoveryOptions,
        #[command(flatten)]
        evidence: BodyEvidenceOptions,
    },
    /// Browse knowledge pages by declared metadata, ordered by title and path
    Browse {
        #[command(flatten)]
        options: DiscoveryOptions,
    },
}

#[derive(clap::Args)]
pub struct BodyEvidenceOptions {
    /// Search literal Markdown body lines, including code and link addresses (live only)
    #[arg(long, conflicts_with = "snapshot")]
    body: bool,
    /// Interpret the body query as a Rust regular expression, matched within each line
    #[arg(long, requires = "body")]
    regex: bool,
    /// Ignore case in body matches (default: case-sensitive)
    #[arg(long, requires = "body")]
    ignore_case: bool,
    /// Surrounding body lines (0..5, default 1); each is limited to 400 Unicode characters
    #[arg(long, requires = "body")]
    context: Option<usize>,
}

#[derive(clap::Args)]
pub struct DiscoveryOptions {
    /// Query the exact snapshot returned by wiki prepare, instead of scanning live files
    #[arg(long)]
    snapshot: Option<String>,
    /// Maximum pages to return, or matching lines with --body (1..100)
    #[arg(long, default_value_t = 20)]
    limit: usize,
    /// Skip this many results in the current ordering
    #[arg(long, default_value_t = 0)]
    offset: usize,
    /// Exact declared category; an empty value selects uncategorized pages
    #[arg(long)]
    category: Option<String>,
    /// Exact recorded source lifecycle status; an empty value selects unknown status
    #[arg(long)]
    source_status: Option<String>,
    /// Exact frontmatter type, such as concept, document or exploration
    #[arg(long = "type")]
    format: Option<String>,
    /// Include daily observations and managed reviews
    #[arg(long)]
    include_sources: bool,
    /// Emit hits, total, next offset and metadata facet counts as JSON
    #[arg(long)]
    json: bool,
}

pub async fn run(opts: &super::GlobalOptions, cmd: WikiCommand) -> miette::Result<()> {
    match cmd {
        WikiCommand::DocumentIdentity { command } => {
            super::document_identity::run(opts, command).await
        }
        WikiCommand::Prepare => {
            let config = load_config(&find_config(opts)?)?;
            let session = lk_vault::MutationSession::acquire(&config.vault.root_path())
                .map_err(|error| miette::miette!("{error}"))?;
            let snapshot = lk_vault::prepare_search(&session, &config.vault.dirs)
                .map_err(|error| miette::miette!("{error}"))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&snapshot)
                    .map_err(|error| miette::miette!("{error}"))?
            );
            Ok(())
        }

        WikiCommand::Read {
            path,
            section,
            offset,
            limit,
            revision,
        } => {
            let config = load_config(&find_config(opts)?)?;
            let view = lk_vault::inspect_document(
                &config.vault.root_path(),
                &path,
                &lk_vault::DocumentOptions {
                    section,
                    offset,
                    limit: limit.unwrap_or(if section.is_some() { 4000 } else { 20 }),
                    revision: revision.as_deref(),
                },
            )
            .map_err(|e| miette::miette!("{e}"))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&view).map_err(|e| miette::miette!("{e}"))?
            );
            Ok(())
        }
        WikiCommand::Reviews { check } => {
            let config = load_config(&find_config(opts)?)?;
            let reports = lk_vault::list_reviews(&config.vault.root_path(), &config.vault.dirs)
                .map_err(|e| miette::miette!("{e}"))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&reports).map_err(|e| miette::miette!("{e}"))?
            );
            if check
                && reports
                    .iter()
                    .any(|report| report.effective_state.requires_attention())
            {
                return Err(miette::miette!(
                    "knowledge reviews require reassessment or repair; see JSON report"
                ));
            }
            Ok(())
        }
        WikiCommand::Evidence { paths, archive } => {
            let config = load_config(&find_config(opts)?)?;
            let root = config.vault.root_path();
            let _mutation = if archive {
                Some(
                    lk_vault::MutationSession::acquire(&root)
                        .map_err(|e| miette::miette!("vault mutation: {e}"))?,
                )
            } else {
                None
            };
            let report = if archive {
                lk_vault::archive_evidence(&root, &paths)
            } else {
                lk_vault::inspect_evidence(&root, &paths)
            }
            .map_err(|e| miette::miette!("{e}"))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&report).map_err(|e| miette::miette!("{e}"))?
            );
            Ok(())
        }
        WikiCommand::Links {
            path,
            target,
            limit,
            offset,
            revision,
        } => {
            let config = load_config(&find_config(opts)?)?;
            let report = lk_vault::inspect_links(
                &config.vault.root_path(),
                &path,
                &lk_vault::LinkOptions {
                    target: target.as_deref(),
                    limit,
                    offset,
                    revision: revision.as_deref(),
                },
            )
            .map_err(|error| miette::miette!("{error}"))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&report)
                    .map_err(|error| miette::miette!("{error}"))?
            );
            Ok(())
        }
        WikiCommand::Index { root } => run_index(opts, root).await,
        WikiCommand::Log { root } => run_log(opts, root).await,
        WikiCommand::Map { root } => run_map(opts, root).await,
        WikiCommand::Refresh { root } => run_refresh(opts, root).await,
        WikiCommand::Concepts { json } => run_concepts(opts, json).await,
        WikiCommand::Search {
            query,
            options,
            evidence,
        } => {
            if evidence.body {
                run_body_search(opts, &query.join(" "), options, evidence).await
            } else {
                run_search(opts, Some(&query.join(" ")), options).await
            }
        }
        WikiCommand::Browse { options } => run_search(opts, None, options).await,
    }
}

async fn run_body_search(
    opts: &super::GlobalOptions,
    query: &str,
    options: DiscoveryOptions,
    evidence: BodyEvidenceOptions,
) -> miette::Result<()> {
    let config = load_config(&find_config(opts)?)?;
    let report = lk_vault::search_body(
        &config.vault.root_path(),
        &config.vault.dirs,
        &lk_vault::BodySearchOptions {
            search: lk_vault::SearchOptions {
                query: Some(query),
                snapshot: options.snapshot.as_deref(),
                limit: options.limit,
                offset: options.offset,
                include_sources: options.include_sources,
                category: options.category.as_deref(),
                source_status: options.source_status.as_deref(),
                format: options.format.as_deref(),
            },
            regex: evidence.regex,
            ignore_case: evidence.ignore_case,
            context: evidence.context.unwrap_or(1),
        },
    )
    .map_err(|error| miette::miette!("{error}"))?;
    if options.json {
        let mut output =
            serde_json::to_value(&report).map_err(|error| miette::miette!("{error}"))?;
        output["mode"] = serde_json::json!("body");
        output["include_sources"] = serde_json::json!(options.include_sources);
        output["category"] = serde_json::json!(options.category);
        output["source_status"] = serde_json::json!(options.source_status);
        output["type"] = serde_json::json!(options.format);
        println!(
            "{}",
            serde_json::to_string_pretty(&output).map_err(|error| miette::miette!("{error}"))?
        );
    } else {
        eprintln!(
            "{} of {} matching lines in {} pages (offset {}; live scan)",
            report.hits.len(),
            report.total,
            report.matching_pages,
            report.offset
        );
        for hit in &report.hits {
            println!(
                "{}:{}:{}  {}  revision={}",
                hit.path, hit.line, hit.column, hit.title, hit.file_revision
            );
            for line in &hit.context {
                println!(
                    "  {}:{}{} {}",
                    line.line,
                    line.column,
                    if line.line == hit.line { ":" } else { "-" },
                    line.text
                );
                if line.truncated_before || line.truncated_after {
                    eprintln!(
                        "  context clipped at line {}; use wiki read with --revision {}",
                        line.line, hit.file_revision
                    );
                }
            }
        }
        if let Some(offset) = report.next_offset {
            eprintln!(
                "next page: repeat with --offset {offset}; live edits may change result positions"
            );
        }
    }
    Ok(())
}

/// Answer which pages a query reaches, in the order the match itself decides.
///
/// The terminal form is columns for an eye; `--json` is the contract, carrying `matched` so a
/// caller can act on the DIFFERENCE between a page the query names and one that mentions it —
/// which is exactly the judgment a skill makes before deciding whether a concept already
/// exists.
async fn run_search(
    opts: &super::GlobalOptions,
    query: Option<&str>,
    options: DiscoveryOptions,
) -> miette::Result<()> {
    if query.is_some_and(|query| query.trim().is_empty()) {
        return Err(miette::miette!(
            "`lore wiki search` needs a query; use `lore wiki browse` to explore metadata"
        ));
    }
    if options.limit == 0 {
        return Err(miette::miette!("`--limit` must be > 0"));
    }
    let config = load_config(&find_config(opts)?)?;
    let result = lk_vault::search(
        &config.vault.root_path(),
        &config.vault.dirs,
        &lk_vault::SearchOptions {
            query,
            snapshot: options.snapshot.as_deref(),
            limit: options.limit,
            offset: options.offset,
            include_sources: options.include_sources,
            category: options.category.as_deref(),
            source_status: options.source_status.as_deref(),
            format: options.format.as_deref(),
        },
    )
    .map_err(|e| miette::miette!("{e}"))?;

    if options.json {
        let mut output = serde_json::to_value(&result).map_err(|e| miette::miette!("{e}"))?;
        output["query"] = serde_json::json!(query);
        output["include_sources"] = serde_json::json!(options.include_sources);
        output["category"] = serde_json::json!(options.category);
        output["source_status"] = serde_json::json!(options.source_status);
        output["type"] = serde_json::json!(options.format);
        println!(
            "{}",
            serde_json::to_string_pretty(&output).map_err(|e| miette::miette!("{e}"))?
        );
        return Ok(());
    }
    eprintln!(
        "{} of {} pages (offset {})",
        result.hits.len(),
        result.total,
        result.offset
    );
    for hit in &result.hits {
        println!("{}  {}  {}", hit.path, hit.title, hit.excerpt);
    }
    if let Some(offset) = result.next_offset {
        eprintln!("next page: repeat with --offset {offset}");
    }
    Ok(())
}

/// Regenerate every page derived from the vault's contents, in one call.
///
/// Each of these is a materialized view, true only while something re-derives it, and every
/// caller that adds pages has to re-derive all of them. One command is what makes naming a subset
/// impossible: a caller cannot refresh two of three, and a page added to the set reaches every
/// caller without any of them changing.
///
/// Every view is attempted even when an earlier one fails, and the failures are reported
/// together. The views are independent — one failing says nothing about the others — and the
/// scheduled pipeline is deliberately not `set -e` for the same reason: a stage that cannot run
/// must not decide for the stages after it.
///
/// `lore schema` stays separate: `AGENTS.md` derives from config, not from the vault, so a run
/// that only added pages has nothing to refresh there.
pub async fn run_refresh(
    opts: &super::GlobalOptions,
    root_override: Option<PathBuf>,
) -> miette::Result<()> {
    use lk_core::vault_path::{INDEX_FILE, MAP_FILE, TIMELINE_FILE};

    let (session, locale, dirs, graph) = resolve_wiki_writer(opts, root_override)?;
    let mut failed: Vec<String> = Vec::new();
    for (page, outcome) in [
        (INDEX_FILE, write_index(&session, locale, &dirs).await),
        (TIMELINE_FILE, write_log(&session, locale, &dirs).await),
        (MAP_FILE, write_map(&session, locale, &dirs, &graph).await),
    ] {
        if let Err(error) = outcome {
            eprintln!("✗ {page}: {error}");
            failed.push(page.to_string());
        }
    }
    if failed.is_empty() {
        return Ok(());
    }
    Err(miette::miette!(
        "{} of the vault's derived pages could not be written: {}",
        failed.len(),
        failed.join(", ")
    ))
}

pub async fn run_map(
    opts: &super::GlobalOptions,
    root_override: Option<PathBuf>,
) -> miette::Result<()> {
    let (session, locale, dirs, graph) = resolve_wiki_writer(opts, root_override)?;
    write_map(&session, locale, &dirs, &graph).await
}

async fn write_map(
    session: &lk_vault::MutationSession,
    locale: Locale,
    dirs: &lk_core::config::VaultDirs,
    graph_config: &lk_core::config::GraphConfig,
) -> miette::Result<()> {
    let vault_root = session.root();
    tracing::info!(vault = %vault_root.display(), "building wiki knowledge map");

    let views = lk_graph::scan::VaultViews::resolve(vault_root, graph_config, dirs)
        .map_err(|e| miette::miette!("scan vault: {e}"))?;
    let graph =
        lk_graph::graph::WikiGraph::build_with_existence(&views.pages, &views.existence, dirs);
    let catalog = lk_graph::map::build_map_catalog(&graph, graph_config, dirs, locale);
    catalog
        .publish(session)
        .map_err(|error| miette::miette!("write map: {error}"))?;
    let rel = std::path::Path::new(&dirs.wiki).join(lk_core::vault_path::MAP_FILE);

    eprintln!("Wrote {}", vault_root.join(&rel).display());
    Ok(())
}

pub async fn run_log(
    opts: &super::GlobalOptions,
    root_override: Option<PathBuf>,
) -> miette::Result<()> {
    let (session, locale, dirs, _graph) = resolve_wiki_writer(opts, root_override)?;
    write_log(&session, locale, &dirs).await
}

async fn write_log(
    session: &lk_vault::MutationSession,
    locale: Locale,
    dirs: &lk_core::config::VaultDirs,
) -> miette::Result<()> {
    tracing::info!(vault = %session.root().display(), "building wiki knowledge log");

    let path = lk_vault::write_timeline(session, locale, dirs)
        .map_err(|e| miette::miette!("write log.md: {e}"))?;

    eprintln!("Wrote {}", path.display());
    Ok(())
}

pub async fn run_index(
    opts: &super::GlobalOptions,
    root_override: Option<PathBuf>,
) -> miette::Result<()> {
    let (session, locale, dirs, _graph) = resolve_wiki_writer(opts, root_override)?;
    write_index(&session, locale, &dirs).await
}

async fn write_index(
    session: &lk_vault::MutationSession,
    locale: Locale,
    dirs: &lk_core::config::VaultDirs,
) -> miette::Result<()> {
    tracing::info!(vault = %session.root().display(), locale = ?locale, "building wiki index");

    let path = lk_vault::write_index(session, locale, dirs)
        .await
        .map_err(|e| miette::miette!("write index.md: {e}"))?;

    eprintln!("Wrote {}", path.display());
    Ok(())
}

/// Acquire once for the whole read/derive/write operation, including a full refresh.
fn resolve_wiki_writer(
    opts: &super::GlobalOptions,
    root_override: Option<PathBuf>,
) -> miette::Result<(
    lk_vault::MutationSession,
    Locale,
    lk_core::config::VaultDirs,
    lk_core::config::GraphConfig,
)> {
    let (root, locale, dirs, graph) = resolve_wiki_context(opts, root_override)?;
    std::fs::create_dir_all(&root).map_err(|e| miette::miette!("create vault: {e}"))?;
    let session = lk_vault::MutationSession::acquire(&root)
        .map_err(|e| miette::miette!("vault mutation: {e}"))?;
    Ok((session, locale, dirs, graph))
}

/// Resolve the context every `lore wiki` page builder needs — vault root, locale, vault
/// dirs, and the graph config (scope, with vault defaults applied). The single resolution
/// path for `index`/`log`/`map`, so they behave identically: an explicit `--root` runs even
/// without a config file (defaults fill in locale/dirs/graph), while the config-derived path
/// surfaces a load error loudly.
fn resolve_wiki_context(
    opts: &super::GlobalOptions,
    root_override: Option<PathBuf>,
) -> miette::Result<(
    PathBuf,
    Locale,
    lk_core::config::VaultDirs,
    lk_core::config::GraphConfig,
)> {
    let super::RootConfig { root, config } = super::resolve_root_config(opts, root_override)?;
    match config {
        Some(config) => Ok((
            root,
            config.vault.locale(),
            config.vault.dirs.clone(),
            config.graph.clone(),
        )),
        // No config file at all (binary-only use under `--root`): defaults fill in.
        None => {
            let dirs = lk_core::config::VaultDirs::default();
            let mut graph = lk_core::config::GraphConfig::default();
            graph.apply_vault_defaults(&dirs);
            Ok((root, Locale::default(), dirs, graph))
        }
    }
}

async fn run_concepts(opts: &super::GlobalOptions, json: bool) -> miette::Result<()> {
    let config_path = find_config(opts)?;
    let config = load_config(&config_path)?;
    let vault_root = config.vault.root_path();
    let concept_dir = vault_root.join(lk_core::vault_path::concepts_dir(&config.vault.dirs));

    let mut entries: Vec<ConceptEntry> = Vec::new();
    // A page this cannot read is a concept the caller will not see. `/lore-process` loads this as
    // its dedup baseline, so an answer that silently omits one has the drain mint a second page
    // for a concept the vault already holds — or overwrite the one it could not read. Collected
    // and refused at the end rather than warned about: a warning on stderr leaves a JSON array
    // that looks complete.
    let mut unreadable: Vec<String> = Vec::new();

    let dir_exists = tokio::fs::metadata(&concept_dir)
        .await
        .map(|m| m.is_dir())
        .unwrap_or(false);

    if !dir_exists {
        if json {
            println!("[]");
        } else {
            println!("# 0 concepts");
        }
        return Ok(());
    }

    let mut dir = tokio::fs::read_dir(&concept_dir)
        .await
        .map_err(|e| miette::miette!("read {}: {e}", concept_dir.display()))?;

    while let Some(entry) = dir.next_entry().await.map_err(|e| miette::miette!("{e}"))? {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "md") {
            continue;
        }
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) => {
                unreadable.push(format!("{}: {e}", path.display()));
                continue;
            }
        };
        let page = match frontmatter::parse_page(&content) {
            Ok(p) => p,
            Err(e) => {
                unreadable.push(format!("{}: {e}", path.display()));
                continue;
            }
        };
        // The file stem, never the `id` frontmatter: a page's ADDRESS is where it sits, which
        // is what a link resolves to and what the graph keys a node on. Reading `id` would be
        // a second answer to the same question, and a page missing the field — hand-authored,
        // or with frontmatter someone edited — would have no address at all and drop out of a
        // registry whose whole job is to be complete.
        let Some(slug) = path.file_stem().and_then(|s| s.to_str()).map(String::from) else {
            unreadable.push(format!("{}: filename is not valid UTF-8", path.display()));
            continue;
        };
        let title = page
            .frontmatter
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or(&slug)
            .to_string();
        let category = page
            .frontmatter
            .get("category")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or_default()
            .to_string();
        let source_count = page.frontmatter.source_count().unwrap_or(0);
        // Registered synonyms beyond the title, so when `/lore-process` loads the vault's
        // concept registry at run-start it can match an alias-only surface form to this
        // concept instead of forking a duplicate page. (The title is already carried in `title`.)
        let aliases = page
            .frontmatter
            .get("aliases")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .filter(|s| *s != title)
                    .map(String::from)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        entries.push(ConceptEntry {
            slug,
            title,
            category,
            source_count,
            aliases,
        });
    }

    if !unreadable.is_empty() {
        return Err(miette::miette!(
            "{} concept page(s) could not be read, so this registry is incomplete and dedup \
             against it would mint a second page for a concept the vault already holds:\n  {}",
            unreadable.len(),
            unreadable.join("\n  ")
        ));
    }

    entries.sort_by(|a, b| a.slug.cmp(&b.slug));

    if json {
        let arr: Vec<serde_json::Value> = entries
            .iter()
            .map(|e| {
                serde_json::json!({
                    "slug": e.slug,
                    "title": e.title,
                    "category": e.category,
                    "source_count": e.source_count,
                    "aliases": e.aliases,
                })
            })
            .collect();
        let out = serde_json::to_string_pretty(&arr).map_err(|e| miette::miette!("JSON: {e}"))?;
        println!("{out}");
    } else {
        println!("# {} concepts", entries.len());
        for e in &entries {
            println!(
                "{}\t{}\t{}\t{}\t{}",
                e.slug,
                e.title,
                e.category,
                e.source_count,
                e.aliases.join(", ")
            );
        }
    }

    Ok(())
}

struct ConceptEntry {
    slug: String,
    title: String,
    category: String,
    source_count: u64,
    aliases: Vec<String>,
}
