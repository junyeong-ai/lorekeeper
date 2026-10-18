//! The native renderer separates fixture jobs from the default launchd installation.
use std::path::Path;
use std::process::{Command, Output};

fn workspace() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("config.yaml"),
        "vault:\n  root: vault\nidentity:\n  name: Test\n  email: test@example.invalid\nsources:\n  notes:\n    type: manual\n    params: {inbox_dir: inbox}\ningest:\n  schedule: '0 9 * * *'\n",
    ).unwrap();
    root
}

fn render(root: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lore"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("LORE_") {
            command.env_remove(key);
        }
    }
    command
        .current_dir(root)
        .args(["schedule", "--bin"])
        .arg(env!("CARGO_BIN_EXE_lore"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn custom_launchd_namespace_and_xml_log_path_do_not_reuse_default_identity() {
    let root = workspace();
    let log_dir = root.path().join("logs & <fixture>");
    let output = render(
        root.path(),
        &[
            "--format",
            "launchd",
            "--namespace",
            "org.example.fixture-1",
            "--log-dir",
            log_dir.to_str().unwrap(),
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("<key>Label</key><string>org.example.fixture-1.ingest</string>"));
    assert!(text.contains("/org.example.fixture-1.ingest.plist"));
    assert!(text.contains("logs &amp; &lt;fixture&gt;/ingest.log</string>"));
    assert!(!text.contains("com.lorekeeper."));
    assert!(
        !log_dir.exists(),
        "rendering must not create log directories"
    );
    assert!(
        !root.path().join("vault").exists(),
        "rendering must not publish a vault"
    );

    let defaults = render(root.path(), &["--format", "launchd"]);
    assert!(defaults.status.success());
    let defaults = String::from_utf8(defaults.stdout).unwrap();
    assert!(defaults.contains("<key>Label</key><string>com.lorekeeper.ingest</string>"));
    assert!(defaults.contains("/Library/Logs/lorekeeper/ingest.log</string>"));
    let isolated = render(
        root.path(),
        &[
            "--format",
            "launchd",
            "--namespace",
            "org.example.fixture-1",
        ],
    );
    assert!(
        String::from_utf8(isolated.stdout)
            .unwrap()
            .contains("/Library/Logs/org.example.fixture-1/ingest.log</string>")
    );
}

#[test]
fn unsafe_namespace_or_log_directory_is_rejected_before_emitting_definitions() {
    let root = workspace();
    for namespace in [
        "",
        ".bad",
        "bad..part",
        "../escape",
        "bad/slash",
        "bad\nline",
        "bad<xml",
        "bad space",
        "-bad",
        "이름",
    ] {
        let output = render(
            root.path(),
            &["--format", "launchd", "--namespace", namespace],
        );
        assert!(!output.status.success(), "accepted {namespace:?}");
        assert!(output.stdout.is_empty());
    }
    for args in [
        vec!["--format", "launchd", "--log-dir", "relative/logs"],
        vec!["--format", "launchd", "--log-dir", "/tmp/line\nbreak"],
        vec!["--format", "cron", "--namespace", "org.example.fixture"],
        vec!["--format", "cron", "--log-dir", "/tmp/fixture"],
    ] {
        let output = render(root.path(), &args);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn direct_schedule_warns_that_period_intent_is_not_semantic_processing() {
    let root = workspace();
    let path = root.path().join("config.yaml");
    let mut config = std::fs::read_to_string(&path).unwrap();
    config.push_str("synthesis:\n  weekly:\n    enabled: true\n    schedule: '0 8 * * 1'\n");
    std::fs::write(&path, config).unwrap();
    for format in ["cron", "launchd"] {
        let output = render(root.path(), &["--format", format]);
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stdout.contains("--defer"), "{stdout}");
        assert!(stderr.contains("no semantic consumer"), "{stderr}");
        assert!(stderr.contains("lore synthesis pending"), "{stderr}");
        assert!(!root.path().join("vault").exists());
    }
}
