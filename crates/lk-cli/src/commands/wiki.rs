//! `lore wiki <subcommand>` — vault-wide markdown utilities that live outside the
//! ingest/synthesis hot path.

use std::path::PathBuf;

use lk_vault::VaultStore;

use super::{find_config, load_config};

#[derive(clap::Subcommand)]
pub enum WikiCommand {
    /// Discover preserved manual originals or RSS collection events (read-only JSON)
    Original {
        /// Explicit vault root; no configuration is required for exact original reads
        #[arg(long)]
        root: Option<PathBuf>,
        /// Read an explicitly declared concept/event payload revision, including historical selections
        #[arg(long, conflicts_with = "member")]
        evidence_concept: Option<PathBuf>,
        /// Exact vault-relative document or daily page path
        path: PathBuf,
        /// Exact manifest member path; include a bounded UTF-8 preview when selected
        #[arg(long)]
        member: Option<String>,
        /// Exact RSS event id from discovery; requires --revision
        #[arg(long, conflicts_with = "member")]
        event: Option<String>,
        /// Unicode character offset within selected RSS title + blank line + body
        #[arg(long, default_value_t = 0, requires = "event")]
        text_offset: usize,
        /// Maximum selected RSS text characters (1..65536)
        #[arg(long, default_value_t = 4096, requires = "event")]
        text_limit: usize,
        /// Maximum manifest members or RSS events (1..100)
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Skip this many manifest members
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Pin the immutable manifest digest from a prior response
        #[arg(long)]
        revision: Option<String>,
    },
    /// Inspect exact imported-document provenance and declared relations (read-only JSON)
    Provenance {
        /// Exact vault-relative imported document path
        path: PathBuf,
        /// Read exact captured identities and relations from wiki prepare
        #[arg(long)]
        snapshot: Option<String>,
        /// Follow declarations made by this page, to its identity, or both
        #[arg(long, value_enum, default_value_t = ProvenanceDirection::Outgoing)]
        direction: ProvenanceDirection,
        /// Exact declared relation name (never inferred support or contradiction)
        #[arg(long)]
        relation: Option<String>,
        /// Maximum entries, including metadata issues and target candidates (1..100)
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Skip this many entries in deterministic collection order
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Pin the entire document collection and query to a prior response
        #[arg(long)]
        revision: Option<String>,
    },
    /// Verify and migrate legacy imported document ownership
    DocumentIdentity {
        #[command(subcommand)]
        command: super::document_identity::IdentityCommand,
    },
    /// Prepare a reusable search and exact concept-citation snapshot from current pages (JSON)
    Prepare,
    /// Read the existing snapshot and its verification time without rereading source pages (JSON)
    Snapshot,
    /// Inspect captured links, citations, declared relations and scoped reviews together (JSON)
    Context {
        /// Exact vault-relative Markdown address
        path: PathBuf,
        /// Snapshot returned by wiki snapshot or prepare; current pages are not reread
        #[arg(long)]
        snapshot: String,
        /// Maximum entries per relation group (1..100)
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Skip this many entries in each relation group
        #[arg(long, default_value_t = 0)]
        offset: usize,
    },
    /// Inspect complete citations captured for an exact concept (read-only JSON)
    Citations {
        /// Exact vault-relative concept Markdown address
        path: PathBuf,
        /// Snapshot returned by wiki prepare; current source files are not consulted
        #[arg(long)]
        snapshot: String,
        /// Maximum complete citations (1..100); several RSS events can share one source page
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Skip this many citations in deterministic captured order
        #[arg(long, default_value_t = 0)]
        offset: usize,
    },
    /// Plan semantic audit work for changed or never-reviewed concepts/documents (read-only JSON)
    AuditPlan {
        /// Maximum subjects (1..100); defaults to graph.audit.batch_size
        #[arg(long)]
        limit: Option<usize>,
        /// Re-read exact original batch members after repairs; repeat for each address
        #[arg(long = "path")]
        paths: Vec<String>,
    },
    /// Record revision-guarded search queries for related-document discovery (JSON)
    AuditDiscover {
        /// JSON path, file_revision and 1..16 query objects (query, phrase, spacing)
        #[arg(long)]
        file: PathBuf,
    },
    /// Record completed semantic reads with current subject and dependency guards (JSON)
    AuditRecord {
        /// JSON assessments with exact subject and semantic input revisions
        #[arg(long)]
        file: PathBuf,
    },
    /// Publish reviewed concept metadata and durable reviews with exact evidence guards
    Curate {
        /// Validate evidence, names and reviews without publishing or journal recovery
        #[arg(long)]
        dry_run: bool,
        /// JSON curation plan; see lore-wiki's curation reference for the contract
        #[arg(long)]
        file: PathBuf,
    },
    /// Compute exact file and authored-page revisions for a local Markdown draft (read-only JSON)
    Fingerprint {
        /// Local Markdown draft; does not publish or read configuration
        #[arg(long)]
        file: PathBuf,
    },
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
        /// Inspect captured link addresses from wiki prepare, without reading live pages
        #[arg(long, conflicts_with_all = ["target", "revision"])]
        snapshot: Option<String>,
        /// Traverse incoming, outgoing or both captured link directions (requires --snapshot)
        #[arg(long, value_enum, requires = "snapshot")]
        direction: Option<LinkDirection>,
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
    /// Refresh SQLite navigation and the small entry guide
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
        matching: VisibleSearchOptions,
        #[command(flatten)]
        evidence: BodyEvidenceOptions,
    },
    /// Browse metadata by title, or newest declared created date (not ingestion time)
    Browse {
        #[command(flatten)]
        options: DiscoveryOptions,
        #[command(flatten)]
        dates: BrowseOptions,
    },
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum ProvenanceDirection {
    Outgoing,
    Incoming,
    Both,
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum LinkDirection {
    Outgoing,
    Incoming,
    Both,
}

#[derive(clap::Args)]
pub struct BodyEvidenceOptions {
    /// Search literal Markdown body lines, including code and link addresses
    #[arg(long)]
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

#[derive(Debug, Clone, Copy, Default, clap::ValueEnum, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BrowseSort {
    #[default]
    Title,
    Created,
}

#[derive(Default, clap::Args)]
pub struct BrowseOptions {
    /// Title/path, or declared created newest first with unknown dates last
    #[arg(long, value_enum, default_value_t = BrowseSort::Title)]
    sort: BrowseSort,
    /// Earliest declared created date, inclusive (YYYY-MM-DD); unknown dates excluded
    #[arg(long = "from", value_parser = parse_browse_date)]
    from: Option<jiff::civil::Date>,
    /// Latest declared created date, inclusive (YYYY-MM-DD); not collection time
    #[arg(long = "to", value_parser = parse_browse_date)]
    to: Option<jiff::civil::Date>,
}

fn parse_browse_date(value: &str) -> Result<jiff::civil::Date, String> {
    lk_vault::parse_created_date(value)
        .ok_or_else(|| "expected a valid YYYY-MM-DD date".to_string())
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
    /// Maximum distinct values per metadata facet (1..100, default 20), ordered by value
    #[arg(long)]
    facet_limit: Option<usize>,
    /// Skip this many distinct values in each metadata facet
    #[arg(long)]
    facet_offset: Option<usize>,
    /// Exact declared category; an empty value selects uncategorized pages
    #[arg(long)]
    category: Option<String>,
    /// Exact recorded source lifecycle status; an empty value selects unknown status
    #[arg(long)]
    source_status: Option<String>,
    /// Exact frontmatter type, such as concept, document or exploration
    #[arg(long = "type")]
    format: Option<String>,
    /// Require every tag (case-insensitive), including preserved manual-source tags
    #[arg(long = "tag")]
    tags: Vec<String>,
    /// Include active knowledge, retired knowledge, or both; source lifecycle is separate
    #[arg(long, value_enum, default_value_t = KnowledgeScope::Active)]
    knowledge_status: KnowledgeScope,
    /// Include daily observations and managed reviews
    #[arg(long)]
    include_sources: bool,
    /// Emit hits, total, next offset and metadata facet counts as JSON
    #[arg(long)]
    json: bool,
}

#[derive(Default, clap::Args)]
pub struct VisibleSearchOptions {
    /// Require a contiguous visible phrase instead of separate words
    #[arg(long, conflicts_with = "body")]
    phrase: bool,
    /// Ignore whitespace within visible-text blocks; matches remain retrieval candidates
    #[arg(long, conflicts_with = "body")]
    spacing: bool,
}

#[derive(Clone, Copy, clap::ValueEnum, Default, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum KnowledgeScope {
    #[default]
    Active,
    Retired,
    All,
}

impl KnowledgeScope {
    fn status(self) -> Option<lk_core::frontmatter::KnowledgeStatus> {
        match self {
            Self::Active => Some(lk_core::frontmatter::KnowledgeStatus::Active),
            Self::Retired => Some(lk_core::frontmatter::KnowledgeStatus::Retired),
            Self::All => None,
        }
    }
}

pub async fn run(opts: &super::GlobalOptions, cmd: WikiCommand) -> miette::Result<()> {
    match cmd {
        WikiCommand::Provenance {
            path,
            snapshot,
            direction,
            relation,
            limit,
            offset,
            revision,
        } => {
            let config = load_config(&find_config(opts)?)?;
            let direction = match direction {
                ProvenanceDirection::Outgoing => lk_vault::ProvenanceDirection::Outgoing,
                ProvenanceDirection::Incoming => lk_vault::ProvenanceDirection::Incoming,
                ProvenanceDirection::Both => lk_vault::ProvenanceDirection::Both,
            };
            let report = lk_vault::inspect_provenance(
                &config.vault.root_path(),
                &config.vault.dirs,
                &path,
                &lk_vault::ProvenanceOptions {
                    direction,
                    relation: relation.as_deref(),
                    limit,
                    offset,
                    revision: revision.as_deref(),
                    snapshot: snapshot.as_deref(),
                },
            )
            .map_err(|e| miette::miette!("{e}"))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&report).map_err(|e| miette::miette!("{e}"))?
            );
            Ok(())
        }
        WikiCommand::Original {
            root,
            evidence_concept,
            path,
            member,
            event,
            text_offset,
            text_limit,
            limit,
            offset,
            revision,
        } => {
            let root = super::resolve_root_config(opts, root)?.root;
            if let Some(concept) = evidence_concept {
                if event.is_none() {
                    let view = lk_vault::inspect_rss_evidence_inventory(
                        &root,
                        &path,
                        &concept,
                        revision.as_deref(),
                        offset,
                        limit,
                    )
                    .map_err(|e| miette::miette!("{e}"))?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&view).map_err(|e| miette::miette!("{e}"))?
                    );
                    return Ok(());
                }
                let citation = lk_core::concept::Citation {
                    path: path.to_string_lossy().into(),
                    revision: revision.clone().ok_or_else(|| {
                        miette::miette!("selected event evidence requires --revision")
                    })?,
                    rss_event: Some(lk_core::rss_evidence::RssEventSelector {
                        schema: lk_core::rss_evidence::RSS_EVIDENCE_SCHEMA.into(),
                        event_id: event.clone().expect("clap requires event"),
                    }),
                };
                let view = lk_vault::inspect_rss_citation(
                    &root,
                    &concept,
                    &citation,
                    text_offset,
                    text_limit,
                )
                .map_err(|e| miette::miette!("{e}"))?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&view).map_err(|e| miette::miette!("{e}"))?
                );
                return Ok(());
            }
            if let Some(view) = lk_vault::inspect_rss_original(
                &root,
                &path,
                &lk_vault::RssOriginalOptions {
                    event: event.as_deref(),
                    offset,
                    limit,
                    text_offset,
                    text_limit,
                    revision: revision.as_deref(),
                },
            )
            .map_err(|e| miette::miette!("{e}"))?
            {
                if member.is_some() {
                    return Err(miette::miette!("RSS originals use --event, not --member"));
                }
                println!(
                    "{}",
                    serde_json::to_string_pretty(&view).map_err(|e| miette::miette!("{e}"))?
                );
                return Ok(());
            }
            if event.is_some() {
                return Err(miette::miette!("RSS original unavailable for this page"));
            }
            let view = lk_vault::inspect_original(
                &root,
                &path,
                &lk_vault::OriginalOptions {
                    member: member.as_deref(),
                    limit,
                    offset,
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
        WikiCommand::DocumentIdentity { command } => {
            super::document_identity::run(opts, command).await
        }
        WikiCommand::AuditPlan { limit, paths } => {
            let config = load_config(&find_config(opts)?)?;
            let root = config.vault.root_path();
            let pages = lk_graph::scan::scan_vault(&root, config.graph.scope.follow_links)
                .map_err(|error| miette::miette!("{error}"))?;
            let result = lk_graph::audit::plan_selected_partitioned(
                &root,
                &config.vault.dirs,
                &pages,
                limit.unwrap_or(config.graph.audit.batch_size),
                &paths,
                config.graph.audit.max_input_bytes,
            )
            .map_err(|error| miette::miette!("{error}"))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&result)
                    .map_err(|error| miette::miette!("{error}"))?
            );
            Ok(())
        }
        WikiCommand::AuditDiscover { file } => {
            let config = load_config(&find_config(opts)?)?;
            let request = serde_json::from_slice(
                &std::fs::read(file).map_err(|error| miette::miette!("{error}"))?,
            )
            .map_err(|error| miette::miette!("{error}"))?;
            let root = config.vault.root_path();
            let session = lk_vault::MutationSession::acquire(&root)
                .map_err(|error| miette::miette!("{error}"))?;
            let pages = lk_graph::scan::scan_vault(&root, config.graph.scope.follow_links)
                .map_err(|error| miette::miette!("{error}"))?;
            lk_graph::audit::record_discovery(&session, &config.vault.dirs, &pages, request)
                .map_err(|error| miette::miette!("{error}"))?;
            println!("{}", serde_json::json!({"recorded": true}));
            Ok(())
        }
        WikiCommand::AuditRecord { file } => {
            let (config, config_revision) = super::load_config_with_revision(&find_config(opts)?)?;
            let results: lk_graph::audit::AuditResults = serde_json::from_slice(
                &std::fs::read(file).map_err(|error| miette::miette!("{error}"))?,
            )
            .map_err(|error| miette::miette!("{error}"))?;
            let root = config.vault.root_path();
            let session = lk_vault::MutationSession::acquire(&root)
                .map_err(|error| miette::miette!("{error}"))?;
            let pages = lk_graph::scan::scan_vault(&root, config.graph.scope.follow_links)
                .map_err(|error| miette::miette!("{error}"))?;
            let selection = results
                .assessments
                .iter()
                .map(|assessment| assessment.path.clone())
                .collect::<Vec<_>>();
            let selected = lk_graph::audit::plan_selected_partitioned(
                &root,
                &config.vault.dirs,
                &pages,
                selection.len(),
                &selection,
                config.graph.audit.max_input_bytes,
            )
            .map_err(|error| miette::miette!("{error}"))?;
            if selected
                .candidates
                .iter()
                .any(|candidate| candidate.discovery.is_none())
            {
                return Err(miette::miette!(
                    "audit requires current related-document discovery; run wiki audit-discover, then reread the expanded audit-plan"
                ));
            }
            let _queue_claim = super::queue::claim_writer(&root.join(".lorekeeper/queue"), false)?;
            let mut readiness = super::queue::readiness::AuditReadiness::load(&root)?;
            for candidate in &selected.candidates {
                readiness.require_ready(&root, candidate)?;
            }
            let mut readiness = Some(readiness);
            let recorded = lk_graph::audit::record_with_guard(
                &session,
                &config.vault.dirs,
                &pages,
                results,
                config.graph.audit.max_input_bytes,
                |assessment, plan| {
                    if let Some(readiness) = readiness.take() {
                        readiness
                            .guard(&root, plan)
                            .map_err(|error| lk_graph::GraphError::Io(error.to_string()))?;
                    }
                    super::work::guard_audit(&root, &config, &config_revision, assessment, plan)
                        .map(|completed| {
                            if completed {
                                lk_graph::audit::AuditReadCoverage::CompletedWork
                            } else {
                                lk_graph::audit::AuditReadCoverage::WholeInput
                            }
                        })
                        .map_err(|error| lk_graph::GraphError::Io(error.to_string()))
                },
            )
            .map_err(|error| miette::miette!("{error}"))?;
            println!("{}", serde_json::json!({"recorded":recorded}));
            Ok(())
        }
        WikiCommand::Curate { file, dry_run } => {
            let config = load_config(&find_config(opts)?)?;
            let request: lk_vault::CurationPlan = serde_json::from_slice(
                &std::fs::read(file).map_err(|error| miette::miette!("{error}"))?,
            )
            .map_err(|error| miette::miette!("{error}"))?;
            let report = if dry_run {
                lk_vault::preview_curation(
                    &config.vault.root_path(),
                    &config.vault.dirs,
                    &config.concepts.categories,
                    request,
                )
            } else {
                let session = lk_vault::MutationSession::acquire(&config.vault.root_path())
                    .map_err(|error| miette::miette!("{error}"))?;
                lk_vault::curate(
                    &session,
                    &config.vault.dirs,
                    &config.concepts.categories,
                    request,
                )
            }
            .map_err(|error| miette::miette!("{error}"))?;
            let mut output =
                serde_json::to_value(report).map_err(|error| miette::miette!("{error}"))?;
            output["dry_run"] = serde_json::json!(dry_run);
            println!(
                "{}",
                serde_json::to_string_pretty(&output)
                    .map_err(|error| miette::miette!("{error}"))?
            );
            Ok(())
        }
        WikiCommand::Fingerprint { file } => {
            let raw = std::fs::read_to_string(&file)
                .map_err(|error| miette::miette!("{}: {error}", file.display()))?;
            let page = lk_vault::parse_page(&raw).map_err(|error| miette::miette!("{error}"))?;
            println!(
                "{}",
                serde_json::json!({
                    "file_revision": lk_core::concept::file_revision(&raw),
                    "source_revision": lk_core::concept::source_revision(&page),
                })
            );
            Ok(())
        }
        WikiCommand::Context {
            path,
            snapshot,
            limit,
            offset,
        } => {
            let config = load_config(&find_config(opts)?)?;
            let context = lk_vault::inspect_page_context(
                &config.vault.root_path(),
                &config.vault.dirs,
                &path,
                &lk_vault::ContextOptions {
                    snapshot: &snapshot,
                    limit,
                    offset,
                },
            )
            .map_err(|error| miette::miette!("{error}"))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&context)
                    .map_err(|error| miette::miette!("{error}"))?
            );
            Ok(())
        }
        WikiCommand::Snapshot => {
            let config = load_config(&find_config(opts)?)?;
            let snapshot = lk_vault::inspect_search_snapshot(
                &config.vault.root_path(),
                &config.vault.dirs,
            )
            .map_err(|error| miette::miette!("read discovery snapshot: {error}; run `lore wiki prepare` to capture current pages"))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&snapshot)
                    .map_err(|error| miette::miette!("{error}"))?
            );
            Ok(())
        }
        WikiCommand::Prepare => {
            let config = load_config(&find_config(opts)?)?;
            let session = lk_vault::MutationSession::acquire(&config.vault.root_path())
                .map_err(|error| miette::miette!("{error}"))?;
            let snapshot = prepare_discovery_snapshot(
                &session,
                &config.vault.dirs,
                config.graph.scope.follow_links,
            )?;
            println!(
                "{}",
                serde_json::to_string_pretty(&snapshot)
                    .map_err(|error| miette::miette!("{error}"))?
            );
            Ok(())
        }
        WikiCommand::Citations {
            path,
            snapshot,
            limit,
            offset,
        } => {
            let config = load_config(&find_config(opts)?)?;
            let report = lk_vault::inspect_concept_citations(
                &config.vault.root_path(),
                &config.vault.dirs,
                &path,
                &lk_vault::CitationOptions {
                    snapshot: &snapshot,
                    limit,
                    offset,
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
            snapshot,
            direction,
            target,
            limit,
            offset,
            revision,
        } => {
            let config = load_config(&find_config(opts)?)?;
            if let Some(snapshot) = snapshot {
                let report = lk_vault::inspect_indexed_links(
                    &config.vault.root_path(),
                    &config.vault.dirs,
                    &path,
                    &lk_vault::IndexedLinkOptions {
                        snapshot: &snapshot,
                        direction: match direction.unwrap_or(LinkDirection::Outgoing) {
                            LinkDirection::Outgoing => lk_vault::LinkDirection::Outgoing,
                            LinkDirection::Incoming => lk_vault::LinkDirection::Incoming,
                            LinkDirection::Both => lk_vault::LinkDirection::Both,
                        },
                        limit,
                        offset,
                    },
                )
                .map_err(|error| miette::miette!("{error}"))?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report)
                        .map_err(|error| miette::miette!("{error}"))?
                );
                return Ok(());
            }
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
        WikiCommand::Refresh { root } => run_refresh(opts, root).await,
        WikiCommand::Concepts { json } => run_concepts(opts, json).await,
        WikiCommand::Search {
            query,
            options,
            matching,
            evidence,
        } => {
            if evidence.body {
                run_body_search(opts, &query.join(" "), options, evidence).await
            } else {
                run_search(
                    opts,
                    Some(&query.join(" ")),
                    options,
                    BrowseOptions::default(),
                    matching,
                )
                .await
            }
        }
        WikiCommand::Browse { options, dates } => {
            run_search(opts, None, options, dates, VisibleSearchOptions::default()).await
        }
    }
}

async fn run_body_search(
    opts: &super::GlobalOptions,
    query: &str,
    options: DiscoveryOptions,
    evidence: BodyEvidenceOptions,
) -> miette::Result<()> {
    if options.facet_limit.is_some() || options.facet_offset.is_some() {
        return Err(miette::miette!(
            "body search does not return metadata facets"
        ));
    }
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
                tags: &options.tags,
                knowledge_status: options.knowledge_status.status(),
                ..Default::default()
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
        output["tag_filters"] = serde_json::json!(options.tags);
        output["knowledge_status"] = serde_json::json!(options.knowledge_status);
        output["phrase"] = serde_json::json!(false);
        output["spacing"] = serde_json::json!(false);
        println!(
            "{}",
            serde_json::to_string_pretty(&output).map_err(|error| miette::miette!("{error}"))?
        );
    } else {
        eprintln!(
            "{} of {} matching lines in {} pages (offset {}; {})",
            report.hits.len(),
            report.total,
            report.matching_pages,
            report.offset,
            report.snapshot.as_deref().map_or_else(
                || "live scan".to_owned(),
                |snapshot| format!("snapshot {snapshot}")
            )
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
    dates: BrowseOptions,
    matching: VisibleSearchOptions,
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
            sort: match dates.sort {
                BrowseSort::Title => lk_vault::BrowseSort::Title,
                BrowseSort::Created => lk_vault::BrowseSort::Created,
            },
            created_from: dates.from,
            created_to: dates.to,
            facet_limit: options.facet_limit.unwrap_or(20),
            facet_offset: options.facet_offset.unwrap_or(0),
            snapshot: options.snapshot.as_deref(),
            limit: options.limit,
            offset: options.offset,
            include_sources: options.include_sources,
            category: options.category.as_deref(),
            source_status: options.source_status.as_deref(),
            format: options.format.as_deref(),
            tags: &options.tags,
            knowledge_status: options.knowledge_status.status(),
            phrase: matching.phrase,
            spacing: matching.spacing,
        },
    )
    .map_err(|e| miette::miette!("{e}"))?;

    if options.json {
        let mut output = serde_json::to_value(&result).map_err(|e| miette::miette!("{e}"))?;
        output["query"] = serde_json::json!(query);
        if query.is_none() {
            output["sort"] = serde_json::json!(dates.sort);
            output["from"] = serde_json::json!(dates.from.map(|date| date.to_string()));
            output["to"] = serde_json::json!(dates.to.map(|date| date.to_string()));
            output["date_basis"] = serde_json::json!("declared-created-not-ingestion");
        }
        output["include_sources"] = serde_json::json!(options.include_sources);
        output["category"] = serde_json::json!(options.category);
        output["source_status"] = serde_json::json!(options.source_status);
        output["type"] = serde_json::json!(options.format);
        output["tag_filters"] = serde_json::json!(options.tags);
        output["knowledge_status"] = serde_json::json!(options.knowledge_status);
        output["phrase"] = serde_json::json!(matching.phrase);
        output["spacing"] = serde_json::json!(matching.spacing);
        println!(
            "{}",
            serde_json::to_string_pretty(&output).map_err(|e| miette::miette!("{e}"))?
        );
        return Ok(());
    }
    if query.is_none()
        && (matches!(dates.sort, BrowseSort::Created) || dates.from.is_some() || dates.to.is_some())
    {
        eprintln!("Date basis: declared created; may be source-authored, not ingestion time.");
    }
    eprintln!(
        "{} of {} pages (offset {})",
        result.hits.len(),
        result.total,
        result.offset
    );
    for hit in &result.hits {
        if query.is_none() && matches!(dates.sort, BrowseSort::Created) {
            println!(
                "{}  {}  {}  {}",
                match hit.created_status {
                    lk_vault::CreatedStatus::Valid => hit.created.clone().unwrap_or_default(),
                    lk_vault::CreatedStatus::Missing => "[undated]".to_string(),
                    lk_vault::CreatedStatus::Invalid => format!(
                        "[invalid: {}{}]",
                        hit.created.as_deref().unwrap_or("non-string"),
                        if hit.created_truncated { "…" } else { "" }
                    ),
                },
                hit.path,
                hit.title,
                hit.excerpt
            );
        } else {
            println!("{}  {}  {}", hit.path, hit.title, hit.excerpt);
        }
    }
    if let Some(offset) = result.next_offset {
        eprintln!("next page: repeat with --offset {offset}");
    }
    Ok(())
}

/// Validate input boundaries against the effective root, including `--root` overrides.
pub(super) fn refuse_navigation_inbox_overlap(
    root: &std::path::Path,
    config: Option<&lk_core::config::Config>,
) -> miette::Result<()> {
    let Some(config) = config else { return Ok(()) };
    for (id, source) in config.enabled_sources() {
        if source.source_type == lk_core::config::SourceType::Manual {
            lk_source::manual_inbox_dir(&source.params, root, &config.vault.dirs)
                .map_err(|error| miette::miette!("{id}: {error}"))?;
        }
    }
    Ok(())
}

/// Refresh derived SQLite discovery and its small entry guide. A failed capture
/// never publishes the guide, and refresh does not manage other Markdown files.
pub async fn run_refresh(
    opts: &super::GlobalOptions,
    root_override: Option<PathBuf>,
) -> miette::Result<()> {
    let super::RootConfig { root, config } = super::resolve_root_config(opts, root_override)?;
    refuse_navigation_inbox_overlap(&root, config.as_ref())?;
    let locale = config
        .as_ref()
        .map(|config| config.vault.locale())
        .unwrap_or_default();
    let dirs = config
        .as_ref()
        .map(|config| config.vault.dirs.clone())
        .unwrap_or_default();
    std::fs::create_dir_all(&root).map_err(|error| miette::miette!("create vault: {error}"))?;
    let session = lk_vault::MutationSession::acquire(&root)
        .map_err(|error| miette::miette!("vault mutation: {error}"))?;
    let plan = lk_vault::plan_navigation_entry(session.root(), locale, &dirs)
        .map_err(|error| miette::miette!("validate navigation entry: {error}"))?;
    let snapshot = prepare_discovery_snapshot(
        &session,
        &dirs,
        config
            .as_ref()
            .is_some_and(|config| config.graph.scope.follow_links),
    )?;
    eprintln!("Prepared navigation snapshot {}", snapshot.snapshot);
    session
        .apply(plan)
        .map_err(|error| miette::miette!("refresh navigation entry: {error}"))?;
    eprintln!("Updated SQLite navigation and entry guide");
    Ok(())
}

fn prepare_discovery_snapshot(
    session: &lk_vault::MutationSession,
    dirs: &lk_core::config::VaultDirs,
    follow_links: bool,
) -> miette::Result<lk_vault::SearchSnapshot> {
    let captured =
        lk_graph::backlinks::capture_concept_citations(session.root(), dirs, follow_links)
            .map_err(|error| miette::miette!("capture concept citations: {error}"))?;
    lk_vault::prepare_search_with_citations(session, dirs, captured.projection(), |selected| {
        captured
            .verify(selected)
            .map_err(|error| std::io::Error::other(error.to_string()).into())
    })
    .map_err(|error| miette::miette!("prepare discovery snapshot: {error}"))
}

async fn run_concepts(opts: &super::GlobalOptions, json: bool) -> miette::Result<()> {
    let config_path = find_config(opts)?;
    let config = load_config(&config_path)?;
    let vault_root = config.vault.root_path();
    let concept_dir = lk_core::vault_path::concepts_dir(&config.vault.dirs);
    let reader = lk_vault::FsVault::new(&vault_root);

    let mut entries: Vec<ConceptEntry> = Vec::new();
    // A page this cannot read is a concept the caller will not see. `/lore-process` loads this as
    // its dedup baseline, so an answer that silently omits one has the drain mint a second page
    // for a concept the vault already holds — or overwrite the one it could not read. Collected
    // and refused at the end rather than warned about: a warning on stderr leaves a JSON array
    // that looks complete.
    let mut unreadable: Vec<String> = Vec::new();

    let paths = reader
        .list_markdown_recursive(&concept_dir)
        .await
        .map_err(|e| miette::miette!("read {}: {e}", concept_dir.display()))?;
    for path in paths {
        let page = match reader.read_page(&path).await {
            Ok(Some(page)) => page,
            Ok(None) => {
                unreadable.push(format!(
                    "{}: concept disappeared during inventory",
                    path.display()
                ));
                continue;
            }
            Err(e) => {
                unreadable.push(format!("{}: {e}", path.display()));
                continue;
            }
        };
        if !page.frontmatter.is_concept_or_legacy() {
            continue;
        }
        // The collection-relative address, never the `id` frontmatter: a page's ADDRESS is where it sits, which
        // is what a link resolves to and what the graph keys a node on. Reading `id` would be
        // a second answer to the same question, and a page missing the field — hand-authored,
        // or with frontmatter someone edited — would have no address at all and drop out of a
        // registry whose whole job is to be complete.
        let address = path
            .strip_prefix(&concept_dir)
            .map_err(|error| miette::miette!("{error}"))?
            .with_extension("");
        let Some(slug) = address
            .to_str()
            .map(|value| value.replace(std::path::MAIN_SEPARATOR, "/"))
        else {
            unreadable.push(format!("{}: filename is not valid UTF-8", path.display()));
            continue;
        };
        let title = page
            .frontmatter
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| {
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or(&slug)
            })
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    #[derive(Parser)]
    struct WikiParser {
        #[command(subcommand)]
        command: WikiCommand,
    }

    #[test]
    fn visible_matching_options_belong_only_to_search() {
        WikiParser::command().debug_assert();
        assert!(WikiParser::try_parse_from(["wiki", "browse"]).is_ok());
        assert!(WikiParser::try_parse_from(["wiki", "browse", "--phrase"]).is_err());
        assert!(WikiParser::try_parse_from(["wiki", "browse", "--spacing"]).is_err());
        assert!(
            WikiParser::try_parse_from(["wiki", "search", "CloudRun", "--spacing", "--phrase"])
                .is_ok()
        );
        for matching in ["--phrase", "--spacing"] {
            assert!(
                WikiParser::try_parse_from(["wiki", "search", "CloudRun", "--body", matching])
                    .is_err()
            );
        }
    }
}
