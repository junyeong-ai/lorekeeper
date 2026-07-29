//! `lore graph normalize --fix` renames a page and repoints what cites it.
//!
//! The two sets are not the same set, which is the whole point of these tests. Rename
//! candidates are the wiki's own pages — only they are addressed by slug, and slugifying a
//! daily or synthesis filename would rewrite a DATE (`2026-W30` → `2026-w30`) into a path
//! the pipeline never writes. Citations, meanwhile, live mostly OUTSIDE the wiki: a daily
//! page is the ordinary source of one. Rewriting only the rename scope leaves those
//! pointing at a file that no longer exists — and `lore graph broken` cannot report it,
//! because it matches destinations at the id level and so believes the link still resolves.

use std::path::PathBuf;
use std::process::{Command, Output};

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

    fn vault(&self) -> PathBuf {
        self.root.path().join("vault")
    }

    fn write(&self, rel: &str, content: &str) {
        let path = self.vault().join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, content).expect("write");
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.vault().join(rel)).expect("read")
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
}

fn concept(id: &str) -> String {
    format!(
        "---\nid: {id}\ntype: concept\ntitle: \"Bad Name\"\naliases: [\"Bad Name\"]\n\
         created: 2026-05-23\nupdated: 2026-05-23\nsource_count: 0\n---\n\n\
         ## Synthesis\n\nA concept.\n\n## Sources\n\n## Related\n"
    )
}

const DAILY: &str = "daily/notes/2026-05-23.md";

#[test]
fn a_rename_repoints_the_citations_that_live_outside_the_rename_scope() {
    let ws = Workspace::new();
    ws.write("wiki/concepts/Bad_Name.md", &concept("Bad_Name"));
    ws.write(
        DAILY,
        "---\nid: notes-2026-05-23\ntype: daily\n---\n\n\
         ## Related Concepts\n\n- [Bad Name](../../wiki/concepts/Bad_Name.md)\n",
    );

    let out = ws.run(&["graph", "normalize", "--fix"]);
    assert!(
        out.status.success(),
        "normalize --fix failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        ws.vault().join("wiki/concepts/bad-name.md").exists(),
        "the page must be renamed to its canonical slug"
    );
    assert!(
        ws.read(DAILY).contains("../../wiki/concepts/bad-name.md"),
        "the daily page's citation must follow the rename:\n{}",
        ws.read(DAILY)
    );
    assert!(
        !ws.read(DAILY).contains("Bad_Name.md"),
        "no citation may be left at the old address:\n{}",
        ws.read(DAILY)
    );
}

/// A date-named page outside the wiki must never be proposed for rename: `2026-W30`
/// slugifies to `2026-w30`, an address the pipeline does not write, so renaming it would
/// strand the synthesis page its writer looks for.
#[test]
fn a_date_named_page_outside_the_wiki_is_never_renamed() {
    let ws = Workspace::new();
    ws.write(
        "synthesis/weekly/2026-W30.md",
        "---\nid: synthesis-2026-W30\ntype: weekly-synthesis\n---\n\n## Themes\n\n",
    );
    ws.write("wiki/concepts/Bad_Name.md", &concept("Bad_Name"));

    let out = ws.run(&["graph", "normalize", "--fix"]);
    assert!(out.status.success(), "normalize --fix must succeed");
    // Read the directory entry rather than probing a path: on a case-insensitive volume
    // `2026-w30.md` "exists" as `2026-W30.md` itself, so `Path::exists` cannot see a
    // case-only rename at all — the on-disk name is the only witness.
    assert_eq!(
        std::fs::read_dir(ws.vault().join("synthesis/weekly"))
            .expect("read synthesis dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
        vec!["2026-W30.md".to_string()],
        "a dated page keeps its filename; lowercasing it would strand the page its \
         writer looks for"
    );
}

/// Check-only is the default, and it must leave the vault untouched.
#[test]
fn without_fix_nothing_is_renamed_or_repointed() {
    let ws = Workspace::new();
    ws.write("wiki/concepts/Bad_Name.md", &concept("Bad_Name"));
    ws.write(
        DAILY,
        "---\nid: notes-2026-05-23\ntype: daily\n---\n\n\
         ## Related Concepts\n\n- [Bad Name](../../wiki/concepts/Bad_Name.md)\n",
    );
    let before = ws.read(DAILY);

    let out = ws.run(&["graph", "normalize"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "a pending rename is a finding, which `lore graph` reports as exit 1"
    );
    assert!(ws.vault().join("wiki/concepts/Bad_Name.md").exists());
    assert_eq!(ws.read(DAILY), before);
}
