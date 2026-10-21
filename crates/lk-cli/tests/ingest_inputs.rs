use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn run(config: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lore"));
    for (key, _) in std::env::vars() {
        if key.starts_with("LORE_") {
            command.env_remove(key);
        }
    }
    command
        .arg("--config")
        .arg(config)
        .args(args)
        .output()
        .unwrap()
}

fn config(root: &Path, limit: usize, sources: &[&str]) -> PathBuf {
    let mut content = format!(
        "vault: {{root: vault, timezone: UTC, locale: en}}\nidentity: {{name: Test, email: test@example.invalid}}\nllm: {{provider: queue}}\ningest:\n  inputs: {{max_source_bytes: {limit}, max_run_bytes: {limit}}}\nsources:\n"
    );
    for source in sources {
        std::fs::create_dir_all(root.join("vault").join(source)).unwrap();
        content.push_str(&format!(
            "  {source}:\n    type: manual\n    params: {{inbox_dir: {source}, archive_after_ingest: false}}\n"
        ));
    }
    let path = root.join("config.yaml");
    std::fs::write(&path, content).unwrap();
    path
}

fn note(root: &Path, source: &str, name: &str, bytes: usize) {
    std::fs::write(
        root.join("vault").join(source).join(format!("{name}.md")),
        format!("# {name}\n\n{}\n", "x".repeat(bytes)),
    )
    .unwrap();
}

#[test]
fn rejected_source_releases_run_capacity_and_preserves_its_inbox() {
    let temp = tempfile::tempdir().unwrap();
    let config = config(temp.path(), 16 * 1024, &["bad", "good"]);
    for index in 0..5 {
        note(temp.path(), "bad", &format!("bad-{index}"), 4096);
    }
    note(temp.path(), "good", "good", 32);
    let output = run(&config, &["ingest"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("admission budget"), "{stderr}");
    let documents = temp.path().join("vault/wiki/documents");
    assert!(documents.join("good.md").exists(), "{stderr}");
    assert_eq!(std::fs::read_dir(documents).unwrap().count(), 1);
    assert_eq!(
        std::fs::read_dir(temp.path().join("vault/bad"))
            .unwrap()
            .count(),
        5
    );
    assert!(temp.path().join("vault/good/good.md").exists());
}

#[test]
fn run_capacity_refuses_a_later_large_source_and_still_admits_a_small_one() {
    let temp = tempfile::tempdir().unwrap();
    let config = config(temp.path(), 8192, &["a", "b", "c"]);
    note(temp.path(), "a", "first", 5000);
    note(temp.path(), "b", "second", 5000);
    note(temp.path(), "c", "last", 32);
    let output = run(&config, &["ingest"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("admission budget"), "{stderr}");
    let documents = temp.path().join("vault/wiki/documents");
    assert!(documents.join("first.md").exists(), "{stderr}");
    assert!(!documents.join("second.md").exists(), "{stderr}");
    assert!(documents.join("last.md").exists(), "{stderr}");
}

#[test]
fn repeated_full_ingest_gives_each_source_the_first_admission_turn() {
    let temp = tempfile::tempdir().unwrap();
    let config = config(temp.path(), 8192, &["a", "b"]);
    note(temp.path(), "a", "first", 5000);
    note(temp.path(), "b", "second", 5000);
    let documents = temp.path().join("vault/wiki/documents");

    let first = run(&config, &["ingest"]);
    let diagnostic = String::from_utf8_lossy(&first.stderr);
    assert!(!first.status.success(), "{diagnostic}");
    assert!(diagnostic.contains("admission budget"), "{diagnostic}");
    assert!(documents.join("first.md").exists(), "{diagnostic}");
    assert!(!documents.join("second.md").exists(), "{diagnostic}");

    let second = run(&config, &["ingest"]);
    let diagnostic = String::from_utf8_lossy(&second.stderr);
    assert!(!second.status.success(), "{diagnostic}");
    assert!(diagnostic.contains("admission budget"), "{diagnostic}");
    assert!(documents.join("second.md").exists(), "{diagnostic}");
    assert!(temp.path().join("vault/a/first.md").exists());
    assert!(temp.path().join("vault/b/second.md").exists());
}

#[tokio::test]
async fn first_admission_turn_also_rotates_when_collected_items_fit_but_planning_does_not() {
    let temp = tempfile::tempdir().unwrap();
    let path = config(temp.path(), 32 * 1024, &["a", "b"]);
    note(temp.path(), "a", "first", 5000);
    note(temp.path(), "b", "second", 5000);
    let loaded = lk_core::config::Config::load(&path).unwrap();
    let context = lk_source::ExtractContext {
        target_date: jiff::Timestamp::now()
            .to_zoned(jiff::tz::TimeZone::UTC)
            .date(),
        timezone: jiff::tz::TimeZone::UTC,
        locale: loaded.vault.locale(),
        identity: loaded.identity.clone(),
        vault_root: loaded.vault.root_path(),
        vault_dirs: loaded.vault.dirs.clone(),
        documents: None,
        projection_cache: None,
        originals: loaded.ingest.originals.clone(),
        max_input_bytes: loaded.ingest.inputs.max_source_bytes.get(),
    };
    let mut raw_bytes = 0;
    let mut event_bytes = 0;
    for id in ["a", "b"] {
        let mut captured = lk_source::capture_manual(&loaded.sources[id].params, &context)
            .await
            .unwrap();
        captured.project_links(Path::new("wiki/documents")).unwrap();
        raw_bytes += captured
            .items
            .iter()
            .map(|item| serde_json::to_vec(item).unwrap().len())
            .sum::<usize>();
        event_bytes += lk_pipeline::normalize_events(
            id,
            lk_core::config::SourceType::Manual,
            captured.items,
            &context.timezone,
        )
        .iter()
        .map(|event| serde_json::to_vec(event).unwrap().len())
        .sum::<usize>();
    }
    let limit = event_bytes - 1;
    assert!(raw_bytes <= limit);
    config(temp.path(), limit, &["a", "b"]);
    let documents = temp.path().join("vault/wiki/documents");
    for expected in ["first.md", "second.md"] {
        let output = run(&path, &["ingest"]);
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{diagnostic}");
        assert!(
            diagnostic.contains("pipeline: serialized input exceeds"),
            "{diagnostic}"
        );
        assert!(!diagnostic.contains(" extract:"), "{diagnostic}");
        assert!(documents.join(expected).exists(), "{diagnostic}");
    }
}

#[test]
fn preview_filtered_and_historical_ingest_preserve_the_full_run_turn() {
    let temp = tempfile::tempdir().unwrap();
    let config = config(temp.path(), 8192, &["a", "b"]);
    note(temp.path(), "a", "first", 5000);
    note(temp.path(), "b", "second", 5000);
    let cursor = temp.path().join("vault/.lorekeeper/ingest-cursor.json");
    std::fs::create_dir_all(cursor.parent().unwrap()).unwrap();
    let reserved = "{\"next_source\":\"b\"}";
    std::fs::write(&cursor, reserved).unwrap();

    let preview = run(&config, &["ingest", "--dry-run"]);
    let diagnostic = String::from_utf8_lossy(&preview.stderr);
    assert!(!preview.status.success(), "{diagnostic}");
    assert!(diagnostic.find("collect b").unwrap() < diagnostic.find("collect a").unwrap());
    assert_eq!(std::fs::read_to_string(&cursor).unwrap(), reserved);
    assert!(!temp.path().join("vault/wiki").exists());
    for args in [vec!["ingest", "a"], vec!["ingest", "--date", "2000-01-01"]] {
        let _ = run(&config, &args);
        assert_eq!(std::fs::read_to_string(&cursor).unwrap(), reserved);
    }

    let today = jiff::Timestamp::now()
        .to_zoned(jiff::tz::TimeZone::UTC)
        .date()
        .to_string();
    let _ = run(&config, &["ingest", "--date", &today]);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&std::fs::read(cursor).unwrap()).unwrap()["next_source"],
        "a"
    );
}

#[test]
fn malformed_ingest_cursor_is_not_reset_or_ignored_by_preview() {
    let temp = tempfile::tempdir().unwrap();
    let config = config(temp.path(), 8192, &["a", "b"]);
    note(temp.path(), "a", "first", 32);
    let cursor = temp.path().join("vault/.lorekeeper/ingest-cursor.json");
    std::fs::create_dir_all(cursor.parent().unwrap()).unwrap();
    std::fs::write(&cursor, "invalid").unwrap();
    for args in [vec!["ingest", "--dry-run"], vec!["ingest"]] {
        let output = run(&config, &args);
        assert!(!output.status.success());
        assert_eq!(std::fs::read_to_string(&cursor).unwrap(), "invalid");
        assert!(!temp.path().join("vault/wiki").exists());
    }
}

#[test]
fn rotating_first_admission_keeps_colliding_document_owners_distinct_and_stable() {
    let temp = tempfile::tempdir().unwrap();
    let config = config(temp.path(), 16 * 1024, &["a", "b"]);
    for (source, body) in [("a", "First owner"), ("b", "Second owner")] {
        std::fs::write(
            temp.path().join("vault").join(source).join("same.md"),
            format!("# Shared title\n\n{body}\n"),
        )
        .unwrap();
    }
    let cursor = temp.path().join("vault/.lorekeeper/ingest-cursor.json");
    std::fs::create_dir_all(cursor.parent().unwrap()).unwrap();
    std::fs::write(&cursor, "{\"next_source\":\"b\"}").unwrap();
    let snapshot = || {
        std::fs::read_dir(temp.path().join("vault/wiki/documents"))
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                let raw = std::fs::read_to_string(&path).unwrap();
                let parsed = lk_core::frontmatter::parse_page(&raw).unwrap();
                (
                    path.file_name().unwrap().to_owned(),
                    parsed
                        .frontmatter
                        .get("source_file")
                        .unwrap()
                        .as_str()
                        .unwrap()
                        .to_owned(),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    let first = run(&config, &["ingest"]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let owners = snapshot();
    assert_eq!(owners.len(), 2);
    assert!(owners[std::ffi::OsStr::new("shared-title.md")].ends_with("/b/same.md"));
    let second = run(&config, &["ingest"]);
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(snapshot(), owners);
}

#[test]
fn dry_run_capacity_rejection_does_not_publish_or_retire_input() {
    let temp = tempfile::tempdir().unwrap();
    let config = config(temp.path(), 4096, &["notes"]);
    note(temp.path(), "notes", "too-large", 5000);
    let output = run(&config, &["ingest", "--dry-run"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("admission budget"));
    assert!(temp.path().join("vault/notes/too-large.md").exists());
    assert!(!temp.path().join("vault/wiki").exists());
    assert!(!temp.path().join("vault/.lorekeeper").exists());
}
