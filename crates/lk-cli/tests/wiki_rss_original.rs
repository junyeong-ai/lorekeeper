//! RSS evidence discovery stays bounded and independent of mutable feed configuration.
use std::process::Command;
#[test]
fn original_cli_pins_rss_event_body_without_changing_manual_shape() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    let config = root.join("config.yaml");
    std::fs::write(&config,format!("vault:\n  root: {}\nidentity:\n  name: Test\n  email: test@example.com\nsources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: inbox\n",root.display())).unwrap();
    let e:lk_core::event::Event=serde_json::from_value(serde_json::json!({"id":"feed:2026-10-18:0000000000000001","source_id":"feed","source_type":"rss","timestamp":"2026-10-18T00:00:00Z","date":"2026-10-18","title":"Entry","body":"원문 mechanism","url":"https://example.com","author":null,"labels":[],"category":null,"performance_category":null,"is_self":false,"is_personal":false,"metadata":{}})).unwrap();
    let session = lk_vault::MutationSession::acquire(root).unwrap();
    let capture = lk_core::rss_original::RssOriginalCapture::from_events(&[&e]).unwrap();
    let reference = lk_vault::publish_rss_original(&session, &capture).unwrap();
    drop(session);
    std::fs::write(root.join("daily.md"),format!("---\ntype: daily\nsource: feed\ncreated: 2026-10-18\nsource_record: {}\n---\nAbridged.\n",serde_json::json!({"rss_original":reference}))).unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_lore"))
            .arg("--config")
            .arg(&config)
            .args(args)
            .output()
            .unwrap()
    };
    let out = run(&["wiki", "original", "daily.md", "--limit", "1"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let view: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(view["authority"], "page_bound_collection");
    assert!(view["selected"].is_null());
    let id = view["events"][0]["event_id"].as_str().unwrap();
    let revision = view["revision"].as_str().unwrap();
    assert!(
        !run(&["wiki", "original", "daily.md", "--event", id])
            .status
            .success()
    );
    let out = run(&[
        "wiki",
        "original",
        "daily.md",
        "--event",
        id,
        "--revision",
        revision,
        "--text-offset",
        "8",
        "--text-limit",
        "2",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let selected: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(selected["selected"]["text"], "문 ");
    std::fs::write(
        root.join("manual.md"),
        "---\ntype: document\n---\nLegacy manual.\n",
    )
    .unwrap();
    let out = run(&["wiki", "original", "manual.md"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let manual: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(manual["available"], false);
    assert!(manual.get("kind").is_none());
}

fn local_feed(body: String) -> (String, std::thread::JoinHandle<()>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/feed.xml", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let worker = std::thread::spawn(move || {
        let started = std::time::Instant::now();
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        && started.elapsed() < std::time::Duration::from_secs(10) =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(10))
                }
                other => panic!("local feed was not requested: {other:?}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let mut chunk = [0u8; 1024];
            let received = stream.read(&mut chunk).unwrap();
            assert!(received > 0, "request ended before headers");
            request.extend_from_slice(&chunk[..received]);
            assert!(request.len() <= 8192, "test request header limit");
        }
        write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/atom+xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
    });
    (url, worker)
}

#[test]
fn rss_ingest_preserves_original_before_page_and_dry_run_stores_nothing() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("vault");
    std::fs::create_dir(&root).unwrap();
    let timestamp = jiff::Timestamp::now();
    let feed = format!(
        r#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom"><title>Feed</title><id>feed</id><updated>{timestamp}</updated><entry><id>stable-item</id><title>Collected title</title><updated>{timestamp}</updated><summary type="text">original mechanism and limitation</summary><link href="https://example.com/item"/></entry></feed>"#
    );
    let config = tmp.path().join("config.yaml");
    let ingest = |dry_run: bool| {
        let (url, server) = local_feed(feed.clone());
        std::fs::write(&config,format!("vault:\n  root: {}\n  timezone: UTC\nidentity:\n  name: Test\n  email: test@example.com\nsources:\n  feed:\n    type: rss\n    params:\n      feeds:\n        - id: stable-feed\n          url: {url}\n",root.display())).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_lore"));
        for (key, _) in std::env::vars() {
            if key.starts_with("LORE_") {
                command.env_remove(key);
            }
        }
        command
            .arg("--config")
            .arg(&config)
            .args(["ingest", "feed"]);
        if dry_run {
            command.arg("--dry-run");
        }
        let out = command.output().unwrap();
        server.join().unwrap();
        out
    };
    let out = ingest(true);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read_dir(&root).unwrap().count(),
        0,
        "dry-run has no vault writes"
    );
    let out = ingest(false);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let date = timestamp.to_zoned(jiff::tz::TimeZone::UTC).date();
    let page = std::path::PathBuf::from(format!("daily/feed/{date}.md"));
    let original = lk_vault::inspect_rss_original(&root, &page, &Default::default())
        .unwrap()
        .unwrap();
    assert_eq!(original.authority, "page_bound_collection");
    assert_eq!(original.total_events, 1);
    let reference = original.reference.unwrap();
    let manifest: lk_core::rss_original::RssOriginalManifest =
        serde_json::from_slice(&std::fs::read(root.join(reference.manifest_path)).unwrap())
            .unwrap();
    let object = root.join(format!(
        "{}/events/{}.json",
        lk_core::rss_original::RSS_ORIGINAL_STORE,
        manifest.events[0].revision
    ));
    let payload = lk_core::rss_original::verify_payload(
        &manifest,
        &manifest.events[0],
        &std::fs::read(&object).unwrap(),
    )
    .unwrap();
    assert!(payload.body.contains("original mechanism and limitation"));
    assert_eq!(payload.metadata["feed_id"], "stable-feed");
    assert_eq!(
        payload.event_timestamp.as_millisecond(),
        timestamp.as_millisecond()
    );
    assert!(payload.captured_at.is_none());
    let previous_page = std::fs::read(root.join(&page)).unwrap();
    std::fs::write(&object, b"corrupt").unwrap();
    let out = ingest(false);
    assert!(!out.status.success(), "occupied original must fail ingest");
    assert_eq!(
        std::fs::read(root.join(&page)).unwrap(),
        previous_page,
        "failed original publication preserves the previous page"
    );
    assert_eq!(
        std::fs::read(object).unwrap(),
        b"corrupt",
        "occupied address is never overwritten"
    );
}
