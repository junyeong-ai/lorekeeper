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

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_lore"));
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("LORE_") {
                command.env_remove(key);
            }
        }
        command
            .arg("--config")
            .arg(self.root.path().join("config.yaml"))
            .arg("ingest");
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
fn explicit_ingest_waits_then_collects_after_the_writer_releases() {
    let ws = Workspace::new();
    let held = lk_vault::MutationSession::acquire(&ws.vault()).unwrap();
    let before = snapshot(&ws.vault());
    let stderr = tempfile::NamedTempFile::new().unwrap();
    let mut child = ws
        .command()
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
    assert!(ws.vault().join("wiki/documents/note.md").is_file());
}
