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

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.root.path().join("vault").join(rel)).expect("read page")
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

/// The base config plus a `graph:` section, written before any page so every command in the test
/// reads the same one.
fn with_graph(section: &str) -> Workspace {
    let ws = Workspace::new();
    let base = std::fs::read_to_string(ws.root.path().join("config.yaml")).expect("base config");
    std::fs::write(
        ws.root.path().join("config.yaml"),
        format!("{base}graph:\n{section}"),
    )
    .expect("config");
    ws
}

fn excluding(glob: &str) -> Workspace {
    with_graph(&format!("  scope:\n    exclude: [\"{glob}\"]\n"))
}

fn excluding_from_orphans(id: &str) -> Workspace {
    with_graph(&format!("  metrics:\n    orphan_exclude: [\"{id}\"]\n"))
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
///
/// The vault here is clean in EVERY channel, and each is asserted equal to `[]` rather than
/// merely being an array. On a fixture that populates a list, `is_array()` holds under any
/// serialization — including one that omits the field when empty, which is the regression.
#[test]
fn the_json_report_carries_every_channel_field_on_a_clean_vault() {
    let ws = Workspace::new();
    ws.write(
        "wiki/concepts/cited.md",
        &concept("cited", "Cited", "A concept, cited and uncontested."),
    );
    ws.write(
        "daily/notes/2026-05-23.md",
        "---\nid: notes-2026-05-23\ntype: daily\ntitle: \"Notes\"\n\
         created: 2026-05-23\nupdated: 2026-05-23\n---\n\n\
         ## Related concepts\n\n- [Cited](../../wiki/concepts/cited.md)\n",
    );
    assert_eq!(ws.code(&["wiki", "index"]), 0);
    assert_eq!(ws.code(&["graph", "lint"]), 0, "the fixture must be clean");
    let raw = ws.stdout(&["graph", "--json", "lint"]);
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
    let data = &parsed["data"];

    for path in [
        "violations.broken",
        "violations.invalid_categories",
        "violations.duplicate_concepts",
        "violations.address_collisions",
        "violations.unnormalized",
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
        assert_eq!(
            cursor,
            &serde_json::json!([]),
            "`{path}` must be an empty array, present — got {cursor:?}\n{raw}"
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

/// A page whose frontmatter will not parse still linked what it linked. Losing its links with
/// its fields makes the vault read BETTER than it is, twice over: a genuinely broken link
/// disappears, and every page that page cited reads as uncited. The likeliest author of such
/// frontmatter is the drain itself, an agent editing a marker line into it.
#[test]
fn a_page_with_unparseable_frontmatter_still_reports_its_broken_link() {
    let ws = sound_vault();
    ws.write(
        "daily/notes/2026-05-24.md",
        "---\nid: notes-2026-05-24\ntype: daily\ntitle: Notes: today\n---\n\n\
         ## Related concepts\n\n- [Cited](../../wiki/concepts/cited.md)\n\
         - [Ghost](../../wiki/concepts/ghost.md)\n",
    );

    let raw = ws.stdout(&["graph", "--json", "lint"]);
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
    let broken = parsed["data"]["violations"]["broken"]
        .as_array()
        .expect("broken array");
    assert_eq!(
        broken.len(),
        1,
        "the ghost link must still be reported\n{raw}"
    );
    let orphans = parsed["data"]["observations"]["orphans"]
        .as_array()
        .expect("orphans array");
    assert!(
        !orphans
            .iter()
            .any(|o| o.as_str() == Some("wiki/concepts/cited")),
        "a page this page cites is not an orphan\n{raw}"
    );
}

/// Existence is a question about a FILE, so it is asked of the path a link names — not of the
/// page id that path slugifies to. Slugifying is lossy: `Bad_Name.md`, `bad--name.md` and
/// `bad name.md` all share `bad-name`, so answering by id reports a destination that is dead in
/// Obsidian, dead on GitHub and dead in git as sound.
#[test]
fn a_link_to_a_destination_no_file_answers_exits_one() {
    let ws = sound_vault();
    ws.write(
        "daily/notes/2026-05-24.md",
        "---\nid: notes-2026-05-24\ntype: daily\ntitle: \"Notes\"\n\
         created: 2026-05-24\nupdated: 2026-05-24\n---\n\n\
         ## Related concepts\n\n- [Cited](../../wiki/concepts/Cited_Page.md)\n",
    );

    let out = ws.run(&["graph", "broken"]);
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert_eq!(
        out.status.code(),
        Some(1),
        "the file on disk is `cited.md`; no filesystem opens `Cited_Page.md`\n{stdout}"
    );
    assert!(stdout.contains("wiki/concepts/Cited_Page.md"), "{stdout}");
}

/// The other half of that rule: a spelling the FILESYSTEM answers to is not a dead destination.
///
/// `Cited.md` and `cited.md` are one file on APFS and NTFS — `cat` opens either — and the
/// directory segments of an address already resolve by that same fold. Answering the file
/// segment exactly made one address resolve two ways within a single path: a link a reader can
/// follow was reported broken, and `backlinks-sync` left the citation out of the cited page's
/// sources. What the two spellings cost on a filesystem that keeps them apart is a violation
/// with its own channel and its own repair, asserted below, so folding here hides nothing.
#[test]
fn a_link_whose_case_the_filesystem_folds_is_not_broken() {
    let ws = sound_vault();
    ws.write(
        "daily/notes/2026-05-24.md",
        "---\nid: notes-2026-05-24\ntype: daily\ntitle: \"Notes\"\n\
         created: 2026-05-24\nupdated: 2026-05-24\n---\n\n\
         ## Related concepts\n\n- [Cited](../../wiki/concepts/Cited.md)\n",
    );

    let out = ws.run(&["graph", "broken"]);
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    assert!(stdout.contains("0 broken link(s)"), "{stdout}");

    // And the file whose name is not its own slug is still named, under the channel whose
    // repair is renaming it.
    let raw = ws.stdout(&["graph", "--json", "lint"]);
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
    assert!(
        parsed["data"]["violations"]["broken"]
            .as_array()
            .is_some_and(|b| b.is_empty()),
        "{raw}"
    );
}

/// Two files whose paths slugify to one page id. The id is the graph's node key, so one of them
/// silently loses its node: its edges are attributed to its twin, and it vanishes from orphans,
/// hubs and drift while still counted in the totals. `A B.md` beside `a-b.md` is enough.
#[test]
fn two_files_at_one_address_exit_one() {
    let ws = sound_vault();
    ws.write(
        "wiki/documents/A B.md",
        "---\nid: a-b-spaced\ntype: document\ntitle: \"A B spaced\"\ncreated: 2026-05-23\n---\n\n# A B spaced\n",
    );
    ws.write(
        "wiki/documents/a-b.md",
        "---\nid: a-b\ntype: document\ntitle: \"A B hyphen\"\ncreated: 2026-05-23\n---\n\n# A B hyphen\n",
    );

    let out = ws.run(&["graph", "lint"]);
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert_eq!(out.status.code(), Some(1), "{stdout}");
    assert!(stdout.contains("One address, two files"), "{stdout}");
    assert!(stdout.contains("wiki/documents/A B.md"), "{stdout}");
}

/// A vault directory whose spelling on disk differs from the configured one — the case a
/// case-insensitive filesystem folds for you. Both outcomes are correct and which one applies is
/// a property of the FILESYSTEM, so the test asks it rather than assuming: where the two names
/// reach one directory the vault works and its pages must be analysed (a raw prefix test matched
/// none of them, so every command reported an empty vault and exited 0); where they are kept
/// apart the vault is split in half — pages under one name, the catalog written under the other
/// — and running is worse than refusing.
#[test]
fn a_vault_directory_spelled_differently_on_disk() {
    let ws = Workspace::new();
    ws.write(
        "Wiki/concepts/folded.md",
        &concept(
            "folded",
            "Folded",
            "Links nothing that exists: [Ghost](ghost.md)",
        ),
    );
    let folds = ws.root.path().join("vault/wiki").is_dir();

    let out = ws.run(&["graph", "lint"]);
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    let stderr = String::from_utf8(out.stderr).expect("utf8");

    if folds {
        assert_eq!(
            out.status.code(),
            Some(1),
            "one directory, two spellings: its pages are the tool's to check\n{stdout}{stderr}"
        );
        assert!(stdout.contains("pages: 1"), "{stdout}");
        assert!(stdout.contains("ghost.md"), "{stdout}");
    } else {
        assert_eq!(
            out.status.code(),
            Some(2),
            "two directories: refuse rather than analyse one and write to the other\n{stdout}{stderr}"
        );
        assert!(stderr.contains("keeps the two apart"), "{stderr}");
    }
}

/// A filename that disagrees with its own normalized slug is a violation both CLAUDE.md files
/// name, and `graph normalize` exits 1 on it — but `graph lint`, which the shipped pipeline uses
/// as its only verdict stage, carried no such channel, so the pipeline stayed green on it.
#[test]
fn a_filename_that_is_not_its_own_slug_exits_one_from_lint_too() {
    let ws = sound_vault();
    ws.write(
        "wiki/concepts/Not A Slug.md",
        &concept(
            "not-a-slug",
            "Not A Slug",
            "A page whose file is named otherwise.",
        ),
    );
    assert_eq!(ws.code(&["wiki", "index"]), 0);

    assert_eq!(
        ws.code(&["graph", "normalize"]),
        1,
        "the single check must report it"
    );
    let out = ws.run(&["graph", "lint"]);
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert_eq!(out.status.code(), Some(1), "and so must lint\n{stdout}");
    assert!(stdout.contains("not their own slug"), "{stdout}");
}

/// A link to a page the user DELETED must not resolve. Obsidian moves a deleted page into
/// `.trash`, so a scan that walked dot-directories would find the file and report the link sound
/// — the one case `lk-graph/CLAUDE.md` names for the rule.
#[test]
fn a_link_into_a_dot_directory_does_not_resolve() {
    let ws = sound_vault();
    ws.write(
        ".trash/deleted.md",
        &concept("deleted", "Deleted", "Moved here by the editor."),
    );
    ws.write(
        "daily/notes/2026-05-24.md",
        "---\nid: notes-2026-05-24\ntype: daily\ntitle: \"Notes\"\n\
         created: 2026-05-24\nupdated: 2026-05-24\n---\n\n\
         ## Related concepts\n\n- [Deleted](../../.trash/deleted.md)\n",
    );

    let out = ws.run(&["graph", "broken"]);
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert_eq!(
        out.status.code(),
        Some(1),
        "a deleted page is not a destination\n{stdout}"
    );
    assert!(stdout.contains(".trash/deleted.md"), "{stdout}");
}

/// `metrics.orphan_exclude` is a documented setting, and the CLI has to actually pass it: the
/// library function's own test cannot see the command dropping it on the way in.
#[test]
fn the_configured_orphan_exclude_reaches_the_report() {
    let ws = excluding_from_orphans("wiki/concepts/uncited");
    ws.write(
        "wiki/concepts/uncited.md",
        &concept("uncited", "Uncited", "Nothing points here yet."),
    );
    assert_eq!(ws.code(&["wiki", "index"]), 0);

    let raw = ws.stdout(&["graph", "--json", "lint"]);
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
    assert_eq!(
        parsed["data"]["observations"]["orphans"],
        serde_json::json!([]),
        "the excluded id must not be reported as an orphan\n{raw}"
    );
}

/// Two files colliding on one address inside a folder the tool does NOT manage is not this
/// tool's contradiction to report. It has no repair to name — no `--fix`, and `scope.exclude`
/// cannot reach a set computed before the exclusion — so reporting it gates the scheduled
/// pipeline forever on content the same series decided is not the pipeline's to lint.
#[test]
fn two_unmanaged_files_at_one_address_do_not_gate() {
    let ws = sound_vault();
    ws.write("clippings/Note A.md", "# Note A\n");
    ws.write("clippings/note-a.md", "# Note a\n");

    let out = ws.run(&["graph", "lint"]);
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert_eq!(
        out.status.code(),
        Some(0),
        "a user's own folder is not the pipeline's to repair\n{stdout}"
    );
    assert!(!stdout.contains("One address"), "{stdout}");
}

/// `normalize --fix` renames the files and THEN repoints the citations. A page it cannot read
/// has no links to repoint — the scan already resolved them all — so reading it anyway turns a
/// foreign file in a user's folder into a run that aborts with the vault half-repointed, and
/// every re-run aborts at the same file.
#[test]
fn a_file_the_tool_cannot_read_does_not_abort_a_rename_midway() {
    let ws = sound_vault();
    ws.write(
        "wiki/concepts/Bad_Name.md",
        &concept(
            "bad-name",
            "Bad Name",
            "A page whose file is named otherwise.",
        ),
    );
    ws.write(
        "daily/notes/2026-05-24.md",
        "---\nid: notes-2026-05-24\ntype: daily\ntitle: \"Notes\"\n\
         created: 2026-05-24\nupdated: 2026-05-24\n---\n\n\
         ## Related concepts\n\n- [Bad Name](../../wiki/concepts/Bad_Name.md)\n",
    );
    // Not UTF-8, in a folder the tool does not manage.
    std::fs::write(
        ws.root.path().join("vault/clippings/legacy.md"),
        [0xff, 0xfe, 0x41],
    )
    .or_else(|_| {
        std::fs::create_dir_all(ws.root.path().join("vault/clippings")).and_then(|_| {
            std::fs::write(
                ws.root.path().join("vault/clippings/legacy.md"),
                [0xff, 0xfe, 0x41],
            )
        })
    })
    .expect("write non-utf8");

    let out = ws.run(&["graph", "normalize", "--fix"]);
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    let stderr = String::from_utf8(out.stderr).expect("utf8");
    assert_eq!(out.status.code(), Some(0), "{stdout}{stderr}");
    assert!(
        ws.read("daily/notes/2026-05-24.md")
            .contains("concepts/bad-name.md"),
        "the citation must be repointed: {}",
        ws.read("daily/notes/2026-05-24.md")
    );
}

/// One link, one answer. A destination that names no file is broken — and it must not also be
/// an edge, a citation in the page it slugifies onto, and that page's exemption from orphan
/// detection. `Bad_Name.md` beside a real `bad-name.md` is enough: the id matches while the
/// address does not, and reading the id alone had the same vault answering three ways at once.
#[test]
fn a_link_whose_address_is_missing_is_not_a_citation_of_the_page_it_resembles() {
    let ws = sound_vault();
    ws.write(
        "wiki/concepts/bad-name.md",
        &concept("bad-name", "Bad Name", "Nothing reaches this."),
    );
    ws.write(
        "daily/notes/2026-05-24.md",
        "---\nid: notes-2026-05-24\ntype: daily\ntitle: \"Notes\"\n\
         created: 2026-05-24\nupdated: 2026-05-24\n---\n\n\
         ## Related concepts\n\n- [Bad Name](../../wiki/concepts/Bad_Name.md)\n",
    );

    let raw = ws.stdout(&["graph", "--json", "lint"]);
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
    assert_eq!(
        parsed["data"]["violations"]["broken"]
            .as_array()
            .map(Vec::len),
        Some(1),
        "the address names no file\n{raw}"
    );
    assert!(
        parsed["data"]["observations"]["orphans"]
            .as_array()
            .expect("orphans")
            .iter()
            .any(|o| o.as_str() == Some("wiki/concepts/bad-name")),
        "nothing reaches it, so it is an orphan\n{raw}"
    );

    assert_eq!(ws.code(&["graph", "backlinks-sync"]), 0);
    let page = ws.read("wiki/concepts/bad-name.md");
    assert!(
        !page.contains("2026-05-24"),
        "a link that opens nothing is not provenance\n{page}"
    );
}

/// A citation on an excluded page is still a citation. The mutating commands read the same
/// whole-vault view the read-only ones do, so a narrowing meant for `hubs`/`cluster` cannot make
/// a page they REWRITE disappear. Applying the globs to their scan instead makes an excluded
/// citer invisible, and `backlinks-sync` then deletes the citation record it justifies — a
/// silent data loss that exits 0 and that `graph broken` cannot report afterwards, because the
/// dangling link it leaves behind sits on the very page the globs exclude.
#[test]
fn a_citation_on_an_excluded_page_survives_the_sweeps_that_rewrite_the_vault() {
    let ws = excluding("daily/**");
    ws.write(
        "wiki/concepts/cited.md",
        "---\nid: cited\ntype: concept\ntitle: \"Cited\"\ncategory: \"\"\n\
         created: 2026-05-23\nupdated: 2026-05-23\nsource_count: 1\n---\n\n\
         ## Synthesis\n\nA concept.\n\n## Sources\n\n\
         - [Notes](../../daily/notes/2026-05-23.md)\n\n## Related\n",
    );
    ws.write(
        "daily/notes/2026-05-23.md",
        "---\nid: notes-2026-05-23\ntype: daily\ntitle: \"Notes\"\n\
         created: 2026-05-23\nupdated: 2026-05-23\n---\n\n\
         ## Related concepts\n\n- [Cited](../../wiki/concepts/cited.md)\n",
    );

    assert_eq!(ws.code(&["graph", "backlinks-sync"]), 0);
    let page = ws.read("wiki/concepts/cited.md");
    assert!(
        page.contains("- [Notes](../../daily/notes/2026-05-23.md)"),
        "the sweep deleted a citation its excluded citer justifies\n{page}"
    );
    assert!(page.contains("source_count: 1"), "{page}");
}

/// An observation-tuning knob must not be able to silence a violation. `orphan_exclude` is
/// documented as "page ids never reported as orphans"; a catalog that disagrees with the disk is
/// a different question, and answering it through the same filter makes one config key decide
/// both channels.
#[test]
fn an_observation_knob_cannot_silence_a_violation() {
    let ws = excluding_from_orphans("wiki/concepts/uncited");
    ws.write(
        "wiki/concepts/uncited.md",
        &concept("uncited", "Uncited", "Nothing points here yet."),
    );
    ws.write("wiki/index.md", "# Index\n");

    let out = ws.run(&["graph", "index-sync"]);
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert_eq!(
        out.status.code(),
        Some(1),
        "a page the catalog omits is drift whatever the orphan filter says\n{stdout}"
    );
    assert!(stdout.contains("wiki/concepts/uncited"), "{stdout}");
}

/// Drift is the difference between the catalog on disk and the one its BUILDER produces, asked of
/// the builder. A second opinion about what belongs in the catalog reports a page the builder
/// does not carry: `--fix` appends it, the next `wiki index` (which the pipeline runs BEFORE
/// `graph lint`) drops it again, and the vault stays contradicted by two repairs undoing each
/// other — permanently red, with nothing a caller can do about it.
#[test]
fn a_page_the_catalog_does_not_carry_is_not_drift() {
    let ws = sound_vault();
    ws.write(
        "wiki/scratch/note.md",
        "---\nid: note\ntype: document\ntitle: \"Note\"\ncreated: 2026-05-23\n---\n\n# Note\n",
    );

    for pass in 0..2 {
        let out = ws.run(&["graph", "index-sync"]);
        let stdout = String::from_utf8(out.stdout).expect("utf8");
        assert_eq!(
            out.status.code(),
            Some(0),
            "pass {pass}: a page the builder never catalogs is not drift\n{stdout}"
        );
        assert_eq!(ws.code(&["wiki", "index"]), 0);
    }
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

/// A stray link in the user's OWN note is not a violation of the vault's contract. The pipeline
/// neither wrote that page nor can repair it, so gating the scheduled run on it would report
/// content this tool does not manage. The note still EXISTS, so a managed page linking it
/// resolves — and a file whose frontmatter will not parse is a file, so that resolves too.
#[test]
fn a_page_this_tool_does_not_manage_is_not_a_link_source() {
    let ws = Workspace::new();
    ws.write(
        "wiki/concepts/a.md",
        &concept(
            "a",
            "A",
            "Links a real note and a real malformed file: \
             [note](../../Archive/note.md) [m](../../Archive/malformed.md)",
        ),
    );
    ws.write(
        "Archive/note.md",
        "# My note\n\nA stray link: [gone](./nope.md)\n",
    );
    ws.write(
        "Archive/malformed.md",
        "---\nid: malformed\ntitle: unclosed\n",
    );
    assert_eq!(ws.code(&["wiki", "index"]), 0);

    let out = ws.run(&["graph", "lint"]);
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert_eq!(
        out.status.code(),
        Some(0),
        "neither the user's stray link nor a real file's parse failure is a violation\n{stdout}"
    );
    assert!(
        !stdout.contains("nope"),
        "the user's own note must not be linted as a source\n{stdout}"
    );
    assert!(
        !stdout.contains("malformed"),
        "a file that exists must resolve, whatever its frontmatter\n{stdout}"
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
