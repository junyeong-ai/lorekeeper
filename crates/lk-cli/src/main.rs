mod commands;

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser)]
#[command(
    name = "lore",
    version,
    about = "Knowledge ingestion pipeline for Obsidian"
)]
struct Cli {
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
enum Command {
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
    /// Generate synthesis reports
    Synthesis {
        #[command(subcommand)]
        period: commands::synthesis::Period,
    },
    /// Show last ingest time per source
    Status,
    /// Check pipeline health (warn if a source is overdue vs ingest.schedule; 48h fallback when unscheduled)
    Health {
        /// Treat first-install (all "never") as failure as well
        #[arg(long)]
        strict: bool,
    },
    /// Audit materialized vault pages against the text-cleanliness contract (exits non-zero on any defect)
    Doctor,
    /// Show personal performance category distribution
    Performance,
    /// Validate config file
    Validate,
    /// Print crontab entries (ingest, synthesis, maintenance)
    Schedule {
        /// Override the binary name in cron lines (default: "lore")
        #[arg(long, default_value = "lore")]
        bin: String,
    },
    /// Generate wiki/AGENTS.md — single source of truth for page formats
    Schema {
        /// Vault root override (default: vault.root from config)
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Prune the ingest log and drained queue files past the configured retention (default 90 days); streaming event logs are permanent
    Maintenance,
    /// Link graph analysis for the vault
    Graph {
        /// Root directory of the vault (overrides config.yaml vault.root)
        #[arg(long)]
        root: Option<PathBuf>,
        /// Output in JSON format (envelope: {"ok": true, "data": …})
        #[arg(long)]
        json: bool,
        #[command(subcommand)]
        command: commands::graph::GraphCommand,
    },
    /// Wiki-level utilities (catalog generation, future maintenance ops)
    Wiki {
        #[command(subcommand)]
        cmd: commands::wiki::WikiCommand,
    },
    /// Inspect the LLM work queue (`/lore-process` consumes this)
    Queue {
        #[command(subcommand)]
        command: commands::queue::QueueCommand,
    },
}

#[derive(clap::Subcommand)]
enum InitTarget {
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

#[tokio::main]
async fn main() -> miette::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    let opts = commands::GlobalOptions {
        config: cli.config,
        template_dir: cli.template_dir,
    };

    // `lore graph` has its own exit-code convention (0=ok, 1=findings, 2=error) so
    // it returns ExitCode directly instead of going through miette.
    if let Command::Graph {
        root,
        json,
        command,
    } = cli.command
    {
        let code = commands::graph::run(&opts, command, json, root);
        std::process::exit(code);
    }

    match cli.command {
        Command::Init { target } => match target {
            InitTarget::Credentials { vault } => commands::init::credentials(&opts, vault).await,
            InitTarget::Schema { root } => commands::schema::run(&opts, root).await,
        },
        Command::Validate => commands::validate::run(&opts).await,
        Command::Schema { root } => commands::schema::run(&opts, root).await,
        Command::Status => commands::status::run(&opts).await,
        Command::Ingest {
            source,
            date,
            dry_run,
        } => commands::ingest::run(&opts, source, date, dry_run).await,
        Command::Synthesis { period } => commands::synthesis::run(&opts, period).await,
        Command::Health { strict } => commands::health::run(&opts, strict).await,
        Command::Doctor => commands::doctor::run(&opts).await,
        Command::Performance => commands::performance::run(&opts).await,
        Command::Schedule { bin } => commands::schedule::run(&opts, &bin).await,
        Command::Maintenance => commands::maintenance::run(&opts).await,
        Command::Graph { .. } => unreachable!(),
        Command::Wiki { cmd } => commands::wiki::run(&opts, cmd).await,
        Command::Queue { command } => commands::queue::run(&opts, command).await,
    }
}
