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
