use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Output};

fn run(config: &Path, cwd: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lore"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("LORE_") {
            command.env_remove(key);
        }
    }
    command
        .current_dir(cwd)
        .env(
            "XDG_CONFIG_HOME",
            config.parent().unwrap().parent().unwrap(),
        )
        .arg("--config")
        .arg(config)
        .args(args)
        .output()
        .unwrap()
}

fn assert_success(output: Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn snapshot(root: &Path) -> BTreeMap<std::path::PathBuf, Vec<u8>> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .map(Result::unwrap)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| {
            (
                entry.path().strip_prefix(root).unwrap().to_path_buf(),
                std::fs::read(entry.path()).unwrap(),
            )
        })
        .collect()
}

#[test]
fn starter_uses_selected_vault_and_deployment_preserves_user_config() {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("unrelated working directory");
    let config_dir = root.path().join("settings/lorekeeper");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&config_dir).unwrap();
    let config = config_dir.join("config.yaml");
    let mut starter: serde_json::Value =
        lk_core::yaml::from_str(include_str!("../../../config.starter.yaml")).unwrap();

    for name in ["First Vault", "다른 지식 저장소"] {
        let vault = root.path().join(name);
        std::fs::create_dir_all(vault.join("inbox")).unwrap();
        std::fs::write(
            vault.join("inbox/note.md"),
            format!("# {name}\n\nOnly this vault's input.\n"),
        )
        .unwrap();
        starter["vault"]["root"] = serde_json::json!(format!("../../{name}"));
        std::fs::write(&config, serde_json::to_vec_pretty(&starter).unwrap()).unwrap();

        assert_success(run(&config, &cwd, &["validate"]));
        let schedule = run(
            &config,
            &cwd,
            &[
                "schedule",
                "--format",
                "launchd",
                "--bin",
                env!("CARGO_BIN_EXE_lore"),
            ],
        );
        assert!(
            schedule.status.success(),
            "{}",
            String::from_utf8_lossy(&schedule.stderr)
        );
        assert!(
            String::from_utf8_lossy(&schedule.stdout)
                .contains("<string>com.lorekeeper.ingest</string>")
        );
        assert_success(run(&config, &cwd, &["schema"]));
        let before = snapshot(&vault);
        assert_success(run(&config, &cwd, &["ingest", "--dry-run"]));
        assert_eq!(snapshot(&vault), before);
        assert_success(run(&config, &cwd, &["ingest"]));
        let documents: Vec<_> = std::fs::read_dir(vault.join("wiki/documents"))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(documents.len(), 1);
        assert!(
            std::fs::read_to_string(documents[0].path())
                .unwrap()
                .contains(name)
        );
        assert!(!vault.join("inbox/note.md").exists());
        assert!(!vault.join(".lorekeeper/credentials.json").exists());
        assert!(!run(&config, &cwd, &["queue", "check"]).status.success());
    }

    let before = std::fs::read(&config).unwrap();
    let data = root.path().join("installed data");
    assert_success(run(
        &config,
        &cwd,
        &[
            "self",
            "deploy",
            "--skills",
            "none",
            "--data-dir",
            data.to_str().unwrap(),
        ],
    ));
    assert_eq!(std::fs::read(&config).unwrap(), before);
    for name in ["config.example.yaml", "config.starter.yaml"] {
        let original = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(name);
        assert_eq!(
            std::fs::read(config_dir.join(name)).unwrap(),
            std::fs::read(original).unwrap()
        );
    }
}
