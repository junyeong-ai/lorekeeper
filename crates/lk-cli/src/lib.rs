//! The `lore` command line, as a library.
//!
//! The binary is a four-line shell over [`run`]. Everything the CLI *is* —
//! the argument tree, the dispatch — lives here so a test can walk the real
//! [`Cli`] rather than a hand-kept description of it, which would drift from
//! the surface it claims to describe.

pub mod commands;

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser)]
#[command(
    name = "lore",
    version,
    about = "Knowledge ingestion pipeline for Obsidian"
)]
pub struct Cli {
    /// Path to config file (default: ./config.yaml or ~/.config/lorekeeper/config.yaml)
    #[arg(long, global = true, env = "LORE_CONFIG")]
    config: Option<PathBuf>,

    /// Override template directory (default: embedded templates; this overrides them)
    #[arg(long, global = true, env = "LORE_TEMPLATE_DIR")]
    template_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand)]
pub enum Command {
    /// Scaffold project files interactively
    Init {
        #[command(subcommand)]
        target: InitTarget,
    },
    /// Run ingestion for one or all sources
    Ingest {
        /// Source ID (omit to run all enabled sources)
        source: Option<String>,
        /// Override date (YYYY-MM-DD, default: today)
        #[arg(long)]
        date: Option<String>,
        /// Preview without writing to vault
        #[arg(long)]
        dry_run: bool,
    },
    /// Manage the task board: what is meant to be done, and what carried into today
    Task {
        #[command(subcommand)]
        cmd: commands::task::TaskCommand,
    },
    /// The day, read off the board — committed work, what woke, what is due
    Agenda {
        /// Read the day against this date instead of today (YYYY-MM-DD)
        #[arg(long)]
        date: Option<String>,
        /// Emit the day as JSON — the contract a skill or a script reads
        #[arg(long)]
        json: bool,
    },
    /// Generate synthesis reports
    Synthesis {
        #[command(subcommand)]
        period: commands::synthesis::Period,
    },
    /// One line per subsystem: the installation, source currency, the LLM queue, page
    /// contracts and the link graph — each naming the command that owns it
    Status,
    /// Check pipeline health (warn if a source is overdue vs ingest.schedule; 48h fallback when unscheduled)
    Health {
        /// Treat first-install (all "never") as failure as well
        #[arg(long)]
        strict: bool,
    },
    /// Audit materialized vault pages against their contracts — text cleanliness, and a section
    /// whose input was recorded and never answered (exits non-zero on any defect, or on a page
    /// it could not read)
    Doctor,
    /// Show personal performance category distribution
    Performance,
    /// Validate config file
    Validate,
    /// Inspect resolved configuration values
    Config {
        #[command(subcommand)]
        cmd: commands::config::ConfigCommand,
    },
    /// Print scheduled-task definitions (ingest, synthesis, maintenance)
    Schedule {
        /// Override the binary path used in the generated entries (default: "lore")
        #[arg(long, default_value = "lore")]
        bin: String,
        /// Output format. `launchd` is preferred on macOS: it runs a job missed during
        /// sleep once the machine wakes, whereas cron drops it.
        #[arg(long, value_enum, default_value_t = commands::schedule::Format::Cron)]
        format: commands::schedule::Format,
        /// Absolute directory holding the installed pipeline scripts (the installer's
        /// `<data>/pipelines`). With it, the daily and weekly entries run those scripts —
        /// ingest and weekly synthesis are only their FIRST stage, and the bare subcommands
        /// never drain the LLM queue or apply its results. Without it, those two entries are
        /// the bare subcommands and the drain must be scheduled some other way.
        #[arg(long)]
        pipeline_dir: Option<std::path::PathBuf>,
    },
    /// Generate wiki/AGENTS.md — single source of truth for page formats
    Schema {
        /// Vault root override (default: vault.root from config)
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Prune the ingest log and drained queue files past the configured retention (default 90 days); streaming event logs are permanent, and each source's latest log entry is kept whatever its age
    Maintenance {
        /// Report what would be pruned, and delete nothing
        #[arg(long)]
        dry_run: bool,
    },
    /// Link graph analysis for the vault
    Graph {
        /// Root directory of the vault (overrides config.yaml vault.root)
        #[arg(long)]
        root: Option<PathBuf>,
        /// Output in JSON format. `ok` is the verdict the exit code carries: false exactly
        /// when the vault contradicts itself
        #[arg(long)]
        json: bool,
        #[command(subcommand)]
        command: commands::graph::GraphCommand,
    },
    /// Answer which concept page a name addresses — the question the ingest pipeline asks
    /// before it routes an extraction, so a caller can ask it before writing a page
    Resolve {
        /// The concept name to look up, in any spelling
        name: String,
        /// Emit the answer as JSON
        #[arg(long)]
        json: bool,
        /// Vault root override (default: vault.root from config)
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Wiki-level utilities (catalog generation, future maintenance ops)
    Wiki {
        #[command(subcommand)]
        cmd: commands::wiki::WikiCommand,
    },
    /// This installation's own lifecycle: what it is, whether it is coherent, and updating it
    #[command(name = "self")]
    Installation {
        #[command(subcommand)]
        cmd: commands::installation::SelfCommand,
    },
    /// Inspect the LLM work queue (`/lore-process` consumes this)
    Queue {
        #[command(subcommand)]
        command: commands::queue::QueueCommand,
    },
}

#[derive(clap::Subcommand)]
pub enum InitTarget {
    /// Interactively write `<vault>/.lorekeeper/credentials.json`
    Credentials {
        /// Vault root override (default: vault.root from config)
        #[arg(long)]
        vault: Option<PathBuf>,
    },
    /// Generate wiki/AGENTS.md (page format reference)
    Schema {
        /// Vault root override (default: vault.root from config)
        #[arg(long)]
        root: Option<PathBuf>,
    },
}

/// Parse the process arguments and run the selected command.
pub async fn run() -> miette::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    let opts = commands::GlobalOptions {
        config: cli.config,
        template_dir: cli.template_dir,
    };

    // `lore graph` has its own exit-code convention (0=sound, 1=contradicted, 2=error) so
    // it returns ExitCode directly instead of going through miette.
    if let Command::Graph {
        root,
        json,
        command,
    } = cli.command
    {
        let code = commands::graph::run(&opts, command, json, root).await;
        std::process::exit(code);
    }

    // Its exit code IS the answer — 0 owned, 1 absent, 2 ambiguous — so it bypasses miette
    // for the same reason `graph` does.
    if let Command::Resolve { name, json, root } = cli.command {
        let code = commands::resolve::run(&opts, name, json, root).await;
        std::process::exit(code);
    }

    match cli.command {
        Command::Init { target } => match target {
            InitTarget::Credentials { vault } => commands::init::credentials(&opts, vault).await,
            InitTarget::Schema { root } => commands::schema::run(&opts, root).await,
        },
        Command::Validate => commands::validate::run(&opts).await,
        Command::Config { cmd } => commands::config::run(&opts, cmd).await,
        Command::Schema { root } => commands::schema::run(&opts, root).await,
        Command::Status => commands::status::run(&opts).await,
        Command::Ingest {
            source,
            date,
            dry_run,
        } => commands::ingest::run(&opts, source, date, dry_run).await,
        Command::Task { cmd } => commands::task::run(&opts, cmd).await,
        Command::Agenda { date, json } => commands::agenda::run(&opts, date, json).await,
        Command::Synthesis { period } => commands::synthesis::run(&opts, period).await,
        Command::Health { strict } => commands::health::run(&opts, strict).await,
        Command::Doctor => commands::doctor::run(&opts).await,
        Command::Performance => commands::performance::run(&opts).await,
        Command::Schedule {
            bin,
            format,
            pipeline_dir,
        } => commands::schedule::run(&opts, &bin, format, pipeline_dir.as_deref()).await,
        Command::Maintenance { dry_run } => commands::maintenance::run(&opts, dry_run).await,
        Command::Graph { .. } | Command::Resolve { .. } => unreachable!(),
        Command::Wiki { cmd } => commands::wiki::run(&opts, cmd).await,
        Command::Installation { cmd } => commands::installation::run(&opts, cmd).await,
        Command::Queue { command } => commands::queue::run(&opts, command).await,
    }
}
