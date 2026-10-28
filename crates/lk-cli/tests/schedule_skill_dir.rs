//! Scheduled jobs use configured execution profiles and captured, credential-free paths.
use std::path::Path;
use std::process::{Command, Output};

fn workspace(engine: Option<&str>) -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    let mut config = "vault:\n  root: vault\nidentity:\n  name: Test\n  email: test@example.com\nsources:\n  notes:\n    type: manual\n    params: {inbox_dir: inbox}\ningest:\n  schedule: '0 9 * * *'\n".to_owned();
    if let Some(engine) = engine {
        config.push_str(&format!("execution:\n  default_profile: primary\n  profiles:\n    primary: {{engine: {engine}, timeout_seconds: 60}}\n"));
    }
    std::fs::write(directory.path().join("config.yaml"), config).unwrap();
    directory
}

fn render(root: &Path, format: &str, environment: &[(&str, &std::ffi::OsStr)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lore"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("LORE_") {
            command.env_remove(key);
        }
    }
    command
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME");
    for key in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "no_proxy",
    ] {
        command.env_remove(key);
    }
    command.env("ANTHROPIC_API_KEY", "fixture-must-not-be-exported");
    command.env("OPENAI_API_KEY", "fixture-must-not-be-exported");
    command.env("LORE_INSTALL_DATA_DIR", root.join("runtime data"));
    for (key, value) in environment {
        command.env(key, value);
    }
    command
        .current_dir(root)
        .args(["schedule", "--format", format, "--bin"])
        .arg(env!("CARGO_BIN_EXE_lore"))
        .output()
        .unwrap()
}

#[test]
fn scheduled_semantic_work_requires_an_explicit_profile() {
    let directory = workspace(None);
    for format in ["cron", "launchd"] {
        let output = render(directory.path(), format, &[]);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("execution.default_profile"));
        assert!(!directory.path().join("vault").exists());
    }
}

#[test]
fn both_schedulers_capture_runtime_and_only_the_selected_hosts_existing_profile() {
    for (engine, key, other) in [
        ("claude-code", "CLAUDE_CONFIG_DIR", "CODEX_HOME"),
        ("codex", "CODEX_HOME", "CLAUDE_CONFIG_DIR"),
    ] {
        let directory = workspace(Some(engine));
        let profile = directory.path().join("selected profile");
        std::fs::create_dir(&profile).unwrap();
        let missing = directory.path().join("not-selected-missing");
        for format in ["cron", "launchd"] {
            let output = render(
                directory.path(),
                format,
                &[
                    (key, std::ffi::OsStr::new("selected profile")),
                    (other, missing.as_os_str()),
                ],
            );
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let text = String::from_utf8(output.stdout).unwrap();
            assert!(text.contains(key));
            assert!(text.contains(profile.canonicalize().unwrap().to_str().unwrap()));
            if format == "cron" {
                let expected = format!("{key}={}", profile.canonicalize().unwrap().display());
                assert!(text.lines().any(|line| line == expected));
            }
            assert!(text.contains("LORE_INSTALL_DATA_DIR"));
            assert!(text.contains(directory.path().join("runtime data").to_str().unwrap()));
            assert!(!text.contains(other));
            assert!(!text.contains("fixture-must-not-be-exported"));
            assert!(!directory.path().join("vault").exists());
        }
    }
}

#[cfg(unix)]
#[test]
fn cron_routes_literal_selected_host_paths_after_the_working_directory() {
    let directory = workspace(Some("codex"));
    for name in ["selected ", "selected both'\"\\% "] {
        let profile = directory.path().join(name);
        std::fs::create_dir(&profile).unwrap();
        let output = render(
            directory.path(),
            "cron",
            &[
                ("CODEX_HOME", profile.as_os_str()),
                ("PATH", std::ffi::OsStr::new("/usr/bin:/bin")),
            ],
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(!text.lines().any(|line| line.starts_with("CODEX_HOME=")));
        let commands = text
            .lines()
            .filter_map(|line| line.split_once(" && ").map(|(_, command)| command))
            .collect::<Vec<_>>();
        assert!(!commands.is_empty());
        assert!(
            commands
                .iter()
                .all(|command| command.starts_with("CODEX_HOME="))
        );
    }
}

#[test]
fn stage_overrides_capture_both_hosts_without_capturing_unused_profiles() {
    for explicit_claude_directory in [false, true] {
        let directory = workspace(Some("codex"));
        let path = directory.path().join("config.yaml");
        let claude = directory.path().join("claude");
        let codex = directory.path().join("codex");
        std::fs::create_dir(&claude).unwrap();
        std::fs::create_dir(&codex).unwrap();
        let mut config = std::fs::read_to_string(&path).unwrap();
        config.push_str("    reader: {engine: claude-code, timeout_seconds: 60");
        if explicit_claude_directory {
            config.push_str(", config_directory: claude");
        }
        config.push_str("}\n");
        std::fs::write(&path, &config).unwrap();
        let missing = directory.path().join("unused");
        let output = render(
            directory.path(),
            "cron",
            &[
                ("CODEX_HOME", codex.as_os_str()),
                ("CLAUDE_CONFIG_DIR", missing.as_os_str()),
            ],
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !String::from_utf8(output.stdout)
                .unwrap()
                .contains("CLAUDE_CONFIG_DIR")
        );

        config = config.replace("execution:\n", "execution:\n  stages: {inspect: reader}\n");
        std::fs::write(&path, config).unwrap();
        for format in ["cron", "launchd"] {
            let output = render(
                directory.path(),
                format,
                &[
                    ("CODEX_HOME", codex.as_os_str()),
                    (
                        "CLAUDE_CONFIG_DIR",
                        if explicit_claude_directory {
                            missing.as_os_str()
                        } else {
                            claude.as_os_str()
                        },
                    ),
                ],
            );
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let text = String::from_utf8(output.stdout).unwrap();
            assert!(text.contains("CODEX_HOME"));
            assert_eq!(
                text.contains("CLAUDE_CONFIG_DIR"),
                !explicit_claude_directory
            );
            assert!(!text.contains("fixture-must-not-be-exported"));
            assert!(!directory.path().join("vault").exists());
        }
    }
}

#[test]
fn an_explicit_missing_selected_profile_is_not_silently_replaced() {
    let directory = workspace(Some("codex"));
    for value in ["", "missing"] {
        let output = render(
            directory.path(),
            "cron",
            &[("CODEX_HOME", std::ffi::OsStr::new(value))],
        );
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn relative_and_empty_path_entries_are_anchored_before_the_worker_changes_directory() {
    let directory = workspace(Some("codex"));
    let path = std::env::join_paths([std::path::PathBuf::from("tools"), std::path::PathBuf::new()])
        .unwrap();
    for format in ["cron", "launchd"] {
        let output = render(directory.path(), format, &[("PATH", path.as_os_str())]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(
            text.contains(directory.path().join("tools").to_str().unwrap()),
            "{text}"
        );
        assert!(text.contains("daily"));
    }
}
