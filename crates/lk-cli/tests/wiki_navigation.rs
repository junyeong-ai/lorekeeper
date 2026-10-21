//! SQLite navigation replaces Markdown exports; concept identity inventory remains complete.
use std::process::{Command, Output};

fn run(root: &std::path::Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_lore"));
    // Hermetic: depend only on the test's --config, never an ambient LORE_* env var.
    for (key, _) in std::env::vars() {
        if key.starts_with("LORE_") {
            cmd.env_remove(key);
        }
    }
    cmd.arg("--config")
        .arg(root.join("config.yaml"))
        .args(args)
        .output()
        .expect("spawn lore")
}

#[test]
fn declared_documents_do_not_become_concepts_through_their_directory() {
    let root = tempfile::tempdir().unwrap();
    let vault = root.path().join("vault");
    let concepts = vault.join("wiki/concepts");
    std::fs::create_dir_all(&concepts).unwrap();
    std::fs::write(root.path().join("config.yaml"), "vault:\n  root: vault\n  locale: en\nidentity: {name: Test, email: test@example.invalid}\nsources:\n  notes: {type: manual, params: {inbox_dir: inbox}}\n").unwrap();
    let document = "---\ntype: document\ntitle: Report\naliases: [DocumentAlias, Legacy]\nsource_count: 77\n---\n## Synthesis\nAn authored document statement about [Legacy](legacy.md).\n\n## Sources\n\n";
    std::fs::write(concepts.join("report.md"), document).unwrap();
    std::fs::write(concepts.join("legacy.md"), "---\ntitle: Legacy\naliases: [LegacyAlias]\nsource_count: 0\n---\n## Synthesis\nA legacy definition.\n\n## Sources\n\n").unwrap();
    let json = |args: &[&str]| {
        let output = run(root.path(), args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    for name in ["Report", "DocumentAlias"] {
        let output = run(root.path(), &["resolve", name, "--json"]);
        assert_eq!(output.status.code(), Some(1));
        let answer: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(answer["verdict"], "absent");
    }
    assert_eq!(json(&["resolve", "Legacy", "--json"])["slug"], "legacy");
    let listed = json(&["wiki", "concepts", "--json"]);
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["slug"], "legacy");
    let lint = json(&["graph", "--json", "lint"]);
    assert_eq!(
        lint["data"]["violations"]["duplicate_concepts"],
        serde_json::json!([])
    );
    let captured = json(&["wiki", "prepare"]);
    for snapshot in [None, captured["snapshot"].as_str()] {
        let mut args = vec!["wiki", "search", "Report", "--json"];
        if let Some(snapshot) = snapshot {
            args.extend(["--snapshot", snapshot]);
        }
        let search = json(&args);
        assert_eq!(search["total"], 1);
        assert_eq!(search["hits"][0]["synthesis"]["status"], "not-applicable");
    }
    let synced = json(&["graph", "--json", "backlinks-sync"]);
    assert_eq!(synced["data"]["skipped"], serde_json::json!([]));
    assert!(
        synced["data"]["updated"]
            .as_array()
            .unwrap()
            .iter()
            .all(|page| page["path"] != "wiki/concepts/report.md")
    );
    assert_eq!(
        std::fs::read_to_string(concepts.join("report.md")).unwrap(),
        document
    );
    assert!(
        std::fs::read_to_string(concepts.join("legacy.md"))
            .unwrap()
            .contains("lorekeeper:concept-sources:v1")
    );
}

#[test]
fn removed_markdown_export_commands_are_rejected_without_writes() {
    let dir = tempfile::tempdir().unwrap();
    for args in [
        vec!["wiki", "index"],
        vec!["wiki", "log"],
        vec!["wiki", "map"],
        vec!["graph", "index-sync"],
        vec!["graph", "index-sync", "--fix"],
        vec!["wiki", "refresh", "--archive-replace", "wiki/index.md=old"],
    ] {
        let out = run(dir.path(), &args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        let error = String::from_utf8_lossy(&out.stderr);
        assert!(
            error.contains("unrecognized subcommand") || error.contains("unexpected argument"),
            "{args:?}: {error}"
        );
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    }
}

#[test]
fn authored_guide_lookalikes_remain_graph_nodes_and_broken_link_sources() {
    let root = tempfile::tempdir().unwrap();
    let wiki = root.path().join("vault/wiki");
    std::fs::create_dir_all(&wiki).unwrap();
    std::fs::write(root.path().join("config.yaml"), "vault:\n  root: vault\nidentity:\n  name: T\n  email: t@example.com\nsources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: inbox\n").unwrap();
    for name in ["index.md", "AGENTS.md", "index!.md", "AGENTS!.md"] {
        std::fs::write(
            wiki.join(name),
            "---\ntype: document\ntitle: Authored\n---\nSee [Missing](missing.md).\n",
        )
        .unwrap();
    }
    // A real same-file alias of a configured guide remains operational navigation.
    #[cfg(unix)]
    std::os::unix::fs::symlink("index.md", wiki.join("guide-alias.md")).unwrap();
    let exported = run(root.path(), &["graph", "--json", "export"]);
    assert!(exported.status.success());
    let exported: serde_json::Value = serde_json::from_slice(&exported.stdout).unwrap();
    let ids: Vec<_> = exported["data"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| node["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["wiki/AGENTS!", "wiki/index!"]);
    let broken = run(root.path(), &["graph", "--json", "broken"]);
    assert_eq!(broken.status.code(), Some(1));
    let broken: serde_json::Value = serde_json::from_slice(&broken.stdout).unwrap();
    let sources: Vec<_> = broken["data"]["broken"]
        .as_array()
        .unwrap()
        .iter()
        .map(|link| link["source"].as_str().unwrap())
        .collect();
    assert_eq!(sources, ["wiki/AGENTS!", "wiki/index!"]);
}

#[cfg(unix)]
#[test]
fn source_directory_aliases_do_not_turn_guides_into_citation_evidence() {
    let root = tempfile::tempdir().unwrap();
    let vault = root.path().join("vault");
    let wiki = vault.join("wiki");
    std::fs::create_dir_all(wiki.join("concepts")).unwrap();
    std::fs::write(root.path().join("config.yaml"), "vault:\n  root: vault\n  dirs:\n    daily: observations\nidentity:\n  name: T\n  email: t@example.com\nsources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: inbox\ngraph:\n  scope:\n    follow_links: true\n").unwrap();
    std::os::unix::fs::symlink("wiki", vault.join("observations")).unwrap();
    let concept = "---\ntype: concept\ntitle: Topic\nsource_count: 0\n---\n# Topic\n\n## Synthesis\n\n## Sources\n\n## Related\n";
    std::fs::write(wiki.join("concepts/topic.md"), concept).unwrap();
    for name in ["index.md", "AGENTS.md", "index!.md"] {
        std::fs::write(
            wiki.join(name),
            format!(
                "---\ntype: document\ntitle: {name}\n---\n[Topic](../wiki/concepts/topic.md)\n"
            ),
        )
        .unwrap();
    }
    std::os::unix::fs::symlink("index.md", wiki.join("guide-alias.md")).unwrap();
    for rss_metadata in [false, true] {
        if rss_metadata {
            // Operational guides are not RSS origins merely because a source walk
            // reaches them and they contain a field with an evidence-like name.
            std::fs::write(wiki.join("AGENTS.md"), "---\ntype: schema\nconcept_evidence: not-rss-evidence\n---\n[Topic](../wiki/concepts/topic.md)\n").unwrap();
        }
        let output = run(
            root.path(),
            &["graph", "--json", "backlinks-sync", "--dry-run"],
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let output: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            output["data"]["updated"][0]["added"],
            serde_json::json!(["observations/index!"])
        );
        let citations = output["data"]["resynthesize"][0]["citations"]
            .as_array()
            .unwrap();
        assert_eq!(citations.len(), 1);
        assert_eq!(citations[0]["path"], "observations/index!.md");
    }
    assert_eq!(
        std::fs::read_to_string(wiki.join("concepts/topic.md")).unwrap(),
        concept
    );
}

/// `lore wiki concepts --json` is the dedup baseline `/lore-process` loads at run start: it matches
/// a surface form against these slugs, titles and ALIASES to decide whether an extraction names an
/// existing concept or a new one. An empty or partial answer forks a duplicate page for a concept
/// the vault already holds, which is the one outcome the convergence contract exists to prevent —
/// so the registry is asserted through the binary, entry by entry.
#[test]
fn wiki_concepts_lists_every_concept_with_the_aliases_dedup_needs() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let root = dir.path();
    let concepts = root.join("vault/wiki/concepts");
    std::fs::create_dir_all(&concepts).expect("concepts dir");
    std::fs::create_dir_all(root.join("vault/inbox")).expect("inbox dir");
    std::fs::write(
        root.join("config.yaml"),
        "vault:\n  root: vault\nidentity:\n  name: T\n  email: t@e.com\n\
         sources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: inbox\n",
    )
    .expect("config");
    std::fs::write(
        concepts.join("rag.md"),
        "---\nid: rag\ntype: concept\ntitle: RAG\n\
         aliases: [\"RAG\", \"Retrieval Augmented Generation\", \"retrieval-augmented generation\"]\n\
         category: technique\n\
         source_count: 3\n---\n\n## Synthesis\nR.\n",
    )
    .expect("rag");
    std::fs::write(
        concepts.join("embeddings.md"),
        "---\nid: embeddings\ntype: concept\ntitle: Embeddings\n---\n\n\
         ## Synthesis\nE.\n",
    )
    .expect("embeddings");

    let out = run(root, &["wiki", "concepts", "--json"]);
    assert!(
        out.status.success(),
        "wiki concepts failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let listed: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("--json must emit a JSON array");
    let entries = listed.as_array().expect("a JSON array");
    assert_eq!(
        entries.len(),
        2,
        "every concept page is a registry entry: {listed}"
    );

    let rag = entries
        .iter()
        .find(|entry| entry["slug"] == "rag")
        .unwrap_or_else(|| panic!("rag missing from the registry: {listed}"));
    assert_eq!(rag["title"], "RAG");
    assert_eq!(rag["category"], "technique");
    assert_eq!(rag["source_count"], 3);
    // The title is carried in `title`; `aliases` is the SYNONYMS beyond it, which is what makes an
    // alias-only surface form resolve to this page instead of forking one.
    // Two synonyms, so a collection truncated to one is visible — with a single alias, `.take(1)`
    // was indistinguishable from keeping them all.
    assert_eq!(
        rag["aliases"],
        serde_json::json!([
            "Retrieval Augmented Generation",
            "retrieval-augmented generation"
        ]),
        "aliases must exclude the title and keep every synonym: {listed}"
    );

    let embeddings = entries
        .iter()
        .find(|entry| entry["slug"] == "embeddings")
        .unwrap_or_else(|| panic!("embeddings missing from the registry: {listed}"));
    assert_eq!(embeddings["aliases"], serde_json::json!([]));
    assert_eq!(embeddings["source_count"], 0);
}

/// The same registry, refusing rather than under-reporting. A page it cannot parse is a concept
/// the caller will not see, and `/lore-process` loads this as its dedup baseline — so an answer
/// that silently omits one has the drain mint a second page for a concept the vault already
/// holds, or overwrite the one it could not read. A `tracing::warn` left a JSON array that
/// looked complete, which is the worst of the three outcomes.
#[test]
fn wiki_concepts_refuses_a_registry_it_could_not_read_completely() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let root = dir.path();
    let concepts = root.join("vault/wiki/concepts");
    std::fs::create_dir_all(&concepts).expect("concepts dir");
    std::fs::create_dir_all(root.join("vault/inbox")).expect("inbox dir");
    std::fs::write(
        root.join("config.yaml"),
        "vault:\n  root: vault\nidentity:\n  name: T\n  email: t@e.com\n\
         sources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: inbox\n",
    )
    .expect("config");
    std::fs::write(
        concepts.join("good.md"),
        "---\nid: wiki/concepts/good\ntype: concept\ntitle: Good\n\
         created: 2026-01-01\nupdated: 2026-01-01\nsource_count: 0\n---\n\n## Synthesis\n",
    )
    .expect("good");
    // Unclosed frontmatter: parseable as text, not as a page.
    std::fs::write(
        concepts.join("broken.md"),
        "---\nid: wiki/concepts/broken\ntype: concept\ntitle: Broken\n",
    )
    .expect("broken");

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_lore"))
        .arg("--config")
        .arg(root.join("config.yaml"))
        .args(["wiki", "concepts", "--json"])
        .output()
        .expect("spawn lore");

    let stdout = String::from_utf8(out.stdout).expect("utf8");
    let stderr = String::from_utf8(out.stderr).expect("utf8");
    assert!(
        !out.status.success(),
        "an incomplete registry must not read as a complete one\nstdout: {stdout}"
    );
    assert!(
        stderr.contains("broken.md"),
        "the refusal must name the page that could not be read\n{stderr}"
    );
    assert!(
        !stdout.contains("wiki/concepts/good"),
        "no partial array may be emitted alongside the refusal\n{stdout}"
    );
}
