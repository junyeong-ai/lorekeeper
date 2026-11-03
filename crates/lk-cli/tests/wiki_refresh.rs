use std::path::Path;
use std::process::{Command, Output};

fn run(config: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_lore"))
        .arg("--config")
        .arg(config)
        .args(args)
        .output()
        .unwrap()
}

fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("config.yaml");
    std::fs::write(&config, "vault:\n  root: vault\n  dirs:\n    wiki: knowledge\nidentity:\n  name: T\n  email: t@example.com\nsources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: inbox\n").unwrap();
    std::fs::create_dir_all(temp.path().join("vault/knowledge/documents")).unwrap();
    (temp, config)
}

#[test]
fn refresh_rebuilds_sqlite_discovery_without_materializing_markdown_lists() {
    let (temp, config) = fixture();
    let vault = temp.path().join("vault");
    let documents = vault.join("knowledge/documents");
    let page = |name: &str| {
        format!("---\ntype: document\ntitle: {name}\n---\n# {name}\n\nOriginal authored body.\n")
    };
    std::fs::write(documents.join("alpha.md"), page("Alpha")).unwrap();
    for name in ["log.md", "map.md", "catalog/authored.md"] {
        let path = vault.join("knowledge").join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "Preserve this existing user file.\n").unwrap();
    }
    let refresh = run(&config, &["wiki", "refresh"]);
    assert!(
        refresh.status.success(),
        "{}",
        String::from_utf8_lossy(&refresh.stderr)
    );
    let entry = std::fs::read_to_string(vault.join("knowledge/index.md")).unwrap();
    assert!(entry.contains("lore wiki browse"));
    assert!(!entry.contains("[Alpha]"));
    assert!(vault.join(".lorekeeper/search.sqlite").is_file());
    std::fs::write(documents.join("beta.md"), page("Beta")).unwrap();
    assert!(run(&config, &["wiki", "refresh"]).status.success());
    let found = run(&config, &["wiki", "search", "Beta", "--json"]);
    assert!(
        found.status.success(),
        "{}",
        String::from_utf8_lossy(&found.stderr)
    );
    let found: serde_json::Value = serde_json::from_slice(&found.stdout).unwrap();
    assert_eq!(found["hits"][0]["path"], "knowledge/documents/beta.md");
    let lint = run(&config, &["graph", "--json", "lint"]);
    assert!(
        lint.status.success(),
        "{}",
        String::from_utf8_lossy(&lint.stderr)
    );
    let lint: serde_json::Value = serde_json::from_slice(&lint.stdout).unwrap();
    assert!(lint["data"]["violations"].get("index").is_none());
    for name in ["log.md", "map.md", "catalog/authored.md"] {
        assert_eq!(
            std::fs::read_to_string(vault.join("knowledge").join(name)).unwrap(),
            "Preserve this existing user file.\n"
        );
    }
    assert_eq!(
        std::fs::read_to_string(documents.join("alpha.md")).unwrap(),
        page("Alpha")
    );
    assert_eq!(
        std::fs::read_to_string(vault.join("knowledge/index.md")).unwrap(),
        entry
    );
}

#[test]
fn failed_capture_preserves_the_existing_guide_and_snapshot() {
    let (temp, config) = fixture();
    let vault = temp.path().join("vault");
    assert!(run(&config, &["wiki", "refresh"]).status.success());
    let entry = std::fs::read(vault.join("knowledge/index.md")).unwrap();
    let snapshot = std::fs::read(vault.join(".lorekeeper/search.sqlite")).unwrap();
    std::fs::write(
        vault.join("knowledge/documents/unreadable.md"),
        [0xff, 0xfe],
    )
    .unwrap();
    let failed = run(&config, &["wiki", "refresh"]);
    assert!(!failed.status.success());
    assert!(!String::from_utf8_lossy(&failed.stderr).contains("citation capture changed"));
    assert_eq!(
        std::fs::read(vault.join("knowledge/index.md")).unwrap(),
        entry
    );
    assert_eq!(
        std::fs::read(vault.join(".lorekeeper/search.sqlite")).unwrap(),
        snapshot
    );
    assert!(!vault.join("knowledge/log.md").exists());
    assert!(!vault.join("knowledge/map.md").exists());
}

#[test]
fn guide_collision_is_refused_without_touching_authored_files_or_creating_a_snapshot() {
    let (temp, config) = fixture();
    let vault = temp.path().join("vault");
    let original = "# Authored index\n\nKeep original prose.\n";
    std::fs::write(vault.join("knowledge/index.md"), original).unwrap();
    assert!(!run(&config, &["wiki", "refresh"]).status.success());
    assert_eq!(
        std::fs::read_to_string(vault.join("knowledge/index.md")).unwrap(),
        original
    );
    assert!(!vault.join(".lorekeeper/search.sqlite").exists());
    assert!(!vault.join(".lorekeeper/navigation-archives").exists());
}

#[test]
fn refresh_refuses_manual_inbox_overlap_before_capture() {
    let (temp, config) = fixture();
    let vault = temp.path().join("vault");
    let wiki = vault.join("knowledge");
    let inboxes = vec![wiki.clone()];
    #[cfg(unix)]
    let inboxes = {
        let mut inboxes = inboxes;
        let alias = temp.path().join("inbox-alias");
        std::os::unix::fs::symlink(&wiki, &alias).unwrap();
        inboxes.push(alias);
        inboxes
    };
    for inbox in inboxes {
        std::fs::write(&config, format!("vault:\n  root: vault\n  dirs:\n    wiki: knowledge\nidentity:\n  name: T\n  email: t@example.com\nsources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: {}\n", serde_json::to_string(&inbox.to_string_lossy()).unwrap())).unwrap();
        let output = run(&config, &["wiki", "refresh"]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("overlaps"));
        assert!(!vault.join(".lorekeeper/search.sqlite").exists());
        assert!(!wiki.join("index.md").exists());
    }
}

#[cfg(unix)]
mod publication_guards {
    use super::*;
    use std::io::{ErrorKind, Read, Write};
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::process::{Child, ExitStatus, Stdio};
    use std::time::{Duration, Instant};

    struct PreparedRefresh {
        child: Child,
        errors: UnixStream,
        filled: usize,
    }

    impl Drop for PreparedRefresh {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    impl PreparedRefresh {
        fn build(config: &Path) -> Self {
            let (reader, mut writer) = UnixStream::pair().unwrap();
            writer.set_nonblocking(true).unwrap();
            let mut filled = 0;
            loop {
                match writer.write(&[b'x'; 4096]) {
                    Ok(bytes) => filled += bytes,
                    Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                    Err(error) => panic!("{error}"),
                }
                assert!(
                    filled < 1024 * 1024,
                    "stderr backpressure must remain bounded"
                );
            }
            writer.set_nonblocking(false).unwrap();
            let mut command = Command::new(env!("CARGO_BIN_EXE_lore"));
            for (key, _) in std::env::vars_os() {
                if key.to_string_lossy().starts_with("LORE_") {
                    command.env_remove(key);
                }
            }
            let case = config.parent().unwrap();
            for (key, directory) in [
                ("XDG_CONFIG_HOME", "runtime/config"),
                ("XDG_DATA_HOME", "runtime/data"),
                ("LORE_INSTALL_DATA_DIR", "runtime/data"),
            ] {
                std::fs::create_dir_all(case.join(directory)).unwrap();
                command.env(key, case.join(directory));
            }
            let child = command
                .arg("--config")
                .arg(config)
                .args(["workflow", "--"])
                .arg(env!("CARGO_BIN_EXE_lore"))
                .arg("--config")
                .arg(config)
                .args(["wiki", "refresh"])
                .current_dir(case.join("vault"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::from(OwnedFd::from(writer)))
                .spawn()
                .unwrap();
            let mut held = Self {
                child,
                errors: reader,
                filled,
            };
            let dirs = lk_core::config::VaultDirs {
                wiki: "knowledge".into(),
                ..Default::default()
            };
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                if lk_vault::inspect_search_snapshot(&case.join("vault"), &dirs).is_ok() {
                    assert!(held.child.try_wait().unwrap().is_none());
                    assert!(!case.join("vault/knowledge/index.md").exists());
                    return held;
                }
                assert!(
                    Instant::now() < deadline,
                    "refresh did not prepare a snapshot"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        fn finish(mut self) -> (ExitStatus, String) {
            self.errors.set_nonblocking(true).unwrap();
            let mut captured = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                let mut buffer = [0; 8192];
                loop {
                    match self.errors.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(bytes) => captured.extend_from_slice(&buffer[..bytes]),
                        Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                        Err(error) => panic!("{error}"),
                    }
                }
                if let Some(status) = self.child.try_wait().unwrap() {
                    let mut rest = Vec::new();
                    self.errors.read_to_end(&mut rest).unwrap();
                    captured.extend(rest);
                    assert!(captured.starts_with(&vec![b'x'; self.filled]));
                    return (
                        status,
                        String::from_utf8(captured[self.filled..].to_vec()).unwrap(),
                    );
                }
                assert!(Instant::now() < deadline, "refresh did not finish");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    #[test]
    fn refresh_refuses_configuration_drift_before_guide_publication() {
        let (case, config) = fixture();
        let held = PreparedRefresh::build(&config);
        let before = std::fs::read_to_string(&config).unwrap();
        std::fs::write(
            &config,
            before.replace("root: vault", "root: vault\n  locale: en"),
        )
        .unwrap();
        let (status, errors) = held.finish();
        assert!(!status.success(), "{errors}");
        assert!(
            errors.contains("selected configuration changed"),
            "{errors}"
        );
        assert!(!case.path().join("vault/knowledge/index.md").exists());
        assert!(
            !case
                .path()
                .join("vault/.lorekeeper/mutation/pending.json")
                .exists()
        );
        assert!(
            case.path()
                .join("vault/.lorekeeper/search.sqlite")
                .is_file()
        );
    }

    #[test]
    fn refresh_preserves_guide_created_after_snapshot_preparation() {
        let (case, config) = fixture();
        let held = PreparedRefresh::build(&config);
        let root = case.path().join("vault");
        let guide = root.join("knowledge/index.md");
        let authored = "# Authored guide\n\nPreserve this concurrent edit.\n";
        std::fs::write(&guide, authored).unwrap();
        let (status, errors) = held.finish();
        assert!(!status.success(), "{errors}");
        assert_eq!(std::fs::read_to_string(guide).unwrap(), authored);
        assert!(root.join(".lorekeeper/search.sqlite").is_file());
        assert!(!root.join(".lorekeeper/mutation/pending.json").exists());
    }

    #[test]
    fn refresh_accepts_same_directory_alias_at_guide_publication() {
        let (case, config) = fixture();
        let selected = case.path().join("selected");
        let alias = case.path().join("alias");
        std::os::unix::fs::symlink(case.path().join("vault"), &selected).unwrap();
        std::os::unix::fs::symlink(case.path().join("vault"), &alias).unwrap();
        let raw = std::fs::read_to_string(&config).unwrap();
        std::fs::write(&config, raw.replace("root: vault", "root: selected")).unwrap();
        let held = PreparedRefresh::build(&config);
        std::fs::remove_file(&selected).unwrap();
        std::os::unix::fs::symlink(alias, selected).unwrap();
        let (status, errors) = held.finish();
        assert!(status.success(), "{errors}");
        assert!(case.path().join("vault/knowledge/index.md").is_file());
        assert!(
            case.path()
                .join("vault/.lorekeeper/search.sqlite")
                .is_file()
        );
    }

    #[test]
    fn refresh_refuses_root_replacement_before_guide_publication() {
        let (case, config) = fixture();
        let held = PreparedRefresh::build(&config);
        let root = case.path().join("vault");
        let original = case.path().join("original-vault");
        std::fs::rename(&root, &original).unwrap();
        std::fs::create_dir_all(root.join("knowledge")).unwrap();
        std::fs::create_dir_all(root.join(".lorekeeper/mutation")).unwrap();
        std::fs::write(root.join(".lorekeeper/mutation/lock"), "").unwrap();
        let (status, errors) = held.finish();
        assert!(!status.success(), "{errors}");
        assert!(
            errors.contains("workflow vault selection changed"),
            "{errors}"
        );
        assert!(!root.join("knowledge/index.md").exists());
        assert!(!root.join(".lorekeeper/search.sqlite").exists());
        assert!(!original.join("knowledge/index.md").exists());
        assert!(original.join(".lorekeeper/search.sqlite").is_file());
        assert!(!root.join(".lorekeeper/mutation/pending.json").exists());
        assert!(!original.join(".lorekeeper/mutation/pending.json").exists());
    }
}
