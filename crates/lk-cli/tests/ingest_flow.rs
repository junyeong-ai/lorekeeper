//! End-to-end tests of the `lore ingest` phase contract, exercised through the
//! real binary: dry-run side-effect-freeness, the write → flush → archive
//! ordering outcomes, partial-failure exit codes, and idempotent re-runs.
//!
//! The manual source drives every scenario — it is the one adapter that needs
//! no network or credentials, so these tests run hermetically. Its `inbox_dir`
//! is configured RELATIVE on purpose: the run only finds the inbox if relative
//! params resolve against the vault root (never the CWD the test happens to
//! run in), so vault-root anchoring is proven end-to-end here too.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Workspace {
    root: tempfile::TempDir,
}

impl Workspace {
    /// A config-file + vault pair with one manual source reading `vault/inbox`.
    /// `extra_sources` is appended verbatim under `sources:`.
    fn new(extra_sources: &str) -> Self {
        let root = tempfile::TempDir::new().expect("tempdir");
        std::fs::create_dir_all(root.path().join("vault/inbox")).expect("inbox dir");
        let config = format!(
            "vault:\n  root: vault\nidentity:\n  name: Tester\n  email: tester@example.com\n\
             sources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: inbox\n\
             {extra_sources}"
        );
        std::fs::write(root.path().join("config.yaml"), config).expect("config");
        Self { root }
    }

    fn vault(&self) -> PathBuf {
        self.root.path().join("vault")
    }

    fn drop_note(&self, name: &str, body: &str) {
        std::fs::write(self.vault().join("inbox").join(name), body).expect("drop note");
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_lore"));
        // Scrub every Lorekeeper env var so the run depends ONLY on the test's
        // `--config` and tempdir — a developer's ambient `LORE_CONFIG`,
        // `LORE_TEMPLATE_DIR`, or `LORE_JIRA_*` credentials must never leak in and
        // make a test non-hermetic (e.g. turning the no-credentials jira failure
        // into a live network call).
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

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                out.extend(files_under(&p));
            } else {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn dry_run_is_side_effect_free() {
    let ws = Workspace::new("");
    ws.drop_note("note.md", "# Dry Note\n\nbody\n");

    let out = ws.run(&["ingest", "--dry-run"]);
    assert!(
        out.status.success(),
        "dry-run must succeed: {}",
        stderr_of(&out)
    );
    assert!(stderr_of(&out).contains("[dry-run]"));

    // The vault holds exactly what we created — no pages, no `.lorekeeper`
    // state (queue, ingest log), and the inbox file untouched.
    let files = files_under(&ws.vault());
    assert_eq!(
        files,
        vec![ws.vault().join("inbox/note.md")],
        "dry-run must not write anything"
    );
}

#[test]
fn ingest_writes_pages_flushes_queue_then_archives() {
    let ws = Workspace::new("");
    ws.drop_note("note.md", "# Real Note\n\nknowledge body\n");

    let out = ws.run(&["ingest"]);
    assert!(
        out.status.success(),
        "ingest must succeed: {}",
        stderr_of(&out)
    );

    // Phase 2 outcome: the manual file became exactly one document page.
    let documents = files_under(&ws.vault().join("wiki/documents"));
    assert_eq!(documents.len(), 1, "one inbox file → one document page");

    // Phase 4 outcome + the queue durability invariant: a visible `.jsonl` may
    // only reference pages that exist (flush runs after all writes).
    let queue_files: Vec<PathBuf> = files_under(&ws.vault().join(".lorekeeper/queue"))
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .collect();
    assert_eq!(queue_files.len(), 1, "one run → one queue file");
    let queue_body = std::fs::read_to_string(&queue_files[0]).unwrap();
    assert!(!queue_body.trim().is_empty(), "summary task must be queued");
    for line in queue_body.lines() {
        let task: serde_json::Value = serde_json::from_str(line).expect("task line is JSON");
        let target = task["target"]["vault_path"]
            .as_str()
            .expect("task has target");
        assert!(
            ws.vault().join(target).is_file(),
            "queued task targets a page that exists: {target}"
        );
        assert!(task["cache_hash"].as_str().is_some_and(|h| h.len() == 32));
    }

    // Phase 5 outcome: the ingest log records the source as success.
    let log = std::fs::read_to_string(ws.vault().join(".lorekeeper/ingest.jsonl")).unwrap();
    assert!(log.contains(r#""source_id":"notes""#) && log.contains(r#""status":"success""#));

    // Phase 6 outcome: archive ran last — the inbox is empty, the file moved
    // under `archived/{date}/`.
    assert!(
        !ws.vault().join("inbox/note.md").exists(),
        "consumed file must be archived"
    );
    let archived = files_under(&ws.vault().join("inbox/archived"));
    assert_eq!(archived.len(), 1);
    assert!(archived[0].ends_with("note.md"));
}

#[test]
fn rerun_with_empty_inbox_is_a_clean_noop() {
    let ws = Workspace::new("");
    ws.drop_note("note.md", "# Note\n\nbody\n");
    assert!(ws.run(&["ingest"]).status.success());

    let documents = files_under(&ws.vault().join("wiki/documents"));
    assert_eq!(documents.len(), 1);
    let page_before = std::fs::read_to_string(&documents[0]).unwrap();
    let queue_before = files_under(&ws.vault().join(".lorekeeper/queue")).len();

    // The inbox was archived, so the re-run has nothing to do. It must still
    // exit 0 (warning about the pending queue file, not failing on it) and
    // leave the materialized page byte-identical.
    let out = ws.run(&["ingest"]);
    assert!(
        out.status.success(),
        "no-op re-run must succeed: {}",
        stderr_of(&out)
    );
    assert!(
        stderr_of(&out).contains("pending queue file"),
        "undrained queue must be surfaced, never silently ignored"
    );

    let page_after = std::fs::read_to_string(&documents[0]).unwrap();
    assert_eq!(page_before, page_after, "re-run must not perturb the page");
    assert_eq!(
        files_under(&ws.vault().join(".lorekeeper/queue")).len(),
        queue_before,
        "an empty run must not emit a queue file"
    );
    let log = std::fs::read_to_string(ws.vault().join(".lorekeeper/ingest.jsonl")).unwrap();
    assert!(
        log.contains(r#""status":"skipped""#),
        "no-op run is logged as skipped"
    );
}

#[test]
fn fetch_failure_in_one_source_fails_the_run_but_not_the_healthy_one() {
    // A jira source with no credentials fails (at adapter construction — env is
    // scrubbed in `run`, so it can never reach the network). The run must exit
    // non-zero (cron observability) while the healthy manual source still writes
    // its pages — and still archives: every vault write and the queue flush
    // succeeded, so its knowledge is durably materialized and another source's
    // fetch failure does not strand the inbox.
    let ws =
        Workspace::new("  tracker:\n    type: jira\n    params:\n      jql: \"updated >= -1d\"\n");
    ws.drop_note("note.md", "# Note\n\nbody\n");

    let out = ws.run(&["ingest"]);
    assert!(!out.status.success(), "a failed source must fail the run");

    assert_eq!(files_under(&ws.vault().join("wiki/documents")).len(), 1);
    assert!(
        !ws.vault().join("inbox/note.md").exists(),
        "healthy source still archives"
    );

    let log = std::fs::read_to_string(ws.vault().join(".lorekeeper/ingest.jsonl")).unwrap();
    assert!(log.contains(r#""source_id":"notes""#) && log.contains(r#""status":"success""#));
    assert!(log.contains(r#""source_id":"tracker""#) && log.contains(r#""status":"failed""#));
}

#[test]
fn unknown_source_id_is_an_error() {
    let ws = Workspace::new("");
    let out = ws.run(&["ingest", "ghost"]);
    assert!(!out.status.success());
    assert!(stderr_of(&out).contains("not found"));
}

/// `lore doctor` reports a section that records an input nothing answered. Whether that is a
/// DEFECT depends on the queue, which it has to ask: the same page reads as work in flight
/// while its task is pending, and as lost once the queue file is gone.
///
/// The distinction is not cosmetic. The remediation the report prints tells a reader to fill the
/// section by hand and stamp its completion marker; done to work that is merely pending, that
/// silences the queue (`lore queue count` drops to 0, so the scheduled drain skips the session),
/// the extraction never happens, and `doctor` then certifies the vault clean.
#[test]
fn a_section_with_a_pending_task_is_work_in_flight_not_a_defect() {
    let ws = Workspace::new("");
    ws.drop_note("note.md", "# Queued Note\n\nbody\n");
    assert!(ws.run(&["ingest"]).status.success());

    let pending = ws.run(&["queue", "count"]);
    assert_eq!(
        String::from_utf8_lossy(&pending.stdout).trim(),
        "1",
        "the ingest left this page's summary pending"
    );

    let out = ws.run(&["doctor"]);
    let stderr = stderr_of(&out);
    assert!(
        out.status.success(),
        "queued work is not a vault defect\n{stderr}"
    );
    assert!(
        stderr.contains("queued for a drain"),
        "the pending sections must still be reported, just not as defects\n{stderr}"
    );
    assert!(
        !stderr.contains("Read the marker before"),
        "the lost-work remediation must not print for work that is pending\n{stderr}"
    );

    // Archive the run the way a drain does, and the same page becomes a finding.
    let queue = ws.vault().join(".lorekeeper/queue");
    let processed = queue.join("processed");
    std::fs::create_dir_all(&processed).expect("processed dir");
    for file in files_under(&queue) {
        if file.extension().is_some_and(|e| e == "jsonl") {
            std::fs::rename(&file, processed.join(file.file_name().expect("name")))
                .expect("archive");
        }
    }

    let out = ws.run(&["doctor"]);
    let stderr = stderr_of(&out);
    assert_eq!(
        out.status.code(),
        Some(1),
        "with nothing pending, an unanswered section is a defect\n{stderr}"
    );
    assert!(stderr.contains("Read the marker before"), "{stderr}");
    // The one marker a reader must never stamp by hand, named as the exception.
    assert!(stderr.contains("is the exception"), "{stderr}");
}

/// `--dry-run` has to report the number the doctor's remediation tells a reader to compare: a
/// re-render REPLACES a daily page's event list, so the count is the whole difference between
/// repairing the page and truncating it.
#[test]
fn a_dry_run_reports_the_event_count_it_would_write() {
    let ws = Workspace::new("");
    ws.drop_note("a.md", "# A\n\nbody\n");
    let out = ws.run(&["ingest", "--dry-run"]);
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "{stderr}");
    // The manual source writes document pages, which state no event count; a daily source's
    // page does, and the same reporting path prints both.
    assert!(stderr.contains("would write:"), "{stderr}");
}
