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
        Self::with_notes_options("", extra_sources)
    }

    /// The same, with `notes_options` appended verbatim to the `notes` source's own keys —
    /// `concept_policy: supported-new`, say, which is what gives a page a related-concepts section.
    fn with_notes_options(notes_options: &str, extra_sources: &str) -> Self {
        let root = tempfile::TempDir::new().expect("tempdir");
        std::fs::create_dir_all(root.path().join("vault/inbox")).expect("inbox dir");
        let config = format!(
            "vault:\n  root: vault\nidentity:\n  name: Tester\n  email: tester@example.com\n\
             sources:\n  notes:\n    type: manual\n{notes_options}    params:\n      inbox_dir: inbox\n\
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
#[ignore = "requires LORE_TEST_DOCUMENT_PYTHON and LORE_TEST_TESSDATA for the pinned optional runtime"]
fn binary_documents_preserve_exact_originals_through_publication_and_retirement() {
    let ws = Workspace::with_notes_options("    concept_policy: supported-new\n", "");
    let config_path = ws.root.path().join("config.yaml");
    let mut config = std::fs::read_to_string(&config_path).unwrap();
    let python = PathBuf::from(
        std::env::var_os("LORE_TEST_DOCUMENT_PYTHON").expect("LORE_TEST_DOCUMENT_PYTHON"),
    );
    let models = PathBuf::from(std::env::var_os("LORE_TEST_TESSDATA").expect("LORE_TEST_TESSDATA"));
    config.push_str(&format!("\nllm: {{provider: queue}}\ningest:\n  documents:\n    python: {}\n    ocr:\n      tessdata_dir: {}\n      languages: [kor, eng]\n      layout: single-block\n", serde_json::to_string(&python).unwrap(), serde_json::to_string(&models).unwrap()));
    std::fs::write(&config_path, config).unwrap();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../lk-source/src/content/fixtures");
    let names = ["policy.docx", "policy.xlsx", "policy.pdf", "policy-ko.png"];
    for name in names {
        std::fs::copy(fixtures.join(name), ws.vault().join("inbox").join(name)).unwrap();
    }
    let dry_run = ws.run(&["ingest", "--dry-run"]);
    assert!(dry_run.status.success(), "{}", stderr_of(&dry_run));
    assert_eq!(files_under(&ws.vault()).len(), 4);
    let result = ws.run(&["ingest"]);
    assert!(result.status.success(), "{}", stderr_of(&result));
    let documents = files_under(&ws.vault().join("wiki/documents"));
    assert_eq!(
        documents.len(),
        4,
        "same file stem across formats retains separate source ownership"
    );
    let mut formats = std::collections::BTreeSet::new();
    for path in documents {
        let page =
            lk_core::frontmatter::parse_page(&std::fs::read_to_string(path).unwrap()).unwrap();
        let record = page.frontmatter.get("source_record").unwrap();
        formats.insert(record["input_format"].as_str().unwrap().to_string());
        let reference: lk_core::manual_original::ManualOriginalReference =
            serde_json::from_value(record["manual_original"].clone()).unwrap();
        let original =
            std::fs::read(ws.vault().join(reference.entry_vault_path().unwrap())).unwrap();
        assert!(
            names
                .iter()
                .any(|name| std::fs::read(fixtures.join(name)).unwrap() == original)
        );
    }
    assert_eq!(
        formats,
        ["docx", "xlsx", "pdf", "png"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    );
    let archived = files_under(&ws.vault().join("inbox/archived"));
    assert_eq!(archived.len(), 4);
    for path in archived {
        assert_eq!(
            std::fs::read(&path).unwrap(),
            std::fs::read(fixtures.join(path.file_name().unwrap())).unwrap()
        );
    }
    let rerun = ws.run(&["ingest"]);
    assert!(rerun.status.success(), "{}", stderr_of(&rerun));
    assert_eq!(files_under(&ws.vault().join("wiki/documents")).len(), 4);
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
fn manual_provenance_reaches_semantic_inputs_without_owning_page_state() {
    let ws = Workspace::with_notes_options("    concept_policy: supported-new\n", "");
    let original = "---\ntitle: Historical note\ncreated: 2020-01-02\nstatus: superseded\nllm_inputs: {summary_done: forged}\nsource_identity: {repository: forged, document: forged}\n---\nOriginal evidence.\n";
    ws.drop_note("history.md", original);
    let out = ws.run(&["ingest"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let documents = files_under(&ws.vault().join("wiki/documents"));
    assert_eq!(documents.len(), 1);
    let page =
        lk_core::frontmatter::parse_page(&std::fs::read_to_string(&documents[0]).unwrap()).unwrap();
    let provenance = &page.frontmatter.get("source_record").unwrap()["manual_frontmatter"];
    assert_eq!(provenance["status"], "superseded");
    assert_eq!(provenance["created"], "2020-01-02");
    assert!(page.frontmatter.get("source_identity").is_none());
    assert!(
        page.frontmatter
            .get("llm_inputs")
            .unwrap()
            .get("summary_done")
            .is_none()
    );
    let mut kinds = std::collections::BTreeSet::new();
    for path in files_under(&ws.vault().join(".lorekeeper/queue")) {
        if path.extension().is_none_or(|ext| ext != "jsonl") {
            continue;
        }
        for line in std::fs::read_to_string(path).unwrap().lines() {
            let task: serde_json::Value = serde_json::from_str(line).unwrap();
            let text = task["input"]["text"].as_str().unwrap();
            assert!(text.contains("superseded") && text.contains("2020-01-02"));
            assert!(text.contains("manual_frontmatter"));
            kinds.insert(task["kind"].as_str().unwrap().to_owned());
        }
    }
    assert_eq!(
        kinds,
        ["extract-concepts".to_owned(), "summarize".to_owned()].into()
    );
    let archived = files_under(&ws.vault().join("inbox/archived"));
    assert_eq!(archived.len(), 1);
    assert_eq!(std::fs::read_to_string(&archived[0]).unwrap(), original);
    assert!(!ws.run(&["queue", "check"]).status.success());
}

#[test]
fn unreadable_inbox_does_not_record_a_successful_observation() {
    let ws = Workspace::new("");
    let source = ws.vault().join("inbox/broken.txt");
    std::fs::write(&source, [0xff, 0xfe]).unwrap();
    let out = ws.run(&["ingest"]);
    assert!(!out.status.success());
    assert_eq!(std::fs::read(source).unwrap(), [0xff, 0xfe]);
    assert!(files_under(&ws.vault().join("wiki/documents")).is_empty());
    let log = std::fs::read_to_string(ws.vault().join(".lorekeeper/ingest.jsonl")).unwrap();
    for line in log.lines() {
        let entry: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(entry["status"], "failed");
    }
}

#[test]
fn missing_inbox_is_a_failure_but_an_existing_empty_inbox_is_quiet() {
    let ws = Workspace::new("");
    let inbox = ws.vault().join("inbox");
    std::fs::remove_dir(&inbox).unwrap();
    assert!(!ws.run(&["ingest"]).status.success());
    assert!(!inbox.exists());
    std::fs::create_dir(&inbox).unwrap();
    let out = ws.run(&["ingest"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(files_under(&ws.vault().join("wiki/documents")).is_empty());
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

/// A re-render EMPTIES a section whose answer nothing recorded, and says so BEFORE it writes.
///
/// This is the pipeline's own path — the task is enqueued again and a drain refills it — but the
/// same state describes a body somebody wrote and never stamped, and the write is not
/// reversible — and `lore doctor` reports exactly that state and prescribes this re-render as
/// the repair, so an unannounced loss happens by following the tool's own instructions.
#[test]
fn a_re_render_names_the_unanswered_section_it_is_about_to_empty() {
    let ws = Workspace::new("");
    ws.drop_note(
        "note.md",
        "# Rollback\n\nThe only record of the rollback decision.\n",
    );
    assert!(ws.run(&["ingest"]).status.success());

    // Fill the summary by hand and do NOT stamp — what an agent does when it cannot show the
    // content came from this input, which is what `doctor` now tells it to do.
    let page = ws.vault().join("wiki/documents/rollback.md");
    let before = std::fs::read_to_string(&page).expect("page");
    let heading = before
        .lines()
        .find(|line| line.starts_with("## "))
        .expect("the page carries a section heading")
        .to_owned();
    std::fs::write(
        &page,
        before.replace(
            &format!("{heading}\n"),
            &format!("{heading}\n\nHAND WRITTEN: the only record of this decision.\n"),
        ),
    )
    .expect("write");

    // Re-drop the same note so the source can re-render the page.
    ws.drop_note(
        "note.md",
        "# Rollback\n\nThe only record of the rollback decision.\n",
    );
    let out = ws.run(&["ingest"]);
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "{stderr}");
    assert!(
        stderr.contains("emptying an unanswered section"),
        "the loss must be named before the write\n{stderr}"
    );
    assert!(
        stderr.contains(heading.trim_start_matches("## ")),
        "and it must name which section\n{stderr}"
    );
    assert!(
        !std::fs::read_to_string(&page)
            .expect("page")
            .contains("HAND WRITTEN"),
        "the body is gone — the point is that this was reported, not that it survived"
    );
}

/// The warning takes the mood of the write it describes.
///
/// Every other line a dry run prints says "would write"; a bare "emptying" among them reads as
/// a loss that already happened, which is the one thing an operator runs `--dry-run` to avoid
/// being told falsely.
#[test]
fn a_dry_run_says_it_would_empty_the_section_rather_than_that_it_did() {
    let ws = Workspace::new("");
    ws.drop_note("note.md", "# Rollback\n\nThe rollback decision.\n");
    assert!(ws.run(&["ingest"]).status.success());

    let page = ws.vault().join("wiki/documents/rollback.md");
    let before = std::fs::read_to_string(&page).expect("page");
    let heading = before
        .lines()
        .find(|line| line.starts_with("## "))
        .expect("the page carries a section heading")
        .to_owned();
    std::fs::write(
        &page,
        before.replace(
            &format!("{heading}\n"),
            &format!("{heading}\n\nHAND WRITTEN: the only record of this decision.\n"),
        ),
    )
    .expect("write");

    ws.drop_note("note.md", "# Rollback\n\nThe rollback decision.\n");
    let out = ws.run(&["ingest", "--dry-run"]);
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "{stderr}");
    assert!(
        stderr.contains("[dry-run] would empty an unanswered section"),
        "a dry run warns in the conditional\n{stderr}"
    );
    assert!(
        std::fs::read_to_string(&page)
            .expect("page")
            .contains("HAND WRITTEN"),
        "and writes nothing, which is what makes the conditional the true mood"
    );
}

/// A section the render REFILLS is not a section this write empties.
///
/// A cache miss says only that the section was not answered for this input; whether the render
/// leaves it empty is the template's business. Related-concepts renders the ACCUMULATED links,
/// carried forward precisely so a citation is never retracted. Reporting that as a loss would
/// fire on the ordinary state of every page between an ingest and a drain — which is how an
/// operator learns to skip the one line that reports an irreversible one.
///
/// The two tests above pick the first `## ` heading, which on a document page is the summary —
/// the section that genuinely does empty — so this needs its own fixture.
#[test]
fn a_section_the_render_refills_is_not_reported_as_emptied() {
    let ws = Workspace::with_notes_options("    concept_policy: supported-new\n", "");
    ws.drop_note("note.md", "# Rollback\n\nThe rollback decision.\n");
    assert!(ws.run(&["ingest"]).status.success());

    // A citation on the page and no `concepts_done` — exactly what a drain's result leaves
    // behind until `queue apply` stamps it.
    let page = ws.vault().join("wiki/documents/rollback.md");
    let before = std::fs::read_to_string(&page).expect("page");
    std::fs::write(
        &page,
        format!("{}\n- [Ghost](../concepts/ghost.md)\n", before.trim_end()),
    )
    .expect("write");

    ws.drop_note("note.md", "# Rollback\n\nThe rollback decision.\n");
    let out = ws.run(&["ingest"]);
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "{stderr}");

    let after = std::fs::read_to_string(&page).expect("page");
    assert!(
        after.contains("concepts/ghost.md"),
        "the citation is carried forward, which is what makes the warning false\n{after}"
    );
    // The LAST `## ` heading of a document page is the related-concepts one, whatever the
    // configured locale spells it.
    let concepts_heading = after
        .lines()
        .rfind(|line| line.starts_with("## "))
        .expect("the page carries section headings")
        .trim_start_matches("## ")
        .to_owned();
    assert!(
        !stderr.contains(&format!(
            "emptying an unanswered section: wiki/documents/rollback.md: {concepts_heading}"
        )),
        "a section the render refills must not be reported as emptied\n{stderr}"
    );
}

#[test]
fn manual_original_survives_retirement_and_asset_only_edits_revise_the_same_owner() {
    let ws = Workspace::new("");
    // Keep input in place to exercise asset-only changes against the same location owner.
    let config = ws.root.path().join("config.yaml");
    let text = std::fs::read_to_string(&config).unwrap().replace(
        "inbox_dir: inbox",
        "inbox_dir: inbox\n      archive_after_ingest: false",
    );
    std::fs::write(config, text).unwrap();
    let raw =
        "---\r\ntitle: Asset note\r\n---\r\n# Source heading\r\n\r\n![figure](assets/plot.png)\r\n";
    std::fs::create_dir(ws.vault().join("inbox/assets")).unwrap();
    std::fs::write(ws.vault().join("inbox/assets/plot.png"), [0, 255, 1]).unwrap();
    ws.drop_note("note.md", raw);
    let before = files_under(&ws.vault());
    let dry = ws.run(&["ingest", "--dry-run"]);
    assert!(dry.status.success(), "{}", stderr_of(&dry));
    assert_eq!(files_under(&ws.vault()), before);
    let run = ws.run(&["ingest"]);
    assert!(run.status.success(), "{}", stderr_of(&run));
    let pages = files_under(&ws.vault().join("wiki/documents"));
    assert_eq!(pages.len(), 1);
    let original_page = std::fs::read_to_string(&pages[0]).unwrap();
    let relative = pages[0]
        .strip_prefix(ws.vault())
        .unwrap()
        .to_string_lossy()
        .to_string();
    let query = ws.run(&[
        "wiki", "original", &relative, "--member", "note.md", "--limit", "1",
    ]);
    assert!(query.status.success(), "{}", stderr_of(&query));
    let query: serde_json::Value = serde_json::from_slice(&query.stdout).unwrap();
    assert_eq!(query["preview"], raw);
    assert_eq!(query["members"].as_array().unwrap().len(), 1);
    let manifest = query["reference"]["manifest_revision"]["value"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(ws.vault().join("inbox/note.md").exists());
    assert!(original_page.contains("../../.lorekeeper/evidence/manual/"));
    assert!(original_page.contains("files/assets/plot.png"));
    std::fs::write(ws.vault().join("inbox/assets/plot.png"), [0, 255, 2]).unwrap();
    let second = ws.run(&["ingest"]);
    assert!(second.status.success(), "{}", stderr_of(&second));
    assert_eq!(files_under(&ws.vault().join("wiki/documents")), pages);
    let query = ws.run(&["wiki", "original", &relative]);
    let query: serde_json::Value = serde_json::from_slice(&query.stdout).unwrap();
    assert_ne!(query["reference"]["manifest_revision"]["value"], manifest);
    let after = std::fs::read_to_string(&pages[0]).unwrap();
    let old = lk_core::frontmatter::parse_page(&original_page).unwrap();
    let new = lk_core::frontmatter::parse_page(&after).unwrap();
    assert_eq!(
        old.frontmatter.get("source_file"),
        new.frontmatter.get("source_file")
    );
    assert_ne!(
        old.frontmatter.get("llm_inputs"),
        new.frontmatter.get("llm_inputs")
    );
    // Losing the external input does not lose the actual original lookup.
    std::fs::remove_dir_all(ws.vault().join("inbox")).unwrap();
    let q = ws.run(&["wiki", "original", &relative, "--member", "note.md"]);
    assert!(q.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&q.stdout).unwrap()["preview"],
        raw
    );
}

#[test]
fn manual_html_preserves_raw_and_marks_unsupported_closure_without_retiring() {
    let ws = Workspace::new("");
    let raw = "<!doctype html><html><head><style>body{background:url(unknown.png)}</style></head><body><h1>Saved page</h1><p>A paragraph with an <img src='image.png' alt='chart'> image.</p></body></html>";
    ws.drop_note("saved.html", raw);
    std::fs::write(ws.vault().join("inbox/image.png"), [0, 255]).unwrap();
    let out = ws.run(&["ingest"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(ws.vault().join("inbox/saved.html").exists());
    let pages = files_under(&ws.vault().join("wiki/documents"));
    let relative = pages[0]
        .strip_prefix(ws.vault())
        .unwrap()
        .to_string_lossy()
        .to_string();
    let q = ws.run(&["wiki", "original", &relative, "--member", "saved.html"]);
    assert!(q.status.success(), "{}", stderr_of(&q));
    let q: serde_json::Value = serde_json::from_slice(&q.stdout).unwrap();
    assert_eq!(q["preview"], raw);
    assert!(q["unsupported_total"].as_u64().unwrap() > 0);
    let page = std::fs::read_to_string(&pages[0]).unwrap();
    assert!(page.contains("files/image.png"));
    assert!(!page.contains("file:///inbox"));
    let help = ws.run(&["wiki", "original", "--help"]);
    assert!(help.status.success());
    assert!(
        String::from_utf8_lossy(&help.stdout).contains("read-only")
            || String::from_utf8_lossy(&help.stdout).contains("JSON")
    );
}

#[test]
fn a_captured_pdf_attachment_needs_no_standalone_decoder_to_preserve_the_note() {
    let ws = Workspace::new("");
    ws.drop_note(
        "annotation.md",
        "# Annotation\n\nRead the [original paper](paper.pdf).\n",
    );
    let bytes = include_bytes!("../../lk-source/src/content/fixtures/policy.pdf");
    std::fs::write(ws.vault().join("inbox/paper.pdf"), bytes).unwrap();
    let result = ws.run(&["ingest"]);
    assert!(result.status.success(), "{}", stderr_of(&result));
    assert_eq!(files_under(&ws.vault().join("wiki/documents")).len(), 1);
    assert!(!ws.vault().join("inbox/annotation.md").exists());
    assert_eq!(
        std::fs::read(ws.vault().join("inbox/paper.pdf")).unwrap(),
        bytes
    );
}

#[test]
fn missing_local_asset_is_a_loud_refusal_without_retirement_or_false_page() {
    let ws = Workspace::new("");
    ws.drop_note("missing.md", "# Missing\n\n![x](missing.png)\n");
    let out = ws.run(&["ingest"]);
    assert!(!out.status.success());
    assert!(ws.vault().join("inbox/missing.md").exists());
    assert!(files_under(&ws.vault().join("wiki/documents")).is_empty());
    assert!(files_under(&ws.vault().join(".lorekeeper/evidence/manual")).is_empty());
}

#[test]
fn explicit_obsidian_embed_preserves_asset_before_retiring_only_the_entry() {
    let ws = Workspace::new("");
    ws.drop_note("obsidian.md", "# Obsidian\n\n![[figure.png]]\n");
    std::fs::write(ws.vault().join("inbox/figure.png"), [0, 255]).unwrap();
    let out = ws.run(&["ingest"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!ws.vault().join("inbox/obsidian.md").exists());
    assert!(ws.vault().join("inbox/figure.png").exists());
    let pages = files_under(&ws.vault().join("wiki/documents"));
    let page = std::fs::read_to_string(&pages[0]).unwrap();
    assert!(page.contains("files/figure.png"));
    assert!(!page.contains("![[figure.png]]\n\n##"));
    let parsed = lk_core::frontmatter::parse_page(&page).unwrap();
    let reference: lk_core::manual_original::ManualOriginalReference = serde_json::from_value(
        parsed.frontmatter.get("source_record").unwrap()["manual_original"].clone(),
    )
    .unwrap();
    assert_eq!(
        std::fs::read(ws.vault().join(reference.entry_vault_path().unwrap())).unwrap(),
        b"# Obsidian\n\n![[figure.png]]\n"
    );
}

#[test]
fn immutable_publication_collision_refuses_page_queue_and_retirement_then_retry_recovers() {
    let ws = Workspace::new("");
    ws.drop_note("n.md", "# N\n\nbody\n");
    let objects = ws.vault().join(".lorekeeper/evidence/manual/objects");
    std::fs::create_dir_all(&objects).unwrap();
    let digest = lk_core::manual_original::OriginalDigest::of(b"# N\n\nbody\n").value;
    std::fs::write(objects.join(&digest), "wrong bytes").unwrap();
    let failed = ws.run(&["ingest"]);
    assert!(!failed.status.success());
    assert!(ws.vault().join("inbox/n.md").exists());
    assert!(files_under(&ws.vault().join("wiki/documents")).is_empty());
    assert!(
        files_under(&ws.vault().join(".lorekeeper/queue"))
            .iter()
            .all(|p| p.extension().is_none_or(|e| e != "jsonl"))
    );
    assert_eq!(
        std::fs::read_to_string(objects.join(&digest)).unwrap(),
        "wrong bytes"
    );
    std::fs::remove_file(objects.join(digest)).unwrap();
    let retried = ws.run(&["ingest"]);
    assert!(retried.status.success(), "{}", stderr_of(&retried));
    assert!(!ws.vault().join("inbox/n.md").exists());
}

#[test]
fn partial_manual_failure_and_cross_entry_dependencies_keep_remaining_inputs_self_contained() {
    let ws = Workspace::new("");
    ws.drop_note("A.md", "# A\n\n[B](B.md) ![missing](missing.png)\n");
    ws.drop_note("B.md", "# B\n\nIndependent body\n");
    let partial = ws.run(&["ingest"]);
    assert!(!partial.status.success());
    assert!(ws.vault().join("inbox/A.md").exists());
    assert!(ws.vault().join("inbox/B.md").exists());
    assert_eq!(files_under(&ws.vault().join("wiki/documents")).len(), 1);
    std::fs::write(ws.vault().join("inbox/missing.png"), [0, 255]).unwrap();
    let retry = ws.run(&["ingest"]);
    assert!(retry.status.success(), "{}", stderr_of(&retry));
    assert!(!ws.vault().join("inbox/A.md").exists());
    assert!(
        ws.vault().join("inbox/B.md").exists(),
        "a captured dependency is not automatically removed as an entry"
    );
    assert!(ws.vault().join("inbox/missing.png").exists());
    assert_eq!(files_under(&ws.vault().join("wiki/documents")).len(), 2);
}

#[test]
fn an_excluded_forecast_entry_still_protects_its_consumed_dependency_from_retirement() {
    let ws = Workspace::new("");
    ws.drop_note("A.md", "# Future A\n\n[B](B.md)\n");
    ws.drop_note("B.md", "# B\n\nbody\n");
    std::fs::File::options()
        .write(true)
        .open(ws.vault().join("inbox/A.md"))
        .unwrap()
        .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(3 * 86400))
        .unwrap();
    let run = ws.run(&["ingest"]);
    assert!(run.status.success(), "{}", stderr_of(&run));
    assert_eq!(files_under(&ws.vault().join("wiki/documents")).len(), 1);
    assert!(ws.vault().join("inbox/A.md").exists());
    assert!(ws.vault().join("inbox/B.md").exists());
    let dry = ws.run(&["ingest", "--dry-run"]);
    assert!(dry.status.success());
    assert!(stderr_of(&dry).contains("retirement blocked"));
    assert!(!stderr_of(&dry).contains("retirement eligible"));
}

#[test]
fn managed_pages_cannot_be_captured_or_retired_as_manual_input() {
    for managed in [
        "knowledge/concepts",
        "observations/source",
        "me/weekly",
        "synthesis/weekly",
        ".lorekeeper/input",
    ] {
        let workspace = Workspace::new("");
        let config_path = workspace.root.path().join("config.yaml");
        let config = std::fs::read_to_string(&config_path)
            .unwrap()
            .replace(
                "root: vault",
                "root: vault\n  dirs:\n    wiki: knowledge\n    daily: observations",
            )
            .replace("inbox_dir: inbox", &format!("inbox_dir: {managed}"));
        std::fs::write(&config_path, config).unwrap();
        let input = workspace.vault().join(managed);
        std::fs::create_dir_all(&input).unwrap();
        let page = input.join("existing.md");
        let original = "---\ntype: concept\ntitle: Existing knowledge\n---\n\nCurated knowledge.\n";
        std::fs::write(&page, original).unwrap();
        for args in [
            vec!["validate"],
            vec!["ingest", "notes", "--dry-run"],
            vec!["ingest", "notes"],
            vec!["fetch", "https://example.com/article"],
        ] {
            let output = workspace.run(&args);
            assert!(!output.status.success(), "{managed}: {args:?}");
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("overlaps"),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(std::fs::read_to_string(&page).unwrap(), original);
            assert!(!input.join("archived").exists());
            assert!(!workspace.vault().join("knowledge/documents").exists());
        }
    }
}
