use lk_core::concept::{file_revision, source_revision};
use serde_json::{Value, json};
use std::{path::Path, process::Command};

fn run(config: &Path, args: &[&str]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lore"));
    for (key, _) in std::env::vars().filter(|(key, _)| key.starts_with("LORE_")) {
        command.env_remove(key);
    }
    command
        .arg("--config")
        .arg(config)
        .args(args)
        .output()
        .unwrap()
}
fn ok(config: &Path, args: &[&str]) -> Value {
    let output = run(config, args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn reviewed_curation_is_read_only_in_preview_guarded_and_discoverable_after_publication() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config.yaml");
    std::fs::write(&config,format!("vault:\n  root: {}\n  locale: en\nidentity:\n  name: Test\n  email: test@example.com\nsources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: inbox\n",root.path().display())).unwrap();
    let concept = "wiki/concepts/stored.md";
    let source = "wiki/documents/source.md";
    let review = "wiki/explorations/assessment.md";
    std::fs::create_dir_all(root.path().join("wiki/concepts")).unwrap();
    std::fs::create_dir_all(root.path().join("wiki/documents")).unwrap();
    let concept_before = "---\ntype: concept\ntitle: Stored\naliases: [Stored]\nsource_count: 1\nllm_inputs: {synthesis: current, synthesis_done: current}\n---\n# Stored\n\n## Synthesis\nA case previously presented as general knowledge.\n\n## Sources\n- [Source](../documents/source.md)\n\n## Related\n";
    let source_before = "---\ntype: document\ntitle: Source\n---\n## Summary\nA single incident with no reusable separate subject.\n\n## Related Concepts\n- [Stored](../concepts/stored.md)\n";
    std::fs::write(root.path().join(concept), concept_before).unwrap();
    std::fs::write(root.path().join(source), source_before).unwrap();
    let assessment = format!(
        "---\ntype: exploration\ntitle: Case admission assessment\nreview:\n  status: resolved\n  subjects: [{concept}]\n  evidence:\n    - path: {source}\n      revision: {}\n  resolution: Keep the case in its source; retire the standalone navigation node.\n---\n## Question\nDoes this case warrant an independent knowledge node?\n\n## Synthesis\nThe source describes one incident without a stable subject. Preserve its evidence and address.\n",
        source_revision(&lk_vault::parse_page(source_before).unwrap())
    );
    let plan = root.path().join("curation.json");
    let request = json!({"evidence":[{"path":source,"revision":file_revision(source_before)}],"concepts":[{"path":concept,"revision":file_revision(concept_before),"aliases":["Stored","저장된 사례"],"tags":["source-assessment"],"knowledge_status":"retired","knowledge_review":review}],"reviews":[{"path":review,"revision":null,"content":assessment}]});
    std::fs::write(&plan, serde_json::to_vec(&request).unwrap()).unwrap();
    ok(
        &config,
        &[
            "wiki",
            "curate",
            "--file",
            plan.to_str().unwrap(),
            "--dry-run",
        ],
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join(concept)).unwrap(),
        concept_before
    );
    assert!(!root.path().join(review).exists());
    assert!(!root.path().join(".lorekeeper").exists());
    ok(
        &config,
        &["wiki", "curate", "--file", plan.to_str().unwrap()],
    );
    let after = std::fs::read_to_string(root.path().join(concept)).unwrap();
    let before_page = lk_vault::parse_page(concept_before).unwrap();
    let after_page = lk_vault::parse_page(&after).unwrap();
    assert_eq!(after_page.body, before_page.body);
    assert_eq!(
        after_page.frontmatter.get("llm_inputs"),
        before_page.frontmatter.get("llm_inputs")
    );
    let snapshot = ok(&config, &["wiki", "prepare"]);
    let id = snapshot["snapshot"].as_str().unwrap();
    assert_eq!(
        ok(
            &config,
            &[
                "wiki",
                "search",
                "Stored",
                "--snapshot",
                id,
                "--type",
                "concept",
                "--json"
            ]
        )["total"],
        0
    );
    let retired = ok(
        &config,
        &[
            "wiki",
            "search",
            "저장된 사례",
            "--snapshot",
            id,
            "--knowledge-status",
            "retired",
            "--json",
        ],
    );
    assert_eq!(retired["total"], 1);
    assert_eq!(retired["hits"][0]["reviews"][0]["relation"], "subject");
    assert_eq!(
        retired["hits"][0]["reviews"][0]["effective_state"],
        "resolved"
    );
    assert_eq!(retired["hits"][0]["source_count"], 1);
    assert_eq!(retired["hits"][0]["tags"], json!(["source-assessment"]));
    let draft = root.path().join("draft.md");
    std::fs::write(&draft, &after).unwrap();
    let fingerprint = ok(
        &config,
        &["wiki", "fingerprint", "--file", draft.to_str().unwrap()],
    );
    assert_eq!(fingerprint["file_revision"], file_revision(&after));
    assert_eq!(fingerprint["source_revision"], source_revision(&after_page));
    let discovery = root.path().join("discovery.json");
    std::fs::write(
        &discovery,
        serde_json::to_vec(&json!({
            "path": concept,
            "file_revision": fingerprint["file_revision"],
            "queries": [{"query": "Stored"}]
        }))
        .unwrap(),
    )
    .unwrap();
    ok(
        &config,
        &[
            "wiki",
            "audit-discover",
            "--file",
            discovery.to_str().unwrap(),
        ],
    );
    let audit = ok(&config, &["wiki", "audit-plan", "--path", concept]);
    assert_eq!(audit["unreviewed"], 2);
    assert_eq!(audit["counts_by_kind"]["concept"]["unreviewed"], 1);
    assert_eq!(audit["counts_by_kind"]["document"]["unreviewed"], 1);
    let candidate = &audit["candidates"][0];
    assert_eq!(candidate["path"], concept);
    assert_eq!(candidate["kind"], "concept");
    let result = root.path().join("audit.json");
    std::fs::write(&result, serde_json::to_vec(&json!({"assessments":[{
        "path":concept,"file_revision":candidate["file_revision"],
        "input_revision":candidate["input_revision"],"disposition":"reviewed",
        "reason":"Verified the source-only assessment, retained evidence and retired discovery state.",
        "reviews":[review]
    }]})).unwrap()).unwrap();
    let recorded = ok(
        &config,
        &["wiki", "audit-record", "--file", result.to_str().unwrap()],
    );
    assert_eq!(recorded["recorded"], json!([concept]));
    let coverage = ok(&config, &["wiki", "audit-plan"]);
    assert_eq!(coverage["reviewed_current"], 1);
    assert_eq!(coverage["counts_by_kind"]["concept"]["reviewed_current"], 1);
    assert_eq!(coverage["counts_by_kind"]["document"]["unreviewed"], 1);
    assert_eq!(
        ok(&config, &["wiki", "audit-plan", "--path", concept])["candidates"][0]["path"],
        concept
    );
    std::fs::write(
        root.path().join(source),
        source_before.replace("single incident", "different incident"),
    )
    .unwrap();
    let stale = run(
        &config,
        &["wiki", "audit-record", "--file", result.to_str().unwrap()],
    );
    assert!(!stale.status.success());
    assert_eq!(ok(&config, &["wiki", "audit-plan"])["changed"], 1);
    let refused = run(
        &config,
        &["wiki", "curate", "--file", plan.to_str().unwrap()],
    );
    assert!(!refused.status.success());
    assert_eq!(
        std::fs::read_to_string(root.path().join(concept)).unwrap(),
        after
    );
}
