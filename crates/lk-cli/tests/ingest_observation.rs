use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

struct LocalServer {
    base: String,
    stopped: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl LocalServer {
    fn new(handler: impl Fn(&url::Url) -> (&'static str, String) + Send + 'static) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server_base = base.clone();
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = stopped.clone();
        let worker = std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                        continue;
                    }
                    other => panic!("fixture accept failed: {other:?}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let mut chunk = [0; 1024];
                    let received = stream.read(&mut chunk).unwrap();
                    assert!(received > 0);
                    request.extend_from_slice(&chunk[..received]);
                    assert!(request.len() <= 16 * 1024);
                }
                let request = std::str::from_utf8(&request).unwrap();
                let target = request
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap();
                let url = url::Url::parse(&server_base).unwrap().join(target).unwrap();
                let (content_type, body) = handler(&url);
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        Self {
            base,
            stopped,
            worker: Some(worker),
        }
    }
}

impl Drop for LocalServer {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        if let Err(error) = self.worker.take().unwrap().join()
            && !std::thread::panicking()
        {
            std::panic::resume_unwind(error);
        }
    }
}

fn run(config: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lore"));
    for (key, _) in std::env::vars() {
        if key.starts_with("LORE_") {
            command.env_remove(key);
        }
    }
    command
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .arg("--config")
        .arg(config)
        .args(args)
        .output()
        .unwrap()
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn collect(root: &Path, dir: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        if !dir.exists() {
            return;
        }
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                collect(root, &path, files);
            } else {
                files.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    std::fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut files = BTreeMap::new();
    collect(root, root, &mut files);
    files
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn partial_daily_refetch_preserves_pages_queue_and_work_observations() {
    let temp = tempfile::TempDir::new().unwrap();
    let vault = temp.path().join("vault");
    let config = temp.path().join("config.yaml");
    let credentials = temp.path().join("credentials.json");
    let at = jiff::Timestamp::now() - jiff::SignedDuration::from_hours(1);
    let date = at.to_zoned(jiff::tz::TimeZone::UTC).date().to_string();
    let changed = Arc::new(AtomicBool::new(false));
    let has_changed = changed.clone();
    let server = LocalServer::new(move |url| {
        let result = match url.path() {
            "/rest/api/2/myself" => serde_json::json!({"name": "fixture-user"}),
            "/rest/api/2/search" => {
                let status = if has_changed.load(Ordering::Relaxed) {
                    "done"
                } else {
                    "indeterminate"
                };
                let issues: Vec<_> = ["TEST-1", "TEST-2"]
                    .into_iter()
                    .map(|key| {
                        serde_json::json!({
                            "key": key,
                            "fields": {
                                "summary": format!("Work {key}"), "updated": at.to_string(),
                                "status": {"name": status, "statusCategory": {"key": status}},
                                "assignee": {"name": "fixture-user"},
                            },
                        })
                    })
                    .collect();
                serde_json::json!({"issues": issues, "total": 2})
            }
            other => panic!("unexpected Jira fixture path {other}"),
        };
        ("application/json", result.to_string())
    });
    let mut creds = lk_source::credentials::Credentials::default();
    creds.atlassian.insert(
        "fixture".into(),
        lk_source::credentials::AtlassianCredentials {
            site_url: server.base.clone(),
            auth: lk_source::credentials::AtlassianAuthMethod::PersonalAccessToken {
                token: "fixture-token".into(),
            },
        },
    );
    lk_source::credentials::CredentialStore::new(&credentials)
        .unwrap()
        .lock()
        .unwrap()
        .save(&creds)
        .unwrap();
    let write_config = |cap| {
        std::fs::write(&config, format!(
        "vault:\n  root: {}\n  timezone: UTC\nidentity:\n  name: Test\n  email: test@example.invalid\ncredentials:\n  file: {}\npersonal:\n  tasks: {{}}\nsources:\n  issues:\n    type: jira\n    instance: fixture\n    concept_policy: supported-new\n    params:\n      jql: 'project = TEST'\n      max_issues: {cap}\n  tasks:\n    type: tasks\n    enabled: false\n", vault.display(), credentials.display()
    )).unwrap()
    };
    write_config(10);
    assert_success(&run(&config, &["ingest", "issues", "--date", &date]));
    let daily = vault.join(format!("daily/issues/{date}.md"));
    let before_page = std::fs::read(&daily).unwrap();
    let before_queue = snapshot(&vault.join(".lorekeeper/queue"));
    let before_work = snapshot(&vault.join(".lorekeeper/proposals"));
    assert!(!before_queue.is_empty());
    assert!(!before_work.is_empty());
    assert_eq!(
        lk_task::Candidates::new(&vault)
            .read_all(&["issues".into()])
            .unwrap()
            .candidates
            .len(),
        2
    );
    let parsed =
        lk_core::frontmatter::parse_page(std::str::from_utf8(&before_page).unwrap()).unwrap();
    assert_eq!(
        parsed
            .frontmatter
            .get("event_count")
            .and_then(|value| value.as_u64()),
        Some(2)
    );

    changed.store(true, Ordering::Relaxed);
    write_config(1);
    let output = run(&config, &["ingest", "issues", "--date", &date]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot replace a daily snapshot"));
    assert_eq!(std::fs::read(&daily).unwrap(), before_page);
    assert_eq!(snapshot(&vault.join(".lorekeeper/queue")), before_queue);
    assert_eq!(snapshot(&vault.join(".lorekeeper/proposals")), before_work);
    assert_eq!(
        lk_task::Candidates::new(&vault)
            .read_all(&["issues".into()])
            .unwrap()
            .candidates
            .len(),
        2
    );
    let entries = std::fs::read_to_string(vault.join(".lorekeeper/ingest.jsonl")).unwrap();
    let last: serde_json::Value = serde_json::from_str(entries.lines().last().unwrap()).unwrap();
    assert_eq!(last["status"], "failed");
    assert_eq!(last["event_count"], 0);
    let state: serde_json::Value = serde_json::from_slice(
        &std::fs::read(vault.join(".lorekeeper/periods/state.json")).unwrap(),
    )
    .unwrap();
    let observed = &state["batches"][0]["sources"][0];
    assert_eq!(observed["source_id"], "issues");
    assert_eq!(observed["outcome"], "partial");
    assert_eq!(observed["scope"]["kind"], "configured-query");

    let before = snapshot(&vault);
    let dry_run = run(&config, &["ingest", "issues", "--date", &date, "--dry-run"]);
    assert!(!dry_run.status.success());
    assert_eq!(snapshot(&vault), before);
}

#[test]
fn partial_streaming_refetch_accumulates_new_events_without_dropping_retained_ones() {
    let temp = tempfile::TempDir::new().unwrap();
    let vault = temp.path().join("vault");
    let config = temp.path().join("config.yaml");
    let at = jiff::Timestamp::now() - jiff::SignedDuration::from_hours(1);
    let date = at.to_zoned(jiff::tz::TimeZone::UTC).date().to_string();
    let changed = Arc::new(AtomicBool::new(false));
    let has_changed = changed.clone();
    let server = LocalServer::new(move |url| {
        assert_eq!(url.path(), "/feed.xml");
        let ids = if has_changed.load(Ordering::Relaxed) {
            vec!["new", "old-a", "old-b"]
        } else {
            vec!["old-a", "old-b"]
        };
        let entries = ids.into_iter().map(|id| format!("<entry><id>{id}</id><title>{id}</title><updated>{at}</updated><summary>{id} evidence</summary></entry>")).collect::<String>();
        (
            "application/atom+xml",
            format!(
                "<feed xmlns=\"http://www.w3.org/2005/Atom\"><id>feed</id><title>Feed</title><updated>{at}</updated>{entries}</feed>"
            ),
        )
    });
    let write_config = |cap| {
        std::fs::write(&config, format!(
        "vault:\n  root: {}\n  timezone: UTC\nidentity:\n  name: Test\n  email: test@example.invalid\nsources:\n  feed:\n    type: rss\n    params:\n      max_items_per_feed: {cap}\n      feeds:\n        - id: fixture\n          url: {}/feed.xml\n", vault.display(), server.base
    )).unwrap()
    };
    write_config(10);
    assert_success(&run(&config, &["ingest", "feed", "--date", &date]));
    changed.store(true, Ordering::Relaxed);
    write_config(1);
    let output = run(&config, &["ingest", "feed", "--date", &date]);
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("cannot replace a daily snapshot"));
    let path = PathBuf::from(format!("daily/feed/{date}.md"));
    let text = std::fs::read_to_string(vault.join(&path)).unwrap();
    let parsed = lk_core::frontmatter::parse_page(&text).unwrap();
    assert_eq!(
        parsed
            .frontmatter
            .get("event_count")
            .and_then(|value| value.as_u64()),
        Some(3)
    );
    let original = lk_vault::inspect_rss_original(&vault, &path, &Default::default())
        .unwrap()
        .unwrap();
    assert_eq!(original.total_events, 3);
    for id in ["new", "old-a", "old-b"] {
        assert!(text.contains(&format!("{id} evidence")));
    }
}

#[test]
fn partial_document_capture_publishes_readable_documents_and_records_its_limits() {
    let temp = tempfile::TempDir::new().unwrap();
    let vault = temp.path().join("vault");
    let inbox = vault.join("inbox");
    std::fs::create_dir_all(&inbox).unwrap();
    std::fs::write(
        inbox.join("good.md"),
        "# Good evidence\n\nRetain this knowledge.\n",
    )
    .unwrap();
    std::fs::write(
        inbox.join("bad.md"),
        "# Missing attachment\n\n![figure](missing.png)\n",
    )
    .unwrap();
    let config = temp.path().join("config.yaml");
    std::fs::write(&config, format!(
        "vault:\n  root: {}\n  timezone: UTC\nidentity:\n  name: Test\n  email: test@example.invalid\nsources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: inbox\n", vault.display()
    )).unwrap();
    let output = run(&config, &["ingest", "notes"]);
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("cannot replace a daily snapshot"));
    let documents = snapshot(&vault.join("wiki/documents"));
    assert_eq!(documents.len(), 1);
    assert!(
        std::str::from_utf8(documents.values().next().unwrap())
            .unwrap()
            .contains("Retain this knowledge.")
    );
    assert!(!snapshot(&vault.join(".lorekeeper/queue")).is_empty());
    assert!(inbox.join("good.md").is_file());
    assert!(inbox.join("bad.md").is_file());
    let entries = std::fs::read_to_string(vault.join(".lorekeeper/ingest.jsonl")).unwrap();
    let last: serde_json::Value = serde_json::from_str(entries.lines().last().unwrap()).unwrap();
    assert_eq!(last["status"], "failed");
    assert_eq!(last["event_count"], 1);
    let state: serde_json::Value = serde_json::from_slice(
        &std::fs::read(vault.join(".lorekeeper/periods/state.json")).unwrap(),
    )
    .unwrap();
    let observed = &state["batches"][0]["sources"][0];
    assert_eq!(observed["source_id"], "notes");
    assert_eq!(observed["outcome"], "partial");
    assert_eq!(observed["scope"]["kind"], "unverified");
}
