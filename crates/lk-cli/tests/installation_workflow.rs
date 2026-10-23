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

    fn command(&self, config: &Path) -> Command {
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
            .env("LORE_INSTALL_DATA_DIR", &self.data_dir)
            .arg("--config")
            .arg(config);
        command
    }

    fn workflow(&self, config: &Path) -> Command {
        let mut command = self.command(config);
        command.args(["workflow", "--"]);
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
        "#!/bin/sh\nif [ \"$1\" = --version ]; then printf 'codex-cli 0.163.0\\n'; exit 0; fi\ncase \"$*\" in *--output-schema*) cat > /dev/null; printf '%s\\n' '{\"status\":\"complete\",\"body\":\"An active consumer reads project skills from its vault.\",\"reason\":null}'; exit 0 ;; esac\npwd -P > \"$TEST_INSTALLATION_ROOT/host-cwd\"\n: > \"$TEST_INSTALLATION_ROOT/host-ready\"\nattempt=0\nwhile [ ! -e \"$TEST_INSTALLATION_ROOT/host-release\" ]; do\n  attempt=$((attempt + 1))\n  [ \"$attempt\" -lt 3000 ] || exit 92\n  sleep 0.01\ndone\nexit 91\n",
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
