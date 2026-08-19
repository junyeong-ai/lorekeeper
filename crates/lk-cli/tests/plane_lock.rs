//! A plane that cannot be held refuses every WRITE and no READ.
//!
//! The rule is one line at the call site and the failure it prevents is silent: writing an
//! unheld plane lost board lines and transition records while reporting success. Nothing
//! measured that, which is how it shipped, and nothing measured the fix either — so a read
//! quietly joining the refusal would ship the same way.

use std::path::Path;
use std::process::{Command, Output};

fn run(config: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_lore"));
    for (key, _) in std::env::vars() {
        if key.starts_with("LORE_") {
            cmd.env_remove(key);
        }
    }
    cmd.args(args)
        .arg("--config")
        .arg(config)
        .output()
        .expect("spawn lore")
}

/// What a command SAID, as one line. `miette` wraps to the terminal's width, so a phrase read
/// back out of the rendering breaks on a path long enough to push it over — which is a test that
/// passes or fails on where the temp directory happened to be.
fn said(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace("│ ", "")
}

fn vault(root: &Path) -> std::path::PathBuf {
    let config = root.join("config.yaml");
    std::fs::write(
        &config,
        format!(
            "vault:\n  root: {}/vault\n  timezone: UTC\nidentity:\n  name: t\n  email: t@t.com\n\
             sources:\n  s1:\n    type: tasks\npersonal:\n  tracked_sources: [s1]\n  \
             performance_categories: [pd]\n  tasks:\n    board: tasks.md\n",
            root.display()
        ),
    )
    .expect("write config");
    std::fs::create_dir_all(root.join("vault/personal")).expect("create vault");
    config
}

/// A DIRECTORY where the lock file goes: the plane can never be held, and it is the one cause
/// reproducible without a filesystem that lacks locking.
fn wedge(root: &Path) {
    let lock = root.join("vault/.lorekeeper/tasks.lock");
    std::fs::create_dir_all(lock.parent().expect("parent")).expect("create dir");
    let _ = std::fs::remove_file(&lock);
    std::fs::create_dir(&lock).expect("wedge the lock");
}

#[test]
fn a_plane_that_cannot_be_held_refuses_writes_and_serves_reads() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = vault(tmp.path());
    assert!(
        run(&config, &["task", "add", "a task"]).status.success(),
        "the vault works before it is wedged"
    );
    wedge(tmp.path());

    for args in [
        vec!["task", "add", "another"],
        vec!["task", "sync"],
        vec!["task", "rollover"],
        vec!["task", "propose"],
        vec!["task", "remind", "due"],
    ] {
        let out = run(&config, &args);
        assert!(!out.status.success(), "{args:?} must refuse");
        let told = said(&out);
        assert!(
            told.contains("Clear whatever sits at that path"),
            "{args:?} must name the repair its cause calls for: {told}"
        );
    }

    for args in [
        vec!["task", "list"],
        vec!["task", "remind", "list"],
        vec!["agenda"],
        vec!["agenda", "--json"],
        vec!["status"],
    ] {
        let out = run(&config, &args);
        assert!(
            out.status.success(),
            "{args:?} reads and must not be refused: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// A view that could not be written to says so, rather than reporting a quiet day over a vault
/// where the nightly close, every command and the reminder timer are all being turned away.
#[test]
fn a_view_says_the_plane_cannot_be_held() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = vault(tmp.path());
    run(&config, &["task", "add", "a task"]);
    wedge(tmp.path());

    let json = run(&config, &["agenda", "--json"]);
    let day: serde_json::Value =
        serde_json::from_slice(&json.stdout).expect("the contract is still JSON");
    assert!(
        day["unwritable"]
            .as_str()
            .is_some_and(|why| why.contains("Clear whatever sits at that path")),
        "unwritable: {}",
        day["unwritable"]
    );

    assert!(
        said(&run(&config, &["status"])).contains("! board"),
        "the front door marks it"
    );
}
