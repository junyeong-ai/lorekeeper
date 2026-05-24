use super::{find_config, load_config};

pub async fn run(opts: &super::GlobalOpts, bin: &str) -> miette::Result<()> {
    let config_path = find_config(opts)?;
    let config = load_config(&config_path)?;
    let cwd = std::env::current_dir().unwrap_or_default();
    let cwd_str = shell_escape(&cwd.display().to_string());
    let bin_escaped = shell_escape(bin);

    // Pass --config/--template-dir through to scheduled commands so they don't depend on CWD.
    let mut flags = String::new();
    if let Some(p) = opts.config.as_ref().or(Some(&config_path)) {
        flags.push_str(&format!(
            " --config {}",
            shell_escape(&p.display().to_string())
        ));
    }
    if let Some(p) = opts.template_dir.as_ref() {
        flags.push_str(&format!(
            " --template-dir {}",
            shell_escape(&p.display().to_string())
        ));
    }

    println!("# lorekeeper scheduled tasks");
    println!("# Paste into your crontab: crontab -e");
    println!("# Requires: {bin_escaped} in PATH (cargo install --path crates/lk-cli)");
    println!("# Working dir: {}", cwd.display());
    println!();

    for (id, sc) in config.enabled_sources() {
        if let Some(ref sched) = sc.schedule {
            let id_escaped = shell_escape(id);
            println!("{sched} cd {cwd_str} && {bin_escaped}{flags} ingest {id_escaped}");
        }
    }

    for (period, sched) in config.synthesis.schedules() {
        println!("{sched} cd {cwd_str} && {bin_escaped}{flags} synthesis {period} --previous");
    }

    Ok(())
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
}
