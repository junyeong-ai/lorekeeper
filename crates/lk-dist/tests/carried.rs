//! Holds what the binary carries to what the repository holds, and the release-target table to
//! the two places outside Rust that name a target.
//!
//! The skills, pipelines, templates and config example are compiled in, so they cannot be a
//! different version from the binary — but they can be the wrong FILES if the manifest that
//! collects them stops matching the tree. That is what these read.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn tree(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .flatten()
    {
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            found.extend(
                tree(&path)
                    .into_iter()
                    .map(|inner| format!("{name}/{inner}")),
            );
        } else {
            found.push(name.to_string());
        }
    }
    found.sort();
    found
}

/// A skill added to the repository used to need remembering in three places — the packaging
/// script, both installers, both uninstallers — and one that was forgotten passed every gate
/// and then reached nobody. It is now carried by the binary, and this is what holds the
/// manifest that collects it to the directory it collects from.
#[test]
fn the_binary_carries_every_skill_the_repository_holds() {
    let root = repo_root().join(".claude/skills");
    let mut on_disk: Vec<String> = std::fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("read {}: {e}", root.display()))
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    on_disk.sort();
    assert!(!on_disk.is_empty(), "no skills under {}", root.display());
    assert_eq!(lk_dist::skill_names(), on_disk);

    for name in on_disk {
        let dir = root.join(&name);
        let mut carried: Vec<String> = lk_dist::skill_files(&name)
            .iter()
            .map(|file| file.relative.to_string())
            .collect();
        carried.sort();
        assert_eq!(carried, tree(&dir), "skill `{name}` carries other files");

        for file in lk_dist::skill_files(&name) {
            assert_eq!(
                file.contents,
                read(&dir.join(file.relative)),
                "`{name}/{}` is carried with other bytes than the repository holds",
                file.relative
            );
        }
    }
}

#[test]
fn the_binary_carries_the_pipeline_scripts_the_repository_holds() {
    for file in lk_dist::pipeline_files() {
        let path = repo_root().join("scripts").join(file.relative);
        assert_eq!(file.contents, read(&path), "{} drifted", path.display());
    }
    assert_eq!(lk_dist::pipeline_names().len(), 3);
}

#[test]
fn the_binary_carries_the_templates_it_renders_with() {
    let dir = repo_root().join("templates");
    let mut carried: Vec<String> = lk_dist::template_files()
        .iter()
        .map(|file| file.relative.to_string())
        .collect();
    carried.sort();
    assert_eq!(carried, tree(&dir));
    for file in lk_dist::template_files() {
        assert_eq!(file.contents, read(&dir.join(file.relative)));
    }
}

#[test]
fn the_binary_carries_the_config_example() {
    let files = lk_dist::config_files();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].relative, "config.example.yaml");
    assert_eq!(
        files[0].contents,
        read(&repo_root().join("config.example.yaml"))
    );
}

/// An archive an installer asks for has to be one the release builds, and a target the release
/// builds has to be one something installs. They are separate lists in separate languages, so a
/// target renamed on either side is a 404 at install time — the first thing a new user sees, and
/// the one failure they cannot work around.
#[test]
fn the_release_builds_exactly_the_targets_this_table_declares() {
    let release = read(&repo_root().join(".github/workflows/release.yml"));
    let mut built: Vec<&str> = release
        .lines()
        .filter_map(|line| line.trim().strip_prefix("- target: "))
        .map(str::trim)
        .collect();
    built.sort_unstable();
    assert!(!built.is_empty(), "release.yml declares no build targets");

    let mut declared: Vec<&str> = lk_dist::ReleaseTarget::ALL
        .iter()
        .map(|target| target.triple)
        .collect();
    declared.sort_unstable();
    assert_eq!(built, declared);
}

/// `install.sh` selects a target before any binary exists, so it carries its own copy of the
/// mapping. This is what holds the two answers to "which archive is this machine's" together.
#[test]
fn the_shell_installer_selects_the_targets_this_table_declares() {
    let arms = uname_arms(&read(&repo_root().join("scripts/install.sh")));

    let mut declared: Vec<(String, String)> = lk_dist::ReleaseTarget::ALL
        .iter()
        .filter(|target| target.is_self_replaceable())
        .flat_map(|target| {
            target.machines.iter().map(move |machine| {
                (
                    format!("{}-{machine}", target.os),
                    target.triple.to_string(),
                )
            })
        })
        .collect();
    declared.sort();
    assert_eq!(arms, declared);
}

/// Windows is the target the shell installer cannot select and `install.ps1` installs.
#[test]
fn the_powershell_installer_selects_the_archive_the_shell_one_cannot() {
    let script = read(&repo_root().join("scripts/install.ps1"));
    for target in lk_dist::ReleaseTarget::ALL
        .iter()
        .filter(|target| !target.is_self_replaceable())
    {
        assert!(
            script.contains(target.triple),
            "install.ps1 names no {} archive",
            target.triple
        );
    }
}

/// The `case` arms of `detect_platform`, as `(uname pair, triple)`.
///
/// Read from the one function that declares them, and every arm has to be one of two shapes:
/// a mapping to a triple, or the catch-all that refuses an unlisted platform. An arm this
/// cannot read fails the test rather than being skipped, because a mapping half-read compares
/// as a mapping that disagrees — and that is the answer that gets acted on.
fn uname_arms(script: &str) -> Vec<(String, String)> {
    let body = script
        .split_once("detect_platform() {")
        .expect("install.sh declares detect_platform")
        .1
        .split_once("\n}")
        .expect("detect_platform is closed")
        .0;

    let mut arms = Vec::new();
    for line in body.lines().map(str::trim) {
        if !line.ends_with(";;") {
            continue;
        }
        if line.starts_with("*)") {
            continue;
        }
        let (keys, rest) = line.split_once(')').unwrap_or_else(|| {
            panic!("`{line}` ends an arm without opening one");
        });
        let triple = rest
            .trim_start()
            .strip_prefix("echo \"")
            .and_then(|echoed| echoed.split('"').next())
            .unwrap_or_else(|| panic!("arm `{line}` names no target"));
        for key in keys.split('|') {
            arms.push((key.trim().to_string(), triple.to_string()));
        }
    }
    assert!(!arms.is_empty(), "detect_platform declares no arms");
    arms.sort();
    arms
}
