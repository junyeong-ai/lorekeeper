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

fn aliased_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let (directory, config) = fixture();
    let alias = directory.path().join("selected");
    std::os::unix::fs::symlink(directory.path().join("vault"), &alias).unwrap();
    let raw = std::fs::read_to_string(&config)
        .unwrap()
        .replace("root: vault", "root: selected");
    std::fs::write(&config, raw).unwrap();
    (directory, config, alias)
}

fn retarget(alias: &Path, target: &Path) {
    std::fs::remove_file(alias).unwrap();
    std::os::unix::fs::symlink(target, alias).unwrap();
}

fn workflow(config: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lore"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("LORE_") {
            command.env_remove(key);
        }
    }
    command.env(
        "LORE_INSTALL_DATA_DIR",
        config.parent().unwrap().join("data"),
    );
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
fn inherited_root_context_does_not_grant_workflow_reentrancy() {
    let (directory, config) = fixture();
    let marker = directory.path().join("must-not-start");
    let output = workflow(&config)
        .arg(env!("CARGO_BIN_EXE_lore"))
        .args(["workflow", "--", "/usr/bin/touch"])
        .arg(&marker)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("workflow busy"));
    assert!(!marker.exists());
    assert!(
        workflow(&config)
            .arg("/usr/bin/true")
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn a_workflow_pins_the_selected_configuration_revision() {
    let (directory, config) = fixture();
    let output = workflow(&config)
        .args(["/bin/sh", "-c", "printf '\\n# edited during the session\\n' >> \"$LORE_CONFIG\"; \"$LORE_BIN\" config vault-root"])
        .output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("configuration changed"));
    assert!(
        workflow(&config)
            .arg("/usr/bin/true")
            .status()
            .unwrap()
            .success()
    );
    let other = directory.path().join("other");
    std::fs::create_dir_all(other.join("vault")).unwrap();
    std::fs::copy(&config, other.join("config.yaml")).unwrap();
    let redirected = workflow(&config)
        .arg(env!("CARGO_BIN_EXE_lore"))
        .arg("--config")
        .arg(other.join("config.yaml"))
        .arg("schema")
        .output()
        .unwrap();
    assert!(!redirected.status.success());
    assert!(String::from_utf8_lossy(&redirected.stderr).contains("configuration changed"));
    assert!(!other.join("vault/wiki/AGENTS.md").exists());
}

#[test]
fn retargeted_config_root_cannot_publish_into_another_live_workflow() {
    let (first, config, alias) = aliased_fixture();
    let before = std::fs::read(&config).unwrap();
    let (mut owner, mut output) = held(
        &config,
        "printf 'ready\\n'; read -r release; \"$LORE_BIN\" schema 2>&1",
    );
    let (second, second_config) = fixture();
    retarget(&alias, &second.path().join("vault"));
    let (mut other_owner, _) = held(&second_config, "printf 'ready\\n'; read -r release");
    assert!(
        !workflow(&second_config)
            .arg("/usr/bin/true")
            .output()
            .unwrap()
            .status
            .success()
    );
    owner
        .0
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"release\n")
        .unwrap();
    let mut error = String::new();
    output.read_to_string(&mut error).unwrap();
    assert!(!owner.0.wait().unwrap().success());
    assert!(
        error.contains("workflow vault selection changed"),
        "{error}"
    );
    assert_eq!(std::fs::read(&config).unwrap(), before);
    for directory in [first.path(), second.path()] {
        assert!(!directory.join("vault/wiki/AGENTS.md").exists());
    }
    other_owner
        .0
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"release\n")
        .unwrap();
    assert!(other_owner.0.wait().unwrap().success());
}

#[test]
fn same_directory_alias_changes_and_root_overrides_remain_valid() {
    let (directory, config, alias) = aliased_fixture();
    let root = directory.path().join("vault");
    let equivalent = directory.path().join("equivalent");
    std::os::unix::fs::symlink(&root, &equivalent).unwrap();
    let (mut owner, _) = held(
        &config,
        "printf 'ready\\n'; read -r release; \"$LORE_BIN\" schema",
    );
    retarget(&alias, &equivalent);
    owner
        .0
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"release\n")
        .unwrap();
    assert!(owner.0.wait().unwrap().success());
    assert!(root.join("wiki/AGENTS.md").exists());
    let same = workflow(&config)
        .arg(env!("CARGO_BIN_EXE_lore"))
        .args(["schema", "--root"])
        .arg(&equivalent)
        .output()
        .unwrap();
    assert!(same.status.success(), "{same:?}");
    let (other, _) = fixture();
    let foreign = workflow(&config)
        .arg(env!("CARGO_BIN_EXE_lore"))
        .args(["schema", "--root"])
        .arg(other.path().join("vault"))
        .output()
        .unwrap();
    assert!(!foreign.status.success());
    assert!(String::from_utf8_lossy(&foreign.stderr).contains("workflow vault selection changed"));
    assert!(!other.path().join("vault/wiki/AGENTS.md").exists());
}

#[test]
fn replacing_the_vault_directory_at_the_same_path_refuses_child_publication() {
    let (directory, config) = fixture();
    let root = directory.path().join("vault");
    let (mut owner, mut output) = held(
        &config,
        "printf 'ready\\n'; read -r release; \"$LORE_BIN\" schema 2>&1",
    );
    std::fs::rename(&root, directory.path().join("old-vault")).unwrap();
    std::fs::create_dir(&root).unwrap();
    owner
        .0
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"release\n")
        .unwrap();
    let mut error = String::new();
    output.read_to_string(&mut error).unwrap();
    assert!(!owner.0.wait().unwrap().success());
    assert!(
        error.contains("workflow vault selection changed"),
        "{error}"
    );
    assert!(!root.join("wiki/AGENTS.md").exists());
}

#[test]
fn partial_or_malformed_root_context_is_not_accepted() {
    let (directory, config) = fixture();
    let root = directory.path().join("vault");
    for identity in [
        None,
        Some("not-json"),
        Some("{\"device\":0,\"inode\":0,\"unknown\":1}"),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_lore"));
        command
            .arg("--config")
            .arg(&config)
            .args(["config", "vault-root"])
            .env("LORE_VAULT_ROOT", &root)
            .env("LORE_VAULT_ROOT_SELECTOR", &root)
            .env_remove("LORE_VAULT_ROOT_IDENTITY");
        if let Some(identity) = identity {
            command.env("LORE_VAULT_ROOT_IDENTITY", identity);
        }
        let output = command.output().unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("workflow vault"), "{error}");
    }
    let output = Command::new(env!("CARGO_BIN_EXE_lore"))
        .arg("--config")
        .arg(&config)
        .args(["config", "vault-root"])
        .env("LORE_CONFIG", config.canonicalize().unwrap())
        .env(
            "LORE_CONFIG_REVISION",
            lk_core::concept::file_revision(&std::fs::read_to_string(&config).unwrap()),
        )
        .env("LORE_WORKFLOW_ID", "0".repeat(32))
        .env_remove("LORE_VAULT_ROOT")
        .env_remove("LORE_VAULT_ROOT_SELECTOR")
        .env_remove("LORE_VAULT_ROOT_IDENTITY")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("workflow vault context is missing"));
}

#[cfg(target_os = "linux")]
#[test]
fn a_workflow_preserves_non_unicode_physical_roots_and_bare_path_bytes() {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    let (directory, config, alias) = aliased_fixture();
    let root = directory
        .path()
        .join(std::ffi::OsString::from_vec(b"vault-\xff".to_vec()));
    std::fs::rename(directory.path().join("vault"), &root).unwrap();
    retarget(&alias, &root);
    let output = workflow(&config)
        .args([
            "/bin/sh",
            "-c",
            "\"$LORE_BIN\" schema --root \"$LORE_VAULT_ROOT\" && \"$LORE_BIN\" config schema-path --root \"$LORE_VAULT_ROOT\" && \"$LORE_BIN\" config vault-root",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let root = root.canonicalize().unwrap();
    let mut expected = root.join("wiki/AGENTS.md").as_os_str().as_bytes().to_vec();
    expected.push(b'\n');
    expected.extend_from_slice(root.as_os_str().as_bytes());
    expected.push(b'\n');
    assert_eq!(output.stdout, expected);
    assert!(root.join("wiki/AGENTS.md").exists());
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
fn child_uses_selected_executable_ahead_of_another_installation() {
    use std::os::unix::fs::PermissionsExt;

    let (dir, config) = fixture();
    let other = dir.path().join("other-bin");
    std::fs::create_dir(&other).unwrap();
    std::fs::write(other.join("lore"), "#!/bin/sh\nprintf wrong-installation\n").unwrap();
    std::fs::set_permissions(other.join("lore"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = workflow(&config)
        .env("PATH", &other)
        .args([
            "/bin/sh",
            "-c",
            "command -v lore; printf '%s\\n' \"$LORE_BIN\"; lore --version",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    let executable = Path::new(env!("CARGO_BIN_EXE_lore"))
        .canonicalize()
        .unwrap();
    let text = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<_> = text.lines().collect();
    assert!(same_file::is_same_file(Path::new(lines[0]), &executable).unwrap());
    assert!(same_file::is_same_file(Path::new(lines[1]), &executable).unwrap());
    assert_eq!(lines[2], format!("lore {}", env!("CARGO_PKG_VERSION")));
}

#[test]
fn renamed_executable_cannot_silently_delegate_to_another_lore() {
    let (dir, config) = fixture();
    let renamed = dir.path().join("selected-custom-name");
    std::fs::copy(env!("CARGO_BIN_EXE_lore"), &renamed).unwrap();
    let output = Command::new(&renamed)
        .arg("--config")
        .arg(&config)
        .args(["workflow", "--", "/bin/sh", "-c", "printf must-not-start"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("canonical name"));
    assert!(!dir.path().join("vault/.lorekeeper/workflow.lock").exists());
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

#[test]
fn a_waiting_workflow_refuses_changed_configuration_before_starting_its_child() {
    let (directory, config) = fixture();
    let (mut holder, _) = held(&config, "printf 'ready\\n'; read -r release");
    let marker = directory.path().join("must-not-start");
    let error = directory.path().join("wait-error");
    let mut waiter = Command::new(env!("CARGO_BIN_EXE_lore"))
        .arg("--config")
        .arg(&config)
        .env("LORE_INSTALL_DATA_DIR", directory.path().join("data"))
        .args(["workflow", "--wait", "--", "/usr/bin/touch"])
        .arg(&marker)
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&error).unwrap())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !std::fs::read_to_string(&error)
        .unwrap()
        .contains("Waiting for the vault workflow lease")
    {
        assert!(std::time::Instant::now() < deadline);
        assert!(waiter.try_wait().unwrap().is_none());
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let raw = std::fs::read_to_string(&config)
        .unwrap()
        .replace("name: Test", "name: Changed");
    std::fs::write(&config, raw).unwrap();
    let status = loop {
        if let Some(status) = waiter.try_wait().unwrap() {
            break status;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    assert!(!status.success());
    assert!(
        std::fs::read_to_string(error)
            .unwrap()
            .contains("configuration changed")
    );
    assert!(!marker.exists());
    holder
        .0
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"release\n")
        .unwrap();
    assert!(holder.0.wait().unwrap().success());
}

#[test]
fn a_waiting_workflow_refuses_retargeted_root_before_starting_its_child() {
    let (directory, config, alias) = aliased_fixture();
    let (_holder, _) = held(&config, "printf 'ready\\n'; read -r release");
    let marker = directory.path().join("must-not-start");
    let error = directory.path().join("root-wait-error");
    let mut command = Command::new(env!("CARGO_BIN_EXE_lore"));
    command
        .arg("--config")
        .arg(&config)
        .env("LORE_INSTALL_DATA_DIR", directory.path().join("data"))
        .args(["workflow", "--wait", "--", "/usr/bin/touch"])
        .arg(&marker)
        .stderr(std::fs::File::create(&error).unwrap());
    let mut waiter = Held(command.spawn().unwrap());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !std::fs::read_to_string(&error)
        .unwrap()
        .contains("Waiting for the vault workflow lease")
    {
        assert!(std::time::Instant::now() < deadline);
        assert!(waiter.0.try_wait().unwrap().is_none());
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let (other, _) = fixture();
    retarget(&alias, &other.path().join("vault"));
    let status = loop {
        if let Some(status) = waiter.0.try_wait().unwrap() {
            break status;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    assert!(!status.success());
    assert!(!marker.exists());
    assert!(
        std::fs::read_to_string(error)
            .unwrap()
            .contains("workflow vault selection changed")
    );
}
