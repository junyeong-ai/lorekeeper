//! Pins the hand-written lists in the shipped installers to what the repository actually holds.
//!
//! Both are lists a human must remember to update, in files no compiler reads. A seventh skill
//! added under `.claude/skills` passed every gate and was then never packaged, never published
//! and never installed, because the name had to be added to `install.sh` and `install.ps1`
//! separately. The MSRV is the same shape: one `rust-version` and several restatements of it,
//! with nothing comparing them.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Every directory under `.claude/skills`, which is what the release workflow packages (it
/// enumerates the directory) and therefore what a release ships.
fn shipped_skills() -> Vec<String> {
    let dir = repo_root().join(".claude/skills");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert!(!names.is_empty(), "no skills under {}", dir.display());
    names
}

/// The quoted names on the one line of `script` that declares its skill list. Read from the
/// declaration rather than by scanning the file, so a legacy scheduled-task name elsewhere in it
/// is not mistaken for a skill — and so both directions can be compared as sets: a missing name
/// is never installed, and a leftover name asks the release for an archive it does not contain.
fn declared_skills(script: &str, anchor: &str) -> Vec<String> {
    let path = repo_root().join(script);
    let body = read(&path);
    let line = body
        .lines()
        .find(|line| line.contains(anchor))
        .unwrap_or_else(|| panic!("{script} no longer declares its skill list with `{anchor}`"));
    let mut names: Vec<String> = line
        .split(['\'', '"'])
        .filter(|token| token.starts_with("lore-"))
        .map(str::to_owned)
        .collect();
    names.sort();
    names
}

#[test]
fn install_scripts_list_every_skill() {
    let skills = shipped_skills();
    for (script, anchor) in [
        ("scripts/install.sh", "for skill in "),
        ("scripts/install.ps1", "$SkillNames = @("),
    ] {
        assert_eq!(
            declared_skills(script, anchor),
            skills,
            "{script}'s skill list must be exactly the directories under .claude/skills"
        );
    }
}

/// `rust-version` is the one the toolchain enforces; the CI job name, the toolchain it pins, and
/// both READMEs restate it. All of them are read by people deciding whether they can build this.
#[test]
fn every_stated_msrv_matches_the_manifest() {
    let root = repo_root();
    let manifest = read(&root.join("Cargo.toml"));
    let msrv = manifest
        .lines()
        .find_map(|line| line.strip_prefix("rust-version = "))
        .map(|value| value.trim().trim_matches('"').to_string())
        .expect("workspace.package must declare rust-version");

    let ci = read(&root.join(".github/workflows/ci.yml"));
    assert!(
        ci.contains(&format!("MSRV ({msrv})")),
        "the CI job name must state MSRV {msrv}"
    );
    assert!(
        ci.contains(&format!("dtolnay/rust-toolchain@{msrv}.0")),
        "the MSRV job must pin toolchain {msrv}.0"
    );
    for readme in ["README.md", "README.en.md"] {
        let body = read(&root.join(readme));
        assert!(
            body.contains(&msrv),
            "{readme} must state MSRV {msrv} (badge and prose)"
        );
    }
}
