mod commands;

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser)]
#[command(name = "lore", about = "Knowledge ingestion pipeline for Obsidian")]
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
        /// Skip dedup, force re-ingest
        #[arg(long)]
        force: bool,
    },
    /// Generate synthesis reports
    Synthesis {
        #[command(subcommand)]
        period: SynthesisPeriod,
    },
    /// Show last ingest time per source
    Status,
    /// Check pipeline health (warn if source not ingested in 48h)
    Health {
        /// Treat first-install (all "never") as failure as well
        #[arg(long)]
        strict: bool,
    },
    /// Show personal work category distribution
    Performance,
    /// Validate config file
    Validate,
    /// Print crontab entries for all scheduled sources
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
    /// Prune ingest log and dedup cache entries older than 90 days
    Maintenance,
    /// Wikilink graph analysis for the vault
    Graph {
        /// Root directory of the vault (overrides config.yaml vault.root)
        #[arg(long)]
        root: Option<PathBuf>,
        /// Output in JSON format (envelope: {"ok": true, "data": …})
        #[arg(long)]
        json: bool,
        #[command(subcommand)]
        command: commands::graph::GraphCmd,
    },
    /// Wiki-level utilities (catalog generation, future maintenance ops)
    Wiki {
        #[command(subcommand)]
        cmd: commands::wiki::WikiCmd,
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

#[derive(clap::Subcommand)]
enum SynthesisPeriod {
    Weekly {
        #[arg(long, conflicts_with = "previous")]
        date: Option<String>,
        /// Synthesize the just-completed period (last week)
        #[arg(long)]
        previous: bool,
    },
    Monthly {
        #[arg(long, conflicts_with = "previous")]
        date: Option<String>,
        /// Synthesize the just-completed period (last month)
        #[arg(long)]
        previous: bool,
    },
    Quarterly {
        #[arg(long, conflicts_with = "previous")]
        date: Option<String>,
        /// Synthesize the just-completed period (last quarter)
        #[arg(long)]
        previous: bool,
    },
    Annual {
        #[arg(long, conflicts_with = "previous")]
        year: Option<i32>,
        /// Synthesize the just-completed period (last year)
        #[arg(long)]
        previous: bool,
    },
}

#[tokio::main]
async fn main() -> miette::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    let opts = commands::GlobalOpts {
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
            force,
        } => commands::ingest::run(&opts, source, date, dry_run, force).await,
        Command::Synthesis { period } => commands::synthesis::run(&opts, period.into()).await,
        Command::Health { strict } => commands::health::run(&opts, strict).await,
        Command::Performance => commands::performance::run(&opts).await,
        Command::Schedule { bin } => commands::schedule::run(&opts, &bin).await,
        Command::Maintenance => commands::maintenance::run(&opts).await,
        Command::Graph { .. } => unreachable!(),
        Command::Wiki { cmd } => commands::wiki::run(&opts, cmd).await,
    }
}

impl From<SynthesisPeriod> for commands::synthesis::Period {
    fn from(p: SynthesisPeriod) -> Self {
        use commands::synthesis::Period;
        match p {
            SynthesisPeriod::Weekly { date, previous } => Period::Weekly { date, previous },
            SynthesisPeriod::Monthly { date, previous } => Period::Monthly { date, previous },
            SynthesisPeriod::Quarterly { date, previous } => Period::Quarterly { date, previous },
            SynthesisPeriod::Annual { year, previous } => Period::Annual { year, previous },
        }
    }
}
