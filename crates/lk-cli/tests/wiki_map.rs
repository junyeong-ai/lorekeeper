//! End-to-end test of `lore wiki map` through the real binary: the navigation map is
//! written, lists concepts by citation cluster with valid path-form wikilinks, is
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
        "---\nid: wiki/concepts/alpha\ntitle: Alpha\n---\n## Synthesis\nSee [[beta]].\n",
    )
    .expect("alpha");
    std::fs::write(
        concepts.join("beta.md"),
        "---\nid: wiki/concepts/beta\ntitle: Beta\n---\n## Synthesis\nSee [[alpha]].\n",
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
        content.contains("[[wiki/concepts/alpha|alpha]]"),
        "map lists concepts by unambiguous path link with leaf display:\n{content}"
    );
    assert!(
        content.contains("[[wiki/concepts/beta|beta]]"),
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
        !content.contains("[[wiki/map"),
        "map.md must never list itself:\n{content}"
    );
}
