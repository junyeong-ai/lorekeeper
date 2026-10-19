//! Scheduled pipelines retain an explicitly selected skill installation.

use std::path::Path;
use std::process::{Command, Output};

fn schedule(root: &Path, format: &str, skill_dir: Option<&Path>) -> Output {
    schedule_profile(root, format, skill_dir, None)
}

fn schedule_profile(
    root: &Path,
    format: &str,
    skill_dir: Option<&Path>,
    profile: Option<&Path>,
) -> Output {
    schedule_profile_on_path(root, format, skill_dir, profile, None)
}

fn schedule_profile_on_path(
    root: &Path,
    format: &str,
    skill_dir: Option<&Path>,
    profile: Option<&Path>,
    path: Option<&std::ffi::OsStr>,
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lore"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("LORE_") {
            command.env_remove(key);
        }
    }
    command.env_remove("CLAUDE_CONFIG_DIR");
    if let Some(path) = path {
        command.env("PATH", path);
    }
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
    command.env("ANTHROPIC_API_KEY", "fixture-key-must-not-be-exported");
    if let Some(dir) = profile {
        command.env("CLAUDE_CONFIG_DIR", dir);
    }
    if let Some(dir) = skill_dir {
        command.env("LORE_SKILL_DIR", dir);
    }
    command
        .current_dir(root)
        .args(["schedule", "--format", format, "--bin"])
        .arg(env!("CARGO_BIN_EXE_lore"))
        .arg("--pipeline-dir")
        .arg(root.join("pipelines"))
        .output()
        .unwrap()
}

#[cfg(unix)]
#[test]
fn captured_claude_runs_after_chdir_without_pinning_a_symlink_target() {
    use std::os::unix::{fs::PermissionsExt, fs::symlink};

    let root = workspace();
    let cwd = root.path().canonicalize().unwrap();
    let tools = cwd.join("tools");
    let elsewhere = cwd.join("vault");
    std::fs::create_dir(&tools).unwrap();
    std::fs::create_dir(&elsewhere).unwrap();
    let versions: Vec<_> = ["first", "second"]
        .into_iter()
        .map(|version| {
            let file = root.path().join(format!("claude-{version}"));
            std::fs::write(&file, format!("#!/bin/sh\nprintf '{version}\\n'\n")).unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
            file
        })
        .collect();
    for (path_entry, captured) in [
        (std::path::PathBuf::from("tools"), tools.join("claude")),
        (std::path::PathBuf::new(), cwd.join("claude")),
        (tools.clone(), tools.join("claude")),
    ] {
        let path = std::env::join_paths([path_entry]).unwrap();
        for format in ["cron", "launchd"] {
            let _ = std::fs::remove_file(&captured);
            symlink(&versions[0], &captured).unwrap();
            let output =
                schedule_profile_on_path(root.path(), format, None, None, Some(path.as_os_str()));
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let text = String::from_utf8(output.stdout).unwrap();
            let expected = if format == "cron" {
                format!("CLAUDE_BIN={}\n", captured.display())
            } else {
                format!(
                    "<key>CLAUDE_BIN</key><string>{}</string>",
                    captured.display()
                )
            };
            assert!(text.contains(&expected), "{text}");
            let run = || {
                Command::new(&captured)
                    .current_dir(&elsewhere)
                    .output()
                    .unwrap()
            };
            let first = run();
            assert!(first.status.success());
            assert_eq!(first.stdout, b"first\n");
            // A normal CLI update moves the symlink; schedules must follow it.
            std::fs::remove_file(&captured).unwrap();
            symlink(&versions[1], &captured).unwrap();
            let second = run();
            assert!(second.status.success());
            assert_eq!(second.stdout, b"second\n");
        }
    }
}

fn workspace() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("config.yaml"),
        "vault:\n  root: vault\nidentity:\n  name: Test\n  email: test@example.invalid\n\
         ingest:\n  schedule: '0 9 * * *'\n\
         sources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: inbox\n",
    )
    .unwrap();
    root
}

#[test]
fn both_schedulers_capture_the_absolute_explicit_skill_directory() {
    let root = workspace();
    let skills = root.path().join("project skills & tools");
    std::fs::create_dir(&skills).unwrap();
    let canonical = skills.canonicalize().unwrap();
    for format in ["cron", "launchd"] {
        let out = schedule(
            root.path(),
            format,
            Some(Path::new("project skills & tools")),
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let text = String::from_utf8(out.stdout).unwrap();
        if format == "cron" {
            assert!(text.contains(&format!("LORE_SKILL_DIR={}", canonical.display())));
        } else {
            assert!(text.contains("<key>LORE_SKILL_DIR</key>"));
            assert!(text.contains(&format!(
                "<string>{}</string>",
                canonical.display().to_string().replace('&', "&amp;")
            )));
        }
    }
}

#[test]
fn absent_override_keeps_the_runtime_default_and_invalid_override_emits_no_schedule() {
    let root = workspace();
    let file = root.path().join("not-a-directory");
    std::fs::write(&file, "not a skill directory").unwrap();
    for format in ["cron", "launchd"] {
        let default = schedule(root.path(), format, None);
        assert!(default.status.success());
        assert!(!String::from_utf8_lossy(&default.stdout).contains("LORE_SKILL_DIR"));
        for invalid in [Path::new("missing"), file.as_path(), Path::new("")] {
            let refused = schedule(root.path(), format, Some(invalid));
            assert!(!refused.status.success());
            assert!(refused.stdout.is_empty());
            assert!(String::from_utf8_lossy(&refused.stderr).contains("LORE_SKILL_DIR"));
        }
    }
}

#[test]
fn selected_claude_profile_is_canonicalized_without_exporting_secrets() {
    let root = workspace();
    let profile = root.path().join("Claude profile & settings");
    std::fs::create_dir(&profile).unwrap();
    let canonical = profile.canonicalize().unwrap();
    let file = root.path().join("config.yaml");
    for format in ["cron", "launchd"] {
        let output = schedule_profile(
            root.path(),
            format,
            None,
            Some(Path::new("Claude profile & settings")),
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        if format == "cron" {
            assert!(text.contains(&format!("CLAUDE_CONFIG_DIR={}", canonical.display())));
        } else {
            assert!(text.contains("<key>CLAUDE_CONFIG_DIR</key>"));
            assert!(text.contains(&format!(
                "<string>{}</string>",
                canonical.display().to_string().replace('&', "&amp;")
            )));
        }
        assert!(!text.contains("ANTHROPIC_API_KEY"));
        assert!(!text.contains("fixture-key-must-not-be-exported"));
        let default = schedule_profile(root.path(), format, None, None);
        assert!(default.status.success());
        assert!(!String::from_utf8_lossy(&default.stdout).contains("CLAUDE_CONFIG_DIR"));
        for invalid in [Path::new("missing-profile"), file.as_path(), Path::new("")] {
            let refused = schedule_profile(root.path(), format, None, Some(invalid));
            assert!(!refused.status.success());
            assert!(refused.stdout.is_empty());
            assert!(String::from_utf8_lossy(&refused.stderr).contains("CLAUDE_CONFIG_DIR"));
        }
    }
}
