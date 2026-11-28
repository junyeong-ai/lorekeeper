//! A private home for every `lore` process a test spawns.
//!
//! `lore` resolves user skill roots (`~/.claude`, `~/.agents`), installation claims, its data
//! and configuration directories and configuration discovery from `HOME` and the XDG roots.
//! A test that inherited the developer's environment would read, claim and write that real
//! home, and its installation claims would block an installer running there. Each test
//! binary includes this module and uses the part it needs.
#![allow(dead_code)]
#![allow(clippy::disallowed_methods)]

use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

/// Variables that would select a real installation, configuration or host home instead.
const INHERITED: [&str; 6] = [
    "USERPROFILE",
    "LOCALAPPDATA",
    "LORE_CONFIG",
    "LORE_INSTALL_DATA_DIR",
    "CLAUDE_CONFIG_DIR",
    "CODEX_HOME",
];

/// This test process's home, created once in Cargo's integration-test scratch directory.
pub fn path() -> &'static Path {
    static HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    HOME.get_or_init(|| {
        tempfile::Builder::new()
            .prefix("home-")
            .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .expect("create the isolated test home")
    })
    .path()
}

/// Points `command`, and every process it starts, at this process's private home: `HOME` and
/// each XDG root, with the variables in `INHERITED` removed. A test that needs its own roots
/// sets them on the command afterwards.
pub fn isolate(command: &mut Command) -> &mut Command {
    let home = path();
    for variable in INHERITED {
        command.env_remove(variable);
    }
    command
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
}

/// The `lore` binary under test, isolated by `isolate`.
pub fn lore_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lore"));
    isolate(&mut command);
    command
}
