//! `lore validate` reports what disk inspection found, and the report IS the product — the
//! command exits 0 either way, so a body that checked nothing would look exactly like a clean
//! vault. Each decision is unit-tested beside the code that makes it; this pins the wiring from
//! those decisions to the output a user reads.

use std::process::{Command, Output};

fn run(config: &std::path::Path) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_lore"));
    // Hermetic: depend only on the test's --config, never an ambient LORE_* env var.
    for (key, _) in std::env::vars() {
        if key.starts_with("LORE_") {
            cmd.env_remove(key);
        }
    }
    cmd.arg("validate")
        .arg("--config")
        .arg(config)
        .output()
        .expect("spawn lore")
}

fn write_config(root: &std::path::Path, dirs: &str) -> std::path::PathBuf {
    let path = root.join("config.yaml");
    std::fs::write(
        &path,
        format!(
            "vault:\n  root: {}\n  dirs:\n{dirs}identity:\n  name: t\n  email: t@t.com\n\
             sources:\n  s1:\n    type: gmail\n    params:\n      include_queries: [\"label:x\"]\n",
            root.display()
        ),
    )
    .unwrap();
    path
}

#[test]
fn a_vault_dir_that_is_not_a_directory_reaches_the_user_as_a_warning() {
    let tmp = tempfile::tempdir().unwrap();
    // A file where a vault root should be: validation used to pass and the first write failed
    // with a bare `Not a directory (os error 20)` from inside the pipeline.
    std::fs::write(tmp.path().join("wiki"), "not a directory\n").unwrap();
    let config = write_config(tmp.path(), "    wiki: wiki\n");

    let out = run(&config);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "validate reports, it does not fail: {stderr}"
    );
    assert!(
        stderr.contains("vault.dirs.wiki") && stderr.contains("not a"),
        "the finding must reach the user:\n{stderr}"
    );
    assert!(
        stderr.contains("Config valid"),
        "the config itself is valid — only the disk disagrees:\n{stderr}"
    );
}

#[test]
fn a_vault_whose_dirs_are_what_they_say_produces_no_warning() {
    let tmp = tempfile::tempdir().unwrap();
    for dir in ["daily", "me", "synthesis", "wiki"] {
        std::fs::create_dir(tmp.path().join(dir)).unwrap();
    }
    let config = write_config(tmp.path(), "    wiki: wiki\n");
    // `lore schema` owns AGENTS.md; generate it so the only remaining warning is silenced and
    // "no warnings" means the disk checks agreed rather than that they were never reached.
    let mut schema = Command::new(env!("CARGO_BIN_EXE_lore"));
    schema
        .arg("schema")
        .arg("--config")
        .arg(&config)
        .output()
        .expect("spawn lore schema");

    let out = run(&config);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(
        !stderr.contains("warning:"),
        "a vault that matches its config has nothing to report:\n{stderr}"
    );
}
