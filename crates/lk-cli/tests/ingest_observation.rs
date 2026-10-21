use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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
fn source_recovery_binds_observed_site_and_principal_instead_of_rotating_credentials() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("vault");
    let config = temp.path().join("config.yaml");
    let credentials = temp.path().join("credentials.json");
    let principal = Arc::new(AtomicUsize::new(1));
    let searches = Arc::new(AtomicUsize::new(0));
    let make_server = || {
        let principal = principal.clone();
        let searches = searches.clone();
        LocalServer::new(move |url| {
            let body = match url.path() {
                "/rest/api/user/current" => match principal.load(Ordering::Relaxed) {
                    0 => serde_json::json!({}),
                    id => serde_json::json!({"username": format!("principal-{id}")}),
                },
                "/rest/api/content/search" => {
                    searches.fetch_add(1, Ordering::Relaxed);
                    serde_json::json!({"results": []})
                }
                path => panic!("unexpected identity fixture path {path}"),
            };
            ("application/json", body.to_string())
        })
    };
    let old_site = make_server();
    let new_site = make_server();
    let day = jiff::Timestamp::now()
        .to_zoned(jiff::tz::TimeZone::UTC)
        .date()
        .yesterday()
        .unwrap();
    std::fs::write(&config, format!(
        "vault: {{root: {}, timezone: UTC, locale: en}}\nidentity: {{name: Test, email: test@example.invalid}}\ncredentials: {{file: {}}}\nllm: {{provider: noop}}\ningest:\n  recovery: {{start_date: '{day}', max_days_per_source_per_run: 1}}\nsources:\n  docs:\n    type: confluence\n    instance: fixture\n    params: {{cql: 'type = page'}}\n",
        root.display(), credentials.display()
    )).unwrap();
    let change_credentials = |site: &str, token: &str| {
        let mut creds = lk_source::credentials::Credentials::default();
        creds.atlassian.insert(
            "fixture".into(),
            lk_source::credentials::AtlassianCredentials {
                site_url: site.into(),
                auth: lk_source::credentials::AtlassianAuthMethod::PersonalAccessToken {
                    token: token.into(),
                },
            },
        );
        lk_source::credentials::CredentialStore::new(&credentials)
            .unwrap()
            .lock()
            .unwrap()
            .save(&creds)
            .unwrap();
    };
    change_credentials(&old_site.base, "first-fixture-token");
    assert_success(&run(
        &config,
        &["ingest", "docs", "--date", &day.to_string()],
    ));
    let state = root.join(".lorekeeper/periods/state.json");
    let before = std::fs::read(&state).unwrap();
    let receipt: serde_json::Value = serde_json::from_slice(&before).unwrap();
    assert_eq!(receipt["version"], 2);
    assert_eq!(
        receipt["batches"][0]["sources"][0]["collection_identity"]["authority"],
        old_site.base
    );
    let search_count = searches.load(Ordering::Relaxed);
    let preview = |expected: usize| {
        let output = run(&config, &["ingest", "--recover", "--dry-run"]);
        assert_success(&output);
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        assert!(
            diagnostic.contains(&format!("{expected} source-day selections")),
            "{diagnostic}"
        );
        assert_eq!(std::fs::read(&state).unwrap(), before);
        assert_eq!(searches.load(Ordering::Relaxed), search_count);
    };
    preview(0);
    change_credentials(&old_site.base, "rotated-fixture-token");
    preview(0);
    change_credentials(&new_site.base, "rotated-fixture-token");
    preview(1);
    change_credentials(&old_site.base, "rotated-fixture-token");
    principal.store(2, Ordering::Relaxed);
    preview(1);

    principal.store(0, Ordering::Relaxed);
    let original_config = std::fs::read_to_string(&config).unwrap();
    std::fs::write(
        &config,
        format!("{original_config}  tasks: {{type: tasks}}\npersonal: {{tasks: {{}}}}\n"),
    )
    .unwrap();
    let failed = run(&config, &["ingest", "--recover", "--dry-run"]);
    assert!(!failed.status.success());
    let diagnostic = String::from_utf8_lossy(&failed.stderr);
    assert!(
        diagnostic.contains("collection identity unavailable"),
        "{diagnostic}"
    );
    assert!(
        diagnostic.contains("[recovery] tasks") && diagnostic.contains("would collect"),
        "{diagnostic}"
    );
    assert_eq!(std::fs::read(&state).unwrap(), before);
    assert_eq!(searches.load(Ordering::Relaxed), search_count);
}

#[test]
fn source_recovery_refuses_a_different_collection_than_its_planned_identity() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("vault");
    let config = temp.path().join("config.yaml");
    let credentials = temp.path().join("credentials.json");
    let lookups = Arc::new(AtomicUsize::new(0));
    let identity_lookups = lookups.clone();
    let server = LocalServer::new(move |url| {
        let body = match url.path() {
            "/rest/api/user/current" => {
                let principal = if identity_lookups.fetch_add(1, Ordering::Relaxed) == 0 {
                    "planned-account"
                } else {
                    "collected-account"
                };
                serde_json::json!({"username": principal})
            }
            "/rest/api/content/search" => serde_json::json!({"results": []}),
            path => panic!("unexpected identity fixture path {path}"),
        };
        ("application/json", body.to_string())
    });
    let day = jiff::Timestamp::now()
        .to_zoned(jiff::tz::TimeZone::UTC)
        .date()
        .yesterday()
        .unwrap();
    let original = root.join(format!("daily/docs/{day}.md"));
    std::fs::create_dir_all(original.parent().unwrap()).unwrap();
    let before = "---\ntype: daily\nsource: docs\n---\n## Summary\nRetained original knowledge.\n";
    std::fs::write(&original, before).unwrap();
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
    std::fs::write(&config, format!(
        "vault: {{root: {}, timezone: UTC}}\nidentity: {{name: Test, email: test@example.invalid}}\ncredentials: {{file: {}}}\ningest:\n  recovery: {{start_date: '{day}', max_days_per_source_per_run: 1}}\nsources:\n  docs: {{type: confluence, instance: fixture, params: {{cql: 'type = page'}}}}\n",
        root.display(), credentials.display()
    )).unwrap();
    let output = run(&config, &["ingest", "--recover"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("collection identity changed"));
    assert_eq!(std::fs::read_to_string(&original).unwrap(), before);
    assert!(!root.join(".lorekeeper/queue").exists());
    let receipts: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.join(".lorekeeper/periods/state.json")).unwrap(),
    )
    .unwrap();
    assert!(receipts["batches"].as_array().unwrap().iter().all(|batch| {
        batch["sources"].as_array().unwrap().iter().all(|source| {
            source.get("collection_identity").is_none() && source["outcome"] == "failed"
        })
    }));
}

#[test]
fn disabled_extraction_retains_document_and_rss_evidence_until_reactivated() {
    use lk_core::rss_evidence::{
        ConceptEvidence, RSS_EVIDENCE_SCHEMA, RssEventSelection, bindings,
    };
    for rss in [false, true] {
        let temp = tempfile::TempDir::new().unwrap();
        let vault = temp.path().join("vault");
        let config = temp.path().join("config.yaml");
        let at = jiff::Timestamp::now() - jiff::SignedDuration::from_hours(1);
        let date = at.to_zoned(jiff::tz::TimeZone::UTC).date().to_string();
        let server = LocalServer::new(move |url| {
            assert_eq!(url.path(), "/feed.xml");
            (
                "application/atom+xml",
                format!(
                    "<feed xmlns=\"http://www.w3.org/2005/Atom\"><id>feed</id><title>Feed</title><updated>{at}</updated><entry><id>observation</id><title>Observation</title><updated>{at}</updated><summary>Recorded evidence.</summary></entry></feed>"
                ),
            )
        });
        let params = if rss {
            format!(
                "feeds:\n        - id: fixture\n          url: {}/feed.xml\n          fetch_full_text: false",
                server.base
            )
        } else {
            std::fs::create_dir_all(vault.join("inbox")).unwrap();
            std::fs::write(
                vault.join("inbox/observation.md"),
                "# Observation\n\nRecorded evidence.\n",
            )
            .unwrap();
            "inbox_dir: inbox\n      archive_after_ingest: false".into()
        };
        let write_config = |policy| {
            std::fs::write(
                &config,
                format!(
                    "vault:\n  root: {}\n  timezone: UTC\n  locale: en\nidentity: {{name: Test, email: test@example.invalid}}\nllm: {{provider: queue}}\nsources:\n  observations:\n    type: {}\n    concept_policy: {policy}\n    params:\n      {params}\n",
                    vault.display(),
                    if rss { "rss" } else { "manual" },
                ),
            )
            .unwrap();
        };
        let ingest = || {
            assert_success(&run(&config, &["ingest", "observations", "--date", &date]));
        };
        let extraction_statuses = || {
            let output = run(&config, &["queue", "status", "--json"]);
            assert_success(&output);
            let status: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            status["data"]["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|task| task["kind"] == "extract-concepts")
                .map(|task| task["status"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        write_config("disabled");
        ingest();
        let origin = if rss {
            PathBuf::from(format!("daily/observations/{date}.md"))
        } else {
            let documents = snapshot(&vault.join("wiki/documents"));
            assert_eq!(documents.len(), 1);
            PathBuf::from("wiki/documents").join(documents.keys().next().unwrap())
        };
        let read = || std::fs::read_to_string(vault.join(&origin)).unwrap();
        assert!(lk_vault::resolve_section(&read(), |s| s.related_concepts).is_none());
        assert!(extraction_statuses().is_empty());

        write_config("supported-new");
        ingest();
        assert_eq!(extraction_statuses(), ["current"]);
        let concept = Path::new("wiki/concepts/stored.md");
        std::fs::create_dir_all(vault.join(concept).parent().unwrap()).unwrap();
        std::fs::write(
            vault.join(concept),
            "---\ntype: concept\ntitle: Stored\nsource_count: 0\n---\n## Synthesis\nEstablished scope.\n\n## Sources\n\n## Related\n",
        )
        .unwrap();
        let retained = format!(
            "\n- {}\n\nRetained context, not renewed approval.\n\n",
            lk_core::link::md_link("Stored", &lk_core::link::relative_dest(&origin, concept))
        );
        let mut raw = read();
        let section = lk_vault::resolve_section(&raw, |s| s.related_concepts).unwrap();
        let start = section.body.as_ptr() as usize - raw.as_ptr() as usize;
        let end = start + section.body.len();
        raw.replace_range(start..end, &retained);
        if rss {
            let page = lk_vault::parse_page(&raw).unwrap();
            let reference = serde_json::from_value(
                page.frontmatter.get("source_record").unwrap()["rss_original"].clone(),
            )
            .unwrap();
            let manifest = lk_vault::load_rss_manifest(&vault, &reference).unwrap();
            raw = lk_vault::write_bindings(
                &raw,
                &[ConceptEvidence {
                    schema: RSS_EVIDENCE_SCHEMA.into(),
                    concept: concept.to_string_lossy().into(),
                    manifest: reference,
                    events: vec![RssEventSelection {
                        event_id: manifest.events[0].event_id.clone(),
                        revision: manifest.events[0].revision.clone(),
                    }],
                }],
            )
            .unwrap();
        }
        let retained_bindings = bindings(&lk_vault::parse_page(&raw).unwrap()).unwrap();
        std::fs::write(vault.join(&origin), raw).unwrap();
        let reconcile = || {
            assert_success(&run(&config, &["graph", "backlinks-sync"]));
            let raw = std::fs::read_to_string(vault.join(concept)).unwrap();
            let page = lk_vault::parse_page(&raw).unwrap();
            assert_eq!(page.frontmatter.get("source_count").unwrap(), 1);
        };
        reconcile();

        write_config("disabled");
        let before_page = std::fs::read(vault.join(&origin)).unwrap();
        let before_queue = snapshot(&vault.join(".lorekeeper/queue"));
        let templates = temp.path().join("templates");
        std::fs::create_dir_all(&templates).unwrap();
        for headings in ["", "## Related Concepts\n\n## Related Concepts\n\n"] {
            std::fs::write(
                templates.join(if rss {
                    "observations.md.jinja"
                } else {
                    "document.md.jinja"
                }),
                format!(
                    "---\ntype: {}\n---\n## Summary\n\n{headings}",
                    if rss { "daily" } else { "document" }
                ),
            )
            .unwrap();
            let output = run(
                &config,
                &[
                    "--template-dir",
                    templates.to_str().unwrap(),
                    "ingest",
                    "observations",
                    "--date",
                    &date,
                ],
            );
            assert!(!output.status.success());
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .contains("template must contain one related-concepts heading"),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(std::fs::read(vault.join(&origin)).unwrap(), before_page);
            assert_eq!(snapshot(&vault.join(".lorekeeper/queue")), before_queue);
        }
        ingest();
        let disabled = read();
        assert_eq!(
            lk_vault::resolve_section(&disabled, |s| s.related_concepts)
                .unwrap()
                .body,
            retained
        );
        let page = lk_vault::parse_page(&disabled).unwrap();
        assert_eq!(bindings(&page).unwrap(), retained_bindings);
        let inputs = page.frontmatter.get("llm_inputs").unwrap();
        assert!(inputs.get("concepts").is_none());
        assert!(inputs.get("concepts_done").is_none());
        assert_eq!(extraction_statuses(), ["stale"]);
        reconcile();
        if rss {
            let citations = lk_vault::rss_concept_citations(&vault, &origin, &disabled).unwrap();
            assert_eq!(citations["wiki/concepts/stored.md"].len(), 1);
        }

        write_config("supported-new");
        ingest();
        assert!(
            extraction_statuses()
                .iter()
                .any(|status| status == "current")
        );
        let reenabled = read();
        let page = lk_vault::parse_page(&reenabled).unwrap();
        assert_eq!(bindings(&page).unwrap(), retained_bindings);
        assert!(
            page.frontmatter
                .get("llm_inputs")
                .unwrap()
                .get("concepts")
                .is_some()
        );
        assert!(
            page.frontmatter
                .get("llm_inputs")
                .unwrap()
                .get("concepts_done")
                .is_none()
        );
        reconcile();
    }
}

#[test]
fn incomplete_confluence_projection_preserves_published_pages_and_requests() {
    let temp = tempfile::TempDir::new().unwrap();
    let vault = temp.path().join("vault");
    let config = temp.path().join("config.yaml");
    let credentials = temp.path().join("credentials.json");
    let at = jiff::Timestamp::now();
    let date = at.to_zoned(jiff::tz::TimeZone::UTC).date().to_string();
    let response_kind = Arc::new(AtomicUsize::new(0));
    let selected_kind = response_kind.clone();
    let server = LocalServer::new(move |url| {
        let result = match url.path() {
            "/rest/api/user/current" => serde_json::json!({"username": "fixture-user"}),
            "/rest/api/content/search" => {
                let mut page = serde_json::json!({
                    "id": "policy", "title": "Retention policy",
                    "version": {"number": 7, "when": at.to_string(),
                        "by": {"username": "fixture-user"}},
                    "body": {"storage": {"value": "<p>Keep records for 7 days.</p>"}},
                });
                match selected_kind.load(Ordering::Relaxed) {
                    1 => {
                        page.as_object_mut().unwrap().remove("body");
                    }
                    2 => {
                        page["version"].as_object_mut().unwrap().remove("number");
                    }
                    3 => page["body"]["storage"]["value"] = serde_json::json!(""),
                    4 => {
                        page["version"].as_object_mut().unwrap().remove("by");
                    }
                    5 => page["version"]["by"] = serde_json::json!({"displayName": "fixture-user"}),
                    _ => {}
                }
                serde_json::json!({"results": [page, {
                    "id": "stable", "title": "Stable policy",
                    "version": {"number": 1, "when": at.to_string(),
                        "by": {"username": "fixture-user"}},
                    "body": {"storage": {"value": "<p>Unchanged second observation.</p>"}},
                }]})
            }
            other => panic!("unexpected Confluence fixture path {other}"),
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
    std::fs::write(&config, format!(
        "vault:\n  root: {}\n  timezone: UTC\n  locale: en\nidentity: {{name: Test, email: test@example.invalid}}\ncredentials:\n  file: {}\nsources:\n  docs:\n    type: confluence\n    instance: fixture\n    params: {{cql: 'type = page'}}\n",
        vault.display(), credentials.display()
    )).unwrap();
    assert_success(&run(&config, &["ingest", "docs", "--date", &date]));
    let daily = vault.join(format!("daily/docs/{date}.md"));
    let before_page = std::fs::read(&daily).unwrap();
    let before_queue = snapshot(&vault.join(".lorekeeper/queue"));
    assert!(String::from_utf8_lossy(&before_page).contains("Keep records for 7 days."));
    assert!(!before_queue.is_empty());
    for kind in [1, 2, 4, 5] {
        response_kind.store(kind, Ordering::Relaxed);
        let output = run(&config, &["ingest", "docs", "--date", &date]);
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "kind {kind}: {diagnostic}");
        assert!(
            diagnostic.contains("cannot replace a daily snapshot"),
            "{diagnostic}"
        );
        assert_eq!(std::fs::read(&daily).unwrap(), before_page);
        assert_eq!(snapshot(&vault.join(".lorekeeper/queue")), before_queue);
        let log = std::fs::read_to_string(vault.join(".lorekeeper/ingest.jsonl")).unwrap();
        let last: serde_json::Value = serde_json::from_str(log.lines().last().unwrap()).unwrap();
        assert_eq!(last["status"], "failed");
    }
    response_kind.store(3, Ordering::Relaxed);
    assert_success(&run(&config, &["ingest", "docs", "--date", &date]));
    assert!(
        !std::fs::read_to_string(&daily)
            .unwrap()
            .contains("Keep records for 7 days.")
    );
}

#[test]
fn partial_daily_refetch_preserves_pages_queue_and_work_observations() {
    let temp = tempfile::TempDir::new().unwrap();
    let vault = temp.path().join("vault");
    let config = temp.path().join("config.yaml");
    let credentials = temp.path().join("credentials.json");
    let at = jiff::Timestamp::now();
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
    let batches = state["batches"].as_array().unwrap();
    assert!(batches.iter().any(|batch| {
        batch["sources"][0]["source_id"] == "issues" && batch["sources"][0]["outcome"] == "observed"
    }));
    let observed = &batches.last().unwrap()["sources"][0];
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
