#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("vault")).unwrap();
    let config = dir.path().join("config.yaml");
    std::fs::write(
        &config,
        "vault:\n  root: vault\nidentity:\n  name: Test\n  email: test@example.com\nsources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: inbox\n",
    )
    .unwrap();
    (dir, config)
}

fn workflow(config: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lore"));
    command.arg("--config").arg(config).args(["workflow", "--"]);
    command
}

// Drop closes stdin too, so a test failure releases a shell child waiting on its input.
struct Held(Child);
impl Drop for Held {
    fn drop(&mut self) {
        self.0.stdin.take();
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn held(config: &Path, script: &str) -> (Held, BufReader<std::process::ChildStdout>) {
    let mut child = Held(
        workflow(config)
            .args(["/bin/bash", "-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut output = BufReader::new(child.0.stdout.take().unwrap());
    let mut ready = String::new();
    output.read_line(&mut ready).unwrap();
    assert_eq!(ready, "ready\n");
    (child, output)
}

#[test]
fn contention_runs_no_child_and_release_allows_short_writer() {
    let (dir, config) = fixture();
    let (mut holder, _) = held(&config, "printf 'ready\\n'; read -r release");
    let marker = dir.path().join("must-not-exist");
    let refused = workflow(&config)
        .args(["/usr/bin/touch"])
        .arg(&marker)
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(!marker.exists());
    let (_other_dir, other_config) = fixture();
    assert!(
        workflow(&other_config)
            .arg("/usr/bin/true")
            .status()
            .unwrap()
            .success()
    );
    holder
        .0
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"release\n")
        .unwrap();
    assert!(holder.0.wait().unwrap().success());
    // This child takes the ordinary vault mutation lease. Workflow must not hold it.
    assert!(
        workflow(&config)
            .arg(env!("CARGO_BIN_EXE_lore"))
            .arg("schema")
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(dir.path().join("vault/wiki/AGENTS.md").exists());
}

#[test]
fn literal_arguments_exit_status_and_failed_exec_release() {
    let (_dir, config) = fixture();
    let output = workflow(&config)
        .args([
            "/bin/sh",
            "-c",
            "printf '%s|%s' \"$1\" \"$2\"; exit 23",
            "fixture",
            "a b",
            "--literal",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(23));
    assert_eq!(output.stdout, b"a b|--literal");
    assert!(
        !workflow(&config)
            .arg("/definitely/missing/program")
            .status()
            .unwrap()
            .success()
    );
    assert!(
        workflow(&config)
            .arg("/usr/bin/true")
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn exec_shell_child_keeps_lease_after_launcher_is_killed() {
    let (_dir, config) = fixture();
    // A foreground shell child inherits stdin; the trailing command keeps its parent
    // from replacing itself with the child as a last-command optimization.
    let (mut launcher, mut output) = held(
        &config,
        "/bin/bash -c 'printf \"ready\\n\"; read -r release'; :",
    );
    let mut input = launcher.0.stdin.take().unwrap();
    launcher.0.kill().unwrap();
    launcher.0.wait().unwrap();
    assert!(
        !workflow(&config)
            .arg("/usr/bin/true")
            .status()
            .unwrap()
            .success()
    );
    input.write_all(b"release\n").unwrap();
    drop(input);
    let mut rest = String::new();
    output.read_to_string(&mut rest).unwrap();
    assert!(
        workflow(&config)
            .arg("/usr/bin/true")
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn child_gets_selected_canonical_config_and_template_without_cli_flags() {
    let (dir, config) = fixture();
    let template = dir.path().join("templates");
    std::fs::create_dir(&template).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_lore"))
        .current_dir(dir.path())
        .arg("--config")
        .arg("config.yaml")
        .args([
            "--template-dir",
            "templates",
            "workflow",
            "--",
            "/bin/sh",
            "-c",
            "printf '%s\\n%s\\n' \"$LORE_CONFIG\" \"$LORE_TEMPLATE_DIR\"",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!(
            "{}\n{}\n",
            config.canonicalize().unwrap().display(),
            template.canonicalize().unwrap().display()
        )
    );
}

#[test]
fn explicit_wait_runs_after_the_foreground_holder_releases() {
    let (_dir, config) = fixture();
    let (mut holder, _) = held(&config, "printf 'ready\\n'; read -r release");
    let mut waiting = Held(
        Command::new(env!("CARGO_BIN_EXE_lore"))
            .arg("--config")
            .arg(&config)
            .args(["workflow", "--wait", "--", "/usr/bin/true"])
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut stderr = BufReader::new(waiting.0.stderr.take().unwrap());
    let mut line = String::new();
    stderr.read_line(&mut line).unwrap();
    assert!(line.contains("Waiting for"));
    holder
        .0
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"release\n")
        .unwrap();
    assert!(holder.0.wait().unwrap().success());
    assert!(waiting.0.wait().unwrap().success());
}
