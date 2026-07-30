//! `lore graph`'s exit code answers exactly one question: does every claim the vault makes
//! hold?
//!
//! It is the only machine-readable verdict the command family offers, and the scheduled
//! pipeline records it as a stage outcome — so a red night has to mean something a caller can
//! act on. A concept nothing cites yet, and a contradiction between sources that an audit
//! deliberately recorded, are both TRUE statements about a healthy vault: every extraction
//! mints concepts before anything cites them, so counting those makes the exit code
//! permanently non-zero and it stops carrying information. That is what it used to do, and it
//! is why every caller wrapped the command in `|| true` and every skill had to name the lists
//! its reader should ignore — at which point a link pointing at nothing was ignored along with
//! them.

use std::path::PathBuf;
use std::process::{Command, Output};

/// A shipped caller that discards the exit code puts the vault back where this contract found
/// it: every violation invisible. `|| true` was how the pipeline coped with a lint that could
/// never be clean, and it is the one thing that must not come back now that a non-zero exit
/// names a real contradiction — if a stage genuinely should not gate, the honest expression is
/// not to run it as a stage.
#[test]
fn no_shipped_pipeline_discards_a_lore_exit_code() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts");
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("read scripts: {e}")) {
        let path = entry.expect("dir entry").path();
        let name = path.file_name().expect("file name").to_string_lossy();
        if !name.starts_with("lore-") || path.extension().is_none_or(|ext| ext != "sh") {
            continue;
        }
        checked += 1;
        let body = std::fs::read_to_string(&path).expect("read script");
        for (n, line) in body.lines().enumerate() {
            let call = line.contains("lore_cmd") || line.contains("\"$LORE\"");
            let discarded = line.contains("|| true") || line.contains("|| :");
            assert!(
                !(call && discarded),
                "{}:{} suppresses a `lore` exit code: {}",
                name,
                n + 1,
                line.trim()
            );
        }
    }
    assert!(
        checked > 0,
        "no pipeline scripts found under {}",
        dir.display()
    );
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
    // Reported, not suppressed — the exit code is the only thing that changed.
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
    // Edited in place rather than added as a new page: a page the catalog has not seen yet is
    // its own violation, and this test must fail for the broken link alone.
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

#[test]
fn a_broken_link_written_outside_the_analysis_scope_still_gates() {
    let ws = sound_vault();
    // `graph.scope.dirs` defaults to the wiki, and while broken links were a `WikiGraph`
    // property they were only looked for there — so the concept links `queue apply` writes on
    // daily pages, the pipeline's own output, were never checked. A new daily page brings no
    // index drift of its own (the catalog covers the wiki), so this gates on the link alone.
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
    // Re-catalog first: the drift from two new pages is its own violation, and this test must
    // fail for the name collision alone.
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
    // same verdict about it: asking for the list is not discovering a defect.
    let stdout = ws.stdout(&["graph", "orphans"]);
    assert!(stdout.contains("uncited"), "{stdout}");
    assert_eq!(ws.code(&["graph", "orphans"]), 0, "{stdout}");
    assert_eq!(ws.code(&["graph", "broken"]), 0, "{stdout}");
}

#[test]
fn a_concept_due_for_re_audit_is_a_worklist_and_exits_zero() {
    let ws = sound_vault();
    // Multiply cited, never audited (no `audited_sources_hash`) — the worklist's whole
    // population. `/lore-wiki audit` reads the JSON, so a non-zero exit would only mean it
    // cannot run the command under `set -e`.
    ws.write(
        "wiki/concepts/cited.md",
        &concept("cited", "Cited", "A concept.").replace("source_count: 1", "source_count: 2"),
    );

    let out = ws.run(&["graph", "audit-candidates"]);
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert!(stdout.contains("cited"), "worklist empty\n{stdout}");
    assert_eq!(out.status.code(), Some(0), "{stdout}");
}
