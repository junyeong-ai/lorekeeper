//! Pins the hand-written lists in the shipped installers to what the repository actually holds.
//!
//! Both are lists a human must remember to update, in files no compiler reads. A seventh skill
//! added under `skills` passed every gate and was then never packaged, never published
//! and never installed, because the name had to be added to `install.sh` and `install.ps1`
//! separately. The MSRV is the same shape: one `rust-version` and several restatements of it,
//! with nothing comparing them.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Every directory under `skills`, which is what the release workflow packages (it
/// enumerates the directory) and therefore what a release ships.
fn shipped_skills() -> Vec<String> {
    let dir = repo_root().join("skills");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert!(!names.is_empty(), "no skills under {}", dir.display());
    names
}

/// The quoted names on the one line of `script` that declares its skill list. Read from the
/// declaration rather than by scanning the file, so a legacy scheduled-task name elsewhere in it
/// is not mistaken for a skill — and so both directions can be compared as sets: a missing name
/// is never installed, and a leftover name asks the release for an archive it does not contain.
fn declared_skills(script: &str, anchor: &str) -> Vec<String> {
    let path = repo_root().join(script);
    let body = read(&path);
    let line = body
        .lines()
        .find(|line| line.contains(anchor))
        .unwrap_or_else(|| panic!("{script} no longer declares its skill list with `{anchor}`"));
    let mut names: Vec<String> = line
        .split(['\'', '"'])
        .filter(|token| token.starts_with("lore-"))
        .map(str::to_owned)
        .collect();
    names.sort();
    names
}

/// The uninstallers carry their own list because they must work when the binary is gone — the
/// installers no longer do, since `lore self deploy` writes what the binary carries. A skill
/// missing from an uninstaller stays on disk, and stale skill instructions an agent still loads
/// are worse than a leftover file: they describe a binary that is no longer there.
#[test]
fn the_uninstall_scripts_list_every_skill() {
    let skills = shipped_skills();
    for (script, anchor) in [
        ("scripts/uninstall.sh", "SKILL_NAMES=("),
        ("scripts/uninstall.ps1", "$SkillNames = @("),
    ] {
        assert_eq!(
            declared_skills(script, anchor),
            skills,
            "{script}'s skill list must be exactly the directories under skills"
        );
    }
}

/// `scripts/check.sh` says a green run there is the same answer CI gives, and nothing compared
/// the two — so five of ten jobs went unrun while the claim stood, and a release shipped after a
/// green local run whose shellcheck, actionlint, MSRV and audit had never been asked.
#[test]
fn every_ci_gate_is_run_or_declared_unrunnable() {
    let workflow = read(&repo_root().join(".github/workflows/ci.yml"));
    let jobs = workflow
        .lines()
        .skip_while(|line| line.trim() != "jobs:")
        .filter_map(|line| line.strip_prefix("  ")?.strip_suffix(':'))
        .filter(|name| !name.starts_with(char::is_whitespace) && is_job_name(name))
        .collect::<Vec<_>>();
    assert!(jobs.len() > 5, "read no job list out of ci.yml: {jobs:?}");

    let script = read(&repo_root().join("scripts/check.sh"));
    let answered = |verb: &str| -> Vec<&str> {
        script
            .lines()
            .filter_map(|line| line.strip_prefix(&format!("{verb} ")))
            .map(|rest| rest.split_whitespace().next().unwrap_or(""))
            .collect()
    };
    let run = answered("gate");
    let declared = answered("unrunnable");

    for job in jobs {
        assert!(
            run.contains(&job) || declared.contains(&job),
            "ci.yml runs `{job}` and scripts/check.sh neither runs it nor declares why it cannot"
        );
    }
}

/// A key under `jobs:` rather than a nested mapping key that happens to sit at the same indent.
fn is_job_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c == '-')
}

/// The archive an installer asks for has to be one the release builds. They are separate lists in
/// separate languages, so a target renamed on either side is a 404 at install time — the first
/// thing a new user sees, and the one failure they cannot work around.
#[test]
fn every_target_an_installer_asks_for_is_one_the_release_builds() {
    let release = read(&repo_root().join(".github/workflows/release.yml"));
    let built: Vec<&str> = release
        .lines()
        .filter_map(|line| line.trim().strip_prefix("- target: "))
        .map(str::trim)
        .collect();
    assert!(!built.is_empty(), "release.yml declares no build targets");

    for script in ["scripts/install.sh", "scripts/install.ps1"] {
        let body = read(&repo_root().join(script));
        // Anchored on the architecture prefix, not on a well-formed suffix: recognizing only
        // `-musl`/`-darwin`/`-msvc` endings would not see a MISSPELLED target at all, which is
        // the drift this exists for.
        let named: Vec<&str> = body
            .split(['\'', '"'])
            .filter(|token| token.starts_with("x86_64-") || token.starts_with("aarch64-"))
            .collect();
        assert!(!named.is_empty(), "{script} names no build target");
        for target in named {
            assert!(
                built.contains(&target),
                "{script} can ask for `{target}`, which the release does not build"
            );
        }
    }
}

/// The leading word of every command a fenced `bash` block spells, in order.
///
/// Exact over the shell these documents use — quoted strings, comment tails, redirections, line
/// continuations, command substitution, and `|` / `&&` / `||` / `;` chains — and it REFUSES a
/// construct it cannot read rather than guessing at one. Guessing would answer for a document
/// nobody wrote, in either direction: a step reported unpermitted that is not a command, or a
/// command hidden inside something the scan skipped.
fn spelled_commands(markdown: &str, origin: &str) -> Vec<String> {
    let mut found = Vec::new();
    for block in markdown.split("```bash").skip(1) {
        let Some(code) = block.split("```").next() else {
            continue;
        };
        assert!(
            !code.contains("<<") && !code.contains('`'),
            "{origin}: a heredoc or a backtick is outside the shell this reads; \
             spell the step without one, or teach this function the construct"
        );
        for line in code.replace("\\\n", " ").lines() {
            let line = line.trim().trim_start_matches("$ ");
            // A slash-command is an instruction to the session, not a command to a shell.
            if line.starts_with('/') {
                continue;
            }
            found.extend(stages(line, origin).into_iter().filter_map(head));
        }
    }
    found
}

/// One line split into the commands a shell would run, honouring quotes: an operator inside a
/// quoted argument is data, and `--params '{"q":"a|b"}'` is one command, not two.
fn stages(line: &str, origin: &str) -> Vec<String> {
    let (mut stages, mut current) = (Vec::new(), String::new());
    let (mut single, mut double) = (false, false);
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            // `#` opens a comment at the start of a word, never mid-token (`a#b` is one word).
            '#' if !single
                && !double
                && current.chars().next_back().is_none_or(char::is_whitespace) =>
            {
                break;
            }
            '|' | '&' | ';' | '(' | ')' if !single && !double => {
                stages.push(std::mem::take(&mut current));
                continue;
            }
            '$' if !single && chars.peek() == Some(&'(') => {
                chars.next();
                stages.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    assert!(
        !single && !double,
        "{origin}: a quote opens and never closes on `{line}`, so where a command begins is \
         not decidable"
    );
    stages.push(current);
    stages
}

/// The command a stage runs: its first word, past any `NAME=value` assignment prefix.
fn head(stage: String) -> Option<String> {
    stage
        .split_whitespace()
        .find(|word| {
            !word
                .split_once('=')
                .is_some_and(|(name, _)| !name.is_empty() && name.chars().all(is_name_char))
        })
        .map(str::to_owned)
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Executable basenames with skill preapproval, without checking subcommand coverage.
fn permitted_tools(frontmatter: &str) -> Vec<String> {
    frontmatter
        .lines()
        .filter_map(|line| line.trim().strip_prefix("Bash("))
        .filter_map(|rest| rest.strip_suffix(')'))
        .filter_map(|spec| {
            spec.split([' ', ':'])
                .next()
                .filter(|head| !head.is_empty())
                .map(str::to_owned)
        })
        .collect()
}

#[test]
fn a_spelled_command_is_read_the_way_a_shell_would() {
    let read_block = |code: &str| spelled_commands(&format!("```bash\n{code}\n```"), "fixture");

    // An operator inside a quoted argument is data.
    assert_eq!(
        read_block(r#"gws list --params '{"q":"a|b;c"}' | jq -r '.n'"#),
        vec!["gws", "jq"]
    );
    // A comment tail ends the line; a full-line comment is not a command.
    assert_eq!(
        read_block("# not a step\nlore graph lint   # non-zero means it contradicts itself"),
        vec!["lore"]
    );
    // A continuation carries flags, not a new command.
    assert_eq!(
        read_block("atlassian-cli jira search \"x\" \\\n  --limit 5 --fields summary"),
        vec!["atlassian-cli"]
    );
    // Command substitution runs a command, and a `||` chain is two decisions.
    assert_eq!(
        read_block(r#"mv "$f" "$d/" || [ -f "$d/$(basename "$f")" ]"#),
        vec!["mv", "[", "basename"]
    );
    // A redirection is not a command, and an assignment prefix is not one either.
    assert_eq!(
        read_block("ls \"$VAULT/queue/\"*.jsonl 2>/dev/null\nLC_ALL=C sort -z"),
        vec!["ls", "sort"]
    );
}

/// Bash examples use executables with a corresponding skill preapproval entry.
/// This does not validate argument patterns, prose commands or baseline host permissions.
#[test]
fn every_spelled_executable_has_a_skill_preapproval_entry() {
    let mut denied: Vec<String> = Vec::new();
    for dir in std::fs::read_dir(repo_root().join("skills"))
        .expect("skills dir")
        .flatten()
        .filter(|entry| entry.path().is_dir())
    {
        let skill = dir.file_name().to_string_lossy().into_owned();
        let manifest = read(&dir.path().join("SKILL.md"));
        let metadata: serde_json::Value =
            serde_json::from_str(&read(&dir.path().join("agents/claude.json"))).unwrap();
        let permitted = permitted_tools(metadata["allowed-tools"].as_str().expect("Claude tools"));
        if permitted.is_empty() {
            continue;
        }

        let mut sources = vec![(format!("{skill}/SKILL.md"), manifest)];
        let references = dir.path().join("references");
        if references.is_dir() {
            for entry in std::fs::read_dir(&references)
                .expect("references")
                .flatten()
            {
                let name = format!("{skill}/references/{}", entry.file_name().to_string_lossy());
                sources.push((name, read(&entry.path())));
            }
        }

        for (origin, body) in sources {
            for command in spelled_commands(&body, &origin) {
                if !permitted.contains(&command) {
                    denied.push(format!("{origin}: spells `{command}`"));
                }
            }
        }
    }
    denied.sort();
    denied.dedup();
    assert!(
        denied.is_empty(),
        "these executable basenames appear in Bash examples without a skill preapproval entry:\n  {}",
        denied.join("\n  ")
    );
}

/// The two scripts agree about where an install lives.
///
/// `install.sh --install-dir /opt/bin` put the binary somewhere `uninstall.sh` had no way to be
/// told about, so it reported "Nothing to uninstall" and left it there.
#[test]
fn the_uninstaller_takes_every_path_flag_the_installer_does() {
    for (installer, uninstaller, flags) in [
        (
            "scripts/install.sh",
            "scripts/uninstall.sh",
            ["--install-dir", "--data-dir"],
        ),
        // The PowerShell pair had the same hole and no gate: `install.ps1 -InstallDir C:\tools`
        // was invisible to its uninstaller, which reported nothing to remove.
        (
            "scripts/install.ps1",
            "scripts/uninstall.ps1",
            ["$InstallDir", "$DataDir"],
        ),
    ] {
        let install = read(&repo_root().join(installer));
        let uninstall = read(&repo_root().join(uninstaller));
        for flag in flags {
            assert!(install.contains(flag), "{installer} lost {flag}");
            assert!(
                uninstall.contains(flag),
                "{uninstaller} cannot be told about {flag}, so an install made with it is invisible"
            );
        }
    }
}

/// No skill spells out a vocabulary `AGENTS.md` already generates.
///
/// Enumerating the `document_type` values in a skill copies a list the schema generator emits
/// from `DOCUMENT_TYPES` into the very file those skills are told to read ("derive everything
/// from AGENTS.md — never hardcode"). The generated page is the only place the vocabulary
/// belongs; a copy has to be deleted rather than kept in sync, since `data` is an ordinary
/// English word and detecting drift between the copies takes a windowed search.
#[test]
fn no_skill_restates_a_vocabulary_agents_md_generates() {
    for skill in glob_skill_markdown() {
        let body = read(&skill);
        for (at, _) in body.match_indices("document_type") {
            let line = body[..at].rfind('\n').map_or(0, |i| i + 1);
            let line = &body[line..at + body[at..].find('\n').unwrap_or(0)];
            let named: Vec<&&str> = lk_core::document::DOCUMENT_TYPES
                .iter()
                .filter(|value| names_word(line, value))
                .collect();
            // One value is a legitimate example (`document_type: note` in a sample page); two or
            // more on one line is the vocabulary restated.
            assert!(
                named.len() < 2,
                "{} restates the document_type vocabulary ({named:?}) that AGENTS.md generates \
                 from DOCUMENT_TYPES — point at it instead:\n  {line}",
                skill.display()
            );
        }
    }
}

/// Whether `text` contains `word` as a whole word, so `note` does not match `notebook`.
fn names_word(text: &str, word: &str) -> bool {
    text.match_indices(word).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + word.len()..].chars().next();
        let boundary = |c: Option<char>| c.is_none_or(|c| !c.is_alphanumeric() && c != '_');
        boundary(before) && boundary(after)
    })
}

/// Every `.md` under `skills`, SKILL.md and `references/` alike.
fn glob_skill_markdown() -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![repo_root().join("skills")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
            .flatten()
        {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "md") {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

/// Scheduled and semantic consumers refresh SQLite discovery and its small entry guide,
/// never request the removed Markdown corpus lists.
#[test]
fn knowledge_finalization_refreshes_sqlite_and_the_declared_guides() {
    let mut refreshing = 0;
    for skill in glob_skill_markdown()
        .into_iter()
        .chain([repo_root().join("crates/lk-cli/src/commands/schema.rs")])
    {
        let body = read(&skill);
        refreshing += usize::from(body.contains("lore wiki refresh"));
        for removed in [
            "lore wiki index",
            "lore wiki log",
            "lore wiki map",
            "lore graph index-sync",
        ] {
            assert!(
                !body.contains(removed),
                "{} still invokes removed {removed}",
                skill.display()
            );
        }
    }
    assert!(
        refreshing > 0,
        "no skill refreshes SQLite knowledge discovery"
    );
    let mut named: Vec<_> = lk_core::vault_path::GENERATED_WIKI_PAGES
        .iter()
        .map(|(page, _)| *page)
        .collect();
    named.sort_unstable();
    let mut reserved = lk_core::vault_path::RESERVED_WIKI_FILES.to_vec();
    reserved.sort_unstable();
    assert_eq!(
        named, reserved,
        "each reserved guide declares its generating command"
    );
}

/// An adapter that ships and appears in no document is one nobody can configure. Both READMEs
/// carried a source table naming eight of nine — `confluence` appeared in neither, so the only way
/// to discover it was to read the enum. `config.example.yaml` is the file the installer copies and
/// the READMEs call the reference for every source, so each type has to appear there too, with
/// params its own adapter accepts: a sample that fails `lore validate` is worse than no sample.
#[test]
fn every_source_type_is_documented_and_exemplified() {
    use strum::IntoEnumIterator;

    let root = repo_root();
    let example = read(&root.join("config.example.yaml"));
    let readmes: Vec<(&str, String)> = ["README.md", "README.en.md"]
        .into_iter()
        .map(|name| (name, read(&root.join(name))))
        .collect();

    for source_type in lk_core::config::SourceType::iter() {
        let wire = source_type.to_string();
        assert!(
            example.contains(&format!("type: {wire}")),
            "config.example.yaml has no `{wire}` source, which the READMEs call the reference for \
             every source"
        );
        for (name, body) in &readmes {
            assert!(
                source_table(body).contains(&wire),
                "{name}'s source table has no `{wire}` row, so a shipped adapter is undiscoverable"
            );
        }
    }

    // Where a source's pages land is one branch in the pipeline — `manual` writes documents,
    // every other type writes a daily page. Stated as that rule, `lore-ingest` cannot fall behind
    // a new adapter; stated as a list, it silently does. The gate is that it stays a rule: no
    // adapter name beside the path template it routes to.
    const DAILY_TEMPLATE: &str = "`<daily>/{source-id}/DATE.md`";
    let enumerated: Vec<String> = read(&root.join("skills/lore-ingest/SKILL.md"))
        .lines()
        .filter(|line| line.contains(DAILY_TEMPLATE))
        .flat_map(|line| {
            let lowered = line.to_lowercase();
            lk_core::config::SourceType::iter()
                .map(|source_type| source_type.to_string())
                .filter(move |wire| wire != "manual" && lowered.contains(wire.as_str()))
        })
        .collect();
    assert!(
        enumerated.is_empty(),
        "lore-ingest names {enumerated:?} beside {DAILY_TEMPLATE} instead of stating the rule \
         every source type follows — a list goes stale the moment an adapter is added"
    );
}

/// The type names in the first column of the README's source table.
///
/// Located by its heading and bounded by the end of the table, not collected from every line in
/// the file that happens to start like a row: a standalone `| `x` | … |` line elsewhere satisfied
/// the looser rule, and a legitimate row whose cell is not backticked failed it. Both directions
/// were wrong, in a gate whose whole purpose is to say where it looked.
fn source_table(body: &str) -> Vec<String> {
    let table = body
        .lines()
        .skip_while(|line| !is_sources_heading(line))
        .skip(1)
        .skip_while(|line| !line.trim_start().starts_with('|'))
        .take_while(|line| line.trim_start().starts_with('|'));
    table
        .filter_map(|row| row.trim().trim_start_matches('|').split('|').next())
        .map(|cell| cell.trim().trim_matches('`').trim().to_owned())
        .filter(|cell| !cell.is_empty() && !cell.starts_with('-'))
        .collect()
}

/// The heading the source table sits under, in either README's language.
fn is_sources_heading(line: &str) -> bool {
    let heading = line.trim();
    heading.starts_with("## ") && matches!(&heading[3..], "Sources" | "소스")
}

/// Every source in the shipped example must pass its own adapter's parameter validation. The
/// example is what `install.sh` copies and what a user edits, and `Config::load` alone checks only
/// the core keys — a typo in an adapter's params sat in it until someone ran `lore validate`.
#[test]
fn every_source_in_the_example_config_validates_against_its_adapter() {
    let path = repo_root().join("config.example.yaml");
    let config = lk_core::config::Config::load(&path).expect("the shipped example must load");
    let mut checked = 0;
    // Disabled sources included: the example disables most of them, and a broken sample is
    // exactly what a user hits when they enable one.
    for (id, source) in &config.sources {
        lk_source::validate_params(source.source_type, &source.params).unwrap_or_else(|e| {
            panic!(
                "config.example.yaml sources.{id} ({}): {e}",
                source.source_type
            )
        });
        checked += 1;
    }
    assert!(checked > 0, "the example config declares no sources");
}

/// `rust-version` is the one the toolchain enforces, and the READMEs are the only other place it
/// belongs: they are what a person reads to decide whether they can build this. CI reads it out of
/// the manifest, so a restatement there would be a fourth copy of one number and there is nothing
/// in the workflow to compare.
#[test]
fn both_readmes_state_the_msrv_the_manifest_declares() {
    let root = repo_root();
    let manifest = read(&root.join("Cargo.toml"));
    let msrv = manifest
        .lines()
        .find_map(|line| line.strip_prefix("rust-version = "))
        .map(|value| value.trim().trim_matches('"').to_string())
        .expect("workspace.package must declare rust-version");

    for readme in ["README.md", "README.en.md"] {
        let body = read(&root.join(readme));
        assert!(
            body.contains(&msrv),
            "{readme} must state MSRV {msrv} (badge and prose)"
        );
    }
    let ci = read(&root.join(".github/workflows/ci.yml"));
    assert!(
        !ci.contains(&msrv),
        "the CI workflow must not restate MSRV {msrv} — it reads `rust-version` from the manifest"
    );
}
