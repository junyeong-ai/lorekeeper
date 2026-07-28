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

/// One scheduled command, format-independent.
struct Job {
    /// Suffix of the launchd label / identity in comments (`ingest`, `synthesis-weekly`, …).
    name: String,
    /// 5-field cron expression from config.
    schedule: String,
    /// Arguments after the binary.
    args: Vec<String>,
}

pub async fn run(opts: &super::GlobalOptions, bin: &str, format: Format) -> miette::Result<()> {
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

    let mut jobs: Vec<Job> = Vec::new();
    let mut push = |name: &str, schedule: &str, tail: &[&str]| {
        let mut args = flags.clone();
        args.extend(tail.iter().map(|s| (*s).to_string()));
        jobs.push(Job {
            name: name.to_string(),
            schedule: schedule.to_string(),
            args,
        });
    };

    if let Some(ref sched) = config.ingest.schedule {
        push("ingest", sched, &["ingest"]);
    }
    if config.synthesis.weekly.enabled
        && let Some(sched) = &config.synthesis.weekly.schedule
    {
        push(
            "synthesis-weekly",
            sched,
            &["synthesis", "weekly", "--previous"],
        );
    }
    if let Some(personal) = &config.personal {
        for (period, sched) in personal.review_schedules() {
            push(
                &format!("synthesis-{period}"),
                sched,
                &["synthesis", period, "--previous"],
            );
        }
    }
    if let Some(ref sched) = config.maintenance.schedule {
        push("maintenance", sched, &["maintenance"]);
        push("queue-prune", sched, &["queue", "prune"]);
    }

    match format {
        Format::Cron => print_cron(bin, &cwd, &jobs),
        Format::Launchd => print_launchd(bin, &cwd, &jobs)?,
    }
    Ok(())
}

fn print_cron(bin: &str, cwd: &std::path::Path, jobs: &[Job]) {
    let cwd_str = shell_escape(&cwd.display().to_string());
    let bin_escaped = shell_escape(bin);

    println!("# lorekeeper scheduled tasks");
    println!("# Paste into your crontab: crontab -e");
    println!("# Requires: {bin_escaped} in PATH (cargo install --path crates/lk-cli)");
    println!("# Working dir: {}", cwd.display());
    println!("#");
    println!("# On macOS prefer `lore schedule --format launchd`: cron SKIPS a job whose time");
    println!("# passed while the machine was asleep, which silently drops a day's ingest.");
    println!();

    for job in jobs {
        let args = job
            .args
            .iter()
            .map(|a| shell_escape(a))
            .collect::<Vec<_>>()
            .join(" ");
        println!("{} cd {cwd_str} && {bin_escaped} {args}", job.schedule);
    }
}

/// Emit one plist per job, separated by a header naming the file it belongs in.
///
/// Jobs are emitted rather than written so the operator reviews before installing — these
/// files run unattended with the user's credentials.
fn print_launchd(bin: &str, cwd: &std::path::Path, jobs: &[Job]) -> miette::Result<()> {
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
    println!("# lorekeeper scheduled tasks (launchd)");
    println!("#");
    println!("# launchd does not create the log directory, and a job whose StandardOutPath");
    println!("# is unwritable can fail to spawn — so make it first:");
    println!("#   mkdir -p {home}/Library/Logs/lorekeeper");
    println!("#");
    println!("# Write each block below to the path in its header, then load it:");
    println!("#   launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/<file>");
    println!("#   launchctl kickstart -p gui/$(id -u)/<label>   # run once now");
    println!("#");
    println!("# Unlike cron, launchd runs a job missed during sleep as soon as the machine");
    println!("# wakes, so a closed laptop at the scheduled hour delays the run instead of");
    println!("# dropping it. Logs land in {home}/Library/Logs/lorekeeper/.");
    println!();

    for job in jobs {
        let label = format!("com.lorekeeper.{}", job.name);
        let intervals = cron_to_calendar_intervals(&job.schedule).ok_or_else(|| {
            miette::miette!(
                "schedule `{}` for `{}` uses cron syntax launchd cannot express \
                 (step/range values). Use `--format cron`, or simplify the expression.",
                job.schedule,
                job.name
            )
        })?;

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
        println!(
            "  <key>WorkingDirectory</key><string>{}</string>",
            xml_escape(&cwd.display().to_string())
        );
        println!("  <key>StartCalendarInterval</key>");
        if intervals.len() == 1 {
            println!("  <dict>");
            print_interval(&intervals[0], "    ");
            println!("  </dict>");
        } else {
            println!("  <array>");
            for interval in &intervals {
                println!("    <dict>");
                print_interval(interval, "      ");
                println!("    </dict>");
            }
            println!("  </array>");
        }
        println!(
            "  <key>StandardOutPath</key><string>{}/Library/Logs/lorekeeper/{}.log</string>",
            xml_escape(&home),
            job.name
        );
        println!(
            "  <key>StandardErrorPath</key><string>{}/Library/Logs/lorekeeper/{}.log</string>",
            xml_escape(&home),
            job.name
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

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Minimal shell-escape: leaves safe identifiers untouched, otherwise wraps in single quotes.
/// Additionally escapes `%` because cron treats it as newline-in-command even inside quotes.
fn shell_escape(s: &str) -> String {
    let quoted = if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '.' | ':'))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    };
    quoted.replace('%', r"\%")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launchd_refuses_a_binary_path_it_cannot_exec() {
        // launchd resolves neither a PATH lookup nor a relative path, so both are refused.
        // A relative path is the one that would otherwise slip through a has-a-slash check
        // and emit a plist that looks right.
        let jobs = [Job {
            name: "daily".into(),
            schedule: "0 9 * * *".into(),
            args: vec!["ingest".into()],
        }];
        let cwd = std::path::Path::new("/vault");
        for bin in ["lore", "./lore", "target/release/lore", "../bin/lore"] {
            assert!(
                print_launchd(bin, cwd, &jobs).is_err(),
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
    }
}
