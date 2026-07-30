//! Pins that every `lore` subcommand is named by a skill.
//!
//! The skills are the operator's interface to this binary: `/lore-ingest` carries the
//! command reference, and the others cite the commands their procedure runs. A command
//! that exists but appears in none of them is one no agent following a skill will ever
//! reach — `lore doctor` shipped that way and stayed invisible through three releases.
//!
//! The command list comes from the binary's own `--help`, so it cannot drift from what
//! clap actually accepts. Exemptions are explicit and few: a command earns one only by
//! being unreachable from a skill BY DESIGN, and adding a command forces the choice here.

use std::path::PathBuf;
use std::process::Command;

/// Commands deliberately outside every skill's scope.
const EXEMPT: &[(&str, &str)] = &[("help", "clap's own; not a pipeline operation")];

/// Subcommand names as clap prints them under `Commands:` in the top-level help.
///
/// Colour is forced off on the child: anstream honours an ambient `CLICOLOR_FORCE`, which
/// would wrap the `Commands:` header in SGR escapes and leave this parsing nothing — a
/// failure blaming a help-layout change for the developer's terminal settings.
fn subcommands() -> Vec<String> {
    let out = Command::new(env!("CARGO_BIN_EXE_lore"))
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .env_remove("FORCE_COLOR")
        .arg("--help")
        .output()
        .expect("run lore --help");
    assert!(out.status.success(), "lore --help failed: {out:?}");
    let help = String::from_utf8(out.stdout).expect("help is utf-8");

    // A command line is indented exactly two spaces. Requiring that rather than trusting
    // the first token means a wrapped description — which clap emits at a deeper indent
    // once `wrap_help` is on — is never mistaken for a command name.
    let names: Vec<String> = help
        .lines()
        .skip_while(|line| !line.starts_with("Commands:"))
        .skip(1)
        .take_while(|line| !line.trim().is_empty())
        .filter(|line| line.starts_with("  ") && !line.starts_with("   "))
        .filter_map(|line| line.split_whitespace().next().map(str::to_string))
        .collect();
    assert!(
        names.len() > 5,
        "parsed {names:?} from the Commands: block — the help layout changed"
    );
    names
}

/// Every markdown file the skills ship, by path: the corpus concatenates them, which loses
/// which file a line came from.
fn skill_markdown() -> Vec<PathBuf> {
    let skills_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.claude/skills");
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
    let skills_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.claude/skills");
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
/// `lore wiki index`/`log`/`map` were replaced by `lore wiki refresh` across four skills in one
/// edit; nothing would have caught a fifth mention left behind.
///
/// Each invocation is put to the BINARY rather than compared against parsed help text. A first
/// version compared only the leading subcommand, so `lore wiki rebuild` passed on the strength of
/// `wiki` being real — which is the drift least likely to happen and the only one it could see.
/// `--help` short-circuits clap before argument validation, so a documented invocation's
/// placeholders and flags do not have to be satisfiable to ask whether the command path exists.
///
/// Only backticked invocations count. `when_to_use` carries natural-language triggers — "lore
/// capture", "lore extract" — which are the skill's own name in prose, not commands.
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
        let out = Command::new(env!("CARGO_BIN_EXE_lore"))
            .env("NO_COLOR", "1")
            .args(&path)
            .arg("--help")
            .output()
            .expect("run lore");
        assert!(
            out.status.success(),
            "a skill tells an agent to run `lore {}`, which the binary rejects: {}",
            path.join(" "),
            String::from_utf8_lossy(&out.stderr)
                .lines()
                .next()
                .unwrap_or("")
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
            let command = line.trim().split('#').next().unwrap_or("").trim();
            let Some(args) = command.strip_prefix("lore ") else {
                continue;
            };
            checked += 1;
            let out = Command::new(env!("CARGO_BIN_EXE_lore"))
                .env("NO_COLOR", "1")
                .args(args.split_whitespace())
                .arg("--help")
                .output()
                .expect("run lore");
            assert!(
                out.status.success(),
                "{} runs `{command}` in a fenced block, which the binary rejects: {}",
                path.display(),
                String::from_utf8_lossy(&out.stderr)
                    .lines()
                    .next()
                    .unwrap_or("")
            );
        }
    }
    assert!(
        checked > 5,
        "found only {checked} fenced invocations — did the fences change?"
    );
}
