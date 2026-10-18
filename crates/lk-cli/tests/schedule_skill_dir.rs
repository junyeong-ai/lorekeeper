//! Scheduled pipelines retain an explicitly selected skill installation.

use std::path::Path;
use std::process::{Command, Output};

fn schedule(root: &Path, format: &str, skill_dir: Option<&Path>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lore"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("LORE_") {
            command.env_remove(key);
        }
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
