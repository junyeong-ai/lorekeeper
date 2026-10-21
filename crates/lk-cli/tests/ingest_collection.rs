//! Acquisition budgets and the boundary between collection and page publication.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

struct Foreground(Child);

impl Drop for Foreground {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn command(config: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lore"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("LORE_") {
            command.env_remove(key);
        }
    }
    for key in [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "NO_PROXY",
        "no_proxy",
    ] {
        command.env_remove(key);
    }
    command.arg("--config").arg(config).arg("ingest");
    command
}

fn wait(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "ingest exceeded the test deadline"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn source_override_bounds_progressing_collection_without_holding_the_vault_writer() {
    let root = tempfile::tempdir().unwrap();
    let vault = root.path().join("vault");
    std::fs::create_dir_all(vault.join("inbox")).unwrap();
    std::fs::write(vault.join("inbox/note.md"), "# Note\n\nA healthy source.\n").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let (started, request) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut stream: TcpStream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "no source request arrived");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("accept source request: {error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut buffer = [0; 4096];
        let received = stream.read(&mut buffer).unwrap();
        assert!(received > 0, "source disconnected before sending a request");
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/rss+xml\r\nContent-Length: 100000\r\nConnection: close\r\n\r\n<rss version=\"2.0\">").unwrap();
        stream.flush().unwrap();
        started.send(()).unwrap();
        while Instant::now() < deadline && stream.write_all(b" ").is_ok() {
            std::thread::sleep(Duration::from_millis(20));
        }
    });
    let config = root.path().join("config.yaml");
    std::fs::write(&config, format!(
        "vault:\n  root: vault\n  locale: en\nidentity: {{name: Test, email: test@example.invalid}}\ningest: {{source_timeout_seconds: 60}}\nsources:\n  a-slow:\n    type: rss\n    collection_timeout_seconds: 1\n    params:\n      feeds: [{{id: slow, url: 'http://{address}/feed'}}]\n  z-notes:\n    type: manual\n    params: {{inbox_dir: inbox}}\n"
    )).unwrap();
    let stderr = tempfile::NamedTempFile::new().unwrap();
    let mut child = Foreground(
        command(&config)
            .stdout(Stdio::null())
            .stderr(stderr.reopen().unwrap())
            .spawn()
            .unwrap(),
    );
    request.recv_timeout(Duration::from_secs(5)).unwrap();
    let cursor: serde_json::Value = serde_json::from_slice(
        &std::fs::read(vault.join(".lorekeeper/ingest-cursor.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(cursor["next_source"], "z-notes");
    let held = lk_vault::MutationSession::acquire(&vault)
        .expect("network collection must not hold the vault writer");
    assert!(!vault.join("wiki/documents/note.md").exists());
    drop(held);
    let status = wait(&mut child.0);
    server.join().unwrap();
    let diagnostic = std::fs::read_to_string(stderr.path()).unwrap();
    assert!(!status.success(), "{diagnostic}");
    assert!(
        diagnostic.contains("1-second acquisition budget"),
        "{diagnostic}"
    );
    assert!(
        vault.join("wiki/documents/note.md").is_file(),
        "an independent healthy source must still publish: {diagnostic}"
    );
    let log = std::fs::read_to_string(vault.join(".lorekeeper/ingest.jsonl")).unwrap();
    let entries = log
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert!(
        entries
            .iter()
            .any(|entry| entry["source_id"] == "a-slow" && entry["status"] == "failed")
    );
    assert!(
        entries
            .iter()
            .any(|entry| entry["source_id"] == "z-notes" && entry["status"] == "success")
    );
}

fn assert_config_drift_while_waiting(filtered: bool) {
    let root = tempfile::tempdir().unwrap();
    let vault = root.path().join("vault");
    std::fs::create_dir_all(vault.join("inbox")).unwrap();
    std::fs::write(vault.join("inbox/note.md"), "# Note\n\nOriginal input.\n").unwrap();
    let config = root.path().join("config.yaml");
    let original = "vault: {root: vault}\nidentity: {name: Test, email: test@example.invalid}\nsources:\n  notes:\n    type: manual\n    params: {inbox_dir: inbox}\n";
    std::fs::write(&config, original).unwrap();
    let held = lk_vault::MutationSession::acquire(&vault).unwrap();
    let stderr = tempfile::NamedTempFile::new().unwrap();
    let mut collection = command(&config);
    if filtered {
        collection.arg("notes");
    }
    let mut child = Foreground(
        collection
            .arg("--wait-for-vault")
            .stdout(Stdio::null())
            .stderr(stderr.reopen().unwrap())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !std::fs::read_to_string(stderr.path())
        .unwrap()
        .contains("Waiting for exclusive vault access")
    {
        assert!(child.0.try_wait().unwrap().is_none());
        assert!(
            Instant::now() < deadline,
            "ingest did not reach its configured writer wait"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    std::fs::write(
        &config,
        format!("{original}ingest: {{source_timeout_seconds: 90}}\n"),
    )
    .unwrap();
    drop(held);
    assert!(!wait(&mut child.0).success());
    let diagnostic = std::fs::read_to_string(stderr.path()).unwrap();
    assert!(
        diagnostic.contains("selected configuration changed during collection"),
        "{diagnostic}"
    );
    assert!(vault.join("inbox/note.md").is_file());
    assert!(!vault.join("wiki/documents/note.md").exists());
    assert!(!vault.join(".lorekeeper/queue").exists());
    assert!(!vault.join(".lorekeeper/ingest-cursor.json").exists());
    if !filtered {
        assert!(!diagnostic.contains("collect notes"), "{diagnostic}");
    }
}

#[test]
fn configuration_drift_while_waiting_refuses_captured_observation_publication() {
    assert_config_drift_while_waiting(true);
}

#[test]
fn configuration_drift_while_reserving_refuses_acquisition_and_turn_publication() {
    assert_config_drift_while_waiting(false);
}
