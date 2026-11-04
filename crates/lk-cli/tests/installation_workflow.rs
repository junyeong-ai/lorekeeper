#![cfg(unix)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

struct Fixture {
    directory: tempfile::TempDir,
    binary: PathBuf,
    home: PathBuf,
    config_home: PathBuf,
    data_home: PathBuf,
    data_dir: PathBuf,
    config: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::Builder::new()
            .prefix("lore-install-한글 space-")
            .tempdir()
            .unwrap();
        let binary = directory.path().join("bin/lore");
        std::fs::create_dir(binary.parent().unwrap()).unwrap();
        std::fs::copy(env!("CARGO_BIN_EXE_lore"), &binary).unwrap();
        let home = directory.path().join("home");
        let config_home = directory.path().join("config-home");
        let data_home = directory.path().join("data-home");
        let data_dir = directory.path().join("runtime");
        for path in [&home, &config_home, &data_home] {
            std::fs::create_dir(path).unwrap();
        }
        let config = Self::write_config(directory.path(), "first");
        Self {
            directory,
            binary,
            home,
            config_home,
            data_home,
            data_dir,
            config,
        }
    }

    fn write_config(root: &Path, name: &str) -> PathBuf {
        std::fs::create_dir(root.join(name)).unwrap();
        let config = root.join(format!("{name}.yaml"));
        std::fs::write(
            &config,
            format!(
                "vault:\n  root: {name}\nidentity:\n  name: Test\n  email: test@example.invalid\nsources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: inbox\n"
            ),
        )
        .unwrap();
        config
    }

    fn another_config(&self) -> PathBuf {
        Self::write_config(self.directory.path(), "second")
    }

    fn command_without_config(&self) -> Command {
        let mut command = Command::new(&self.binary);
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("LORE_") {
                command.env_remove(key);
            }
        }
        command
            .current_dir(self.directory.path())
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("XDG_DATA_HOME", &self.data_home)
            .env("LORE_INSTALL_DATA_DIR", &self.data_dir);
        command
    }

    fn command(&self, config: &Path) -> Command {
        let mut command = self.command_without_config();
        command.arg("--config").arg(config);
        command
    }

    fn workflow(&self, config: &Path) -> Command {
        let mut command = self.command(config);
        command.args(["workflow", "--"]);
        command
    }

    fn snapshot(&self) -> Command {
        let mut command = self.command(&self.config);
        command.args(["self", "snapshot", "--"]);
        command
    }

    fn deploy(&self) -> Output {
        self.command(&self.config)
            .args(["self", "deploy", "--skills", "none"])
            .output()
            .unwrap()
    }
}

// Closing stdin also releases the foreground shell when an assertion unwinds.
struct Held(Child);

impl Held {
    fn release(&mut self) {
        self.0
            .stdin
            .take()
            .unwrap()
            .write_all(b"release\n")
            .unwrap();
        assert!(self.0.wait().unwrap().success());
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        self.0.stdin.take();
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn hold(mut command: Command) -> Held {
    let mut child = Held(
        command
            .args(["/bin/sh", "-c", "printf 'ready\\n'; read -r release"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut output = BufReader::new(child.0.stdout.take().unwrap());
    let mut ready = String::new();
    output.read_line(&mut ready).unwrap();
    assert_eq!(ready, "ready\n");
    child
}

fn revision(path: &Path) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    std::io::copy(&mut std::fs::File::open(path).unwrap(), &mut hasher).unwrap();
    hasher.finalize()
}

fn artifacts(data_dir: &Path) -> BTreeMap<PathBuf, blake3::Hash> {
    let mut files = BTreeMap::new();
    for name in ["skills", "templates"] {
        for entry in walkdir::WalkDir::new(data_dir.join(name)) {
            let entry = entry.unwrap();
            if entry.file_type().is_file() {
                files.insert(
                    entry.path().strip_prefix(data_dir).unwrap().to_path_buf(),
                    revision(entry.path()),
                );
            }
        }
    }
    assert!(!files.is_empty());
    files
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_installation_busy(output: &Output) {
    assert!(!output.status.success());
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(diagnostic.contains("installation"), "{diagnostic}");
    assert!(
        diagnostic.contains("busy")
            || diagnostic.contains("in use")
            || diagnostic.contains("active"),
        "{diagnostic}"
    );
}

#[test]
fn read_only_validation_does_not_provision_optional_installation_directories() {
    let fixture = Fixture::new();
    assert_success(
        &fixture
            .command(&fixture.config)
            .arg("validate")
            .output()
            .unwrap(),
    );
    for base in [&fixture.home, fixture.directory.path()] {
        assert!(!base.join(".claude").exists());
        assert!(!base.join(".agents").exists());
        assert!(!base.join(".codex").exists());
    }
    assert!(!fixture.data_dir.exists());
    assert!(!fixture.directory.path().join("first/.lorekeeper").exists());
}

#[test]
fn an_explicit_skill_deployment_can_create_a_missing_home() {
    let fixture = Fixture::new();
    std::fs::remove_dir(&fixture.home).unwrap();
    assert_success(
        &fixture
            .command(&fixture.config)
            .arg("validate")
            .output()
            .unwrap(),
    );
    assert!(!fixture.home.exists());
    assert_success(
        &fixture
            .command(&fixture.config)
            .args(["self", "deploy", "--skills", "user", "--agent", "codex"])
            .output()
            .unwrap(),
    );
    assert!(
        fixture
            .home
            .join(".agents/skills/lore-process/SKILL.md")
            .is_file()
    );
    assert!(!fixture.home.join(".claude").exists());
}

#[test]
fn settled_queue_does_not_allow_deployment_over_an_active_workflow() {
    let fixture = Fixture::new();
    assert_success(&fixture.deploy());
    let before = artifacts(&fixture.data_dir);
    let binary_before = revision(&fixture.binary);
    let mut holder = hold(fixture.workflow(&fixture.config));

    let queue = fixture
        .command(&fixture.config)
        .args(["queue", "check"])
        .output()
        .unwrap();
    assert_success(&queue);
    let queue: serde_json::Value = serde_json::from_slice(&queue.stdout).unwrap();
    assert_eq!(queue["data"]["current"], 0);
    assert_eq!(queue["ok"], true);
    assert_installation_busy(&fixture.deploy());
    assert_eq!(artifacts(&fixture.data_dir), before);
    assert_eq!(revision(&fixture.binary), binary_before);

    let marker = fixture.directory.path().join("must-not-start");
    let refused = fixture
        .workflow(&fixture.config)
        .arg("/usr/bin/touch")
        .arg(&marker)
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("workflow"));
    assert!(!marker.exists());

    holder.release();
    assert_success(&fixture.deploy());
    assert_eq!(artifacts(&fixture.data_dir), before);
}

#[test]
fn distinct_vault_workflows_share_installation_reads_and_exclude_maintenance() {
    let fixture = Fixture::new();
    assert_success(&fixture.deploy());
    let other_config = fixture.another_config();
    let before = artifacts(&fixture.data_dir);
    let binary_before = revision(&fixture.binary);
    let mut first = hold(fixture.workflow(&fixture.config));
    let mut second = hold(fixture.workflow(&other_config));

    assert_installation_busy(&fixture.deploy());
    let uninstall = fixture
        .command(&fixture.config)
        .args(["self", "uninstall", "--yes"])
        .output()
        .unwrap();
    assert_installation_busy(&uninstall);
    assert_eq!(artifacts(&fixture.data_dir), before);
    assert_eq!(revision(&fixture.binary), binary_before);

    first.release();
    assert_installation_busy(&fixture.deploy());
    second.release();
    assert_success(&fixture.deploy());
}

#[test]
fn shared_runtime_excludes_deployment_from_another_binary_and_config_home() {
    let first = Fixture::new();
    let second = Fixture::new();
    assert_success(&first.deploy());
    let before = artifacts(&first.data_dir);
    let mut holder = hold(first.workflow(&first.config));

    let deploy = second
        .command(&second.config)
        .env("LORE_INSTALL_DATA_DIR", &first.data_dir)
        .args(["self", "deploy", "--skills", "none"])
        .output()
        .unwrap();
    assert_installation_busy(&deploy);
    assert_eq!(artifacts(&first.data_dir), before);
    holder.release();
    assert_success(
        &second
            .command(&second.config)
            .env("LORE_INSTALL_DATA_DIR", &first.data_dir)
            .args(["self", "deploy", "--skills", "none"])
            .output()
            .unwrap(),
    );
}

#[test]
fn force_update_of_a_shared_binary_refuses_before_download_with_other_data_and_config() {
    let fixture = Fixture::new();
    let other_config = fixture.another_config();
    let alternate_home = fixture.directory.path().join("alternate-config-home");
    std::fs::create_dir(&alternate_home).unwrap();
    let alternate_data = fixture.directory.path().join("alternate-runtime");
    let binary_before = revision(&fixture.binary);
    let mut holder = hold(fixture.workflow(&fixture.config));
    let mut command = fixture.command(&other_config);
    command
        .env("XDG_CONFIG_HOME", alternate_home)
        .env("LORE_INSTALL_DATA_DIR", &alternate_data);
    for name in [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        command.env(name, "http://127.0.0.1:9");
    }
    let update = command
        .env("NO_PROXY", "")
        .env("no_proxy", "")
        .args([
            "self",
            "update",
            "--version",
            "999.999.999",
            "--yes",
            "--force",
            "--data-dir",
        ])
        .arg(&alternate_data)
        .output()
        .unwrap();
    assert_installation_busy(&update);
    assert!(!String::from_utf8_lossy(&update.stderr).contains("Downloading"));
    assert_eq!(revision(&fixture.binary), binary_before);
    assert!(!alternate_data.join("skills").exists());
    assert!(!alternate_data.join("templates").exists());
    holder.release();
}

#[test]
fn workflow_only_exclusion_does_not_freeze_ingest_admission_state() {
    let fixture = Fixture::new();
    let config = std::fs::read_to_string(&fixture.config).unwrap();
    std::fs::write(
        &fixture.config,
        format!("{config}\nllm: {{provider: noop}}\n"),
    )
    .unwrap();
    let inbox = fixture.directory.path().join("inbox");
    std::fs::create_dir(&inbox).unwrap();
    std::fs::write(
        inbox.join("note.md"),
        "# Captured note\n\nOriginal source.\n",
    )
    .unwrap();
    let mut holder = hold(fixture.workflow(&fixture.config));
    let cursor = fixture
        .directory
        .path()
        .join("first/.lorekeeper/ingest-cursor.json");
    let before = br#"{"next_source":"earlier-source"}"#;
    std::fs::write(&cursor, before).unwrap();
    let result = fixture
        .command(&fixture.config)
        .args(["ingest", "--publication-workflow", "refuse"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("workflow busy"),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let after = std::fs::read(&cursor).unwrap();
    assert_ne!(after, before);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&after).unwrap()["next_source"],
        "notes"
    );
    holder.release();
}

#[test]
fn snapshot_excludes_ingest_admission_and_keeps_writer_claims_through_exec() {
    let fixture = Fixture::new();
    let vault = fixture.directory.path().join("first");
    let state = vault.join(".lorekeeper");
    std::fs::create_dir(&state).unwrap();
    let cursor = state.join("ingest-cursor.json");
    let before = br#"{"next_source":"earlier-source"}"#;
    std::fs::write(&cursor, before).unwrap();
    let mut holder = hold(fixture.snapshot());
    let blocked = fixture
        .command(&fixture.config)
        .args(["ingest"])
        .output()
        .unwrap();
    assert_installation_busy(&blocked);
    assert_eq!(std::fs::read(&cursor).unwrap(), before);
    assert!(matches!(
        lk_vault::MutationClaim::acquire(&vault),
        Err(lk_vault::VaultError::MutationBusy)
    ));
    assert!(matches!(
        lk_queue::claim_queue(&state.join("queue")).unwrap(),
        lk_queue::QueueClaim::Busy
    ));
    let workflow = std::fs::File::open(state.join("workflow.lock")).unwrap();
    assert!(matches!(
        workflow.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    drop(workflow);
    holder.release();
    lk_vault::MutationClaim::acquire(&vault).unwrap();
    assert!(matches!(
        lk_queue::claim_queue(&state.join("queue")).unwrap(),
        lk_queue::QueueClaim::Held(_)
    ));
    assert_eq!(std::fs::read(cursor).unwrap(), before);
}

#[test]
fn refused_installation_contenders_preserve_the_complete_snapshot_namespace() {
    let fixture = Fixture::new();
    let vault = fixture.directory.path().join("first");
    let mut holder = hold(fixture.snapshot());
    let capture = || {
        walkdir::WalkDir::new(&vault)
            .into_iter()
            .map(|entry| {
                let entry = entry.unwrap();
                (
                    entry.path().strip_prefix(&vault).unwrap().to_path_buf(),
                    if entry.file_type().is_file() {
                        Some(revision(entry.path()))
                    } else {
                        None
                    },
                )
            })
            .collect::<BTreeMap<_, _>>()
    };
    let before = capture();
    let destination = vault.join("introduced/runtime");
    let refused = fixture
        .command(&fixture.config)
        .args([
            "self",
            "deploy",
            "--skills",
            "none",
            "--agent",
            "codex",
            "--data-dir",
        ])
        .arg(&destination)
        .output()
        .unwrap();
    assert_installation_busy(&refused);
    assert_eq!(capture(), before);

    let other = Fixture::new();
    let config = std::fs::read_to_string(&other.config)
        .unwrap()
        .replace("root: first", &format!("root: {}", vault.display()));
    std::fs::write(&other.config, config).unwrap();
    let refused = other
        .command(&other.config)
        .args([
            "self",
            "deploy",
            "--skills",
            "none",
            "--agent",
            "codex",
            "--data-dir",
        ])
        .arg(&destination)
        .output()
        .unwrap();
    assert_installation_busy(&refused);
    assert_eq!(capture(), before);
    holder.release();
    assert_success(
        &fixture
            .command(&fixture.config)
            .args([
                "self",
                "deploy",
                "--skills",
                "none",
                "--agent",
                "codex",
                "--data-dir",
            ])
            .arg(destination)
            .output()
            .unwrap(),
    );
}

#[test]
fn snapshot_rejects_a_queue_fifo_without_starting_a_child_or_retaining_claims() {
    use std::os::unix::fs::FileTypeExt;
    let fixture = Fixture::new();
    let queue = fixture.directory.path().join("first/.lorekeeper/queue");
    std::fs::create_dir_all(&queue).unwrap();
    let lock = queue.join(".lock");
    assert!(
        Command::new("mkfifo")
            .arg(&lock)
            .status()
            .unwrap()
            .success()
    );
    let mut child = fixture
        .snapshot()
        .args(["/bin/sh", "-c", "printf child-started"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("snapshot did not exit before the private test deadline");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        std::fs::symlink_metadata(&lock)
            .unwrap()
            .file_type()
            .is_fifo()
    );
    let validation = fixture
        .command(&fixture.config)
        .arg("validate")
        .output()
        .unwrap();
    assert_success(&validation);
}

#[test]
fn snapshot_refuses_busy_or_unsettled_state_without_running_the_child_or_recovery() {
    let fixture = Fixture::new();
    let marker = fixture.directory.path().join("started");
    let invoke = || {
        fixture
            .snapshot()
            .args(["/bin/sh", "-c", "printf started > \"$1\"", "snapshot"])
            .arg(&marker)
            .output()
            .unwrap()
    };
    let mut workflow = hold(fixture.workflow(&fixture.config));
    assert_installation_busy(&invoke());
    assert!(!marker.exists());
    workflow.release();
    let journal = fixture
        .directory
        .path()
        .join("first/.lorekeeper/mutation/pending.json");
    std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
    let before = b"unverified journal bytes must remain unchanged";
    std::fs::write(&journal, before).unwrap();
    let result = invoke();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("pending mutation journal"));
    assert!(!marker.exists());
    assert_eq!(std::fs::read(&journal).unwrap(), before);
    std::fs::remove_file(journal).unwrap();
    assert_success(&invoke());
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "started");
}

#[test]
fn snapshot_preserves_literal_arguments_scope_cwd_and_child_exit() {
    let fixture = Fixture::new();
    let result = fixture.snapshot().args([
        "/bin/sh", "-c", "printf '%s\\n' \"$PWD\" \"$LORE_VAULT_ROOT\" \"$LORE_CONFIG\" \"$@\"; printf diagnostic >&2; exit 7", "snapshot", "one space", "한글", "--literal"
    ]).output().unwrap();
    assert_eq!(result.status.code(), Some(7));
    let stdout = String::from_utf8(result.stdout).unwrap();
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(
        lines,
        [
            fixture
                .directory
                .path()
                .canonicalize()
                .unwrap()
                .to_str()
                .unwrap(),
            fixture
                .directory
                .path()
                .join("first")
                .canonicalize()
                .unwrap()
                .to_str()
                .unwrap(),
            fixture.config.canonicalize().unwrap().to_str().unwrap(),
            "one space",
            "한글",
            "--literal",
        ]
    );
    assert_eq!(String::from_utf8(result.stderr).unwrap(), "diagnostic");
    assert_success(&fixture.deploy());
}

#[test]
fn snapshot_invalid_configuration_and_exec_failure_leave_claims_releasable() {
    let fixture = Fixture::new();
    let missing = fixture.directory.path().join("no-such-program");
    let failed = fixture.snapshot().arg(missing).output().unwrap();
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("exec snapshot command"));
    assert_success(&fixture.deploy());
    std::fs::write(&fixture.config, "invalid: configuration").unwrap();
    let marker = fixture.directory.path().join("not-started");
    let failed = fixture
        .snapshot()
        .args(["/bin/sh", "-c", "touch \"$1\"", "snapshot"])
        .arg(&marker)
        .output()
        .unwrap();
    assert!(!failed.status.success());
    assert!(!marker.exists());
}

#[test]
fn daily_collection_excludes_deployment_before_acquiring_the_vault_workflow() {
    use std::io::Read;
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    let fixture = Fixture::new();
    assert_success(&fixture.deploy());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let (started, request) = mpsc::channel();
    let (release, proceed) = mpsc::channel();
    let server = std::thread::spawn(move || -> std::io::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "no daily source request arrived",
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error),
            }
        };
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        stream.set_write_timeout(Some(Duration::from_secs(10)))?;
        let mut buffer = [0; 4096];
        if stream.read(&mut buffer)? == 0 {
            return Err(std::io::Error::other("daily source request was empty"));
        }
        started.send(()).map_err(std::io::Error::other)?;
        proceed
            .recv_timeout(Duration::from_secs(30))
            .map_err(std::io::Error::other)?;
        let feed = "<?xml version=\"1.0\"?><rss version=\"2.0\"><channel><title>Fixture</title><link>https://example.invalid/</link><description>Empty observed feed</description></channel></rss>";
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/rss+xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{feed}",
            feed.len(),
        )?;
        stream.flush()
    });
    std::fs::write(
        &fixture.config,
        format!(
            "vault:\n  root: first\n  locale: en\nidentity: {{name: Test, email: test@example.invalid}}\nllm: {{provider: noop}}\nsources:\n  news:\n    type: rss\n    params:\n      feeds: [{{id: fixture, url: 'http://{address}/feed'}}]\n"
        ),
    )
    .unwrap();
    let before = artifacts(&fixture.data_dir);
    let binary_before = revision(&fixture.binary);
    let stdout_path = fixture.directory.path().join("daily.stdout");
    let stderr_path = fixture.directory.path().join("daily.stderr");
    let mut command = fixture.command(&fixture.config);
    for name in [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "NO_PROXY",
        "no_proxy",
    ] {
        command.env_remove(name);
    }
    let mut daily = Held(
        command
            .args(["run", "daily", "--json"])
            .stdin(Stdio::null())
            .stdout(std::fs::File::create(&stdout_path).unwrap())
            .stderr(std::fs::File::create(&stderr_path).unwrap())
            .spawn()
            .unwrap(),
    );
    let acquired = request.recv_timeout(Duration::from_secs(15));
    let workflow = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(
            fixture
                .directory
                .path()
                .join("first/.lorekeeper/workflow.lock"),
        )
        .unwrap();
    let workflow_available = workflow.try_lock();
    drop(workflow);
    let deploy = fixture.deploy();
    let after = artifacts(&fixture.data_dir);
    let binary_after = revision(&fixture.binary);
    let _ = release.send(());
    let server_result = server.join().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = daily.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "daily fixture did not finish");
        std::thread::sleep(Duration::from_millis(10));
    };
    let diagnostic = std::fs::read_to_string(&stderr_path).unwrap();
    assert!(
        acquired.is_ok(),
        "{acquired:?}, server: {server_result:?}: {diagnostic}"
    );
    assert!(
        workflow_available.is_ok(),
        "daily collection held the workflow lease: {diagnostic}"
    );
    server_result.unwrap();
    assert_installation_busy(&deploy);
    assert_eq!(after, before);
    assert_eq!(binary_after, binary_before);
    assert!(status.success(), "{diagnostic}");
    let result: serde_json::Value =
        serde_json::from_slice(&std::fs::read(stdout_path).unwrap()).unwrap();
    assert_eq!(result["semantic_calls"], 0);
    assert_eq!(result["ok"], true);
    assert_success(&fixture.deploy());
}

#[test]
fn symlinked_host_skills_exclude_deployment_across_otherwise_distinct_installations() {
    let first = Fixture::new();
    let second = Fixture::new();
    let shared = first.directory.path().join("shared-host-skills");
    std::fs::create_dir(&shared).unwrap();
    for fixture in [&first, &second] {
        let host = fixture.home.join(".claude");
        std::fs::create_dir(&host).unwrap();
        std::os::unix::fs::symlink(&shared, host.join("skills")).unwrap();
    }
    assert_success(
        &first
            .command(&first.config)
            .args([
                "self",
                "deploy",
                "--skills",
                "user",
                "--agent",
                "claude-code",
            ])
            .output()
            .unwrap(),
    );
    let entry = shared.join("lore-process/SKILL.md");
    let before = revision(&entry);
    let mut holder = hold(first.workflow(&first.config));
    let deploy = second
        .command(&second.config)
        .args([
            "self",
            "deploy",
            "--skills",
            "user",
            "--agent",
            "claude-code",
        ])
        .output()
        .unwrap();
    assert_installation_busy(&deploy);
    assert_eq!(revision(&entry), before);
    holder.release();
    assert_success(
        &second
            .command(&second.config)
            .args([
                "self",
                "deploy",
                "--skills",
                "user",
                "--agent",
                "claude-code",
            ])
            .output()
            .unwrap(),
    );
}

#[test]
fn native_consumer_protects_project_skills_in_its_vault_working_directory() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    let first = Fixture::new();
    let second = Fixture::new();
    assert_success(&first.deploy());
    let vault = first.directory.path().join("first");
    let deploy_project = || {
        second
            .command(&second.config)
            .current_dir(&vault)
            .args(["self", "deploy", "--skills", "project", "--agent", "codex"])
            .output()
            .unwrap()
    };
    assert_success(&deploy_project());
    let project_skills = vault.join(".agents/skills");
    let snapshot = || {
        walkdir::WalkDir::new(&project_skills)
            .into_iter()
            .map(Result::unwrap)
            .filter(|entry| entry.file_type().is_file())
            .map(|entry| {
                (
                    entry
                        .path()
                        .strip_prefix(&project_skills)
                        .unwrap()
                        .to_path_buf(),
                    revision(entry.path()),
                )
            })
            .collect::<BTreeMap<_, _>>()
    };
    let before = snapshot();
    assert!(!before.is_empty());

    let fake_host = first.directory.path().join("fake-codex");
    std::fs::write(
        &fake_host,
        "#!/bin/sh\nif [ \"$1\" = --version ]; then printf 'codex-cli 0.163.0\\n'; exit 0; fi\ncase \"$*\" in *--output-last-message*) ;; *--output-schema*) cat > /dev/null; printf '%s\\n' '{\"status\":\"complete\",\"body\":\"An active consumer reads project skills from its vault.\",\"reason\":null}'; exit 0 ;; esac\npwd -P > \"$TEST_INSTALLATION_ROOT/host-cwd\"\n: > \"$TEST_INSTALLATION_ROOT/host-ready\"\nattempt=0\nwhile [ ! -e \"$TEST_INSTALLATION_ROOT/host-release\" ]; do\n  attempt=$((attempt + 1))\n  [ \"$attempt\" -lt 3000 ] || exit 92\n  sleep 0.01\ndone\nexit 91\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake_host, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        &first.config,
        format!(
            "vault:\n  root: first\n  locale: en\nidentity: {{name: Test, email: test@example.invalid}}\nllm: {{provider: queue}}\nsources:\n  notes:\n    type: manual\n    params: {{inbox_dir: inbox}}\n    concept_policy: supported-new\nexecution:\n  default_profile: fixture\n  profiles:\n    fixture:\n      engine: codex\n      executable: {}\n      timeout_seconds: 60\n",
            serde_json::to_string(&fake_host).unwrap(),
        ),
    )
    .unwrap();
    std::fs::create_dir(vault.join("inbox")).unwrap();
    std::fs::write(
        vault.join("inbox/source.md"),
        "# Deployment evidence\n\nAn active consumer reads its project skills from the vault working directory.\n",
    )
    .unwrap();
    assert_success(&first.command(&first.config).arg("ingest").output().unwrap());
    let stdout_path = first.directory.path().join("native.stdout");
    let stderr_path = first.directory.path().join("native.stderr");
    let mut consumer = Held(
        first
            .command(&first.config)
            .env("TEST_INSTALLATION_ROOT", first.directory.path())
            .args(["run", "process", "--json"])
            .stdin(Stdio::null())
            .stdout(std::fs::File::create(&stdout_path).unwrap())
            .stderr(std::fs::File::create(&stderr_path).unwrap())
            .spawn()
            .unwrap(),
    );
    let ready_path = first.directory.path().join("host-ready");
    let deadline = Instant::now() + Duration::from_secs(30);
    let ready = loop {
        if ready_path.is_file() {
            break true;
        }
        if consumer.0.try_wait().unwrap().is_some() || Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let observed_cwd = std::fs::read_to_string(first.directory.path().join("host-cwd"));
    let deploy = deploy_project();
    let after = snapshot();
    std::fs::write(first.directory.path().join("host-release"), "release\n").unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = consumer.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "native fixture did not finish");
        std::thread::sleep(Duration::from_millis(10));
    };
    let diagnostic = std::fs::read_to_string(&stderr_path).unwrap();
    assert!(ready, "fake native consumer did not start: {diagnostic}");
    assert_eq!(
        PathBuf::from(observed_cwd.as_ref().unwrap().trim()),
        vault.canonicalize().unwrap(),
    );
    assert_installation_busy(&deploy);
    assert_eq!(after, before);
    assert!(!status.success(), "fake host must not report completion");
    let result: serde_json::Value =
        serde_json::from_slice(&std::fs::read(stdout_path).unwrap()).unwrap();
    assert_eq!(result["semantic_calls"], 2);
    assert_eq!(result["ok"], false);
    assert_success(&deploy_project());
}

fn uninstall_fixture(root_in_templates: bool) -> Fixture {
    let fixture = Fixture::new();
    assert_success(&fixture.deploy());
    let root = if root_in_templates {
        fixture.data_dir.join("templates")
    } else {
        fixture.config_home.join("lorekeeper/vault")
    };
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("user-note.md"), b"retained knowledge\n").unwrap();
    let body = std::fs::read_to_string(&fixture.config).unwrap();
    std::fs::write(
        &fixture.config,
        body.replace("root: first", &format!("root: {}", root.display())),
    )
    .unwrap();
    fixture
}

#[test]
fn uninstall_refuses_nested_vaults_before_removing_any_payload() {
    for templates in [true, false] {
        let fixture = uninstall_fixture(templates);
        assert_success(
            &fixture
                .workflow(&fixture.config)
                .arg("/usr/bin/true")
                .output()
                .unwrap(),
        );
        let before = artifacts(&fixture.data_dir);
        let binary = revision(&fixture.binary);
        let config = revision(&fixture.config);
        let mut command = fixture.command(&fixture.config);
        command.args(["self", "uninstall", "--yes"]);
        if !templates {
            command.arg("--purge-config");
        }
        let output = command.output().unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("protected vault"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(before, artifacts(&fixture.data_dir));
        assert_eq!(binary, revision(&fixture.binary));
        assert_eq!(config, revision(&fixture.config));
        let root = if templates {
            fixture.data_dir.join("templates")
        } else {
            fixture.config_home.join("lorekeeper/vault")
        };
        assert_eq!(
            std::fs::read(root.join("user-note.md")).unwrap(),
            b"retained knowledge\n"
        );
    }
}

#[test]
fn uninstall_preserves_independent_and_config_nested_vaults_without_purge() {
    for nested in [true, false] {
        let fixture = if nested {
            uninstall_fixture(false)
        } else {
            let fixture = Fixture::new();
            assert_success(&fixture.deploy());
            fixture
        };
        let root = if nested {
            fixture.config_home.join("lorekeeper/vault")
        } else {
            fixture.directory.path().join("first")
        };
        std::fs::write(root.join("user-note.md"), b"retained knowledge\n").unwrap();
        let config = revision(&fixture.config);
        assert_success(
            &fixture
                .command(&fixture.config)
                .args(["self", "uninstall", "--yes"])
                .output()
                .unwrap(),
        );
        assert!(!fixture.binary.exists());
        assert!(!fixture.data_dir.join("templates").exists());
        assert_eq!(config, revision(&fixture.config));
        assert_eq!(
            std::fs::read(root.join("user-note.md")).unwrap(),
            b"retained knowledge\n"
        );
    }
}

#[test]
fn uninstall_allows_missing_vault_and_absent_configuration() {
    for absent_config in [false, true] {
        let fixture = Fixture::new();
        assert_success(&fixture.deploy());
        std::fs::remove_dir_all(fixture.directory.path().join("first")).unwrap();
        let mut command = fixture.command(&fixture.config);
        if absent_config {
            std::fs::remove_file(&fixture.config).unwrap();
            command = fixture.command_without_config();
        }
        assert_success(
            &command
                .args(["self", "uninstall", "--yes", "--purge-config"])
                .output()
                .unwrap(),
        );
        assert!(!fixture.binary.exists());
        assert!(!fixture.config_home.join("lorekeeper").exists());
    }
}

#[test]
fn uninstall_refuses_invalid_or_missing_explicit_configuration() {
    for invalid in [false, true] {
        let fixture = Fixture::new();
        assert_success(&fixture.deploy());
        let before = artifacts(&fixture.data_dir);
        if invalid {
            std::fs::write(&fixture.config, b"vault: [invalid").unwrap();
        } else {
            std::fs::remove_file(&fixture.config).unwrap();
        }
        let output = fixture
            .command(&fixture.config)
            .args(["self", "uninstall", "--yes", "--purge-config"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(fixture.binary.is_file());
        assert_eq!(before, artifacts(&fixture.data_dir));
    }
}

#[test]
fn uninstall_refuses_purging_claimed_coordinators() {
    let mut fixture = Fixture::new();
    fixture.data_dir = fixture.config_home.join("lorekeeper/runtime");
    assert_success(&fixture.deploy());
    let before = artifacts(&fixture.data_dir);
    let output = fixture
        .command(&fixture.config)
        .args(["self", "uninstall", "--yes", "--purge-config"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("protected installation coordination"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(fixture.binary.is_file());
    assert_eq!(before, artifacts(&fixture.data_dir));
}

#[test]
fn uninstall_removes_project_skills_inside_vault_without_removing_knowledge() {
    let fixture = Fixture::new();
    let root = fixture.directory.path().join("first");
    std::fs::write(root.join("user-note.md"), b"retained knowledge\n").unwrap();
    assert_success(
        &fixture
            .command(&fixture.config)
            .current_dir(&root)
            .args(["self", "deploy", "--skills", "project", "--agent", "both"])
            .output()
            .unwrap(),
    );
    assert!(root.join(".agents/skills/lore-wiki/SKILL.md").is_file());
    assert_success(
        &fixture
            .command(&fixture.config)
            .current_dir(&root)
            .args(["self", "uninstall", "--yes"])
            .output()
            .unwrap(),
    );
    assert!(!root.join(".agents/skills/lore-wiki").exists());
    assert!(!root.join(".claude/skills/lore-wiki").exists());
    assert_eq!(
        std::fs::read(root.join("user-note.md")).unwrap(),
        b"retained knowledge\n"
    );
}

#[test]
fn legacy_removal_script_refuses_without_deleting_private_installation() {
    let fixture = Fixture::new();
    assert_success(&fixture.deploy());
    let before = artifacts(&fixture.data_dir);
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/uninstall.sh");
    let output = Command::new("bash")
        .arg(script)
        .args(["--yes", "--install-dir"])
        .arg(fixture.binary.parent().unwrap())
        .arg("--data-dir")
        .arg(&fixture.data_dir)
        .env("HOME", &fixture.home)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("lore self uninstall"));
    assert!(fixture.binary.is_file());
    assert_eq!(before, artifacts(&fixture.data_dir));
}

#[test]
fn uninstall_refuses_templates_aliasing_the_vault() {
    let fixture = Fixture::new();
    assert_success(&fixture.deploy());
    let root = fixture.directory.path().join("first");
    let note = root.join("user-note.md");
    std::fs::write(&note, b"retained knowledge\n").unwrap();
    let templates = fixture.data_dir.join("templates");
    std::fs::remove_dir_all(&templates).unwrap();
    std::os::unix::fs::symlink(&root, &templates).unwrap();
    let output = fixture
        .command(&fixture.config)
        .args(["self", "uninstall", "--yes"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("protected vault"));
    assert!(fixture.binary.is_file());
    assert_eq!(std::fs::read(note).unwrap(), b"retained knowledge\n");
    assert!(
        std::fs::symlink_metadata(templates)
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[test]
fn uninstall_preserves_an_explicitly_selected_carried_example() {
    let fixture = Fixture::new();
    assert_success(&fixture.deploy());
    let selected = fixture.config_home.join("lorekeeper/config.example.yaml");
    let body = std::fs::read_to_string(&fixture.config).unwrap().replace(
        "root: first",
        &format!("root: {}", fixture.directory.path().join("first").display()),
    );
    std::fs::write(&selected, &body).unwrap();
    assert_success(
        &fixture
            .command(&selected)
            .args(["self", "uninstall", "--yes"])
            .output()
            .unwrap(),
    );
    assert!(!fixture.binary.exists());
    assert_eq!(std::fs::read_to_string(selected).unwrap(), body);
}

#[test]
fn uninstall_preserves_selected_configuration_inside_every_owned_tree() {
    for (relative, skills, purge) in [
        ("runtime/templates/selected.yaml", None, false),
        ("runtime/skills/selected.yaml", None, true),
        (
            "home/.agents/skills/lore-wiki/selected.yaml",
            Some("user"),
            false,
        ),
        (
            ".claude/skills/lore-wiki/selected.yaml",
            Some("project"),
            true,
        ),
    ] {
        let fixture = Fixture::new();
        assert_success(&fixture.deploy());
        if let Some(skills) = skills {
            assert_success(
                &fixture
                    .command(&fixture.config)
                    .args(["self", "deploy", "--skills", skills, "--agent", "both"])
                    .output()
                    .unwrap(),
            );
        }
        let selected = fixture.directory.path().join(relative);
        std::fs::create_dir_all(selected.parent().unwrap()).unwrap();
        let body = std::fs::read_to_string(&fixture.config).unwrap().replace(
            "root: first",
            &format!("root: {}", fixture.directory.path().join("first").display()),
        );
        std::fs::write(&selected, &body).unwrap();
        let before = artifacts(&fixture.data_dir);
        let binary = revision(&fixture.binary);
        let mut command = fixture.command(&selected);
        command.args(["self", "uninstall", "--yes"]);
        if purge {
            command.arg("--purge-config");
        }
        let output = command.output().unwrap();
        assert!(!output.status.success(), "{relative} purge={purge}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("selected configuration"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(std::fs::read_to_string(&selected).unwrap(), body);
        assert_eq!(before, artifacts(&fixture.data_dir));
        assert_eq!(binary, revision(&fixture.binary));
    }
}

#[test]
fn uninstall_allows_explicit_purge_of_the_selected_config_directory() {
    let fixture = Fixture::new();
    assert_success(&fixture.deploy());
    let selected = fixture.config_home.join("lorekeeper/config.yaml");
    let root = fixture.directory.path().join("first");
    let body = std::fs::read_to_string(&fixture.config)
        .unwrap()
        .replace("root: first", &format!("root: {}", root.display()));
    std::fs::write(&selected, &body).unwrap();
    std::fs::write(root.join("note.md"), b"user note").unwrap();
    assert_success(
        &fixture
            .command(&selected)
            .args(["self", "uninstall", "--yes", "--purge-config"])
            .output()
            .unwrap(),
    );
    assert!(!fixture.binary.exists());
    assert!(!fixture.config_home.join("lorekeeper").exists());
    assert_eq!(std::fs::read(root.join("note.md")).unwrap(), b"user note");
}
