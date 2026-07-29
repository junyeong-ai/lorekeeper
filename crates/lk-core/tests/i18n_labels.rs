//! Pins every `Strings` field to a reader somewhere in the workspace.
//!
//! A label with no reader is a heading or title the vault can never show — and because
//! `Strings` is also the vocabulary `lore schema` publishes as the page-format spec, an
//! orphaned field describes a section of a page format that does not exist. Two did: one
//! outlived the concept template's metadata section, so the spec named a heading no page
//! had carried since, and one outlived the manual source's move to document pages. The
//! compiler cannot see either — every field is initialized by both locale tables, which is
//! all `#[warn(dead_code)]` asks — so the check lives here.
//!
//! This file names no field, and excludes itself from the corpus regardless: a search for
//! readers cannot count the searcher.

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Every file that could name a label: Rust sources other than the bundle itself, the
/// templates that render headings, and the skills that read them back.
fn readers(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![
        root.join("crates"),
        root.join("templates"),
        root.join(".claude"),
    ];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n == "target") {
                    continue;
                }
                stack.push(path);
                continue;
            }
            if path.ends_with("i18n.rs") || path.ends_with(file!().rsplit('/').next().unwrap()) {
                continue;
            }
            let is_source = path
                .extension()
                .is_some_and(|e| e == "rs" || e == "jinja" || e == "md");
            if is_source && let Ok(text) = std::fs::read_to_string(&path) {
                out.push(text);
            }
        }
    }
    assert!(!out.is_empty(), "found no files to search under {root:?}");
    out
}

#[test]
fn every_label_has_a_reader() {
    let root = workspace_root();
    let bundle = std::fs::read_to_string(root.join("crates/lk-core/src/i18n.rs"))
        .expect("read the i18n bundle");
    let struct_body = bundle
        .split("pub struct Strings {")
        .nth(1)
        .and_then(|rest| rest.split("\n}").next())
        .expect("Strings struct present");

    let fields: Vec<&str> = struct_body
        .lines()
        .filter_map(|l| l.trim().strip_prefix("pub "))
        .filter_map(|l| l.split(':').next())
        .collect();
    assert!(fields.len() > 20, "expected the full bundle: {fields:?}");

    let corpus = readers(&root);
    let orphans: Vec<&str> = fields
        .iter()
        .copied()
        .filter(|field| !corpus.iter().any(|text| text.contains(*field)))
        .collect();

    assert!(
        orphans.is_empty(),
        "these labels have no reader — a heading the vault can never show, and a page-format \
         section `lore schema` would publish without any page carrying it: {orphans:?}"
    );
}
