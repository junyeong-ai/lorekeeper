use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn run(config: &Path, dry_run: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lore"));
    for (key, _) in std::env::vars() {
        if key.starts_with("LORE_") {
            command.env_remove(key);
        }
    }
    command
        .arg("--config")
        .arg(config)
        .args(["ingest", "notes"]);
    if dry_run {
        command.arg("--dry-run");
    }
    command.output().unwrap()
}

fn setup(root: &Path, cap: usize, archive: bool) -> PathBuf {
    std::fs::create_dir_all(root.join("vault/inbox")).unwrap();
    let config = root.join("config.yaml");
    std::fs::write(&config, format!("vault: {{root: vault, timezone: UTC, locale: en}}\nidentity: {{name: Test, email: test@example.invalid}}\nllm: {{provider: noop}}\nsources:\n  notes:\n    type: manual\n    concept_policy: disabled\n    params: {{inbox_dir: inbox, archive_after_ingest: {archive}, max_entries_per_run: {cap}}}\n")).unwrap();
    config
}

fn note(root: &Path, name: &str, body: &str) {
    std::fs::write(root.join("vault/inbox").join(name), body).unwrap();
}

fn state_path(root: &Path) -> PathBuf {
    let directory = std::fs::read_dir(root.join("vault/.lorekeeper/manual-admission"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    directory.join("state.json")
}

fn state(root: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(state_path(root)).unwrap()).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn capability_unprocessed_files_are_reported_without_admission_and_examples_are_bounded() {
    let root = tempfile::tempdir().unwrap();
    let config = setup(root.path(), 2, false);
    let original = include_bytes!("../../lk-source/src/content/fixtures/policy.pdf");
    for number in 0..10 {
        std::fs::write(
            root.path().join(format!("vault/inbox/source-{number}.pdf")),
            original,
        )
        .unwrap();
    }
    for preview in [true, false] {
        let output = run(&config, preview);
        assert!(output.status.success(), "{}", stderr(&output));
        let diagnostic = stderr(&output);
        assert!(diagnostic.contains("0 eligible entries"), "{diagnostic}");
        assert!(
            diagnostic.contains("10 capability-unprocessed"),
            "{diagnostic}"
        );
        assert!(diagnostic.contains("0 pending"), "{diagnostic}");
        assert!(
            diagnostic.contains("ingest.documents is not configured"),
            "{diagnostic}"
        );
        assert_eq!(diagnostic.matches("  ! unprocessed ").count(), 8);
        assert!(diagnostic.contains("2 additional capability-unprocessed files"));
        assert!(!root.path().join("vault/wiki/documents").exists());
        assert!(!root.path().join("vault/.lorekeeper/queue").exists());
        if preview {
            assert!(!root.path().join("vault/.lorekeeper").exists());
        } else {
            assert!(state(root.path())["pending"].as_array().unwrap().is_empty());
            assert!(
                state(root.path())["published"]
                    .as_object()
                    .unwrap()
                    .is_empty()
            );
        }
        assert_eq!(
            std::fs::read(root.path().join("vault/inbox/source-0.pdf")).unwrap(),
            original
        );
    }
}

#[test]
fn capability_unprocessed_attachment_is_preserved_with_its_eligible_note() {
    let root = tempfile::tempdir().unwrap();
    let config = setup(root.path(), 2, true);
    let original = include_bytes!("../../lk-source/src/content/fixtures/policy.pdf");
    std::fs::write(root.path().join("vault/inbox/source.pdf"), original).unwrap();
    note(
        root.path(),
        "note.md",
        "# Eligible note\n\n[Original attachment](source.pdf)\n",
    );
    let output = run(&config, false);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stderr(&output).contains("1 capability-unprocessed"));
    assert_eq!(
        state(root.path())["published"].as_object().unwrap().len(),
        1
    );
    let page =
        std::fs::read_to_string(root.path().join("vault/wiki/documents/eligible-note.md")).unwrap();
    let parsed = lk_core::frontmatter::parse_page(&page).unwrap();
    let reference: lk_core::manual_original::ManualOriginalReference = serde_json::from_value(
        parsed.frontmatter.get("source_record").unwrap()["manual_original"].clone(),
    )
    .unwrap();
    let manifest =
        lk_vault::load_manual_original_manifest(&root.path().join("vault"), &reference).unwrap();
    assert!(
        manifest
            .members
            .iter()
            .any(|member| member.path == "source.pdf")
    );
    assert_eq!(
        std::fs::read(root.path().join("vault/inbox/source.pdf")).unwrap(),
        original
    );
    assert!(!root.path().join("vault/inbox/note.md").exists());
}

#[test]
fn explicit_extensions_preserve_user_exclusion_and_missing_decoder_refusal() {
    for extension in ["md", "pdf"] {
        let root = tempfile::tempdir().unwrap();
        let config = setup(root.path(), 2, false);
        let raw = std::fs::read_to_string(&config).unwrap();
        std::fs::write(
            &config,
            raw.replace(
                "inbox_dir: inbox,",
                &format!("inbox_dir: inbox, extensions: [{extension}],"),
            ),
        )
        .unwrap();
        let original = include_bytes!("../../lk-source/src/content/fixtures/policy.pdf");
        std::fs::write(root.path().join("vault/inbox/source.pdf"), original).unwrap();
        note(
            root.path(),
            "note.md",
            "# Eligible note\n\nOriginal source.\n",
        );
        let output = run(&config, false);
        assert_eq!(
            output.status.success(),
            extension == "md",
            "{}",
            stderr(&output)
        );
        assert!(stderr(&output).contains("0 capability-unprocessed"));
        assert!(stderr(&output).contains("1 excluded by extension"));
        assert_eq!(
            state(root.path())["pending"].as_array().unwrap().len(),
            usize::from(extension == "pdf")
        );
        if extension == "pdf" {
            assert!(stderr(&output).contains("binary document processing is not configured"));
        }
        assert_eq!(
            std::fs::read(root.path().join("vault/inbox/source.pdf")).unwrap(),
            original
        );
    }
}

#[test]
fn retained_entries_drain_bounded_turns_and_unchanged_receipts_do_not_republish() {
    let root = tempfile::tempdir().unwrap();
    let config = setup(root.path(), 2, false);
    for name in ["alpha", "bravo", "charlie"] {
        note(
            root.path(),
            &format!("{name}.md"),
            &format!("# {name}\n\nSource {name}.\n"),
        );
    }
    let first = run(&config, false);
    assert!(!first.status.success(), "{}", stderr(&first));
    assert_eq!(
        state(root.path())["published"].as_object().unwrap().len(),
        2
    );
    assert_eq!(state(root.path())["pending"].as_array().unwrap().len(), 1);
    let second = run(&config, false);
    assert!(second.status.success(), "{}", stderr(&second));
    assert_eq!(
        state(root.path())["published"].as_object().unwrap().len(),
        3
    );
    let before = std::fs::read(root.path().join("vault/wiki/documents/alpha.md")).unwrap();
    let third = run(&config, false);
    assert!(third.status.success(), "{}", stderr(&third));
    assert!(
        stderr(&third).contains("3 verified_published; 0 attempted"),
        "{}",
        stderr(&third)
    );
    assert_eq!(
        std::fs::read(root.path().join("vault/wiki/documents/alpha.md")).unwrap(),
        before
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("vault/inbox/alpha.md")).unwrap(),
        "# alpha\n\nSource alpha.\n"
    );
}

#[test]
fn receipt_reuse_refuses_duplicate_owners_without_retiring_or_rewriting_inputs() {
    for archive in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let config = setup(root.path(), 2, archive);
        let original = "# alpha\n\nExact original.\n";
        note(root.path(), "alpha.md", original);
        assert!(run(&config, false).status.success());
        let owner = root.path().join("vault/wiki/documents/alpha.md");
        let duplicate = owner.with_file_name("duplicate.md");
        let page = std::fs::read(&owner).unwrap();
        std::fs::write(&duplicate, &page).unwrap();
        for body in [original, "# alpha\n\nChanged original.\n"] {
            note(root.path(), "alpha.md", body);
            for preview in [true, false] {
                let before = std::fs::read(state_path(root.path())).unwrap();
                let output = run(&config, preview);
                assert!(!output.status.success(), "{}", stderr(&output));
                assert!(
                    stderr(&output).contains("claimed by multiple pages"),
                    "{}",
                    stderr(&output)
                );
                assert_eq!(std::fs::read(&owner).unwrap(), page);
                assert_eq!(std::fs::read(&duplicate).unwrap(), page);
                assert_eq!(std::fs::read(state_path(root.path())).unwrap(), before);
                assert_eq!(
                    std::fs::read_to_string(root.path().join("vault/inbox/alpha.md")).unwrap(),
                    body
                );
            }
        }
    }
}

#[test]
fn foreign_structured_owner_invalidates_receipt_without_overwriting_that_page() {
    let root = tempfile::tempdir().unwrap();
    let config = setup(root.path(), 2, true);
    let original = "# alpha\n\nExact original.\n";
    note(root.path(), "alpha.md", original);
    assert!(run(&config, false).status.success());
    let owner = root.path().join("vault/wiki/documents/alpha.md");
    let page = std::fs::read_to_string(&owner).unwrap();
    let foreign = page.replacen(
        "---\n",
        "---\nsource_identity: {repository: elsewhere, document: another}\n",
        1,
    );
    std::fs::write(&owner, &foreign).unwrap();
    note(root.path(), "alpha.md", original);
    let output = run(&config, false);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stderr(&output).contains("0 verified_published; 1 attempted"));
    assert_eq!(std::fs::read_to_string(&owner).unwrap(), foreign);
    let target = state(root.path())["published"]["alpha.md"]["target"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(target, "wiki/documents/alpha.md");
    assert!(root.path().join("vault").join(target).exists());
    assert!(!root.path().join("vault/inbox/alpha.md").exists());
}

#[test]
fn invalid_structured_identity_never_falls_back_to_a_location_receipt() {
    let root = tempfile::tempdir().unwrap();
    let config = setup(root.path(), 2, false);
    note(root.path(), "alpha.md", "# alpha\n\nSource.\n");
    assert!(run(&config, false).status.success());
    let owner = root.path().join("vault/wiki/documents/alpha.md");
    let page = std::fs::read_to_string(&owner).unwrap().replacen(
        "---\n",
        "---\nsource_identity: {repository: elsewhere}\n",
        1,
    );
    std::fs::write(&owner, &page).unwrap();
    let before = std::fs::read(state_path(root.path())).unwrap();
    for preview in [true, false] {
        let refused = run(&config, preview);
        assert!(!refused.status.success(), "{}", stderr(&refused));
        assert_eq!(std::fs::read_to_string(&owner).unwrap(), page);
        assert_eq!(std::fs::read(state_path(root.path())).unwrap(), before);
        assert!(root.path().join("vault/inbox/alpha.md").exists());
    }
}

#[cfg(unix)]
#[test]
fn receipt_ownership_preserves_nested_directory_aliases_and_refuses_hardlink_owners() {
    let root = tempfile::tempdir().unwrap();
    let config = setup(root.path(), 2, false);
    note(root.path(), "alpha.md", "# alpha\n\nSource.\n");
    assert!(run(&config, false).status.success());
    let documents = root.path().join("vault/wiki/documents");
    let nested = documents.join("nested");
    std::fs::create_dir(&nested).unwrap();
    let owner = nested.join("alpha.md");
    std::fs::rename(documents.join("alpha.md"), &owner).unwrap();
    assert!(run(&config, false).status.success());
    std::os::unix::fs::symlink(&nested, documents.join("alias")).unwrap();
    let before = std::fs::read(&owner).unwrap();
    let reused = run(&config, false);
    assert!(reused.status.success(), "{}", stderr(&reused));
    assert!(stderr(&reused).contains("1 verified_published; 0 attempted"));
    assert_eq!(std::fs::read(&owner).unwrap(), before);
    std::fs::hard_link(&owner, documents.join("hardlink.md")).unwrap();
    let refused = run(&config, false);
    assert!(!refused.status.success(), "{}", stderr(&refused));
    assert!(stderr(&refused).contains("claimed by multiple pages"));
    assert_eq!(std::fs::read(&owner).unwrap(), before);
    assert!(root.path().join("vault/inbox/alpha.md").exists());
}

#[test]
fn bootstrap_preview_and_lost_or_corrupt_known_state_are_distinct() {
    let root = tempfile::tempdir().unwrap();
    let config = setup(root.path(), 1, false);
    note(root.path(), "alpha.md", "# alpha\n\nOriginal.\n");
    let preview = run(&config, true);
    assert!(preview.status.success(), "{}", stderr(&preview));
    assert!(!root.path().join("vault/.lorekeeper").exists());
    let first = run(&config, false);
    assert!(first.status.success(), "{}", stderr(&first));
    let path = state_path(root.path());
    let before = std::fs::read(&path).unwrap();
    let page = std::fs::read(root.path().join("vault/wiki/documents/alpha.md")).unwrap();
    for corrupt in [None, Some("invalid"), Some("{}"), Some("[]")] {
        if let Some(raw) = corrupt {
            std::fs::write(&path, raw).unwrap();
        } else {
            std::fs::remove_file(&path).unwrap();
        }
        let output = run(&config, false);
        assert!(!output.status.success(), "{}", stderr(&output));
        assert_eq!(
            std::fs::read(root.path().join("vault/wiki/documents/alpha.md")).unwrap(),
            page
        );
        std::fs::write(&path, &before).unwrap();
    }
    assert!(run(&config, false).status.success());
}

#[test]
fn operational_changes_reuse_but_noop_to_queue_republishes_and_enqueues() {
    let root = tempfile::tempdir().unwrap();
    let config = setup(root.path(), 2, false);
    note(root.path(), "alpha.md", "# alpha\n\nOriginal.\n");
    assert!(run(&config, false).status.success());
    let raw = std::fs::read_to_string(&config).unwrap();
    std::fs::write(
        &config,
        raw.replace("max_entries_per_run: 2", "max_entries_per_run: 1"),
    )
    .unwrap();
    let unchanged = run(&config, false);
    assert!(unchanged.status.success(), "{}", stderr(&unchanged));
    assert!(
        stderr(&unchanged).contains("1 verified_published; 0 attempted"),
        "{}",
        stderr(&unchanged)
    );
    std::fs::write(&config, raw.replace("provider: noop", "provider: queue")).unwrap();
    let queued = run(&config, false);
    assert!(queued.status.success(), "{}", stderr(&queued));
    assert!(
        stderr(&queued).contains("0 verified_published; 1 attempted"),
        "{}",
        stderr(&queued)
    );
    assert!(
        std::fs::read_dir(root.path().join("vault/.lorekeeper/queue"))
            .unwrap()
            .next()
            .is_some()
    );
}

#[test]
fn omitted_incoming_owner_protects_dependency_until_owner_publishes_and_retires() {
    let root = tempfile::tempdir().unwrap();
    let config = setup(root.path(), 1, true);
    note(root.path(), "child.md", "# child\n\nDependency.\n");
    note(root.path(), "owner.md", "# owner\n\n[child](child.md)\n");
    let preview = run(&config, true);
    assert!(
        stderr(&preview).contains("retirement blocked"),
        "{}",
        stderr(&preview)
    );
    let first = run(&config, false);
    assert!(!first.status.success(), "{}", stderr(&first));
    assert!(root.path().join("vault/inbox/child.md").exists());
    let second = run(&config, false);
    assert!(second.status.success(), "{}", stderr(&second));
    assert!(root.path().join("vault/inbox/child.md").exists());
    assert!(!root.path().join("vault/inbox/owner.md").exists());
    let third = run(&config, false);
    assert!(third.status.success(), "{}", stderr(&third));
    assert!(!root.path().join("vault/inbox/child.md").exists());
    assert!(
        stderr(&third).contains("1 verified_published; 0 attempted"),
        "{}",
        stderr(&third)
    );
}

#[test]
fn absent_inbox_refuses_but_empty_new_state_bootstraps_and_lost_known_state_refuses() {
    let root = tempfile::tempdir().unwrap();
    let config = setup(root.path(), 1, false);
    let inbox = root.path().join("vault/inbox");
    std::fs::remove_dir(&inbox).unwrap();
    let empty = run(&config, false);
    assert!(!empty.status.success(), "{}", stderr(&empty));
    assert!(!inbox.exists());
    std::fs::create_dir(&inbox).unwrap();
    note(root.path(), "alpha.md", "# alpha\n\nSource.\n");
    assert!(run(&config, false).status.success());
    let path = state_path(root.path());
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    let lost = run(&config, false);
    assert!(!lost.status.success(), "{}", stderr(&lost));
    assert!(!path.exists());
    std::fs::remove_dir_all(&inbox).unwrap();
    assert!(!run(&config, false).status.success());
}

#[test]
fn decoder_refusal_does_not_starve_later_text_or_new_entries() {
    let root = tempfile::tempdir().unwrap();
    let config = setup(root.path(), 1, false);
    let raw = std::fs::read_to_string(&config).unwrap().replace(
        "inbox_dir: inbox,",
        "inbox_dir: inbox, extensions: [docx, md],",
    );
    std::fs::write(&config, raw).unwrap();
    note(root.path(), "bad.docx", "not a configured decoder input");
    note(root.path(), "good.md", "# good\n\nValid source.\n");
    assert!(!run(&config, false).status.success());
    let second = run(&config, false);
    assert!(!second.status.success(), "{}", stderr(&second));
    assert!(root.path().join("vault/wiki/documents/good.md").exists());
    note(root.path(), "another.md", "# another\n\nLater source.\n");
    assert!(!run(&config, false).status.success());
    assert!(!run(&config, false).status.success());
    assert!(root.path().join("vault/wiki/documents/another.md").exists());
    assert!(root.path().join("vault/inbox/bad.docx").exists());
}

#[test]
fn changing_canonical_inbox_under_known_source_refuses_instead_of_bootstrapping() {
    let root = tempfile::tempdir().unwrap();
    let config = setup(root.path(), 1, false);
    note(root.path(), "alpha.md", "# alpha\n\nOriginal.\n");
    assert!(run(&config, false).status.success());
    let before = std::fs::read(state_path(root.path())).unwrap();
    std::fs::create_dir(root.path().join("vault/other")).unwrap();
    std::fs::write(
        root.path().join("vault/other/bravo.md"),
        "# bravo\n\nAnother input.\n",
    )
    .unwrap();
    let raw = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, raw.replace("inbox_dir: inbox", "inbox_dir: other")).unwrap();
    for preview in [true, false] {
        let output = run(&config, preview);
        assert!(!output.status.success(), "{}", stderr(&output));
        assert!(
            stderr(&output).contains("initialization marker"),
            "{}",
            stderr(&output)
        );
        assert!(!root.path().join("vault/wiki/documents/bravo.md").exists());
        assert_eq!(std::fs::read(state_path(root.path())).unwrap(), before);
    }
}

#[test]
fn individually_oversized_normalized_first_entry_cannot_starve_a_later_fit() {
    let root = tempfile::tempdir().unwrap();
    let config = setup(root.path(), 2048, true);
    note(
        root.path(),
        "a-large.md",
        &format!("# large\n\n{}\n", "x".repeat(8192)),
    );
    note(root.path(), "b-small.md", "# small\n\nValid later input.\n");
    let raw = std::fs::read_to_string(&config).unwrap();
    let labels = serde_json::to_string(
        &(0..150)
            .map(|index| format!("classification-{index:03}-{}", "x".repeat(40)))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    std::fs::write(
        &config,
        format!(
            "{}\ningest:\n  inputs: {{max_source_bytes: 12288, max_run_bytes: 12288}}\n",
            raw.replace(
                "    type: manual\n",
                &format!("    type: manual\n    labels: {labels}\n")
            )
        ),
    )
    .unwrap();
    let preview = run(&config, true);
    assert!(!preview.status.success(), "{}", stderr(&preview));
    assert!(
        stderr(&preview).contains("retirement blocked"),
        "{}",
        stderr(&preview)
    );
    assert!(
        !stderr(&preview).contains("retirement eligible"),
        "{}",
        stderr(&preview)
    );
    let first = run(&config, false);
    assert!(!first.status.success(), "{}", stderr(&first));
    assert!(stderr(&first).contains("2 admitted"), "{}", stderr(&first));
    assert!(
        stderr(&first).contains("manual planning: serialized input exceeds"),
        "{}",
        stderr(&first)
    );
    assert!(root.path().join("vault/wiki/documents/small.md").exists());
    assert!(!root.path().join("vault/wiki/documents/large.md").exists());
    assert_eq!(
        state(root.path())["pending"],
        serde_json::json!(["a-large.md"])
    );
    let second = run(&config, false);
    assert!(!second.status.success(), "{}", stderr(&second));
    assert!(
        stderr(&second).contains("1 verified_published"),
        "{}",
        stderr(&second)
    );
    assert_eq!(
        state(root.path())["published"].as_object().unwrap().len(),
        1
    );
    assert_eq!(
        state(root.path())["pending"],
        serde_json::json!(["a-large.md"])
    );
}

#[tokio::test]
async fn nested_owner_projection_refusal_preserves_that_owner_and_admits_later_fit() {
    let root = tempfile::tempdir().unwrap();
    let config = setup(root.path(), 2048, false);
    note(
        root.path(),
        "a-large.md",
        &format!("# large\n\n{}", "![asset](asset.png)\n\n".repeat(32)),
    );
    note(
        root.path(),
        "b-small.md",
        "# small\n\nFits independently.\n",
    );
    std::fs::write(root.path().join("vault/inbox/asset.png"), [0, 255]).unwrap();
    let loaded = lk_core::config::Config::load(&config).unwrap();
    let context = lk_source::ExtractContext {
        target_date: jiff::Timestamp::now()
            .to_zoned(jiff::tz::TimeZone::UTC)
            .date(),
        timezone: jiff::tz::TimeZone::UTC,
        locale: loaded.vault.locale(),
        identity: loaded.identity.clone(),
        vault_root: loaded.vault.root_path(),
        vault_dirs: loaded.vault.dirs.clone(),
        documents: None,
        projection_cache: None,
        originals: loaded.ingest.originals.clone(),
        max_input_bytes: loaded.ingest.inputs.max_source_bytes.get(),
    };
    let mut capture = lk_source::capture_manual(&loaded.sources["notes"].params, &context)
        .await
        .unwrap();
    capture.project_links(Path::new("wiki/documents")).unwrap();
    let item = capture
        .items
        .iter()
        .find(|item| item.title == "large")
        .unwrap();
    let limit = item.body.len();
    let relative = Path::new("wiki/documents/nested/deeper/preserved/large.md");
    assert!(
        lk_vault::relocate_markdown_links(
            &item.body,
            Path::new("wiki/documents/document.md"),
            relative
        )
        .unwrap()
        .len()
            > limit
    );
    let owner = root.path().join("vault").join(relative);
    std::fs::create_dir_all(owner.parent().unwrap()).unwrap();
    let original = format!(
        "---\ntype: document\ntitle: large\ncreated: 2020-01-01\nsource_file: {}\n---\n## Summary\n\n## Content\n\nEstablished owner.\n",
        serde_json::to_string(&context.vault_root.join("inbox/a-large.md")).unwrap()
    );
    std::fs::write(&owner, &original).unwrap();
    let raw = std::fs::read_to_string(&config).unwrap();
    std::fs::write(
        &config,
        format!(
            "{raw}\ningest:\n  documents: {{python: /unused/runtime, max_output_bytes: {limit}}}\n"
        ),
    )
    .unwrap();
    let output = run(&config, false);
    assert!(!output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("manual planning: relocated manual Markdown"),
        "{}",
        stderr(&output)
    );
    assert!(root.path().join("vault/wiki/documents/small.md").exists());
    assert_eq!(std::fs::read_to_string(&owner).unwrap(), original);
    assert_eq!(
        state(root.path())["pending"],
        serde_json::json!(["a-large.md"])
    );
    assert_eq!(
        state(root.path())["published"].as_object().unwrap().len(),
        1
    );
    assert!(root.path().join("vault/inbox/a-large.md").exists());
}
