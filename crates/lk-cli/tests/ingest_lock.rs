//! Explicit ingest writer waiting must not change refusal or read-only defaults.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

struct Workspace {
    root: tempfile::TempDir,
}

impl Workspace {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("vault/inbox")).unwrap();
        std::fs::write(
            root.path().join("config.yaml"),
            "vault:\n  root: vault\nidentity:\n  name: Test\n  email: test@example.invalid\n\
             sources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: inbox\n",
        )
        .unwrap();
        std::fs::write(root.path().join("vault/inbox/note.md"), "# Note\n\nBody.\n").unwrap();
        Self { root }
    }

    fn vault(&self) -> PathBuf {
        self.root.path().join("vault")
    }

    fn base_command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_lore"));
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("LORE_") {
                command.env_remove(key);
            }
        }
        command
            .arg("--config")
            .arg(self.root.path().join("config.yaml"))
            .env("LORE_INSTALL_DATA_DIR", self.root.path().join("data"));
        command
    }

    fn command(&self) -> Command {
        let mut command = self.base_command();
        command.arg("ingest");
        command
    }
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    for entry in walkdir::WalkDir::new(root) {
        let entry = entry.unwrap();
        if entry.file_type().is_file() {
            files.insert(
                entry.path().strip_prefix(root).unwrap().to_path_buf(),
                std::fs::read(entry.path()).unwrap(),
            );
        }
    }
    files
}

#[test]
fn busy_ingest_refuses_by_default_and_waiting_dry_run_remains_read_only() {
    let ws = Workspace::new();
    let _held = lk_vault::MutationSession::acquire(&ws.vault()).unwrap();
    let before = snapshot(&ws.vault());

    let refused = ws.command().output().unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("vault mutation is busy"));
    assert_eq!(snapshot(&ws.vault()), before);

    let preview = ws
        .command()
        .args(["--dry-run", "--wait-for-vault"])
        .output()
        .unwrap();
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    assert_eq!(snapshot(&ws.vault()), before);
}

#[test]
fn filtered_ingest_collects_then_derives_from_pages_after_the_writer_releases() {
    let ws = Workspace::new();
    let held = lk_vault::MutationSession::acquire(&ws.vault()).unwrap();
    let before = snapshot(&ws.vault());
    let stderr = tempfile::NamedTempFile::new().unwrap();
    let mut child = ws
        .command()
        .arg("notes")
        .arg("--wait-for-vault")
        .stdout(Stdio::null())
        .stderr(stderr.reopen().unwrap())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    while !std::fs::read_to_string(stderr.path())
        .unwrap()
        .contains("Waiting for exclusive vault access...")
    {
        assert!(
            child.try_wait().unwrap().is_none(),
            "ingest exited before waiting"
        );
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("ingest did not reach writer wait");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(child.try_wait().unwrap().is_none());
    assert_eq!(snapshot(&ws.vault()), before, "waiting must not publish");
    std::fs::create_dir_all(ws.vault().join("wiki/documents")).unwrap();
    std::fs::write(ws.vault().join("wiki/documents/note.md"), format!(
        "---\ntype: document\ntitle: Note\ncreated: 2020-01-01\nsource_file: {}\n---\n## Summary\n\n## Content\n\nA prior source revision.\n",
        serde_json::to_string(&ws.vault().join("inbox/note.md")).unwrap()
    )).unwrap();
    drop(held);

    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("ingest did not finish after the writer released");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(
        status.success(),
        "{}",
        std::fs::read_to_string(stderr.path()).unwrap()
    );
    assert!(ws.vault().join(".lorekeeper/ingest.jsonl").is_file());
    let page = lk_vault::parse_page(
        &std::fs::read_to_string(ws.vault().join("wiki/documents/note.md")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        page.frontmatter
            .get("created")
            .and_then(|value| value.as_str()),
        Some("2020-01-01"),
        "planning must observe the prior writer's current page after acquiring the lock"
    );
}

#[test]
fn queue_contention_cannot_publish_a_document_without_its_requests() {
    let ws = Workspace::new();
    let queue = ws.vault().join(".lorekeeper/queue");
    std::fs::create_dir_all(&queue).unwrap();
    let held = lk_queue::claim_queue(&queue).unwrap();
    assert!(matches!(held, lk_queue::QueueClaim::Held(_)));
    let result = ws.command().output().unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("queue"));
    assert!(!ws.vault().join("wiki/documents/note.md").exists());
    assert!(ws.vault().join("inbox/note.md").is_file());
    drop(held);
    let result = ws.command().output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(ws.vault().join("wiki/documents/note.md").is_file());
    let files = std::fs::read_dir(&queue)
        .unwrap()
        .map(Result::unwrap)
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .collect::<Vec<_>>();
    assert_eq!(files.len(), 1);
    let requests = lk_queue::read_tasks(&files[0].path()).unwrap();
    assert!(!requests.tasks.is_empty());
    assert!(
        requests
            .tasks
            .iter()
            .all(|task| task.target.vault_path == "wiki/documents/note.md")
    );
}

#[cfg(unix)]
struct Foreground(std::process::Child);

#[cfg(unix)]
impl std::ops::Deref for Foreground {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[cfg(unix)]
impl std::ops::DerefMut for Foreground {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

#[cfg(unix)]
impl Drop for Foreground {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(unix)]
#[test]
fn publication_workflow_wait_refuses_root_drift_without_publishing_or_retiring_inputs() {
    use std::io::{BufRead, BufReader, Write};

    let ws = Workspace::new();
    let alias = ws.root.path().join("selected");
    std::os::unix::fs::symlink(ws.vault(), &alias).unwrap();
    let config = ws.root.path().join("config.yaml");
    let raw = std::fs::read_to_string(&config)
        .unwrap()
        .replace("root: vault", "root: selected");
    std::fs::write(&config, &raw).unwrap();
    let mut holder = Foreground(
        ws.base_command()
            .args([
                "workflow",
                "--",
                "/bin/sh",
                "-c",
                "printf 'ready\\n'; read release",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut ready = String::new();
    BufReader::new(holder.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    assert_eq!(ready, "ready\n");
    let error = tempfile::NamedTempFile::new().unwrap();
    let mut child = Foreground(
        ws.command()
            .args(["notes", "--publication-workflow", "wait"])
            .stdout(Stdio::null())
            .stderr(error.reopen().unwrap())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    while !std::fs::read_to_string(error.path())
        .unwrap()
        .contains("Waiting for the vault workflow lease")
    {
        assert!(Instant::now() < deadline);
        assert!(child.try_wait().unwrap().is_none());
        std::thread::sleep(Duration::from_millis(10));
    }
    let other = ws.root.path().join("other-vault");
    std::fs::create_dir(&other).unwrap();
    let before = snapshot(&other);
    std::fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink(&other, &alias).unwrap();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(!status.success());
    assert!(
        std::fs::read_to_string(error.path())
            .unwrap()
            .contains("workflow vault selection changed")
    );
    assert_eq!(snapshot(&other), before);
    assert!(!ws.vault().join("wiki/documents/note.md").exists());
    assert!(ws.vault().join("inbox/note.md").is_file());
    assert_eq!(std::fs::read_to_string(&config).unwrap(), raw);
    holder
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"release\n")
        .unwrap();
    assert!(holder.wait().unwrap().success());
}

#[cfg(unix)]
#[test]
fn acquisition_wait_refuses_config_drift_before_recovering_cursor_or_manual_state() {
    for filtered in [false, true] {
        let ws = Workspace::new();
        let _held = lk_vault::MutationSession::acquire(&ws.vault()).unwrap();
        let pending = ws.vault().join(".lorekeeper/mutation/pending.json");
        let raw = b"pending journal must remain unread while configuration is no longer current";
        std::fs::write(&pending, raw).unwrap();
        let before = snapshot(&ws.vault());
        let stderr = tempfile::NamedTempFile::new().unwrap();
        let mut command = ws.command();
        if filtered {
            command.arg("notes");
        }
        let mut child = Foreground(
            command
                .arg("--wait-for-vault")
                .stdout(Stdio::null())
                .stderr(stderr.reopen().unwrap())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while !std::fs::read_to_string(stderr.path())
            .unwrap()
            .contains("Waiting for exclusive vault access...")
        {
            assert!(child.try_wait().unwrap().is_none());
            assert!(Instant::now() < deadline, "ingest never waited");
            std::thread::sleep(Duration::from_millis(20));
        }
        let path = ws.root.path().join("config.yaml");
        let config = std::fs::read_to_string(&path)
            .unwrap()
            .replace("name: Test", "name: Changed");
        std::fs::write(path, config).unwrap();
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "ingest did not reject changed config while waiting"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(!status.success());
        let error = std::fs::read_to_string(stderr.path()).unwrap();
        assert!(error.contains("configuration changed"), "{error}");
        assert_eq!(std::fs::read(pending).unwrap(), raw);
        assert_eq!(snapshot(&ws.vault()), before);
    }
}

#[cfg(unix)]
#[test]
fn publication_workflow_wait_collects_network_first_and_preserves_cancel_and_config_guards() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::os::unix::process::ExitStatusExt;
    use std::sync::mpsc;

    for outcome in ["resume", "refuse", "cancel", "config"] {
        let ws = Workspace::new();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let (served, received) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "collection was blocked before its HTTP request"
                        );
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("fixture accept: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = [0; 4096];
            assert!(stream.read(&mut request).unwrap() > 0);
            let now = jiff::Timestamp::now();
            let body = format!(
                "<feed xmlns=\"http://www.w3.org/2005/Atom\"><id>urn:fixture</id><title>Fixture</title><updated>{now}</updated><entry><id>urn:item</id><title>Scoped observation</title><updated>{now}</updated><content type=\"text\">An observed source claim.</content></entry></feed>"
            );
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/atom+xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            stream.flush().unwrap();
            served.send(()).unwrap();
        });
        let config = ws.root.path().join("config.yaml");
        std::fs::write(&config, format!("vault:\n  root: vault\nidentity: {{name: Test, email: test@example.invalid}}\nsources:\n  news:\n    type: rss\n    concept_policy: disabled\n    params:\n      feeds: [{{id: fixture, url: 'http://{address}/feed'}}]\n")).unwrap();
        std::fs::create_dir_all(ws.vault().join(".lorekeeper")).unwrap();
        let workflow = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(ws.vault().join(".lorekeeper/workflow.lock"))
            .unwrap();
        workflow.lock().unwrap();
        let stderr = tempfile::NamedTempFile::new().unwrap();
        let mut command = ws.command();
        for key in [
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
        ] {
            command.env_remove(key);
        }
        let mut child = Foreground(
            command
                .args([
                    "--wait-for-vault",
                    "--publication-workflow",
                    if outcome == "refuse" {
                        "refuse"
                    } else {
                        "wait"
                    },
                ])
                .stdout(Stdio::null())
                .stderr(stderr.reopen().unwrap())
                .spawn()
                .unwrap(),
        );
        received.recv_timeout(Duration::from_secs(10)).unwrap();
        server.join().unwrap();
        if outcome == "refuse" {
            let deadline = Instant::now() + Duration::from_secs(10);
            while child.try_wait().unwrap().is_none() {
                assert!(
                    Instant::now() < deadline,
                    "default workflow contention did not refuse"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(!child.wait().unwrap().success());
            assert!(
                std::fs::read_to_string(stderr.path())
                    .unwrap()
                    .contains("workflow busy")
            );
            assert!(!ws.vault().join("daily").exists());
            assert!(!ws.vault().join(".lorekeeper/queue").exists());
            continue;
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while !std::fs::read_to_string(stderr.path())
            .unwrap()
            .contains("Claiming the vault workflow before publishing")
        {
            assert!(Instant::now() < deadline);
            assert!(child.try_wait().unwrap().is_none());
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(child.try_wait().unwrap().is_none());
        assert!(!ws.vault().join("daily").exists());
        assert!(!ws.vault().join(".lorekeeper/queue").exists());
        let writer = lk_vault::MutationClaim::acquire(&ws.vault())
            .expect("publication wait must not hold the writer");
        drop(writer);
        match outcome {
            "cancel" => {
                let pid =
                    rustix::process::Pid::from_raw(i32::try_from(child.id()).unwrap()).unwrap();
                rustix::process::kill_process(pid, rustix::process::Signal::TERM).unwrap();
            }
            "config" => {
                let raw = std::fs::read_to_string(&config)
                    .unwrap()
                    .replace("name: Test", "name: Changed");
                std::fs::write(&config, raw).unwrap();
            }
            _ => {}
        }
        if outcome == "resume" {
            drop(workflow);
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "publication wait did not end");
            std::thread::sleep(Duration::from_millis(10));
        };
        let error = std::fs::read_to_string(stderr.path()).unwrap();
        assert_eq!(status.success(), outcome == "resume", "{error}");
        if outcome == "resume" {
            assert!(ws.vault().join("daily/news").is_dir());
            assert!(ws.vault().join(".lorekeeper/queue").is_dir());
        } else {
            assert!(!ws.vault().join("daily").exists());
            assert!(!ws.vault().join(".lorekeeper/queue").exists());
            if outcome == "cancel" {
                assert_eq!(status.signal(), Some(15));
            } else {
                assert!(error.contains("configuration changed"), "{error}");
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn publication_wait_is_inert_in_dry_run_and_refuses_nested_workflows() {
    let ws = Workspace::new();
    let _writer = lk_vault::MutationSession::acquire(&ws.vault()).unwrap();
    let workflow = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(ws.vault().join(".lorekeeper/workflow.lock"))
        .unwrap();
    workflow.lock().unwrap();
    let before = snapshot(&ws.vault());
    let preview = ws
        .command()
        .args([
            "--dry-run",
            "--wait-for-vault",
            "--publication-workflow",
            "wait",
        ])
        .output()
        .unwrap();
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    assert_eq!(snapshot(&ws.vault()), before);
    let nested = ws
        .command()
        .args(["--publication-workflow", "wait"])
        .env("LORE_WORKFLOW_ID", "0".repeat(32))
        .output()
        .unwrap();
    assert!(!nested.status.success());
    assert!(String::from_utf8_lossy(&nested.stderr).contains("cannot nest"));
    assert_eq!(snapshot(&ws.vault()), before);
}
