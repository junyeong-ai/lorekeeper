//! Pins that every `lore` subcommand is named by a skill.
//!
//! The skills are the operator's interface to this binary: `/lore-ingest` carries the
//! command reference, and the others cite the commands their procedure runs. A command
//! that exists but appears in none of them is one no agent following a skill will ever
//! reach — `lore doctor` shipped that way and stayed invisible through three releases.
//!
//! The command list is the argument tree ITSELF, walked through `clap::CommandFactory`, so it
//! cannot drift from what the binary accepts. Exemptions are explicit and few: a command earns
//! one only by being unreachable from a skill BY DESIGN, and adding a command forces the choice
//! here.

use clap::CommandFactory;
use std::path::PathBuf;

/// Commands deliberately outside every skill's scope.
const EXEMPT: &[(&str, &str)] = &[("help", "clap's own; not a pipeline operation")];

/// Every subcommand `lore` accepts, from the argument tree rather than from its rendered help.
///
/// `--help` output is a layout: the names sit at one indent, a wrapped description at a deeper
/// one, and colour escapes wrap the header when the terminal asks for them. Reading it means
/// depending on all three, and a parse that comes back empty blames a help-layout change for
/// what may be the developer's own environment. `Cli::command()` is the surface itself.
fn subcommands() -> Vec<String> {
    let names: Vec<String> = lore::Cli::command()
        .get_subcommands()
        .map(|sub| sub.get_name().to_string())
        .collect();
    assert!(!names.is_empty(), "the CLI declares no subcommands");
    names
}

/// Every markdown file the skills ship, by path: the corpus concatenates them, which loses
/// which file a line came from.
fn skill_markdown() -> Vec<PathBuf> {
    let skills_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../skills");
    let mut files = Vec::new();
    let mut stack = vec![skills_dir];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
            .flatten()
        {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "md") {
                files.push(path);
            }
        }
    }
    files.sort();
    assert!(!files.is_empty(), "no skill markdown found");
    files
}

fn skill_corpus() -> String {
    let skills_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../skills");
    let mut corpus = String::new();
    let mut stack = vec![skills_dir.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
            .flatten()
        {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "md") {
                corpus.push_str(&std::fs::read_to_string(&path).expect("read skill file"));
                corpus.push('\n');
            }
        }
    }
    assert!(
        !corpus.is_empty(),
        "no skill markdown found under {}",
        skills_dir.display()
    );
    corpus
}

/// Whether `corpus` names `lore <name>` as a WHOLE word. A plain substring test would let a
/// future `lore doc` pass on the strength of an existing `lore doctor` — the coverage gap
/// this file exists to catch, reported as covered.
fn names_command(corpus: &str, name: &str) -> bool {
    let needle = format!("lore {name}");
    corpus.match_indices(&needle).any(|(at, _)| {
        corpus[at + needle.len()..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric() && c != '-' && c != '_')
    })
}

#[test]
fn every_subcommand_is_named_by_a_skill() {
    let corpus = skill_corpus();
    for name in subcommands() {
        if let Some((_, why)) = EXEMPT.iter().find(|(exempt, _)| *exempt == name) {
            assert!(
                !names_command(&corpus, &name),
                "`lore {name}` is exempt ({why}) but a skill names it anyway — \
                 drop the exemption rather than keeping a stale one"
            );
            continue;
        }
        assert!(
            names_command(&corpus, &name),
            "no skill names `lore {name}`; a command no skill reaches is one no agent runs. \
             Document it (the `/lore-ingest` command table is the operator reference) or \
             add it to EXEMPT with the reason it is unreachable by design."
        );
    }
}

#[test]
fn a_command_name_is_matched_whole_not_as_a_prefix() {
    assert!(names_command("run `lore doctor` to audit", "doctor"));
    assert!(
        !names_command("run `lore doctor` to audit", "doc"),
        "`lore doc` must not be covered by `lore doctor`"
    );
    assert!(names_command("`lore queue prune`", "queue"));
    assert!(!names_command("`lore maintenance`", "maintain"));
}

/// And the reverse: every command a skill instructs the agent to RUN must exist.
///
/// The other direction is checked above — a command no skill names is one no agent reaches. This
/// one is the failure the agent actually experiences: a skill step naming a command the binary
/// does not accept stops the procedure mid-run, with an agent that has already written pages.
/// A stale navigation invocation left in one skill must fail this contract even when
/// other skills already use the current SQLite refresh procedure.
///
/// Validate the binary's complete argument tree, not prefixes or rendered help.
/// Help permits documentation placeholders without dispatching commands.
///
/// Only backticked invocations count; natural-language skill descriptions are not commands.
#[test]
fn every_command_a_skill_tells_an_agent_to_run_exists() {
    let corpus = skill_corpus();
    let paths = backticked_command_paths(&corpus);
    assert!(
        paths.len() > 5,
        "found only {} invocations — did the fences change?",
        paths.len()
    );
    for path in paths {
        let parsed = parse_invocation(&path);
        assert!(
            parsed.is_ok(),
            "a skill tells an agent to run `lore {}`, which the binary rejects: {}",
            path.join(" "),
            parsed.unwrap_err()
        );
    }
}

/// The subcommand path of every `` `lore …` `` span in the corpus, and of every `lore …` line
/// inside a fenced block — a skill gives commands both ways, and both are instructions.
///
/// Flags, placeholders and arguments are dropped: what is being asked is whether the command PATH
/// exists, and `--help` answers that without the arguments having to be valid.
fn backticked_command_paths(corpus: &str) -> Vec<Vec<String>> {
    fn path_of(rest: &str) -> Option<Vec<String>> {
        let path: Vec<String> = rest
            .split_whitespace()
            .take_while(|token| {
                token
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                    && !token.starts_with('-')
            })
            .map(str::to_owned)
            .collect();
        (!path.is_empty()).then_some(path)
    }

    let mut found: Vec<Vec<String>> = Vec::new();
    for span in corpus.split('`').skip(1).step_by(2) {
        if let Some(rest) = span.strip_prefix("lore ")
            && let Some(path) = path_of(rest)
        {
            found.push(path);
        }
    }
    let mut in_fence = false;
    for line in corpus.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence
            && let Some(rest) = line.trim_start().strip_prefix("lore ")
            && let Some(path) = path_of(rest)
        {
            found.push(path);
        }
    }
    found.sort();
    found.dedup();
    found
}

/// Every `lore` line inside a FENCED BLOCK parses whole — flags included.
///
/// The check above strips flags deliberately: the command reference documents optional arguments
/// as `lore health [--strict]`, and `[--strict]` is not a flag. A fenced block is different in
/// kind — everything in one is code, verbatim, which is why the drain protocol's steps are
/// written that way — so its lines go to clap as written.
///
/// `--help` short-circuits clap AFTER argument parsing, so an unknown flag is rejected (exit 2,
/// `unexpected argument '--dryrun' found`) while a valid one prints help. Nothing executes.
///
/// Split a documented invocation the way a shell would, honouring double quotes.
///
/// A naive whitespace split would force every skill to document arguments no person would type:
/// `--note` takes a sentence and `--summary` takes a phrase, so an example without quotes is not
/// the command the skill is telling an agent to run — and a check that only passes for
/// unrealistic examples verifies nothing. A `#` inside quotes survives too, which is how a mail
/// or a Slack permalink is written.
/// Drop a trailing `#` comment, but not a `#` inside quotes — a Gmail permalink carries one.
fn strip_trailing_comment(line: &str) -> &str {
    let mut quoted = false;
    for (index, ch) in line.char_indices() {
        match ch {
            '"' => quoted = !quoted,
            '#' if !quoted => return line[..index].trim_end(),
            _ => {}
        }
    }
    line
}

fn shell_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut started = false;
    for ch in line.chars() {
        match ch {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            c => {
                word.push(c);
                started = true;
            }
        }
    }
    if started {
        words.push(word);
    }
    words
}

// After `--`, an appended help flag is command data. Parsing must never dispatch it.
fn parse_invocation(args: &[String]) -> Result<(), clap::Error> {
    let args = std::iter::once("lore")
        .chain(args.iter().map(String::as_str))
        .chain(["--help"]);
    match lore::Cli::command().try_get_matches_from(args) {
        Ok(_) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

#[test]
fn literal_workflow_arguments_are_parsed_without_configuration_or_execution() {
    let root = tempfile::tempdir().unwrap();
    let missing = root.path().join("missing-config.yaml");
    let args = [
        "--config".into(),
        missing.to_string_lossy().into_owned(),
        "workflow".into(),
        "--".into(),
        "an-external-command-that-must-not-run".into(),
    ];
    assert!(parse_invocation(&args).is_ok());
    assert!(!missing.exists());
    assert!(
        parse_invocation(&shell_words(
            "work refine WORK NODE --max-part-characters 20000 --dry-run"
        ))
        .is_ok()
    );
    assert!(
        parse_invocation(&shell_words(
            "work refine WORK NODE --unknown-refinement-flag"
        ))
        .is_err()
    );
}

/// The `[--flag]` table rows stay unchecked. Reaching them needs a table of which documentation
/// notations to strip, and a check that cannot tell notation from a command fails the build on
/// prose — which is the failure mode this whole file exists to avoid.
#[test]
fn every_fenced_lore_invocation_parses_including_its_flags() {
    let mut checked = 0;
    for path in skill_markdown() {
        let body = std::fs::read_to_string(&path).expect("read skill");
        let mut fenced = false;
        for line in body.lines() {
            if line.trim_start().starts_with("```") {
                fenced = !fenced;
                continue;
            }
            if !fenced {
                continue;
            }
            let command = strip_trailing_comment(line.trim());
            let Some(args) = command.strip_prefix("lore ") else {
                continue;
            };
            checked += 1;
            let parsed = parse_invocation(&shell_words(args));
            assert!(
                parsed.is_ok(),
                "{} runs `{command}` in a fenced block, which the binary rejects: {}",
                path.display(),
                parsed.unwrap_err()
            );
        }
    }
    assert!(
        checked > 5,
        "found only {checked} fenced invocations — did the fences change?"
    );
}
