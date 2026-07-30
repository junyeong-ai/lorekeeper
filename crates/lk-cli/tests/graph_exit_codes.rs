//! `lore graph`'s exit code answers exactly one question: does every claim the vault makes hold?
//!
//! It is the only machine-readable verdict the command family offers, and the scheduled pipeline
//! records it as a stage outcome, so a red night has to mean something a caller can act on. A
//! concept nothing cites yet, and a contradiction between sources that an audit deliberately
//! recorded, are both TRUE statements about a healthy vault: every extraction mints concepts
//! before anything cites them, so a verdict that counted those would never be clean and would
//! carry no information at all — at which point a link pointing at nothing is ignored along with
//! them.

use std::path::PathBuf;
use std::process::{Command, Output};

/// A shipped caller that discards the exit code makes every violation invisible again, so the
/// property is checked by RUNNING the shipped script rather than by reading it.
///
/// Reading it cannot establish this. A grep for `|| true` beside a `lore` call misses the same
/// thing spelled without the space, `; true`, `|| log "…"`, a line continuation, a
/// `soft() { "$@" || true; }` wrapper whose two halves are individually clean, a bare call that
/// never goes through `run()`, and the literal moved to another file — while failing the build on
/// a COMMENT containing the words. Executing `sync_graph` against a vault with one broken link is
/// indifferent to spelling: every way of losing the code produces one visible symptom, a pipeline
/// that reports success.
#[cfg(unix)]
#[test]
fn the_shipped_pipeline_fails_when_the_vault_contradicts_itself() {
    let ws = sound_vault();
    // One broken link on a daily page, which is where the pipeline's own output puts them.
    ws.write(
        "daily/notes/2026-05-24.md",
        "---\nid: notes-2026-05-24\ntype: daily\ntitle: \"Notes\"\n\
         created: 2026-05-24\nupdated: 2026-05-24\n---\n\n\
         ## Related concepts\n\n- [Ghost](../../wiki/concepts/ghost.md)\n",
    );

    let clean = ws.run_pipeline();
    assert!(
        !clean.status.success(),
        "a broken link must fail the pipeline, not just print\n{}",
        String::from_utf8_lossy(&clean.stdout)
    );
    let log = String::from_utf8_lossy(&clean.stdout).to_string();
    assert!(log.contains("✗ graph lint"), "stage not recorded\n{log}");
    assert!(
        log.contains("done with failures: graph lint"),
        "failure not carried to the pipeline's own verdict\n{log}"
    );
}

/// The other half: a vault whose only findings are observations must leave the pipeline green.
/// Every extraction mints concepts before anything cites them, so a pipeline that failed on those
/// would be red every night and its verdict would mean nothing.
#[cfg(unix)]
#[test]
fn the_shipped_pipeline_passes_on_a_vault_whose_findings_are_observations() {
    let ws = sound_vault();
    let out = ws.run_pipeline();
    let log = String::from_utf8_lossy(&out.stdout).to_string();

    assert!(
        out.status.success(),
        "an uncited concept and an open conflict must not fail the pipeline\n{log}"
    );
    assert!(log.contains("✓ graph lint"), "{log}");
    assert!(log.contains("done — all stages ok"), "{log}");
}

struct Workspace {
    root: tempfile::TempDir,
}

impl Workspace {
    fn new() -> Self {
        let root = tempfile::TempDir::new().expect("tempdir");
        std::fs::create_dir_all(root.path().join("vault")).expect("vault dir");
        std::fs::write(
            root.path().join("config.yaml"),
            "vault:\n  root: vault\n  locale: en\n\
             identity:\n  name: Tester\n  email: tester@example.com\n\
             sources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: inbox\n",
        )
        .expect("config");
        Self { root }
    }

    fn write(&self, rel: &str, content: &str) {
        let path = self.root.path().join("vault").join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, content).expect("write");
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_lore"));
        for (key, _) in std::env::vars() {
            if key.starts_with("LORE_") {
                cmd.env_remove(key);
            }
        }
        cmd.arg("--config")
            .arg(self.root.path().join("config.yaml"))
            .args(args)
            .output()
            .expect("spawn lore")
    }

    fn code(&self, args: &[&str]) -> i32 {
        self.run(args).status.code().expect("exit code")
    }

    fn stdout(&self, args: &[&str]) -> String {
        String::from_utf8(self.run(args).stdout).expect("utf8")
    }

    /// The SHIPPED `sync_graph`: the real `scripts/lore-pipeline.sh`, sourced the way the
    /// scheduled jobs source it, so what is under test is the file that ships.
    #[cfg(unix)]
    fn run_pipeline(&self) -> Output {
        let script =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/lore-pipeline.sh");
        Command::new("bash")
            .arg("-c")
            .arg(format!(
                "source {}\npipeline_start\nsync_graph\npipeline_finish",
                script.display()
            ))
            .env("LORE_BIN", env!("CARGO_BIN_EXE_lore"))
            .env("LORE_CONFIG", self.root.path().join("config.yaml"))
            .output()
            .expect("spawn bash")
    }
}

fn concept(id: &str, title: &str, body: &str) -> String {
    format!(
        "---\nid: {id}\ntype: concept\ntitle: \"{title}\"\ncategory: \"\"\n\
         created: 2026-05-23\nupdated: 2026-05-23\nsource_count: 1\n---\n\n\
         ## Synthesis\n\n{body}\n\n## Sources\n\n## Related\n"
    )
}

/// A vault holding both observation channels and no violation: one concept nothing cites,
/// and one cited concept carrying an open `> [!conflict]` callout.
fn sound_vault() -> Workspace {
    let ws = Workspace::new();
    ws.write(
        "wiki/concepts/cited.md",
        &concept(
            "cited",
            "Cited",
            "A concept.\n\n> [!conflict] two sources disagree about the default",
        ),
    );
    ws.write(
        "wiki/concepts/uncited.md",
        &concept("uncited", "Uncited", "Nothing points here yet."),
    );
    ws.write(
        "daily/notes/2026-05-23.md",
        "---\nid: notes-2026-05-23\ntype: daily\ntitle: \"Notes\"\n\
         created: 2026-05-23\nupdated: 2026-05-23\n---\n\n\
         ## Related concepts\n\n- [Cited](../../wiki/concepts/cited.md)\n",
    );
    assert_eq!(ws.code(&["wiki", "index"]), 0, "index must be in sync");
    ws
}

#[test]
fn an_uncited_concept_and_an_open_conflict_are_reported_and_exit_zero() {
    let ws = sound_vault();
    let out = ws.run(&["graph", "lint"]);
    let stdout = String::from_utf8(out.stdout).expect("utf8");

    assert_eq!(
        out.status.code(),
        Some(0),
        "an uncited concept and a recorded disagreement are both true of a healthy vault\n{stdout}"
    );
    // Reported, not suppressed: only the verdict differs.
    assert!(stdout.contains("uncited"), "orphan not listed\n{stdout}");
    assert!(
        stdout.contains("two sources disagree about the default"),
        "conflict not listed\n{stdout}"
    );
    assert!(stdout.contains("No violations"), "{stdout}");
}

#[test]
fn a_link_to_a_page_that_does_not_exist_exits_one() {
    let ws = sound_vault();
    // Edited in place: a page the catalog has not seen yet is its own violation, and this must
    // fail for the broken link alone.
    ws.write(
        "wiki/concepts/uncited.md",
        &concept(
            "uncited",
            "Uncited",
            "Nothing points here yet.\n\n- [Ghost](ghost.md)",
        ),
    );
    let out = ws.run(&["graph", "lint"]);
    let stdout = String::from_utf8(out.stdout).expect("utf8");

    assert_eq!(
        out.status.code(),
        Some(1),
        "broken link must gate\n{stdout}"
    );
    assert!(stdout.contains("Violations"), "{stdout}");
    assert!(stdout.contains("ghost"), "{stdout}");
}

/// `--json` carries every channel field even when the list is EMPTY, because a consumer indexes
/// into it: `/lore-wiki audit` layer 1 reads these exact paths and surfaces each non-empty list.
/// A `skip_serializing_if` would turn "no broken links" into a missing key, which reads as
/// neither empty nor absent at the other end, and the unit tests cannot see it — their fixtures
/// populate every list.
#[test]
fn the_json_report_carries_every_channel_field_on_a_clean_vault() {
    let ws = sound_vault();
    let raw = ws.stdout(&["graph", "--json", "lint"]);
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
    let data = &parsed["data"];

    for path in [
        "violations.broken",
        "violations.invalid_categories",
        "violations.duplicate_concepts",
        "violations.index.missing_from_index",
        "violations.index.missing_from_disk",
        "observations.orphans",
        "observations.hubs",
        "observations.unresolved_conflicts",
    ] {
        let mut cursor = data;
        for part in path.split('.') {
            cursor = &cursor[part];
        }
        assert!(
            cursor.is_array(),
            "`{path}` must be an array even when empty — got {cursor:?}\n{raw}"
        );
    }
    // The channels themselves, so a rename of either is not silently absorbed by the loop above.
    assert!(data["violations"].is_object(), "{raw}");
    assert!(data["observations"].is_object(), "{raw}");
}

#[test]
fn a_broken_link_written_outside_the_analysis_scope_still_gates() {
    let ws = sound_vault();
    // `graph.scope.dirs` defaults to the wiki and chooses the analysis subgraph only, so a link
    // written on a daily page — where `queue apply` writes concept links — is checked like any
    // other. A new daily page brings no index drift of its own, so this gates on the link alone.
    ws.write(
        "daily/notes/2026-05-24.md",
        "---\nid: notes-2026-05-24\ntype: daily\ntitle: \"Notes\"\n\
         created: 2026-05-24\nupdated: 2026-05-24\n---\n\n\
         ## Related concepts\n\n- [Ghost](../../wiki/concepts/ghost.md)\n",
    );

    let out = ws.run(&["graph", "lint"]);
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert_eq!(
        out.status.code(),
        Some(1),
        "a link is broken wherever it was written\n{stdout}"
    );
    assert!(
        stdout.contains("daily/notes/2026-05-24 -> wiki/concepts/ghost"),
        "{stdout}"
    );
}

/// `graph.scope.exclude` narrows the ANALYSIS, not the vault: an excluded page still exists, so
/// a link to it resolves. Both halves are asserted, because applying the globs to the universe
/// instead reports a link to the page as broken, and dropping them from the node set instead
/// silently un-excludes it.
#[test]
fn an_excluded_page_still_exists_but_is_not_analysed() {
    let ws = Workspace::new();
    std::fs::write(
        ws.root.path().join("config.yaml"),
        "vault:\n  root: vault\n  locale: en\n\
         identity:\n  name: Tester\n  email: tester@example.com\n\
         sources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: inbox\n\
         graph:\n  scope:\n    exclude: [\"wiki/concepts/excluded.md\"]\n",
    )
    .expect("config");
    ws.write(
        "wiki/concepts/cites.md",
        &concept("cites", "Cites", "Links it: [Excluded](excluded.md)"),
    );
    ws.write(
        "wiki/concepts/excluded.md",
        &concept("excluded", "Excluded", "Out of the analysis."),
    );
    assert_eq!(ws.code(&["wiki", "index"]), 0);

    let raw = ws.stdout(&["graph", "--json", "lint"]);
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
    let data = &parsed["data"];
    assert_eq!(
        data["violations"]["broken"],
        serde_json::json!([]),
        "the excluded page is on disk, so the link to it resolves\n{raw}"
    );
    assert_eq!(
        data["pages"], 1,
        "the excluded page must still be out of the graph\n{raw}"
    );
}

#[test]
fn a_category_outside_the_configured_vocabulary_exits_one() {
    let ws = sound_vault();
    std::fs::write(
        ws.root.path().join("config.yaml"),
        "vault:\n  root: vault\n  locale: en\n\
         identity:\n  name: Tester\n  email: tester@example.com\n\
         sources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: inbox\n\
         concepts:\n  categories:\n    - id: tool\n      label: Tool\n",
    )
    .expect("config");
    ws.write(
        "wiki/concepts/uncited.md",
        &concept("uncited", "Uncited", "Nothing points here yet.")
            .replace("category: \"\"", "category: invented"),
    );

    let out = ws.run(&["graph", "lint"]);
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert_eq!(
        out.status.code(),
        Some(1),
        "an invented category must gate\n{stdout}"
    );
    assert!(stdout.contains("invented"), "{stdout}");
}

#[test]
fn a_catalog_that_disagrees_with_the_disk_exits_one() {
    let ws = sound_vault();
    ws.write(
        "wiki/concepts/added-after-the-index.md",
        &concept(
            "added-after-the-index",
            "Added",
            "Written after `wiki index` ran.",
        ),
    );

    let out = ws.run(&["graph", "lint"]);
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert_eq!(
        out.status.code(),
        Some(1),
        "index drift must gate\n{stdout}"
    );
    assert!(stdout.contains("+index"), "{stdout}");
}

#[test]
fn one_name_answering_to_two_pages_exits_one() {
    let ws = sound_vault();
    ws.write(
        "wiki/concepts/vector-db.md",
        &concept("vector-db", "Vector DB", "One."),
    );
    ws.write(
        "wiki/concepts/vectordb.md",
        &concept("vectordb", "VectorDB", "The other."),
    );
    // Re-catalog first, so this fails for the name collision alone.
    assert_eq!(ws.code(&["wiki", "index"]), 0);

    let out = ws.run(&["graph", "lint"]);
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert_eq!(
        out.status.code(),
        Some(1),
        "one name on two pages must gate\n{stdout}"
    );
    assert!(stdout.contains("vector-db ~ vectordb"), "{stdout}");
}

#[test]
fn the_single_check_commands_agree_with_lint_about_their_own_channel() {
    let ws = sound_vault();
    // `orphans` reports the same list `lint` puts in its observation channel, so it reaches the
    // same verdict: asking for a list is not discovering a defect.
    let stdout = ws.stdout(&["graph", "orphans"]);
    assert!(stdout.contains("uncited"), "{stdout}");
    assert_eq!(ws.code(&["graph", "orphans"]), 0, "{stdout}");
    assert_eq!(ws.code(&["graph", "broken"]), 0, "{stdout}");
}

#[test]
fn a_concept_due_for_re_audit_is_a_worklist_and_exits_zero() {
    let ws = sound_vault();
    // Multiply cited, never audited (no `audited_sources_hash`) — the worklist's whole
    // population. It is read as JSON, so a non-zero exit would only stop `set -e` callers.
    ws.write(
        "wiki/concepts/cited.md",
        &concept("cited", "Cited", "A concept.").replace("source_count: 1", "source_count: 2"),
    );

    let out = ws.run(&["graph", "audit-candidates"]);
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert!(stdout.contains("cited"), "worklist empty\n{stdout}");
    assert_eq!(out.status.code(), Some(0), "{stdout}");
}
