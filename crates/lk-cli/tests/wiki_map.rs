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

    // Re-derived, not merely present. Asserting only that a second run leaves the files unchanged
    // passes for an implementation that never rewrites an existing view at all — the vault has to
    // CHANGE between the runs, and every view has to follow it.
    let views = ["index.md", "log.md", "map.md"];
    let read_all = || -> Vec<String> {
        views
            .iter()
            .map(|page| std::fs::read_to_string(root.join("vault/wiki").join(page)).expect(page))
            .collect()
    };
    let before = read_all();
    std::fs::write(
        concepts.join("beta.md"),
        "---\nid: wiki/concepts/beta\ntype: concept\ntitle: Beta\ncreated: 2026-05-21\n---\n\
         ## Synthesis\nSees [Alpha](alpha.md).\n",
    )
    .expect("beta");
    assert!(run(root, &["wiki", "refresh"]).status.success());
    let after = read_all();
    for (page, (before, after)) in views.iter().zip(before.iter().zip(after.iter())) {
        assert_ne!(
            before, after,
            "{page} did not follow the vault it is derived from"
        );
        assert!(
            after.contains("Beta") || after.contains("beta"),
            "{page}:\n{after}"
        );
    }

    // And with the vault unchanged, a re-run is a true no-op.
    assert!(run(root, &["wiki", "refresh"]).status.success());
    assert_eq!(after, read_all(), "wiki refresh must be idempotent");
}

/// Every view is attempted even when an earlier one fails, and the command still exits non-zero.
///
/// The views are independent, and the scheduled pipeline is deliberately not `set -e` so that a
/// stage which cannot run does not decide for the stages after it. Collapsing three of its stages
/// into one command imported fail-fast semantics with it: the first `?` left the later views stale
/// on the very run that reported the failure.
#[test]
fn wiki_refresh_attempts_every_view_even_when_one_fails() {
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

    // A directory where the catalog belongs: the write fails and nothing about it is recoverable
    // by the other two, which must still run.
    std::fs::create_dir(
        root.join("vault/wiki")
            .join(lk_core::vault_path::INDEX_FILE),
    )
    .expect("blocking dir");

    let out = run(root, &["wiki", "refresh"]);
    assert!(
        !out.status.success(),
        "a view that could not be written is a failure"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(lk_core::vault_path::INDEX_FILE),
        "the failure must name the page:\n{stderr}"
    );
    for page in [
        lk_core::vault_path::TIMELINE_FILE,
        lk_core::vault_path::MAP_FILE,
    ] {
        assert!(
            root.join("vault/wiki").join(page).is_file(),
            "{page} was skipped because an earlier view failed"
        );
    }
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
