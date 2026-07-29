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

    let names: Vec<String> = help
        .lines()
        .skip_while(|line| !line.starts_with("Commands:"))
        .skip(1)
        .take_while(|line| !line.trim().is_empty())
        .filter_map(|line| line.split_whitespace().next().map(str::to_string))
        .collect();
    assert!(
        names.len() > 5,
        "parsed {names:?} from the Commands: block — the help layout changed"
    );
    names
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

#[test]
fn every_subcommand_is_named_by_a_skill() {
    let corpus = skill_corpus();
    for name in subcommands() {
        if let Some((_, why)) = EXEMPT.iter().find(|(exempt, _)| *exempt == name) {
            assert!(
                !corpus.contains(&format!("lore {name}")),
                "`lore {name}` is exempt ({why}) but a skill names it anyway — \
                 drop the exemption rather than keeping a stale one"
            );
            continue;
        }
        assert!(
            corpus.contains(&format!("lore {name}")),
            "no skill names `lore {name}`; a command no skill reaches is one no agent runs. \
             Document it (the `/lore-ingest` command table is the operator reference) or \
             add it to EXEMPT with the reason it is unreachable by design."
        );
    }
}
