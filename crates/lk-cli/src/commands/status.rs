//! `lore status` — the one command that answers "is anything wrong, and what do I run".
//!
//! Five subsystems each had a command that answered for it and nothing that answered for all
//! of them, so knowing the state of a vault meant knowing which of five to type. This composes
//! them: one line per subsystem, each verdict computed by the very code its own command exits
//! on, each naming what to run next.
//!
//! It reports and does not gate. A summary that failed the shell would be a sixth gate rather
//! than a front door, and the command that owns a subsystem is the one whose exit code answers
//! for it — `lore health` for currency, `lore doctor` for the pages, `lore graph lint` for the
//! structure, `lore self status` for the installation.

use std::fmt::Write as _;

use super::{find_config, load_config};

pub async fn run(opts: &super::GlobalOptions) -> miette::Result<()> {
    let config = load_config(&find_config(opts)?)?;
    let vault_root = config.vault.root_path();
    let now = jiff::Timestamp::now();

    eprintln!(
        "lore {}  {}",
        env!("CARGO_PKG_VERSION"),
        vault_root.display()
    );

    let installation = lk_dist::Installation::detect().map_err(|e| miette::miette!("{e}"))?;
    let agents = vault_root
        .join(&config.vault.dirs.wiki)
        .join(lk_core::vault_path::SCHEMA_FILE);
    let schema = lk_dist::SchemaState::read(&agents, &super::schema::generator());
    let install = lk_dist::InstallationReport::build(&installation, schema);
    line(
        "install",
        match install.stale() {
            0 => "current".to_string(),
            1 => "1 deployed copy differs".to_string(),
            n => format!("{n} deployed copies differ"),
        },
        install.stale() == 0,
        install.repair(),
    );

    // The intent plane, where it is turned on. It is a subsystem with state a person acts on —
    // proposals waiting for an answer, a task carried past the point of being a plan, an edit
    // an editor made that nothing has recorded — and leaving it out meant the one command that
    // answers "is anything wrong" could not see the half of the vault a person touches daily.
    //
    // The mark follows the same rule as every other row: it is `!` when the command that owns
    // it would REFUSE, which for the board means a page a write cannot land on — two lines
    // claiming one id, or a code fence that never closes.
    if let Some(board) = super::task::IntentPlane::survey(opts) {
        line("board", board.state, board.writable, "lore agenda");
    }

    let freshness = super::health::freshness(&config, &vault_root, now).await?;
    let (fresh, stale, never) = (freshness.fresh(), freshness.stale(), freshness.never());
    let mut currency = format!("{fresh} fresh");
    if stale > 0 {
        let _ = write!(currency, " · {stale} stale");
    }
    if never > 0 {
        let _ = write!(currency, " · {never} never ingested");
    }
    line("sources", currency, stale == 0, "lore health");

    let queue = super::queue::queue_load(&vault_root)?;
    line(
        "queue",
        match queue.work {
            0 => "drained".to_string(),
            n => format!("{n} task(s) waiting for an LLM session"),
        },
        queue.work == 0,
        "/lore-process",
    );

    let in_flight = super::queue::work_in_flight(&vault_root);
    let audit = super::doctor::audit(
        &super::doctor::managed_roots(&vault_root, &config.vault.dirs),
        &vault_root,
        &in_flight.keys,
    );
    let mut pages = match audit.defects() {
        0 => format!("{} clean", audit.scanned),
        n => format!("{n} defect(s) in {} pages", audit.scanned),
    };
    if audit.credentials() > 0 {
        let _ = write!(pages, " · {} credential form(s)", audit.credentials());
    }
    line("pages", pages, audit.defects() == 0, "lore doctor");

    // The graph is resolved separately from the config above because `lore graph` carries its
    // own root and scope resolution, and reading it twice is what keeps this line the same
    // verdict `lore graph lint` exits on rather than a second derivation of it.
    match super::graph::lint(opts, None) {
        Ok(report) => {
            let violations = report.violations.count();
            line(
                "graph",
                match violations {
                    0 => format!("sound · {} pages, {} links", report.pages, report.links),
                    n => format!("{n} contradiction(s)"),
                },
                violations == 0,
                "lore graph lint",
            );
        }
        Err(e) => line(
            "graph",
            format!("unreadable: {e}"),
            false,
            "lore graph lint",
        ),
    }

    Ok(())
}

/// One subsystem's row. The gap before the command is added rather than padded into, so a
/// state long enough to fill the column — an error naming a path, say — never runs into the
/// command name and produces a line that reads as one word.
fn line(label: &str, state: String, ok: bool, next: &str) {
    let mark = if ok { '·' } else { '!' };
    eprintln!(
        "  {mark} {}{}  {next}",
        super::pad(label, 9),
        super::pad(&state, 42)
    );
}
