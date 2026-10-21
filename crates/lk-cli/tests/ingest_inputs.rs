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
