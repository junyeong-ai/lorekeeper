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
    assert_eq!(
        retired["hits"][0]["reviews"]["reviews"][0]["relation"],
        "subject"
    );
    assert_eq!(
        retired["hits"][0]["reviews"]["reviews"][0]["effective_state"],
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
    assert_eq!(audit["coverage_scope"], "selection");
    assert_eq!(audit["unreviewed"], 1);
    assert_eq!(audit["counts_by_kind"]["concept"]["unreviewed"], 1);
    assert_eq!(audit["counts_by_kind"]["document"]["unreviewed"], 0);
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
    assert_eq!(coverage["coverage_scope"], "corpus");
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

#[cfg(unix)]
#[test]
fn review_scopes_follow_verified_symlink_owners_without_merging_hardlink_addresses() {
    for kind in ["leaf", "directory", "hardlink", "review-directory"] {
        let root = tempfile::tempdir().unwrap();
        let vault = root.path().join("vault");
        for directory in ["concepts", "documents", "explorations"] {
            std::fs::create_dir_all(vault.join("wiki").join(directory)).unwrap();
        }
        if kind == "review-directory" {
            std::fs::remove_dir(vault.join("wiki/explorations")).unwrap();
            std::fs::create_dir(vault.join("review-storage")).unwrap();
            std::os::unix::fs::symlink("../review-storage", vault.join("wiki/explorations"))
                .unwrap();
        }
        let config = root.path().join("config.yaml");
        std::fs::write(&config, "vault:\n  root: vault\n  locale: en\nidentity:\n  name: Test\n  email: test@example.com\nsources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: inbox\n").unwrap();
        let concept = "wiki/concepts/topic.md";
        let source = "wiki/documents/source.md";
        let review = "wiki/explorations/review.md";
        let before = "---\ntype: concept\ntitle: Topic\n---\n## Synthesis\nA claim.\n\n## Sources\n\n## Related\n";
        std::fs::write(vault.join(concept), before).unwrap();
        std::fs::write(
            vault.join(source),
            "---\ntype: document\ntitle: Source\n---\n## Summary\nSource evidence.\n",
        )
        .unwrap();
        let alias = if kind == "directory" {
            std::os::unix::fs::symlink("wiki/concepts", vault.join("concept-alias")).unwrap();
            "concept-alias/topic.md"
        } else {
            let alias = "wiki/concepts/alias.md";
            if kind != "hardlink" {
                std::os::unix::fs::symlink("topic.md", vault.join(alias)).unwrap();
            } else {
                std::fs::hard_link(vault.join(concept), vault.join(alias)).unwrap();
            }
            alias
        };
        let source_alias = "wiki/documents/source-alias.md";
        std::os::unix::fs::symlink("source.md", vault.join(source_alias)).unwrap();
        let evidence = ok(&config, &["wiki", "evidence", source_alias]);
        let declared = json!({
            "status":"open", "subjects":[alias], "evidence":evidence["citations"],
            "resolution":null
        });
        let content = format!(
            "---\ntype: exploration\ntitle: Assessment\nreview: {declared}\n---\nA scoped unresolved question.\n"
        );
        let plan = root.path().join("plan.json");
        std::fs::write(&plan, serde_json::to_vec(&json!({
            "evidence":[{"path":source_alias,"revision":evidence["file_revisions"][source_alias]}],
            "reviews":[{"path":review,"revision":null,"content":content}]
        })).unwrap()).unwrap();
        ok(
            &config,
            &["wiki", "curate", "--file", plan.to_str().unwrap()],
        );
        let reports = ok(&config, &["wiki", "reviews"]);
        assert_eq!(reports[0]["review"]["subjects"], json!([alias]), "{kind}");
        assert_eq!(reports[0]["review"]["evidence"], evidence["citations"]);
        assert_eq!(reports[0]["errors"], json!([]));
        let snapshot = ok(&config, &["wiki", "prepare"]);
        let id = snapshot["snapshot"].as_str().unwrap();
        let expected = usize::from(kind != "hardlink");
        for capture in [None, Some(id)] {
            let mut args = vec!["wiki", "search", "Topic", "--json"];
            if let Some(id) = capture {
                args.extend(["--snapshot", id]);
            }
            let search = ok(&config, &args);
            let canonical = search["hits"]
                .as_array()
                .unwrap()
                .iter()
                .find(|hit| hit["path"] == concept)
                .unwrap();
            assert_eq!(
                canonical["reviews"]["reviews"].as_array().unwrap().len(),
                expected,
                "{kind}: {search}"
            );
            assert_eq!(canonical["reviews"]["total"], expected);
            assert_eq!(canonical["reviews"]["states"]["open"], expected);
            if kind == "hardlink" {
                let separate = search["hits"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|hit| hit["path"] == alias)
                    .unwrap();
                assert_eq!(separate["reviews"]["total"], 1);
            }
        }
        let context = ok(&config, &["wiki", "context", concept, "--snapshot", id]);
        assert_eq!(context["reviews"]["total"], expected, "{kind}: {context}");
        let source_context = ok(&config, &["wiki", "context", source, "--snapshot", id]);
        assert_eq!(
            source_context["reviews"]["total"], 1,
            "{kind}: {source_context}"
        );
        assert_eq!(
            source_context["reviews"]["reviews"][0]["relation"],
            "evidence"
        );
        let audit = ok(&config, &["wiki", "audit-plan", "--path", concept]);
        assert_eq!(
            audit["candidates"][0]["reviews"].as_array().unwrap().len(),
            expected,
            "{kind}: {audit}"
        );
        if expected == 1 {
            assert_eq!(audit["candidates"][0]["reviews"][0]["state"], "open");
        }
        let fingerprint = ok(
            &config,
            &[
                "wiki",
                "fingerprint",
                "--file",
                vault.join(concept).to_str().unwrap(),
            ],
        );
        std::fs::write(&plan, serde_json::to_vec(&json!({
            "evidence":[{"path":source_alias,"revision":evidence["file_revisions"][source_alias]}],
            "concepts":[{"path":concept,"revision":fingerprint["file_revision"],"knowledge_status":"active","knowledge_review":review}]
        })).unwrap()).unwrap();
        let publication = run(
            &config,
            &["wiki", "curate", "--file", plan.to_str().unwrap()],
        );
        assert_eq!(
            publication.status.success(),
            kind != "hardlink",
            "{kind}: {}",
            String::from_utf8_lossy(&publication.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(vault.join(review)).unwrap(),
            content
        );
        if kind == "hardlink" {
            assert_eq!(
                std::fs::read_to_string(vault.join(concept)).unwrap(),
                before
            );
        } else {
            let review_alias = "wiki/explorations/review-alias.md";
            std::os::unix::fs::symlink("review.md", vault.join(review_alias)).unwrap();
            let current = std::fs::read_to_string(vault.join(concept)).unwrap();
            std::fs::write(&plan, serde_json::to_vec(&json!({
                "evidence":[{"path":source_alias,"revision":evidence["file_revisions"][source_alias]}],
                "concepts":[{"path":concept,"revision":file_revision(&current),"knowledge_status":"active","knowledge_review":review_alias}]
            })).unwrap()).unwrap();
            ok(
                &config,
                &["wiki", "curate", "--file", plan.to_str().unwrap()],
            );
            let audit = ok(&config, &["wiki", "audit-plan", "--path", concept]);
            let dependencies = audit["candidates"][0]["reviews"].as_array().unwrap();
            assert!(
                dependencies.iter().all(|review| review["state"] == "open"),
                "{kind}: {audit}"
            );
            assert!(
                dependencies
                    .iter()
                    .any(|review| review["path"] == review_alias)
            );
        }
        std::fs::remove_file(vault.join(source)).unwrap();
        let reports = ok(&config, &["wiki", "reviews"]);
        assert_eq!(reports[0]["effective_state"], "open", "{kind}: {reports}");
        assert_eq!(reports[0]["errors"], json!([]));
        assert_eq!(reports[0]["archived_evidence"], json!([source_alias]));
        let search = ok(&config, &["wiki", "search", "Topic", "--json"]);
        let canonical = search["hits"]
            .as_array()
            .unwrap()
            .iter()
            .find(|hit| hit["path"] == concept)
            .unwrap();
        assert_eq!(
            canonical["reviews"]["reviews"].as_array().unwrap().len(),
            expected,
            "{kind}: {search}"
        );
        let refused = run(&config, &["wiki", "prepare"]);
        assert!(!refused.status.success());
        let retained = ok(&config, &["wiki", "context", concept, "--snapshot", id]);
        assert_eq!(retained["reviews"]["total"], expected);
        std::fs::remove_file(vault.join(source_alias)).unwrap();
        let snapshot = ok(&config, &["wiki", "prepare"]);
        let context = ok(
            &config,
            &[
                "wiki",
                "context",
                concept,
                "--snapshot",
                snapshot["snapshot"].as_str().unwrap(),
            ],
        );
        assert_eq!(context["reviews"]["total"], expected, "{kind}: {context}");
        let audit = ok(&config, &["wiki", "audit-plan", "--path", concept]);
        for review in audit["candidates"][0]["reviews"].as_array().unwrap() {
            assert_eq!(review["state"], "open", "{kind}: {audit}");
            assert!(review["evidence"][0]["current"].is_null());
            assert!(review["evidence"][0]["archived"].is_object());
        }
        if kind == "leaf" {
            let outside = root.path().join("outside");
            std::fs::create_dir(&outside).unwrap();
            std::os::unix::fs::symlink(&outside, vault.join(".review-scope")).unwrap();
            let mut invalid = declared.clone();
            invalid["subjects"] = json!([concept, ".review-scope/unavailable.md"]);
            let invalid_content =
                format!("---\ntype: exploration\nreview: {invalid}\n---\nAn invalid scope.\n");
            let invalid_path = "wiki/explorations/invalid.md";
            let current = std::fs::read_to_string(vault.join(concept)).unwrap();
            std::fs::write(
                &plan,
                serde_json::to_vec(&json!({
                    "evidence":[{"path":concept,"revision":file_revision(&current)}],
                    "reviews":[{"path":invalid_path,"revision":null,"content":invalid_content}]
                }))
                .unwrap(),
            )
            .unwrap();
            let refused = run(
                &config,
                &["wiki", "curate", "--file", plan.to_str().unwrap()],
            );
            assert!(!refused.status.success());
            assert!(String::from_utf8_lossy(&refused.stderr).contains("escapes vault"));
            assert!(!vault.join(invalid_path).exists());
            std::fs::write(vault.join(invalid_path), invalid_content).unwrap();
            let reports = ok(&config, &["wiki", "reviews"]);
            let invalid = reports
                .as_array()
                .unwrap()
                .iter()
                .find(|review| review["path"] == invalid_path)
                .unwrap();
            assert_eq!(invalid["effective_state"], "invalid");
            assert!(!invalid["errors"].as_array().unwrap().is_empty());
            let search = ok(&config, &["wiki", "search", "Topic", "--json"]);
            assert!(
                search["hits"][0]["reviews"]["reviews"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|review| review["path"] == invalid_path
                        && review["effective_state"] == "invalid")
            );
            std::fs::write(
                vault.join("wiki/concepts/unrelated.md"),
                "---\ntype: concept\ntitle: Unrelated\n---\nAn independent claim.\n",
            )
            .unwrap();
            let audit = ok(
                &config,
                &["wiki", "audit-plan", "--path", "wiki/concepts/unrelated.md"],
            );
            assert_eq!(audit["candidates"][0]["reviews"], json!([]));
            ok(&config, &["wiki", "prepare"]);
        }
    }
}

#[test]
fn invalid_reviews_preserve_typed_subject_scope_across_discovery_and_audit() {
    let root = tempfile::tempdir().unwrap();
    let vault = root.path().join("vault");
    for directory in ["concepts", "documents", "explorations"] {
        std::fs::create_dir_all(vault.join("wiki").join(directory)).unwrap();
    }
    let config = root.path().join("config.yaml");
    std::fs::write(&config, "vault:\n  root: vault\n  locale: en\nidentity:\n  name: Test\n  email: test@example.com\nsources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: inbox\n").unwrap();
    let subject = "wiki/concepts/topic.md";
    let source = "wiki/documents/source.md";
    let review = "wiki/explorations/review.md";
    for (path, content) in [
        (subject, "---\ntype: concept\ntitle: Topic\n---\nA claim.\n"),
        (
            "wiki/concepts/unrelated.md",
            "---\ntype: concept\ntitle: Unrelated\n---\nAn independent claim.\n",
        ),
        (source, "---\ntype: document\n---\nSource evidence.\n"),
    ] {
        std::fs::write(vault.join(path), content).unwrap();
    }
    let evidence = ok(&config, &["wiki", "evidence", source]);
    let base = json!({"status":"open", "subjects":[subject], "evidence":evidence["citations"]});
    let mut cases = Vec::new();
    cases.push(("positional-scope", json!([[subject]]), 0));
    cases.push((
        "positional-review",
        json!(["open", [subject], evidence["citations"], null]),
        0,
    ));
    let mut invalid = base.clone();
    invalid["status"] = "unresolved".into();
    cases.push(("status", invalid, 1));
    let mut invalid = base.clone();
    invalid["unexpected"] = true.into();
    cases.push(("unknown-field", invalid, 1));
    let mut invalid = base.clone();
    invalid["evidence"] = json!([{"path":source,"revision":42}]);
    cases.push(("evidence", invalid, 1));
    let mut invalid = base.clone();
    invalid["subjects"] = subject.into();
    cases.push(("unreadable-scope", invalid, 0));
    let mut invalid = base.clone();
    invalid["status"] = "unresolved".into();
    invalid["subjects"] = json!([subject, "../escaped.md"]);
    cases.push(("invalid-address", invalid, 1));
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("topic.md", vault.join("wiki/concepts/alias.md")).unwrap();
        let mut invalid = base.clone();
        invalid["status"] = "unresolved".into();
        invalid["subjects"] = json!(["wiki/concepts/alias.md"]);
        cases.push(("symlink-scope", invalid, 1));
    }
    for (kind, metadata, expected) in cases {
        let content = format!(
            "---\ntype: exploration\ntitle: Assessment\nreview: {metadata}\n---\nReview prose may mention Topic without declaring it.\n"
        );
        std::fs::write(vault.join(review), content).unwrap();
        let reports = ok(&config, &["wiki", "reviews"]);
        assert_eq!(
            reports[0]["effective_state"], "invalid",
            "{kind}: {reports}"
        );
        assert!(reports[0]["review"].is_null());
        assert!(!reports[0]["errors"].as_array().unwrap().is_empty());
        let snapshot = ok(&config, &["wiki", "prepare"]);
        let id = snapshot["snapshot"].as_str().unwrap();
        for capture in [None, Some(id)] {
            let mut args = vec!["wiki", "search", "Topic", "--json"];
            if let Some(id) = capture {
                args.extend(["--snapshot", id]);
            }
            let search = ok(&config, &args);
            let hit = search["hits"]
                .as_array()
                .unwrap()
                .iter()
                .find(|hit| hit["path"] == subject)
                .unwrap();
            assert_eq!(hit["reviews"]["total"], expected, "{kind}: {search}");
            assert_eq!(hit["reviews"]["states"]["invalid"], expected);
        }
        let context = ok(&config, &["wiki", "context", subject, "--snapshot", id]);
        assert_eq!(context["reviews"]["total"], expected, "{kind}: {context}");
        assert_eq!(context["reviews"]["states"]["invalid"], expected);
        let audit = ok(&config, &["wiki", "audit-plan", "--path", subject]);
        let dependencies = audit["candidates"][0]["reviews"].as_array().unwrap();
        assert_eq!(dependencies.len(), expected, "{kind}: {audit}");
        for dependency in dependencies {
            assert_eq!(dependency["state"], "invalid");
            assert!(!dependency["errors"].as_array().unwrap().is_empty());
        }
        let unrelated = ok(
            &config,
            &["wiki", "audit-plan", "--path", "wiki/concepts/unrelated.md"],
        );
        assert_eq!(unrelated["candidates"][0]["reviews"], json!([]));
    }
}
