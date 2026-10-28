use super::{find_config, load_config};

/// Output format for the generated schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    /// crontab lines (portable; Linux servers, or a Mac that is always awake).
    Cron,
    /// launchd property lists (macOS). Preferred on a laptop: `StartCalendarInterval`
    /// runs a job that was MISSED because the machine was asleep as soon as it wakes,
    /// whereas cron simply skips it — and a closed laptop at 09:00 is the normal case.
    Launchd,
}

const DEFAULT_NAMESPACE: &str = "com.lorekeeper";

fn validate_namespace(namespace: &str) -> miette::Result<()> {
    if namespace.len() > 128
        || namespace.split('.').any(|part| {
            part.is_empty()
                || !part.as_bytes()[0].is_ascii_alphanumeric()
                || !part.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
        })
    {
        return Err(miette::miette!(
            "--namespace requires dot-separated ASCII alphanumeric/hyphen components, each starting with an alphanumeric character (at most 128 bytes)"
        ));
    }
    Ok(())
}

/// One scheduled command, format-independent.
/// When a job runs.
///
/// Two shapes because both schedulers express both NATIVELY, and translating either into the
/// other's form is the approximation this command refuses. A calendar time is `0 9 * * *` in
/// cron and `StartCalendarInterval` in launchd; a repeat is `*/5 * * * *` in cron and
/// `StartInterval` in launchd. What is refused is a cron `*/5` asked to become a calendar
/// interval — a silently different schedule — and that refusal stands.
enum When {
    /// 5-field cron expression from config.
    Calendar(String),
    /// Every N minutes, for a job whose whole point is to check often.
    EveryMinutes(u32),
}

/// How often the reminder timer looks.
///
/// Five minutes rather than one: a reminder exists to interrupt someone, and being interrupted
/// up to five minutes late is not a different event, while a job that wakes a laptop every
/// sixty seconds all day is. Nothing but firing retires a reminder, so a tick missed while the
/// machine slept costs lateness rather than the reminder.
const REMIND_EVERY_MINUTES: u32 = 5;

struct Job {
    /// Suffix of the launchd label / identity in comments (`ingest`, `synthesis-weekly`, …).
    name: String,
    schedule: When,
    /// Program to run. `None` = the `--bin` lore binary; `Some` = a pipeline script, which
    /// uses the captured job environment to find its own tools.
    /// Arguments after the program.
    args: Vec<String>,
}

fn job_env(
    bin: &str,
    config_path: &std::path::Path,
    data_dir: &std::path::Path,
    engines: &[lk_core::execution::AgentEngine],
) -> miette::Result<Vec<(String, String)>> {
    let mut env = vec![
        ("LORE_BIN".into(), bin.into()),
        ("LORE_CONFIG".into(), config_path.display().to_string()),
        (
            "LORE_INSTALL_DATA_DIR".into(),
            data_dir.display().to_string(),
        ),
    ];
    if let Some(path) = std::env::var_os("PATH") {
        let paths = std::env::split_paths(&path)
            .map(|path| {
                std::path::absolute(if path.as_os_str().is_empty() {
                    std::path::Path::new(".")
                } else {
                    &path
                })
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| miette::miette!("resolve scheduled PATH: {e}"))?;
        let path = std::env::join_paths(paths)
            .map_err(|e| miette::miette!("scheduled PATH: {e}"))?
            .into_string()
            .map_err(|_| miette::miette!("scheduled PATH must be UTF-8"))?;
        env.push(("PATH".into(), path));
    }
    for (engine, key) in [
        (
            lk_core::execution::AgentEngine::ClaudeCode,
            "CLAUDE_CONFIG_DIR",
        ),
        (lk_core::execution::AgentEngine::Codex, "CODEX_HOME"),
    ] {
        if engines.contains(&engine) {
            let value = std::env::var_os(key);
            if let Some(path) = resolve_existing_dir(key, value.as_deref())? {
                env.push((key.into(), path.display().to_string()));
            }
        }
    }
    Ok(env)
}

const PROXY_ENV_KEYS: [&str; 8] = [
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
];

/// Keep casing and explicit emptiness: each network client owns precedence.
fn proxy_env(
    mut lookup: impl FnMut(&str) -> Option<std::ffi::OsString>,
) -> miette::Result<Vec<(String, String)>> {
    let mut captured = Vec::new();
    for key in PROXY_ENV_KEYS {
        let Some(raw) = lookup(key) else { continue };
        let invalid = || {
            miette::miette!(
                "invalid {key}: expected UTF-8, control-free text; proxy endpoints must be absolute credential-free URLs without query, fragment or non-root path"
            )
        };
        let value = raw.into_string().map_err(|_| invalid())?;
        if value.chars().any(char::is_control) {
            return Err(invalid());
        }
        if !value.is_empty() && !key.eq_ignore_ascii_case("no_proxy") {
            let url = url::Url::parse(&value).map_err(|_| invalid())?;
            if value.trim() != value
                || !value.get(..url.scheme().len() + 3).is_some_and(|prefix| {
                    prefix.eq_ignore_ascii_case(&format!("{}://", url.scheme()))
                })
                || url.host_str().is_none()
                // The URL parser normalizes an empty userinfo marker away.
                // With paths/query/fragment excluded, a literal @ is userinfo syntax.
                || value.contains('@')
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
                || !matches!(url.path(), "" | "/")
            {
                return Err(invalid());
            }
        }
        captured.push((key.to_owned(), value));
    }
    Ok(captured)
}

fn resolve_existing_dir(
    name: &str,
    value: Option<&std::ffi::OsStr>,
) -> miette::Result<Option<std::path::PathBuf>> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_empty() {
        return Err(miette::miette!("{name} must name an existing directory"));
    }
    let path = std::path::Path::new(value);
    let absolute = path
        .canonicalize()
        .map_err(|e| miette::miette!("resolve {name} {}: {e}", path.display()))?;
    if !absolute.is_dir() || absolute.to_str().is_none() {
        return Err(miette::miette!(
            "{name} must name a directory with a UTF-8 path: {}",
            path.display()
        ));
    }
    Ok(Some(absolute))
}

pub async fn run(
    opts: &super::GlobalOptions,
    bin: &str,
    format: Format,
    namespace: &str,
    log_dir: Option<&std::path::Path>,
) -> miette::Result<()> {
    validate_namespace(namespace)?;
    if format != Format::Launchd && (namespace != DEFAULT_NAMESPACE || log_dir.is_some()) {
        return Err(miette::miette!(
            "--namespace and --log-dir apply only to --format launchd"
        ));
    }
    if log_dir.is_some_and(|path| {
        !path.is_absolute()
            || path.to_str().is_none()
            || path.to_string_lossy().chars().any(char::is_control)
    }) {
        return Err(miette::miette!(
            "--log-dir must be an absolute UTF-8 path without control characters"
        ));
    }
    let config_path = find_config(opts)?;
    let config = load_config(&config_path)?;
    let cwd = std::env::current_dir()
        .map_err(|e| miette::miette!("failed to read current directory: {e}"))?;

    // Pass --config/--template-dir through to scheduled commands so they don't depend on CWD.
    let mut flags: Vec<String> = Vec::new();
    if let Some(p) = opts.config.as_ref().or(Some(&config_path)) {
        flags.push("--config".into());
        flags.push(p.display().to_string());
    }
    if let Some(p) = opts.template_dir.as_ref() {
        flags.push("--template-dir".into());
        flags.push(p.display().to_string());
    }

    let installation = lk_dist::Installation::detect().map_err(|e| miette::miette!("{e}"))?;
    let data_dir = std::path::absolute(installation.data_dir())
        .map_err(|e| miette::miette!("scheduled data directory: {e}"))?;
    let jobs = build_jobs(&config, &flags);
    let mut engines = Vec::new();
    for (enabled, operation) in [
        (
            config.ingest.schedule.is_some()
                && config.llm.provider == lk_core::config::LlmProvider::Queue,
            lk_core::execution::ExecutionOperation::Daily,
        ),
        (
            config.graph.audit.schedule.is_some(),
            lk_core::execution::ExecutionOperation::Knowledge,
        ),
    ] {
        if enabled {
            let (name, _) = config
                .execution
                .resolve_profile(operation, None)
                .map_err(|e| miette::miette!("scheduled semantic job: {e}"))?;
            let mut execution = config.execution.clone();
            execution
                .select_profile(name)
                .map_err(|e| miette::miette!("scheduled semantic job: {e}"))?;
            for name in std::iter::once(name).chain(execution.stages.values().map(String::as_str)) {
                let profile = &execution.profiles[name];
                if !matches!(
                    profile,
                    lk_core::execution::ExecutionProfile::ClaudeCode {
                        config_directory: Some(_),
                        ..
                    }
                ) {
                    engines.push(profile.engine());
                }
            }
        }
    }
    let mut env = job_env(bin, &config_path, &data_dir, &engines)?;
    env.extend(proxy_env(|key| std::env::var_os(key))?);
    if let Some(path) = &opts.template_dir {
        let path = if path.is_absolute() {
            path.clone()
        } else {
            cwd.join(path)
        };
        env.push(("LORE_TEMPLATE_DIR".into(), path.display().to_string()));
    }
    match format {
        Format::Cron => print_cron(bin, &cwd, &jobs, &env)?,
        Format::Launchd => print_launchd(bin, &cwd, &jobs, &env, namespace, log_dir)?,
    }
    Ok(())
}

/// Refuse a crontab line carrying a line break.
///
/// A crontab entry ends at the newline, so one inside any value — an environment value, the
/// working directory, a binary or script path, even a schedule expression, since
/// `validate_cron` splits on whitespace — truncates the entry and leaves its remainder
/// parsed as a line of its own. The check is on the ASSEMBLED line rather than on each
/// source, so it cannot fall behind as new values start reaching one.
fn checked_line(line: String) -> miette::Result<String> {
    if line.contains(['\n', '\r']) {
        return Err(miette::miette!(
            "a scheduled entry contains a line break, which a crontab line cannot carry:\n  \
             {line:?}\nFix the offending value, then re-run."
        ));
    }
    Ok(line)
}

/// Scheduling slots stay stable when their executable changes.
fn build_jobs(config: &lk_core::config::Config, flags: &[String]) -> Vec<Job> {
    let lore_job = |name: &str, schedule: &str, tail: &[&str]| {
        let mut args = flags.to_vec();
        args.extend(tail.iter().map(|value| (*value).to_owned()));
        Job {
            name: name.into(),
            schedule: When::Calendar(schedule.into()),
            args,
        }
    };
    let mut jobs = Vec::new();
    if let Some(schedule) = &config.ingest.schedule {
        jobs.push(lore_job("ingest", schedule, &["run", "daily", "--wait"]));
    }
    if let Some(schedule) = &config.graph.audit.schedule {
        jobs.push(lore_job(
            "knowledge-audit",
            schedule,
            &["run", "knowledge", "--wait"],
        ));
    }
    if config.synthesis.weekly.enabled
        && let Some(schedule) = &config.synthesis.weekly.schedule
    {
        jobs.push(lore_job(
            "synthesis-weekly",
            schedule,
            &[
                "synthesis",
                "weekly",
                "--previous",
                "--defer",
                "--wait-for-vault",
            ],
        ));
    }
    if let Some(personal) = &config.personal {
        for (period, schedule) in personal.review_schedules() {
            jobs.push(lore_job(
                &format!("synthesis-{period}"),
                schedule,
                &[
                    "synthesis",
                    period,
                    "--previous",
                    "--defer",
                    "--wait-for-vault",
                ],
            ));
        }
        if personal.tasks.is_some() {
            let mut args = flags.to_vec();
            args.extend(["task", "remind", "notify"].map(str::to_owned));
            jobs.push(Job {
                name: "remind".into(),
                schedule: When::EveryMinutes(REMIND_EVERY_MINUTES),
                args,
            });
        }
    }

    if let Some(schedule) = &config.maintenance.schedule {
        jobs.push(lore_job(
            "maintenance",
            schedule,
            &["maintenance", "--prune-queue", "--wait-for-vault"],
        ));
    }
    jobs
}

fn is_cron_command_environment(key: &str, value: &str) -> bool {
    PROXY_ENV_KEYS.contains(&key)
        || value.is_empty()
        || value.trim_ascii() != value
        || value.starts_with(['\'', '"'])
}

fn print_cron(
    bin: &str,
    cwd: &std::path::Path,
    jobs: &[Job],
    env: &[(String, String)],
) -> miette::Result<()> {
    let cwd_str = shell_escape(&cwd.display().to_string());
    let bin_escaped = shell_escape(bin);

    // Every line, COMMENTS INCLUDED, is collected and checked before a byte is printed. A
    // comment is not exempt: a line break inside one ends the comment and leaves the rest of
    // the value as a crontab entry in its own right — a directory named
    // `dir\n* * * * * <command>` is enough. And the output is emitted only once the whole
    // file has passed, so a refusal on a later line cannot leave an earlier one already
    // piped into `crontab -`.
    let mut out: Vec<String> = vec![
        "# lorekeeper scheduled tasks".into(),
        "# Paste into your crontab: crontab -e".into(),
        format!("# Requires: {bin_escaped} in PATH (cargo install --path crates/lk-cli)"),
        format!("# Working dir: {}", cwd.display()),
        "#".into(),
        "# On macOS prefer `lore schedule --format launchd`: cron SKIPS a job whose time".into(),
        "# passed while the machine was asleep, which silently drops a day's ingest.".into(),
        String::new(),
    ];

    // cron gives a job almost no environment. Assignment lines apply to every entry below
    // them, which is crontab's own way of saying this — and keeps the job lines readable
    // instead of repeating a full PATH on each one.
    if !env.is_empty() {
        out.push("# Environment for unattended jobs:".into());
        out.extend(
            env.iter()
                .filter(|(key, value)| !is_cron_command_environment(key, value))
                .map(|(key, value)| format!("{key}={value}")),
        );
        out.push(String::new());
    }

    for job in jobs {
        let mut command = String::new();
        for (key, value) in env
            .iter()
            .filter(|(key, value)| is_cron_command_environment(key, value))
        {
            command.push_str(&format!("{key}={} ", shell_escape(value)));
        }
        command.push_str(&shell_escape(bin));
        for arg in &job.args {
            command.push(' ');
            command.push_str(&shell_escape(arg));
        }
        let when = match &job.schedule {
            When::Calendar(expr) => expr.clone(),
            When::EveryMinutes(n) => format!("*/{n} * * * *"),
        };
        out.push(format!("{when} cd {cwd_str} && {command}"));
    }

    let checked: Vec<String> = out
        .into_iter()
        .map(checked_line)
        .collect::<Result<_, _>>()?;
    println!("{}", checked.join("\n"));
    Ok(())
}

/// Emit one plist per job, separated by a header naming the file it belongs in.
///
/// Jobs are emitted rather than written so the operator reviews before installing — these
/// files run unattended with the user's credentials.
fn print_launchd(
    bin: &str,
    cwd: &std::path::Path,
    jobs: &[Job],
    env: &[(String, String)],
    namespace: &str,
    log_dir: Option<&std::path::Path>,
) -> miette::Result<()> {
    validate_xml_text("binary", bin)?;
    validate_xml_text("working directory", &cwd.display().to_string())?;
    // Resolve every calendar and validate every emitted field before writing a header.
    let mut intervals = Vec::with_capacity(jobs.len());
    for job in jobs {
        validate_xml_text("job label", &format!("{namespace}.{}", job.name))?;
        validate_xml_text("job program", bin)?;
        for arg in &job.args {
            validate_xml_text("job argument", arg)?;
        }
        for (key, value) in env.iter() {
            validate_xml_text("environment key", key)?;
            validate_xml_text(key, value)?;
        }
        intervals.push(match &job.schedule {
            When::Calendar(expr) => Some(cron_to_calendar_intervals(expr).ok_or_else(|| {
                miette::miette!(
                    "schedule `{expr}` for `{}` uses cron syntax launchd cannot express. \
                     Use --format cron or simplify its schedule.",
                    job.name
                )
            })?),
            When::EveryMinutes(_) => None,
        });
    }
    // Every path in a plist must be absolute. launchd execs the program directly and does
    // NOT search a PATH, and it expands no shell syntax — neither a bare name nor a `~` nor
    // a relative path resolves. Each yields a job that fails to spawn with nothing but a
    // cryptic status in `launchctl print`, so both are refused up front rather than emitted
    // as a plist that looks right and never runs.
    if !std::path::Path::new(bin).is_absolute() {
        // A bare name is a PATH lookup; a relative path resolves against the same cwd the
        // jobs run in. Suggesting `command -v` for both would echo a relative path straight
        // back and fail again.
        let absolute = if bin.contains(std::path::MAIN_SEPARATOR) {
            cwd.join(bin).display().to_string()
        } else {
            format!("$(command -v {bin})")
        };
        return Err(miette::miette!(
            "launchd needs an absolute path to the binary — it does not search PATH.\n\
             Re-run with: lore schedule --format launchd --bin \"{absolute}\""
        ));
    }
    let home = std::env::var("HOME").map_err(|_| {
        miette::miette!(
            "HOME is unset, so the absolute log and LaunchAgents paths a plist needs \
             cannot be resolved.\nRun `lore schedule --format launchd` from a normal \
             user session."
        )
    })?;
    let default_log_dir =
        std::path::Path::new(&home)
            .join("Library/Logs")
            .join(if namespace == DEFAULT_NAMESPACE {
                "lorekeeper"
            } else {
                namespace
            });
    let log_dir = log_dir.unwrap_or(&default_log_dir);
    validate_xml_text("HOME", &home)?;
    validate_xml_text("log directory", &log_dir.display().to_string())?;
    for job in jobs {
        validate_xml_text(
            "job log path",
            &log_dir
                .join(format!("{}.log", job.name))
                .display()
                .to_string(),
        )?;
    }
    println!("# lorekeeper scheduled tasks (launchd)");
    println!("#");
    println!("# launchd does not create the log directory, and a job whose StandardOutPath");
    println!("# is unwritable can fail to spawn — so make it first:");
    println!(
        "# Log directory (create before loading): {}",
        log_dir.display()
    );
    println!("#");
    println!(
        "# Retention includes queue pruning; review any old {namespace}.queue-prune job separately."
    );
    println!("# Write each block below to the path in its header, then load it:");
    println!("#   launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/<file>");
    println!("#   launchctl kickstart -p gui/$(id -u)/<label>   # run once now");
    println!("#");
    println!("# Unlike cron, launchd runs a job missed during sleep as soon as the machine");
    println!("# wakes, so a closed laptop at the scheduled hour delays the run instead of");
    println!("# dropping it. Logs land in {}.", log_dir.display());
    println!();

    for (job, intervals) in jobs.iter().zip(intervals) {
        let label = format!("{namespace}.{}", job.name);
        println!("# ── {home}/Library/LaunchAgents/{label}.plist");
        println!(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
        println!(
            r#"<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">"#
        );
        println!(r#"<plist version="1.0">"#);
        println!("<dict>");
        println!("  <key>Label</key><string>{label}</string>");
        println!("  <key>ProgramArguments</key>");
        println!("  <array>");
        println!("    <string>{}</string>", xml_escape(bin));
        for arg in &job.args {
            println!("    <string>{}</string>", xml_escape(arg));
        }
        println!("  </array>");
        let job_env: Vec<_> = env.iter().collect();
        if !job_env.is_empty() {
            println!("  <key>EnvironmentVariables</key>");
            println!("  <dict>");
            for (key, value) in job_env {
                println!(
                    "    <key>{}</key><string>{}</string>",
                    xml_escape(key),
                    xml_escape(value)
                );
            }
            println!("  </dict>");
        }
        println!(
            "  <key>WorkingDirectory</key><string>{}</string>",
            xml_escape(&cwd.display().to_string())
        );
        match (&job.schedule, &intervals) {
            (When::EveryMinutes(minutes), _) => {
                println!(
                    "  <key>StartInterval</key><integer>{}</integer>",
                    u64::from(*minutes) * 60
                );
            }
            (_, Some(intervals)) if intervals.len() == 1 => {
                println!("  <key>StartCalendarInterval</key>");
                println!("  <dict>");
                print_interval(&intervals[0], "    ");
                println!("  </dict>");
            }
            (_, Some(intervals)) => {
                println!("  <key>StartCalendarInterval</key>");
                println!("  <array>");
                for interval in intervals {
                    println!("    <dict>");
                    print_interval(interval, "      ");
                    println!("    </dict>");
                }
                println!("  </array>");
            }
            (_, None) => unreachable!("a calendar job always resolves its intervals"),
        }
        println!(
            "  <key>StandardOutPath</key><string>{}</string>",
            xml_escape(
                &log_dir
                    .join(format!("{}.log", job.name))
                    .display()
                    .to_string()
            )
        );
        println!(
            "  <key>StandardErrorPath</key><string>{}</string>",
            xml_escape(
                &log_dir
                    .join(format!("{}.log", job.name))
                    .display()
                    .to_string()
            )
        );
        println!("  <key>RunAtLoad</key><false/>");
        println!("</dict>");
        println!("</plist>");
        println!();
    }
    Ok(())
}

/// One `StartCalendarInterval` entry. `None` means "every", which launchd expresses by
/// omitting the key.
#[derive(Debug, PartialEq, Eq)]
struct CalendarInterval {
    minute: Option<u32>,
    hour: Option<u32>,
    day: Option<u32>,
    month: Option<u32>,
    weekday: Option<u32>,
}

fn print_interval(interval: &CalendarInterval, indent: &str) {
    for (key, value) in [
        ("Minute", interval.minute),
        ("Hour", interval.hour),
        ("Day", interval.day),
        ("Month", interval.month),
        ("Weekday", interval.weekday),
    ] {
        if let Some(v) = value {
            println!("{indent}<key>{key}</key><integer>{v}</integer>");
        }
    }
}

/// Translate a 5-field cron expression into launchd calendar intervals.
///
/// launchd has no step (`*/5`) or range (`1-5`) syntax — it takes explicit values — so a
/// comma list fans out into several intervals and anything richer returns `None` rather
/// than silently scheduling something different from what the config says.
fn cron_to_calendar_intervals(expr: &str) -> Option<Vec<CalendarInterval>> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        return None;
    }
    let parse = |field: &str| -> Option<Vec<Option<u32>>> {
        if field == "*" {
            return Some(vec![None]);
        }
        // `*/n`, `a-b`, and `a-b/n` have no launchd equivalent.
        if field.contains('/') || field.contains('-') {
            return None;
        }
        field
            .split(',')
            .map(|v| v.parse::<u32>().ok().map(Some))
            .collect()
    };

    // cron ORs day-of-month with day-of-week when BOTH are restricted (`0 9 1 * 1` fires on
    // the 1st AND on every Monday), while launchd ANDs every key it is given (only Mondays
    // that fall on the 1st). There is no launchd spelling for the OR, so this is refused
    // like any other inexpressible syntax — a schedule that silently fires on a different
    // set of days is worse than one that refuses to install.
    if fields[2] != "*" && fields[4] != "*" {
        return None;
    }

    let minutes = parse(fields[0])?;
    let hours = parse(fields[1])?;
    let days = parse(fields[2])?;
    let months = parse(fields[3])?;
    let weekdays = parse(fields[4])?;

    let mut out = Vec::new();
    for &minute in &minutes {
        for &hour in &hours {
            for &day in &days {
                for &month in &months {
                    for &weekday in &weekdays {
                        out.push(CalendarInterval {
                            minute,
                            hour,
                            day,
                            month,
                            weekday,
                        });
                    }
                }
            }
        }
    }
    Some(out)
}

fn validate_xml_text(field: &str, value: &str) -> miette::Result<()> {
    if value.chars().all(|c| {
        matches!(c,
        '\u{9}' | '\u{a}' | '\u{d}' | '\u{20}'..='\u{d7ff}' |
        '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')
    }) {
        Ok(())
    } else {
        Err(miette::miette!("invalid XML 1.0 text in {field}"))
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\r', "&#13;")
        .replace('\n', "&#10;")
        .replace('\t', "&#9;")
}

/// Quote a shell word through cron's percent/backslash preprocessing.
/// Isolate literal backslashes so cron cannot collapse pairs before shell parsing.
fn shell_escape(s: &str) -> String {
    let quoted = if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '.' | ':'))
    {
        s.to_string()
    } else {
        let mut quoted = String::from("'");
        for character in s.chars() {
            match character {
                '\'' => quoted.push_str(r"'\''"),
                '\\' => quoted.push_str(r"''\''"),
                _ => quoted.push(character),
            }
        }
        quoted.push('\'');
        quoted
    };
    quoted.replace('%', r"\%")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_break_in_the_working_directory_is_refused_before_anything_prints() {
        // A comment is not exempt: `# Working dir: <cwd>` ends at the break and leaves the
        // rest as a crontab entry of its own, so a directory named `dir\n* * * * * <cmd>`
        // installs a job. Nothing may be printed before the whole file has passed, or a
        // refusal on a later line still leaves this one in `crontab -`.
        let jobs = [Job {
            name: "ingest".into(),
            schedule: When::Calendar("0 9 * * *".into()),
            args: vec!["ingest".into()],
        }];
        let hostile = std::path::Path::new("/v/dir\n* * * * * echo pwned");
        assert!(print_cron("/usr/bin/lore", hostile, &jobs, &[]).is_err());
        // Also with no jobs at all, where no job line exists to catch it.
        assert!(print_cron("/usr/bin/lore", hostile, &[], &[]).is_err());
    }

    #[test]
    fn a_line_break_anywhere_in_an_assembled_line_is_refused() {
        // The check moved onto the composed line precisely so it covers values nobody
        // enumerated — a schedule expression included, since `validate_cron` splits on
        // whitespace and accepts a newline as a field separator.
        assert!(checked_line("0 9 * * * cd /v && lore ingest".into()).is_ok());
        for broken in [
            "PATH=/a\n/b",
            "0 9 * *\n1 cd /v && lore ingest",
            "0 9 * * * cd /v && /pipe\nlines/lore-daily.sh",
            "LORE_CONFIG=/a/b\r/c",
        ] {
            assert!(
                checked_line(broken.to_string()).is_err(),
                "must refuse: {broken:?}"
            );
        }
    }

    fn config_with_schedules() -> lk_core::config::Config {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(
            &path,
            "vault:\n  root: .\nidentity:\n  name: A\n  email: a@b.c\ningest:\n  schedule: \"0 9 * * *\"\n\
             synthesis:\n  weekly:\n    enabled: true\n    schedule: \"0 8 * * 1\"\n\
             maintenance:\n  schedule: \"0 3 * * *\"\n\
             sources:\n  notes:\n    type: manual\n    enabled: true\n    params:\n      inbox_dir: inbox\n",
        )
        .unwrap();
        lk_core::config::Config::load(&path).unwrap()
    }

    #[test]
    fn native_jobs_preserve_slots_schedules_and_period_dependencies() {
        let mut config = config_with_schedules();
        let mut personal = lk_core::config::PersonalConfig::default();
        personal.monthly.schedule = Some("0 8 1 * *".into());
        personal.quarterly.schedule = Some("0 8 1 1,4,7,10 *".into());
        personal.annual.schedule = Some("0 8 1 1 *".into());
        config.personal = Some(personal);
        let jobs = build_jobs(&config, &[]);
        let named = |name: &str| jobs.iter().find(|job| job.name == name).unwrap();
        assert_eq!(named("ingest").args, ["run", "daily", "--wait"]);
        assert_eq!(
            named("maintenance").args,
            ["maintenance", "--prune-queue", "--wait-for-vault"]
        );
        assert!(!jobs.iter().any(|job| job.name == "queue-prune"));
        for period in ["weekly", "monthly", "quarterly", "annual"] {
            assert_eq!(
                named(&format!("synthesis-{period}")).args,
                [
                    "synthesis",
                    period,
                    "--previous",
                    "--defer",
                    "--wait-for-vault"
                ]
            );
        }
        for (period, schedule) in config.personal.as_ref().unwrap().review_schedules() {
            assert!(
                matches!(&named(&format!("synthesis-{period}")).schedule, When::Calendar(value) if value == schedule)
            );
        }
    }

    #[test]
    fn semantic_audit_schedule_is_independent_of_synthesis_and_personal_tracking() {
        let mut config = config_with_schedules();
        config.synthesis.weekly.enabled = false;
        config.personal = None;
        config.graph.audit.schedule = Some("0 7 * * 0".into());
        let jobs = build_jobs(&config, &[]);
        assert!(!jobs.iter().any(|job| job.name == "synthesis-weekly"));
        let audit = jobs
            .iter()
            .find(|job| job.name == "knowledge-audit")
            .unwrap();
        assert_eq!(audit.args, ["run", "knowledge", "--wait"]);
        assert!(matches!(&audit.schedule, When::Calendar(value) if value == "0 7 * * 0"));
        config.graph.audit.schedule = None;
        assert!(
            !build_jobs(&config, &[])
                .iter()
                .any(|job| job.name == "knowledge-audit")
        );
    }

    #[test]
    fn launchd_refuses_a_binary_path_it_cannot_exec() {
        // launchd resolves neither a PATH lookup nor a relative path, so both are refused.
        // A relative path is the one that would otherwise slip through a has-a-slash check
        // and emit a plist that looks right.
        let jobs = [Job {
            name: "daily".into(),
            schedule: When::Calendar("0 9 * * *".into()),
            args: vec!["ingest".into()],
        }];
        let cwd = std::path::Path::new("/vault");
        for bin in ["lore", "./lore", "target/release/lore", "../bin/lore"] {
            assert!(
                print_launchd(bin, cwd, &jobs, &[], DEFAULT_NAMESPACE, None).is_err(),
                "`{bin}` is not something launchd can exec"
            );
        }
    }

    #[test]
    fn escape_simple() {
        assert_eq!(shell_escape("foo"), "foo");
        assert_eq!(shell_escape("foo-bar"), "foo-bar");
        assert_eq!(shell_escape("/usr/bin/lore"), "/usr/bin/lore");
    }

    #[test]
    fn escape_with_spaces() {
        assert_eq!(shell_escape("foo bar"), "'foo bar'");
    }

    #[test]
    fn escape_with_quote() {
        assert_eq!(shell_escape("it's"), r"'it'\''s'");
    }

    #[test]
    fn escape_empty() {
        assert_eq!(shell_escape(""), "''");
    }

    #[test]
    fn escape_percent_for_cron() {
        assert_eq!(shell_escape("foo%bar"), r"'foo\%bar'");
        assert_eq!(shell_escape("100%"), r"'100\%'");
    }

    #[cfg(unix)]
    #[test]
    fn cron_shell_words_survive_both_backslash_preprocessors() {
        for collapse_backslash_pairs in [false, true] {
            for value in [
                "/normal",
                "/with space ",
                "both'\"\\% ",
                "foo\\%bar",
                "foo\\\\bar",
                "",
                "100%",
            ] {
                let mut decoded = String::new();
                let mut escaped = false;
                // Cronie unescapes percent; Apple's cron also unescapes backslash pairs.
                for character in shell_escape(value).chars() {
                    if escaped {
                        if character == '%' || collapse_backslash_pairs && character == '\\' {
                            assert_eq!(decoded.pop(), Some('\\'));
                        }
                        decoded.push(character);
                        escaped = false;
                    } else {
                        assert_ne!(character, '%', "unexpected cron stdin separator");
                        decoded.push(character);
                        escaped = character == '\\';
                    }
                }
                let output = std::process::Command::new("/bin/sh")
                    .args(["-c", &format!("printf '%s' {decoded}")])
                    .output()
                    .unwrap();
                assert!(output.status.success(), "{decoded:?}: {output:?}");
                assert_eq!(output.stdout, value.as_bytes());
            }
        }
    }

    #[test]
    fn daily_cron_becomes_one_interval_omitting_every_fields() {
        // `0 9 * * *` — omitted keys are launchd's "every", so only Minute/Hour appear.
        let got = cron_to_calendar_intervals("0 9 * * *").unwrap();
        assert_eq!(
            got,
            vec![CalendarInterval {
                minute: Some(0),
                hour: Some(9),
                day: None,
                month: None,
                weekday: None,
            }]
        );
    }

    #[test]
    fn quarterly_month_list_fans_out_into_one_interval_each() {
        let got = cron_to_calendar_intervals("0 8 1 1,4,7,10 *").unwrap();
        assert_eq!(got.len(), 4);
        assert_eq!(
            got.iter().map(|i| i.month.unwrap()).collect::<Vec<_>>(),
            vec![1, 4, 7, 10]
        );
        assert!(got.iter().all(|i| i.day == Some(1) && i.hour == Some(8)));
    }

    #[test]
    fn weekday_schedule_is_preserved() {
        let got = cron_to_calendar_intervals("0 8 * * 1").unwrap();
        assert_eq!(got[0].weekday, Some(1));
    }

    #[test]
    fn syntax_launchd_cannot_express_is_refused_not_approximated() {
        // Silently dropping a step/range would schedule something other than the config
        // says — a wrong schedule is worse than a refused one.
        assert!(cron_to_calendar_intervals("*/5 * * * *").is_none());
        assert!(cron_to_calendar_intervals("0 9 * * 1-5").is_none());
        assert!(
            cron_to_calendar_intervals("0 9 * *").is_none(),
            "too few fields"
        );
        assert!(cron_to_calendar_intervals("bad 9 * * *").is_none());
        // cron ORs these two fields; launchd ANDs them — no faithful translation exists.
        assert!(
            cron_to_calendar_intervals("0 9 1 * 1").is_none(),
            "day-of-month AND weekday together must be refused, not approximated"
        );
        // Either one alone is still fine.
        assert!(cron_to_calendar_intervals("0 9 1 * *").is_some());
        assert!(cron_to_calendar_intervals("0 9 * * 1").is_some());
    }

    #[test]
    fn xml_escape_protects_plist_structure() {
        assert_eq!(xml_escape("a&b<c>d"), "a&amp;b&lt;c&gt;d");
        assert_eq!(xml_escape("\r\n\t"), "&#13;&#10;&#9;");
        assert!(validate_xml_text("test", "\r\n\t文😀").is_ok());
        assert!(validate_xml_text("test", "\u{fffe}").is_err());
        assert!(validate_xml_text("test", "\u{ffff}").is_err());
    }
}
