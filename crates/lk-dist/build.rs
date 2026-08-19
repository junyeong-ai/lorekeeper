//! Enumerates the agent skills and the pipeline scripts so `embedded.rs` can `include_str!`
//! each one.
//!
//! Generated rather than listed: the set is a directory tree, and a hand-written list is a
//! copy of it that nothing compares. Every file AND every directory is declared to Cargo, so
//! editing a skill rebuilds the binary that carries it and adding one is picked up.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// The scheduled pipelines, named because `scripts/` also holds the installers and the gate
/// runner — files a user never receives through `lore`.
const PIPELINES: [&str; 3] = ["lore-pipeline.sh", "lore-daily.sh", "lore-weekly.sh"];

fn main() {
    let repo = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"))
        .join("../..");
    let skills = repo.join(".claude/skills");

    let mut out = String::new();
    emit(&mut out, "SKILL_FILES", collect(&skills, &skills));
    emit(
        &mut out,
        "PIPELINE_FILES",
        PIPELINES
            .iter()
            .map(|name| ((*name).to_string(), repo.join("scripts").join(name)))
            .collect(),
    );
    emit(
        &mut out,
        "CONFIG_FILES",
        vec![(
            "config.example.yaml".to_string(),
            repo.join("config.example.yaml"),
        )],
    );

    let dest = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR")).join("embedded.rs");
    std::fs::write(&dest, out).unwrap_or_else(|e| panic!("write {}: {e}", dest.display()));
}

fn collect(dir: &Path, base: &Path) -> Vec<(String, PathBuf)> {
    // A file list alone would not see an ADDED file: Cargo re-runs on a watched path changing,
    // and a new file is a change to its directory.
    println!("cargo:rerun-if-changed={}", dir.display());

    let mut found = Vec::new();
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("directory entry").path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            found.extend(collect(&path, base));
        } else {
            let relative = path
                .strip_prefix(base)
                .expect("entry under base")
                .to_string_lossy()
                .replace('\\', "/");
            found.push((relative, path));
        }
    }
    found
}

fn emit(out: &mut String, name: &str, mut files: Vec<(String, PathBuf)>) {
    // Sorted so the generated file — and therefore the deploy order and every test that reads
    // it — is a function of the tree's contents rather than of directory iteration order.
    files.sort_by(|a, b| a.0.cmp(&b.0));
    assert!(!files.is_empty(), "{name} would be empty");

    writeln!(out, "pub const {name}: &[(&str, &str)] = &[").unwrap();
    for (relative, path) in &files {
        println!("cargo:rerun-if-changed={}", path.display());
        let absolute = path
            .canonicalize()
            .unwrap_or_else(|e| panic!("canonicalize {}: {e}", path.display()));
        writeln!(out, "    ({relative:?}, include_str!({absolute:?})),").unwrap();
    }
    writeln!(out, "];").unwrap();
}
