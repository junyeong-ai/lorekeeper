//! `lore resolve` — the read side of the vault's concept identity.
//!
//! The pipeline routes an extraction to the page that owns its name; this answers the same
//! question from outside a run, so a caller about to write a concept page can ask it instead
//! of guessing. The exit code IS the answer, and each of the three is a different decision,
//! so each is pinned here.

use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

fn lore() -> Command {
    Command::new(env!("CARGO_BIN_EXE_lore"))
}

fn run(root: &Path, name: &str) -> Output {
    lore()
        .args(["resolve", "--root"])
        .arg(root)
        .arg(name)
        .output()
        .expect("lore runs")
}

fn write_concept(root: &Path, slug: &str, title: &str, aliases: &[&str]) {
    let dir = root.join("wiki").join("concepts");
    std::fs::create_dir_all(&dir).expect("concepts dir");
    let aliases = serde_json::to_string(aliases).expect("aliases");
    std::fs::write(
        dir.join(format!("{slug}.md")),
        format!(
            "---\nid: {slug}\ntype: concept\ntitle: \"{title}\"\naliases: {aliases}\n\
             source_count: 0\nllm_inputs:\n---\n\n# {title}\n\n## Synthesis\n\nA concept.\n\n\
             ## Sources\n\n## Related\n"
        ),
    )
    .expect("concept page");
}

/// One page, reached by every spelling of the one name it answers to. The fold is the
/// pipeline's — separators are typography, so a citation written any of these ways addresses
/// the page that already exists rather than minting a rival beside it.
#[test]
fn every_spelling_of_an_owned_name_reaches_its_page() {
    let dir = TempDir::new().expect("tempdir");
    write_concept(
        dir.path(),
        "retrieval-augmented-generation",
        "Retrieval-Augmented Generation",
        &["RAG"],
    );

    for name in [
        "retrieval-augmented-generation",
        "Retrieval-Augmented Generation",
        "retrieval augmented generation",
        "RAG",
        "rag",
    ] {
        let out = run(dir.path(), name);
        let stdout = String::from_utf8(out.stdout).expect("utf8");
        assert_eq!(out.status.code(), Some(0), "`{name}`: {stdout}");
        assert!(
            stdout.starts_with("retrieval-augmented-generation\t"),
            "`{name}`: {stdout}"
        );
    }
}

/// A name nothing answers to exits 1 — the answer that makes writing a page correct, and the
/// reason it is not exit 0: a caller branching on success must not read "absent" as "found".
#[test]
fn a_name_no_page_answers_to_exits_one() {
    let dir = TempDir::new().expect("tempdir");
    write_concept(dir.path(), "rag", "RAG", &[]);

    let out = run(dir.path(), "Pinecone");
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8(out.stdout).expect("utf8").is_empty());
}

/// The fold is spelling, never meaning. `k8s` and `kubernetes` are different names, and
/// answering otherwise would route an extraction onto a page a reviewer never chose.
#[test]
fn two_different_names_for_one_thing_are_two_names() {
    let dir = TempDir::new().expect("tempdir");
    write_concept(dir.path(), "kubernetes", "Kubernetes", &[]);

    assert_eq!(run(dir.path(), "k8s").status.code(), Some(1));
    assert_eq!(run(dir.path(), "kubernetes").status.code(), Some(0));
}

/// Two pages answering to one name is the vault defect `lore graph lint` reports. A citation
/// written now still has to land somewhere, so the answer names both claimants AND which one
/// it would land on — the exit code separates it from an ordinary hit precisely because a
/// caller must not quietly build on an arbitrary choice.
#[test]
fn a_name_two_pages_answer_to_names_both_and_the_one_a_citation_reaches() {
    let dir = TempDir::new().expect("tempdir");
    write_concept(dir.path(), "doc-hub", "Doc Hub", &[]);
    write_concept(dir.path(), "docs-portal", "Docs Portal", &["Doc Hub"]);

    let out = run(dir.path(), "Doc Hub");
    let stderr = String::from_utf8(out.stderr).expect("utf8");
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("doc-hub"), "{stderr}");
    assert!(stderr.contains("docs-portal"), "{stderr}");

    let out = lore()
        .args(["resolve", "--json", "--root"])
        .arg(dir.path())
        .arg("Doc Hub")
        .output()
        .expect("lore runs");
    let parsed: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("--json emits an object");
    assert_eq!(parsed["verdict"], "ambiguous");
    assert_eq!(
        parsed["routed"]["slug"], "doc-hub",
        "a page's own address outranks another page's alias: {parsed}"
    );
    assert_eq!(parsed["claimants"].as_array().expect("array").len(), 2);
}

/// A page whose frontmatter will not parse still answers to its filename. Reporting `absent`
/// for a name the vault holds is the one answer that leads a caller to write a second page,
/// so the address — which parsing has no say in — survives a body that does not.
#[test]
fn a_page_that_will_not_parse_still_answers_to_its_address() {
    let dir = TempDir::new().expect("tempdir");
    let concepts = dir.path().join("wiki").join("concepts");
    std::fs::create_dir_all(&concepts).expect("concepts dir");
    std::fs::write(
        concepts.join("vector-db.md"),
        "---\ntitle: [unclosed\n---\n\n# Vector DB\n",
    )
    .expect("concept page");

    let out = run(dir.path(), "VectorDB");
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    assert!(stdout.starts_with("vector-db\t"), "{stdout}");
}

/// An empty vault answers `absent` rather than failing: there is no concepts directory before
/// the first ingest, and a caller asking whether a name is taken is exactly the caller who
/// runs then.
#[test]
fn a_vault_with_no_concepts_yet_answers_absent() {
    let dir = TempDir::new().expect("tempdir");
    assert_eq!(run(dir.path(), "anything").status.code(), Some(1));
}
