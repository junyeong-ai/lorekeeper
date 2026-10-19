//! Explicit config selection must not turn a root override into a different registry.

use std::path::Path;
use std::process::{Command, Output};

fn resolve(workspace: &Path, selected: Option<&Path>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lore"));
    command
        .current_dir(workspace)
        .env_remove("LORE_CONFIG")
        .env("HOME", workspace.join("home"))
        .env("XDG_CONFIG_HOME", workspace.join("xdg"));
    if let Some(path) = selected {
        command.arg("--config").arg(path);
    }
    command
        .args(["resolve", "--json", "--root"])
        .arg(workspace.join("vault"))
        .arg("Selected Subject")
        .output()
        .unwrap()
}

#[test]
fn root_override_defaults_only_when_no_config_was_selected_or_discovered() {
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path().join("vault");
    std::fs::create_dir_all(vault.join("knowledge/concepts")).unwrap();
    std::fs::write(
        vault.join("knowledge/concepts/subject.md"),
        "---\ntype: concept\ntitle: Selected Subject\n---\n# Selected Subject\n",
    )
    .unwrap();

    let absent = resolve(dir.path(), None);
    assert_eq!(absent.status.code(), Some(1));
    let answer: serde_json::Value = serde_json::from_slice(&absent.stdout).unwrap();
    assert_eq!(answer["proposed_path"], "wiki/concepts/selected-subject.md");

    let missing = resolve(dir.path(), Some(&dir.path().join("missing.yaml")));
    assert_eq!(missing.status.code(), Some(3));
    assert!(missing.stdout.is_empty());

    let config = dir.path().join("config.yaml");
    std::fs::write(&config, "vault:\n  root: vault\n  dirs:\n    wiki: knowledge\nidentity:\n  name: Test\n  email: test@example.com\nsources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: inbox\n").unwrap();
    let owned = resolve(dir.path(), None);
    assert!(
        owned.status.success(),
        "{}",
        String::from_utf8_lossy(&owned.stderr)
    );
    let answer: serde_json::Value = serde_json::from_slice(&owned.stdout).unwrap();
    assert_eq!(answer["path"], "knowledge/concepts/subject.md");

    std::fs::write(&config, "invalid: [yaml").unwrap();
    let invalid = resolve(dir.path(), None);
    assert_eq!(invalid.status.code(), Some(3));
    assert!(invalid.stdout.is_empty());
}

#[cfg(unix)]
#[test]
fn dangling_discovered_config_is_an_error_instead_of_absence() {
    let dir = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink("missing.yaml", dir.path().join("config.yaml")).unwrap();
    let output = resolve(dir.path(), None);
    assert_eq!(output.status.code(), Some(3));
    assert!(output.stdout.is_empty());
}
