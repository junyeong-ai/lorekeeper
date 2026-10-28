//! `lore self` — this installation as a thing with invariants.
//!
//! Everything inside the vault declares what it is and what it was derived from, and a command
//! checks those declarations. The installation had no such account of itself: the binary, the
//! agent skills, the runtime skills and the templates were separately published artifacts,
//! and `lore health` measures the freshness of ingested data, so a scheduled install could
//! fall arbitrarily far behind while every check it had reported green.
//!
//! The skills, the runtime skills and the templates are now compiled into the binary, so they
//! cannot be a different version from it. What remains is whether the deployed COPIES still
//! equal what this binary carries, which `lore self status` answers by comparing bytes and
//! `lore self deploy` repairs.

use std::io::IsTerminal;
use std::path::PathBuf;

#[cfg(unix)]
mod lease;
mod protection;

pub(crate) use protection::Protection;

use lk_dist::{Decision, Installation, InstallationReport, ReleaseClient, SchemaState, SkillLevel};

use super::{GlobalOptions, find_config, load_config};

#[derive(clap::Subcommand)]
pub enum SelfCommand {
    /// Report what this installation is, and whether every deployed copy matches this binary
    Status {
        /// Emit the report as JSON
        #[arg(long)]
        json: bool,
        /// Directory holding the templates and runtime skills (default: the one the last deploy
        /// recorded, else the installer's)
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// Write the skills and templates this binary carries
    Deploy {
        /// Where to write the agent skills. Omitted, they are rewritten wherever they already
        /// are and created nowhere — an install that took none must not acquire them.
        #[arg(long, value_enum)]
        skills: Option<SkillScope>,
        /// Agent receiving interactive skills; unattended skills are always carried locally
        #[arg(long, value_enum, default_value_t = AgentTarget::Both)]
        agent: AgentTarget,
        /// Directory holding the templates and runtime skills (default: the installer's data dir)
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// Replace this binary with a published release, then redeploy what it carries
    Update {
        /// Install this version instead of the latest. Named explicitly, so it may go back.
        #[arg(long)]
        version: Option<String>,
        /// Replace even at the target version or with outstanding queue work.
        /// Invalid configuration still fails.
        #[arg(long)]
        force: bool,
        /// Additionally hold the archive to the build provenance the release attested
        #[arg(long)]
        verify_attestations: bool,
        /// Do not ask for confirmation
        #[arg(long, short = 'y')]
        yes: bool,
        /// Directory holding the templates and runtime skills (default: the one the last deploy
        /// recorded, else the installer's)
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// Remove this binary and everything it deployed. The vault is never touched.
    Uninstall {
        /// Do not ask for confirmation
        #[arg(long, short = 'y')]
        yes: bool,
        /// Also remove the config directory, including `config.yaml`
        #[arg(long)]
        purge_config: bool,
        /// Directory holding the templates and runtime skills (default: the installer's data dir)
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
}

#[derive(clap::ValueEnum, Clone, Copy, PartialEq, Eq)]
pub enum SkillScope {
    User,
    Project,
    None,
}

#[derive(clap::ValueEnum, Clone, Copy, Default)]
pub enum AgentTarget {
    ClaudeCode,
    Codex,
    #[default]
    Both,
}

impl AgentTarget {
    fn engines(self) -> &'static [lk_core::execution::AgentEngine] {
        use lk_core::execution::AgentEngine::{ClaudeCode, Codex};
        match self {
            Self::ClaudeCode => &[ClaudeCode],
            Self::Codex => &[Codex],
            Self::Both => &[ClaudeCode, Codex],
        }
    }
}

impl SkillScope {
    fn level(self) -> Option<SkillLevel> {
        match self {
            SkillScope::User => Some(SkillLevel::User),
            SkillScope::Project => Some(SkillLevel::Project),
            SkillScope::None => None,
        }
    }
}

pub async fn run(opts: &GlobalOptions, cmd: SelfCommand) -> miette::Result<()> {
    match cmd {
        SelfCommand::Status { json, data_dir } => status(opts, json, data_dir),
        SelfCommand::Deploy {
            skills,
            agent,
            data_dir,
        } => {
            deploy(
                opts,
                &installation(data_dir.clone())?,
                data_dir,
                skills,
                agent,
            )
            .await
        }
        SelfCommand::Update {
            version,
            force,
            verify_attestations,
            yes,
            data_dir,
        } => update(opts, version, force, verify_attestations, yes, data_dir).await,
        SelfCommand::Uninstall {
            yes,
            purge_config,
            data_dir,
        } => uninstall(
            opts,
            &installation(data_dir.clone())?,
            data_dir,
            yes,
            purge_config,
        ),
    }
}

fn installation(data_dir: Option<PathBuf>) -> miette::Result<Installation> {
    match data_dir {
        Some(dir) => Installation::detect_with_data_dir(super::resolve_root_override(dir)?),
        None => Installation::detect(),
    }
    .map_err(|e| miette::miette!("{e}"))
}

/// What `<wiki>/AGENTS.md` says generated it, for the vault this machine is configured for.
///
/// A machine with no config is not behind on a contract it does not have — the page is written
/// into a vault, and until one is configured there is no vault. A config that will not load is
/// the same answer for the same reason, and `lore validate` is what reports that.
fn schema_state(opts: &GlobalOptions) -> SchemaState {
    let Ok(config) = find_config(opts).and_then(|path| load_config(&path)) else {
        return SchemaState::Unconfigured;
    };
    let page = config
        .vault
        .root_path()
        .join(&config.vault.dirs.wiki)
        .join(lk_core::vault_path::SCHEMA_FILE);
    SchemaState::read(&page, &super::schema::generator())
}

fn status(opts: &GlobalOptions, json: bool, data_dir: Option<PathBuf>) -> miette::Result<()> {
    let installation = installation(data_dir)?;
    let report = InstallationReport::build(&installation, schema_state(opts));

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report)
                .map_err(|e| miette::miette!("serialize report: {e}"))?
        );
    } else {
        print_report(&report);
    }
    // Its own verdict, like every other check: `lore status` summarises without gating, and the
    // command that owns a subsystem is the one whose exit code answers for it.
    std::process::exit(i32::from(report.stale() > 0));
}

fn print_report(report: &InstallationReport) {
    eprintln!("lore {}  {}", report.version, report.binary.display());
    match &report.target {
        Some(triple) if report.self_replaceable => row("release", triple, None),
        Some(triple) => row(
            "release",
            &format!("{triple} (updates through install.ps1)"),
            None,
        ),
        None => row("release", "built from source — no published archive", None),
    }

    if report.skills.is_empty() {
        row("skills", "not deployed", None);
    }
    for (level, group) in report.skills.iter().chain(&report.runtime_skills) {
        print_group(&format!("skills ({level})"), group);
    }
    print_group("templates", &report.templates);
    print_group("config", &report.config);

    match &report.schema {
        SchemaState::Unconfigured => row("schema", "no vault configured", None),
        SchemaState::Current { path } => row("schema", "current", Some(path)),
        SchemaState::Missing { path } => row("schema", "never generated", Some(path)),
        SchemaState::Stale { path, generator } => row(
            "schema",
            &match generator {
                Some(by) => format!("generated by {by}"),
                None => "build not declared".to_string(),
            },
            Some(path),
        ),
    }

    let repair = report.repair();
    match report.stale() {
        0 => eprintln!("\nevery deployed copy matches this binary"),
        1 => eprintln!("\none does not — `{repair}` rewrites it"),
        n => eprintln!("\n{n} of them do not — `{repair}` rewrites them"),
    }
}

fn print_group(label: &str, group: &lk_dist::DeployedGroup) {
    if !group.deployed {
        row(label, "not deployed", None);
        return;
    }
    let stale = group.stale().count();
    let total = group.artifacts.len();
    if stale == 0 {
        row(label, &format!("{total} current"), Some(&group.location));
        return;
    }
    row(
        label,
        &format!("{stale} of {total} to rewrite"),
        Some(&group.location),
    );
    for artifact in group.stale() {
        let why = match artifact.state {
            lk_dist::ArtifactState::Absent => "absent",
            lk_dist::ArtifactState::Retired => "retired — this binary no longer carries it",
            _ => "differs",
        };
        eprintln!("  {:<18} · {} ({why})", "", artifact.name);
    }
}

fn row(label: &str, state: &str, path: Option<&std::path::Path>) {
    match path {
        Some(path) => eprintln!("  {label:<18}{state:<22} {}", path.display()),
        None => eprintln!("  {label:<18}{state}"),
    }
}

async fn deploy(
    opts: &GlobalOptions,
    installation: &Installation,
    override_dir: Option<PathBuf>,
    skills: Option<SkillScope>,
    agent: AgentTarget,
) -> miette::Result<()> {
    let protection = Protection::mutation(
        opts,
        installation.clone(),
        override_dir,
        agent,
        skills,
        false,
    )?;
    for engine in agent.engines() {
        let selected = installation.for_engine(*engine);
        let installation = &selected;
        let levels: Vec<SkillLevel> = match skills {
            Some(scope) => scope.level().into_iter().collect(),
            None => installation.deployed_skill_levels(),
        };
        for level in levels {
            let root = installation
                .skills_dir(level)
                .ok_or_else(|| miette::miette!("{}", installation.skills_reason(level)))?;
            // The removals come back on BOTH paths, because the failure path is the one where the
            // user most needs them: a deploy that deleted a directory and then failed on a write
            // reported only "permission denied", which reads as "nothing happened".
            let (outcome, failure) = match installation.deploy_skills(level) {
                Ok(outcome) => (outcome, None),
                Err(lk_dist::DeployFailure { done, error }) => (done, Some(error)),
            };
            for removed in &outcome.removed {
                eprintln!(
                    "            removed {} — this build no longer carries it",
                    removed.display()
                );
            }
            if let Some(error) = failure {
                return Err(miette::miette!("{error}"));
            }
            eprintln!("Skills      {} → {}", outcome.written.len(), root.display());
            eprintln!(
                "            {}",
                lk_dist::skill_names()
                    .iter()
                    .map(|name| format!("/{name}"))
                    .collect::<Vec<_>>()
                    .join("  ")
            );
        }
    }

    for engine in AgentTarget::Both.engines() {
        let root = installation
            .for_engine(*engine)
            .deploy_runtime_skills()
            .map_err(|e| miette::miette!("{e}"))?;
        eprintln!("Agent skills {} → {}", engine.as_str(), root.display());
    }

    for (label, outcome) in [
        ("Templates  ", installation.deploy_templates()),
        ("Config     ", installation.deploy_config_examples()),
    ] {
        let dir = outcome.map_err(|e| miette::miette!("{e}"))?;
        eprintln!("{label} → {}", dir.display());
    }
    installation
        .remember()
        .map_err(|e| miette::miette!("{e}"))?;

    // `AGENTS.md` is one of the copies `self status` counts, and `lore schema` is the only
    // thing that writes it — so a deploy that skipped it named itself as the repair for a
    // difference it could not remove, and the exit code never cleared. Rendered in process,
    // because this binary owns the page-format table the page is generated from.
    // Best-effort, like the same step inside `self update`: it runs after everything this
    // command OWNS has landed, and `AGENTS.md` lives in the user's vault — a drive not mounted
    // or a vault the process cannot reach would otherwise report a failed install for one that
    // is complete, and the advised re-run would fail identically forever.
    if let Err(e) = protection.synchronize_schema().await {
        eprintln!("warning: could not regenerate AGENTS.md ({e}) — run `lore schema`");
    }
    Ok(())
}

async fn update(
    opts: &GlobalOptions,
    version: Option<String>,
    force: bool,
    verify_attestations: bool,
    yes: bool,
    data_dir: Option<PathBuf>,
) -> miette::Result<()> {
    let installation = installation(data_dir.clone())?;
    let target = lk_dist::ReleaseTarget::current()
        .ok_or_else(|| miette::miette!("{}", lk_dist::ReleaseTarget::unsupported_reason()))?;
    if !target.is_self_replaceable() {
        return Err(miette::miette!(
            "{} cannot replace a running executable in place — re-run `install.ps1` to update",
            target.triple
        ));
    }

    let running = lk_dist::current_version();
    let requested = version
        .as_deref()
        .map(lk_dist::parse_tag)
        .transpose()
        .map_err(|e| miette::miette!("{e}"))?;

    let client = ReleaseClient::build().map_err(|e| miette::miette!("{e}"))?;
    let latest = match requested {
        // A named version is the answer; asking the channel for another one would only
        // introduce a way for the two to disagree.
        Some(_) => None,
        None => {
            let resolved = client
                .resolve_latest()
                .await
                .map_err(|e| miette::miette!("{e}"))?;
            if !resolved.provenance.is_authoritative() {
                eprintln!(
                    "note: the release API did not answer, so `latest` comes from the web view, \
                     which trails it by minutes after a release is published"
                );
            }
            Some(resolved.version)
        }
    };

    let decision = lk_dist::decide(&running, requested.as_ref(), latest.as_ref(), force)
        .ok_or_else(|| miette::miette!("no version to install"))?;
    let to = match decision {
        Decision::AlreadyCurrent(version) => {
            eprintln!("lore {version} is the current release");
            return Ok(());
        }
        Decision::RefusedDowngrade { running, offered } => {
            return Err(miette::miette!(
                "the latest release is {offered}, older than the running {running} — \
                 install it deliberately with `--version {offered}` if that is intended"
            ));
        }
        Decision::Replace { to, .. } => to,
    };

    let mut initial = Protection::mutation(
        opts,
        installation.clone(),
        data_dir.clone(),
        AgentTarget::Both,
        None,
        true,
    )?;
    refuse_while_work_is_queued(opts, force)?;
    let baseline = lk_dist::sha256_hex(
        &std::fs::read(installation.binary())
            .map_err(|error| miette::miette!("read installed binary: {error}"))?,
    );

    if !confirm(&format!("Replace lore {running} with {to}?"), yes)? {
        eprintln!("Cancelled");
        return Ok(());
    }
    initial.release();

    let archive_name = lk_dist::archive_name(&to, target);
    let url = lk_dist::asset_url(&to, &archive_name);
    eprintln!("Downloading {archive_name}");
    let archive = client
        .fetch(&url)
        .await
        .map_err(|e| miette::miette!("{e}"))?;
    let sidecar = client
        .fetch_text(&format!("{url}.sha256"))
        .await
        .map_err(|e| miette::miette!("{e}"))?;
    lk_dist::verify_sidecar(&archive, &sidecar, &archive_name)
        .map_err(|e| miette::miette!("{e}"))?;

    if verify_attestations {
        verify_attestation_of(&archive, &archive_name)?;
    }

    let member = format!("lore-v{to}-{}/lore", target.triple);
    let binary = lk_dist::read_from_tar_gz(&archive, &member)
        .map_err(|e| miette::miette!("{e}"))?
        .ok_or_else(|| miette::miette!("{archive_name} holds no {member}"))?;

    let protection = Protection::mutation(
        opts,
        installation.clone(),
        data_dir,
        AgentTarget::Both,
        None,
        true,
    )?;
    initial.validate_staged(&protection)?;
    let current = lk_dist::sha256_hex(
        &std::fs::read(installation.binary())
            .map_err(|error| miette::miette!("read installed binary: {error}"))?,
    );
    if current != baseline {
        return Err(miette::miette!(
            "installed binary changed during download; retry the update"
        ));
    }
    let publication = protection.claim_publication()?;
    refuse_while_work_is_queued(opts, force)?;
    protection.recheck_configuration()?;

    lk_dist::install_binary(&binary, installation.binary(), &to)
        .map_err(|e| miette::miette!("{e}"))?;
    eprintln!("Replaced {}", installation.binary().display());
    drop(publication);

    redeploy_through(
        installation.binary(),
        opts,
        installation.data_dir(),
        &protection,
    );

    eprintln!("\nlore {running} → {to}");
    Ok(())
}

fn verify_attestation_of(archive: &[u8], archive_name: &str) -> miette::Result<()> {
    let directory = super::private_directory::temporary("lore-verify-")
        .map_err(|error| miette::miette!("prepare attestation workspace: {error}"))?;
    let staged = directory.path().join(archive_name);
    std::fs::write(&staged, archive)
        .map_err(|error| miette::miette!("stage {}: {error}", staged.display()))?;
    lk_dist::verify_attestation(&staged).map_err(|error| miette::miette!("{error}"))
}

/// Refuse to swap the binary while the queue still holds work.
///
/// A task carries a request from one build and is answered under whichever build's skills are
/// deployed when the drain runs. Replacing both between those two moments is the one genuinely
/// lossy point in an upgrade. Use the same read-only settlement check as `lore queue check`,
/// including unpublished results and interrupted publication journals.
fn refuse_while_work_is_queued(opts: &GlobalOptions, force: bool) -> miette::Result<()> {
    let Some(path) = super::find_optional_config(opts)? else {
        return Ok(());
    };
    let config = load_config(&path)?;
    let state = super::queue::check::inspect(&config.vault.root_path())?;
    if state.is_settled() || force {
        return Ok(());
    }
    Err(miette::miette!(
        "queue is not settled: {} current task(s), {} unreadable task(s), {} unparseable line(s), \
         {} staged result(s), pending mutation: {} \
         — inspect with `lore queue check` and drain with `/lore-process` first, \
         or pass `--force` to update anyway",
        state.current,
        state.unreadable,
        state.unparseable_lines,
        state.staged_results.len(),
        state.pending_mutation
    ))
}

/// Deploy through the NEW binary, never from here.
///
/// This process carries the skills and templates of the version being replaced, so
/// writing them from here would deploy the predecessor over the successor. Best-effort: the
/// binary is already in place and working, so a data directory that cannot be written is a
/// follow-up rather than a failed update.
fn redeploy_through(
    binary: &std::path::Path,
    opts: &GlobalOptions,
    data_dir: &std::path::Path,
    protection: &Protection,
) {
    let mut child = std::process::Command::new(binary);
    // The config the CALLER named, not whatever the child would auto-discover: `--config`
    // decides which vault's `AGENTS.md` the deploy regenerates, and a child that re-discovered
    // it would rewrite the contract of a different vault and leave this one behind.
    if let Ok(path) = find_config(opts) {
        child.arg("--config").arg(path);
    }
    child.args(["self", "deploy", "--data-dir"]).arg(data_dir);
    if let Err(error) = protection.configure(&mut child) {
        eprintln!(
            "warning: could not hand off protected redeployment ({error}); run `lore self deploy`"
        );
        return;
    }
    match child.status() {
        Ok(status) if status.success() => {}
        _ => eprintln!("warning: could not redeploy — run `lore self deploy`"),
    }
}

fn uninstall(
    opts: &GlobalOptions,
    installation: &Installation,
    override_dir: Option<PathBuf>,
    yes: bool,
    purge_config: bool,
) -> miette::Result<()> {
    let _protection = Protection::mutation(
        opts,
        installation.clone(),
        override_dir,
        AgentTarget::Both,
        None,
        true,
    )?;
    let deployments = AgentTarget::Both
        .engines()
        .iter()
        .flat_map(|engine| {
            let view = installation.for_engine(*engine);
            view.deployed_skill_levels()
                .into_iter()
                .map(move |level| (view.clone(), level))
        })
        .collect::<Vec<_>>();
    eprintln!("This removes:");
    eprintln!("  {}", installation.binary().display());
    eprintln!("  {}", installation.templates_dir().display());
    for (view, level) in &deployments {
        if let Some(dir) = view.skills_dir(*level) {
            eprintln!("  {}/lore-*", dir.display());
        }
    }
    if purge_config {
        eprintln!("  {}", installation.config_dir().display());
    }
    // The vault holds the user's knowledge and their credentials; the tool is the disposable
    // half of the pair. Saying so is part of the confirmation, not a footnote after it.
    eprintln!("\nYour vault is not touched.");

    if !confirm("Remove this installation?", yes)? {
        eprintln!("Cancelled");
        return Ok(());
    }

    for (view, level) in deployments {
        let Some(root) = view.skills_dir(level) else {
            continue;
        };
        for name in lk_dist::skill_names() {
            remove_dir(&root.join(name))?;
        }
    }
    remove_dir(&installation.templates_dir())?;
    remove_dir(&installation.data_dir().join("skills"))?;
    // Only when it is now empty: a data directory holding something this tool did not put
    // there is not this tool's to delete.
    let _ = std::fs::remove_dir(installation.data_dir());

    if purge_config {
        remove_dir(installation.config_dir())?;
    } else {
        // The records go with the copies they describe. Leaving `data-dir` behind meant a later
        // bare `self deploy` resurrected the very directory this removed, in preference to the
        // default, with nothing said.
        for name in lk_dist::config_files()
            .iter()
            .map(|file| file.relative)
            .chain(["data-dir", "deployed-skills"])
        {
            remove_file(&installation.config_dir().join(name))?;
        }
    }

    remove_file(installation.binary())?;
    eprintln!("Removed");
    Ok(())
}

fn remove_dir(path: &std::path::Path) -> miette::Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(miette::miette!("remove {}: {e}", path.display())),
    }
}

fn remove_file(path: &std::path::Path) -> miette::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(miette::miette!("remove {}: {e}", path.display())),
    }
}

/// Ask, unless the caller already answered — or unless nobody is there to.
///
/// A scheduled run has no terminal, so a prompt would block a job nobody is watching until it
/// is killed. Refusing and naming the flag is the only outcome that leaves the operator
/// something to act on.
fn confirm(question: &str, yes: bool) -> miette::Result<bool> {
    if yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        return Err(miette::miette!(
            "{question} — no terminal to ask on; pass `--yes` to answer in advance"
        ));
    }
    dialoguer::Confirm::new()
        .with_prompt(question)
        .default(false)
        .interact()
        .map_err(|e| miette::miette!("{e}"))
}

#[cfg(test)]
mod update_boundary_tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, GlobalOptions) {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.yaml");
        std::fs::write(&config, "vault:\n  root: vault\nidentity:\n  name: Test\n  email: test@example.com\nsources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: inbox\n").unwrap();
        (
            dir,
            GlobalOptions {
                config: Some(config),
                template_dir: None,
            },
        )
    }

    #[test]
    fn update_never_swallows_an_explicit_missing_or_invalid_config() {
        let (dir, opts) = fixture();
        for body in [Some("not: [valid"), None] {
            match body {
                Some(body) => std::fs::write(opts.config.as_ref().unwrap(), body).unwrap(),
                None => std::fs::remove_file(opts.config.as_ref().unwrap()).unwrap(),
            }
            for force in [false, true] {
                assert!(
                    refuse_while_work_is_queued(&opts, force).is_err(),
                    "force={force}, body={body:?}, fixture={}",
                    dir.path().display()
                );
            }
        }
        // A selected path that resolves but cannot be read as a file is not absence.
        std::fs::create_dir(opts.config.as_ref().unwrap()).unwrap();
        for force in [false, true] {
            assert!(refuse_while_work_is_queued(&opts, force).is_err());
        }
    }

    #[test]
    fn update_requires_force_for_unparseable_and_unreadable_work() {
        let (dir, opts) = fixture();
        let queue = dir.path().join("vault/.lorekeeper/queue");
        std::fs::create_dir_all(&queue).unwrap();
        // The second record parses but cannot reconstruct its retained typed input.
        let unreadable = serde_json::json!({
            "task_id": "blocked", "kind": "summarize", "created_at": "2026-10-19T00:00:00Z",
            "cache_hash": "invalid", "input": {},
            "target": {"vault_path": "wiki/documents/test.md", "kind": "document-summary", "anchor": "## Summary"}
        }).to_string();
        for body in ["{not-json}".to_string(), unreadable] {
            std::fs::write(queue.join("pending.jsonl"), format!("{body}\n")).unwrap();
            let load = super::super::queue::queue_load(&dir.path().join("vault")).unwrap();
            assert_eq!(load.work, 0);
            assert_eq!(load.unreadable + load.unparseable, 1);
            assert!(refuse_while_work_is_queued(&opts, false).is_err());
            assert!(refuse_while_work_is_queued(&opts, true).is_ok());
        }
    }

    #[test]
    fn update_requires_force_for_results_and_journals_without_changing_them() {
        let (dir, opts) = fixture();
        for force in [false, true] {
            assert!(refuse_while_work_is_queued(&opts, force).is_ok());
        }
        for relative in ["queue/results/unpublished.json", "mutation/pending.json"] {
            let path = dir.path().join("vault/.lorekeeper").join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                &path,
                "retained bytes, not interpreted by the settlement check",
            )
            .unwrap();
            assert!(refuse_while_work_is_queued(&opts, false).is_err());
            assert!(refuse_while_work_is_queued(&opts, true).is_ok());
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                "retained bytes, not interpreted by the settlement check"
            );
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn force_does_not_hide_uninspectable_queue_storage() {
        for relative in ["queue", "queue/results"] {
            let (dir, opts) = fixture();
            let path = dir.path().join("vault/.lorekeeper").join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "not a directory").unwrap();
            for force in [false, true] {
                assert!(refuse_while_work_is_queued(&opts, force).is_err());
            }
        }
    }

    #[test]
    fn update_distinguishes_current_from_completed_retained_tasks() {
        let (dir, opts) = fixture();
        let vault = dir.path().join("vault");
        let queue = vault.join(".lorekeeper/queue");
        std::fs::create_dir_all(&queue).unwrap();
        let target = vault.join("wiki/documents/test.md");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        let input = serde_json::json!({"text": "source", "max_sentences": 5, "locale": "en"});
        let hash = lk_queue::cache_hash(input.as_object().unwrap());
        let task = serde_json::json!({
            "task_id": "current", "kind": "summarize", "created_at": "2026-10-19T00:00:00Z",
            "cache_hash": hash, "input": input,
            "target": {"vault_path": "wiki/documents/test.md", "kind": "document-summary", "anchor": "## Summary"}
        });
        std::fs::write(queue.join("pending.jsonl"), format!("{task}\n")).unwrap();
        for completed in [false, true] {
            let done = if completed {
                format!("  summary_done: {hash}\n")
            } else {
                String::new()
            };
            std::fs::write(
                &target,
                format!(
                    "---\ntype: document\nllm_inputs:\n  summary: {hash}\n{done}---\n## Summary\n\n"
                ),
            )
            .unwrap();
            assert_eq!(refuse_while_work_is_queued(&opts, false).is_ok(), completed);
            assert!(refuse_while_work_is_queued(&opts, true).is_ok());
        }
    }
}
