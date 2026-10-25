use std::process::{Command, Output};

#[test]
fn explicit_concept_link_repair_restores_exact_bodies_without_semantic_republication() {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path().join("vault");
    std::fs::create_dir_all(root.join("wiki/concepts")).unwrap();
    let config = workspace.path().join("config.yaml");
    std::fs::write(&config, "vault: {root: vault, locale: en}\nidentity: {name: Tester, email: tester@example.com}\nsources:\n  notes:\n    type: manual\n    params: {inbox_dir: inbox}\n").unwrap();
    let run = |arguments: &[&str]| -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_lore"));
        for (key, _) in std::env::vars() {
            if key.starts_with("LORE_") {
                command.env_remove(key);
            }
        }
        command
            .arg("--config")
            .arg(&config)
            .args(arguments)
            .output()
            .unwrap()
    };
    let target = "wiki/concepts/openrouter.md";
    let target_raw =
        "---\ntype: concept\ntitle: OpenRouter\n---\n# OpenRouter\n\nExisting owner.\n";
    std::fs::write(root.join(target), target_raw).unwrap();
    let to_revision = lk_core::concept::file_revision(target_raw);
    let mut pages = Vec::new();
    for slug in ["deepseek-v4-pro", "openrouter-fusion-api", "stripe"] {
        let address = format!("wiki/concepts/{slug}.md");
        let expected = format!(
            "---\ntype: concept\ntitle: {slug}\nllm_inputs: {{synthesis: paid, synthesis_done: paid}}\n---\n# {slug}\n\n기존 설명과 완료 마커를 유지한다.\n\n## Related\n\n[OpenRouter](openrouter.md)\n"
        );
        let before = expected.replace("](openrouter.md)", "](wiki/concepts/openrouter.md)");
        assert_eq!(before.len(), expected.len() + 14);
        std::fs::write(root.join(&address), &before).unwrap();
        pages.push((address, before, expected));
    }
    let paid = root.join(".lorekeeper/work/paid/plan.json");
    std::fs::create_dir_all(paid.parent().unwrap()).unwrap();
    std::fs::write(&paid, "Opaque retained paid state.\n").unwrap();
    let lint = run(&["graph", "--json", "lint"]);
    assert_eq!(
        lint.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&lint.stderr)
    );
    let lint: serde_json::Value = serde_json::from_slice(&lint.stdout).unwrap();
    assert_eq!(
        lint["data"]["violations"]["broken"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    let from = "wiki/concepts/wiki/concepts/openrouter.md";
    for (address, before, expected) in &pages {
        let revision = lk_core::concept::file_revision(before);
        let mut args = vec![
            "graph",
            "--json",
            "repair-link",
            address,
            "--from",
            from,
            "--to",
            target,
            "--revision",
            &revision,
            "--to-revision",
            &to_revision,
            "--dry-run",
        ];
        let preview = run(&args);
        assert!(
            preview.status.success(),
            "{}",
            String::from_utf8_lossy(&preview.stderr)
        );
        let preview: serde_json::Value = serde_json::from_slice(&preview.stdout).unwrap();
        assert_eq!(preview["data"]["replacements"], 1);
        assert_eq!(
            preview["data"]["after_revision"],
            lk_core::concept::file_revision(expected)
        );
        assert_eq!(
            std::fs::read_to_string(root.join(address)).unwrap(),
            *before
        );
        args.pop();
        let applied = run(&args);
        assert!(
            applied.status.success(),
            "{}",
            String::from_utf8_lossy(&applied.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(root.join(address)).unwrap(),
            *expected
        );
        assert!(
            !run(&args).status.success(),
            "stale source read must refuse"
        );
    }
    let lint = run(&["graph", "--json", "lint"]);
    assert!(
        lint.status.success(),
        "{}",
        String::from_utf8_lossy(&lint.stderr)
    );
    let lint: serde_json::Value = serde_json::from_slice(&lint.stdout).unwrap();
    assert!(
        lint["data"]["violations"]["broken"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        std::fs::read_to_string(root.join(target)).unwrap(),
        target_raw
    );
    assert_eq!(
        std::fs::read_to_string(paid).unwrap(),
        "Opaque retained paid state.\n"
    );
}
