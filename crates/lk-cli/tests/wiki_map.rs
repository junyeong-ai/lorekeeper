//! End-to-end test of `lore wiki map` through the real binary: the navigation map is
//! written, lists concepts by citation cluster with valid relative links, is
//! byte-identical on re-run (a materialized view), and — because reserved meta-files are
//! excluded from the analysis graph — never lists itself even after it exists on disk.

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
fn wiki_map_writes_navigation_map_and_excludes_reserved_files() {
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
        concepts.join("alpha.md"),
        "---\nid: wiki/concepts/alpha\ntitle: Alpha\n---\n## Synthesis\nSee [Beta](beta.md).\n",
    )
    .expect("alpha");
    std::fs::write(
        concepts.join("beta.md"),
        "---\nid: wiki/concepts/beta\ntitle: Beta\n---\n## Synthesis\nSee [Alpha](alpha.md).\n",
    )
    .expect("beta");

    let out = run(root, &["wiki", "map"]);
    assert!(
        out.status.success(),
        "wiki map failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let map_path = root.join("vault/wiki/map.md");
    let content = std::fs::read_to_string(&map_path).expect("map.md written");
    assert!(
        content.contains("[alpha](concepts/alpha.md)"),
        "map lists concepts by relative link with leaf display:\n{content}"
    );
    assert!(
        content.contains("[beta](concepts/beta.md)"),
        "map lists both concepts:\n{content}"
    );

    // Re-run: map.md now exists in the wiki scope, but as a reserved meta-file it is
    // excluded from the graph — so it never lists itself and the output is byte-identical
    // (no self-feedback distorting the clusters).
    let out2 = run(root, &["wiki", "map"]);
    assert!(out2.status.success(), "second wiki map failed");
    let content2 = std::fs::read_to_string(&map_path).expect("map.md re-read");
    assert_eq!(
        content, content2,
        "map.md must be byte-identical on re-run (materialized view, no self-feedback)"
    );
    assert!(
        !content.contains("(map.md)"),
        "map.md must never list itself:\n{content}"
    );
}

/// `lore wiki refresh` re-derives every page the vault's contents determine, and that it derives
/// ALL of them is the whole reason it exists — five callers naming the commands one at a time is
/// what left the timeline refreshed by none of them. Asserted through the binary, because a
/// command whose body is replaced by `Ok(())` reports success either way.
#[test]
fn wiki_refresh_writes_every_page_derived_from_the_vault() {
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
        concepts.join("alpha.md"),
        "---\nid: wiki/concepts/alpha\ntype: concept\ntitle: Alpha\ncreated: 2026-05-20\n---\n\
         ## Synthesis\nA.\n",
    )
    .expect("alpha");

    let out = run(root, &["wiki", "refresh"]);
    assert!(
        out.status.success(),
        "wiki refresh failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Every page whose content the vault determines, and each carrying the concept that is in it.
    for page in ["index.md", "log.md", "map.md"] {
        let path = root.join("vault/wiki").join(page);
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("wiki refresh did not write {page}: {e}"));
        assert!(
            content.contains("Alpha") || content.contains("alpha"),
            "{page} does not reflect the vault it was derived from:\n{content}"
        );
    }

    // A materialized view: a second run is a no-op.
    let before: Vec<String> = ["index.md", "log.md", "map.md"]
        .iter()
        .map(|page| std::fs::read_to_string(root.join("vault/wiki").join(page)).expect(page))
        .collect();
    assert!(run(root, &["wiki", "refresh"]).status.success());
    let after: Vec<String> = ["index.md", "log.md", "map.md"]
        .iter()
        .map(|page| std::fs::read_to_string(root.join("vault/wiki").join(page)).expect(page))
        .collect();
    assert_eq!(before, after, "wiki refresh must be idempotent");
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
        "---\nid: wiki/concepts/rag\ntype: concept\ntitle: RAG\n\
         aliases: [\"RAG\", \"Retrieval Augmented Generation\"]\ncategory: technique\n\
         source_count: 3\n---\n\n## Synthesis\nR.\n",
    )
    .expect("rag");
    std::fs::write(
        concepts.join("embeddings.md"),
        "---\nid: wiki/concepts/embeddings\ntype: concept\ntitle: Embeddings\n---\n\n\
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
        .find(|entry| entry["slug"] == "wiki/concepts/rag")
        .unwrap_or_else(|| panic!("rag missing from the registry: {listed}"));
    assert_eq!(rag["title"], "RAG");
    assert_eq!(rag["category"], "technique");
    assert_eq!(rag["source_count"], 3);
    // The title is carried in `title`; `aliases` is the SYNONYMS beyond it, which is what makes an
    // alias-only surface form resolve to this page instead of forking one.
    assert_eq!(
        rag["aliases"],
        serde_json::json!(["Retrieval Augmented Generation"]),
        "aliases must exclude the title and keep the synonyms: {listed}"
    );

    let embeddings = entries
        .iter()
        .find(|entry| entry["slug"] == "wiki/concepts/embeddings")
        .unwrap_or_else(|| panic!("embeddings missing from the registry: {listed}"));
    assert_eq!(embeddings["aliases"], serde_json::json!([]));
    assert_eq!(embeddings["source_count"], 0);
}
