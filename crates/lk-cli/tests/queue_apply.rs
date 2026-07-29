//! End-to-end contract of `lore queue apply`, exercised through the real binary.
//!
//! The case that matters is several results landing on ONE page in a single batch. It is
//! not exotic: `lore ingest` warns when pending queue files exist precisely because running
//! it twice before a drain enqueues the same target again, under the same input hash, and
//! both tasks classify current. Drain both and `queue apply` gets two results for one page.

use std::path::{Path, PathBuf};
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
        std::fs::read_to_string(self.vault().join(rel)).expect("read page")
    }

    /// One `extract-concepts` result naming a single concept, as a drain writes it.
    /// Serialized from the real type so the fixture cannot drift from the wire format.
    fn drop_result(&self, task_id: &str, target: &str, hash: &str, concept: &str) {
        let body = serde_json::to_string(&lk_queue::TaskResult {
            task_id: task_id.into(),
            cache_hash: hash.into(),
            target: lk_queue::TaskTarget {
                vault_path: target.into(),
                kind: lk_queue::TargetKind::DailyConcepts,
                anchor: "## Related Concepts".into(),
            },
            date: jiff::civil::date(2026, 5, 23),
            concepts: vec![lk_queue::ReportedConcept {
                concept: lk_core::concept::ExtractedConcept {
                    name: concept.into(),
                    category: None,
                },
                synthesis: None,
            }],
        })
        .expect("serialize result");
        let dir = self
            .vault()
            .join(".lorekeeper")
            .join("queue")
            .join(lk_queue::RESULTS_SUBDIR);
        std::fs::create_dir_all(&dir).expect("results dir");
        std::fs::write(dir.join(format!("{task_id}.json")), body).expect("result");
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

const PAGE: &str = "daily/notes/2026-05-23.md";

fn daily_page(hash: &str) -> String {
    format!(
        "---\nid: notes-2026-05-23\ntype: daily\nllm_inputs:\n  concepts: \"{hash}\"\n---\n\n\
         ## Summary\n\nbody\n\n## Related Concepts\n\n## Key Events\n\n- a\n"
    )
}

fn results_dir(ws: &Workspace) -> PathBuf {
    ws.vault()
        .join(".lorekeeper")
        .join("queue")
        .join(lk_queue::RESULTS_SUBDIR)
}

fn json_files(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// Two results for one page in one batch must BOTH be cited. Every write lands after the
/// apply loop, so a second fold reading the page from disk would see a version predating
/// the first, and its write would drop the first's citation — the same silent loss the
/// accumulating render exists to prevent, reached through batching instead of re-extraction.
#[test]
fn two_results_for_one_page_both_land() {
    let ws = Workspace::new();
    ws.write(PAGE, &daily_page("h"));
    // Same target, same hash — what two ingests before one drain produce.
    ws.drop_result("ext-1", PAGE, "h", "Concept A");
    ws.drop_result("ext-2", PAGE, "h", "Concept B");

    let out = ws.run(&["queue", "apply"]);
    assert!(
        out.status.success(),
        "queue apply failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let page = ws.read(PAGE);
    for slug in ["concept-a", "concept-b"] {
        assert!(
            page.contains(&format!("wiki/concepts/{slug}.md")),
            "{slug} must be cited after applying both results:\n{page}"
        );
    }
    // Both concept pages exist, so both citations resolve.
    for slug in ["concept-a", "concept-b"] {
        assert!(
            ws.vault()
                .join("wiki/concepts")
                .join(format!("{slug}.md"))
                .exists(),
            "{slug} page must be written"
        );
    }
    assert!(
        json_files(&results_dir(&ws)).is_empty(),
        "every applied result must be consumed"
    );
}

/// `--dry-run` reports without touching the vault or consuming a result.
#[test]
fn dry_run_applies_nothing_and_consumes_nothing() {
    let ws = Workspace::new();
    ws.write(PAGE, &daily_page("h"));
    ws.drop_result("ext-1", PAGE, "h", "Concept A");
    let before = ws.read(PAGE);

    let out = ws.run(&["queue", "apply", "--dry-run"]);
    assert!(out.status.success(), "dry-run must succeed");
    assert_eq!(ws.read(PAGE), before, "dry-run must not rewrite the page");
    assert_eq!(
        json_files(&results_dir(&ws)).len(),
        1,
        "dry-run must leave the result for a real run"
    );
    assert!(
        !ws.vault().join("wiki/concepts/concept-a.md").exists(),
        "dry-run must not write concept pages"
    );
}

/// A result whose page has moved on is dropped, not applied — the hash is the same guard
/// `queue status` uses, and the result is consumed so it cannot block the batch forever.
#[test]
fn a_result_whose_page_moved_on_is_dropped() {
    let ws = Workspace::new();
    ws.write(PAGE, &daily_page("newer-hash"));
    ws.drop_result("ext-1", PAGE, "h", "Concept A");

    let out = ws.run(&["queue", "apply"]);
    assert!(out.status.success(), "a dropped result is not a failure");
    assert!(
        !ws.read(PAGE).contains("concept-a.md"),
        "a stale result must not edit the page"
    );
    assert!(json_files(&results_dir(&ws)).is_empty(), "and is consumed");
}
