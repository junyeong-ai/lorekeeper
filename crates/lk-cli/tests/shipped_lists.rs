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

/// The skills are what an agent reads before writing a document page's frontmatter, and three of
/// them enumerate the `document_type` values by hand while `lk_core::document::DOCUMENT_TYPES` is
/// what the vault accepts. A value added there and not here leaves every skill telling agents to
/// choose from a shorter list; a value renamed leaves them writing one the schema does not admit.
///
/// The files are discovered rather than named: any skill markdown whose text near `document_type`
/// enumerates values must enumerate all of them. That catches both drifts, because a rename
/// removes the old name from the code and the file that still lacks the new one fails.
///
/// Judged on the window around each mention rather than the whole file, because `data` is an
/// ordinary English word that appears in prose everywhere — a whole-file search for it would pass
/// no matter what the enumeration said. A mention whose window names SOME of the values is an
/// enumeration and must name them all; one that names none is prose about the field, not a list.
#[test]
fn every_skill_that_enumerates_document_types_enumerates_all_of_them() {
    const WINDOW: usize = 240;
    let declared = lk_core::document::DOCUMENT_TYPES;
    let mut enumerations = 0;
    for skill in glob_skill_markdown() {
        let body = read(&skill);
        for (at, _) in body.match_indices("document_type") {
            let start = body[..at]
                .char_indices()
                .rev()
                .nth(WINDOW)
                .map_or(0, |(i, _)| i);
            let end = body[at..]
                .char_indices()
                .nth(WINDOW)
                .map_or(body.len(), |(i, _)| at + i);
            let window = &body[start..end];
            let named: Vec<&&str> = declared.iter().filter(|v| names_word(window, v)).collect();
            if named.is_empty() {
                continue;
            }
            enumerations += 1;
            assert_eq!(
                named.len(),
                declared.len(),
                "{} enumerates document_type values but names only {named:?} of {declared:?}",
                skill.display()
            );
        }
    }
    assert!(
        enumerations > 0,
        "no skill markdown enumerates the document_type values — did they move?"
    );
}

/// Whether `text` contains `word` as a whole word, so `note` does not match `notebook`.
fn names_word(text: &str, word: &str) -> bool {
    text.match_indices(word).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + word.len()..].chars().next();
        let boundary = |c: Option<char>| c.is_none_or(|c| !c.is_alphanumeric() && c != '_');
        boundary(before) && boundary(after)
    })
}

/// Every `.md` under `.claude/skills`, SKILL.md and `references/` alike.
fn glob_skill_markdown() -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![repo_root().join(".claude/skills")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
            .flatten()
        {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "md") {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

/// A page derived wholesale is only true while something re-derives it, and everything that adds
/// pages to the vault has to. The shipped pipeline must refresh all four, because config can
/// change between scheduled runs; every skill that writes pages must refresh the ones derived
/// from vault CONTENTS, since writing pages is what makes them stale.
///
/// The pipeline ran three of the four: `log.md` was refreshed by nothing, so the knowledge
/// timeline stopped at whenever a person last typed `lore wiki log` — stale on a live vault while
/// a module comment claimed it was regenerated each run. Checking only the drain skill then missed
/// the same omission in three others, which is why the skills are DISCOVERED here: any skill that
/// refreshes one vault-derived page is a skill that writes pages, and must refresh them all.
#[test]
fn everything_that_writes_pages_regenerates_every_generated_wiki_page() {
    use lk_core::vault_path::Derivation;

    let vault_derived: Vec<&str> = lk_core::vault_path::GENERATED_WIKI_PAGES
        .iter()
        .filter(|(_, _, derivation)| *derivation == Derivation::VaultContents)
        .map(|(_, command, _)| *command)
        .collect();

    let pipeline = read(&repo_root().join("scripts/lore-pipeline.sh"));
    for (page, command, _) in lk_core::vault_path::GENERATED_WIKI_PAGES {
        assert!(
            pipeline.contains(&format!("lore_cmd {command}")),
            "scripts/lore-pipeline.sh never runs `lore {command}`, so {page} is never refreshed \
             on a schedule"
        );
    }

    let mut refreshing = 0;
    for skill in glob_skill_markdown() {
        let body = read(&skill);
        let runs: Vec<&&str> = vault_derived
            .iter()
            .filter(|command| body.contains(&format!("lore {command}")))
            .collect();
        if runs.is_empty() {
            continue;
        }
        refreshing += 1;
        assert_eq!(
            runs.len(),
            vault_derived.len(),
            "{} refreshes {runs:?} but not all of {vault_derived:?} — a skill that writes pages \
             leaves the rest stale",
            skill.display()
        );
    }
    assert!(refreshing > 0, "no skill refreshes any generated wiki page");

    // And the two lists describe the same set of files.
    let mut named: Vec<&str> = lk_core::vault_path::GENERATED_WIKI_PAGES
        .iter()
        .map(|(page, _, _)| *page)
        .collect();
    named.sort_unstable();
    let mut reserved = lk_core::vault_path::RESERVED_WIKI_FILES.to_vec();
    reserved.sort_unstable();
    assert_eq!(
        named, reserved,
        "every reserved wiki filename must name the command that generates it"
    );
}

/// An adapter that ships and appears in no document is one nobody can configure. Both READMEs
/// carried a source table naming eight of nine — `confluence` appeared in neither, so the only way
/// to discover it was to read the enum. `config.example.yaml` is the file the installer copies and
/// the READMEs call the reference for every source, so each type has to appear there too, with
/// params its own adapter accepts: a sample that fails `lore validate` is worse than no sample.
#[test]
fn every_source_type_is_documented_and_exemplified() {
    use strum::IntoEnumIterator;

    let root = repo_root();
    let example = read(&root.join("config.example.yaml"));
    let readmes: Vec<(&str, String)> = ["README.md", "README.en.md"]
        .into_iter()
        .map(|name| (name, read(&root.join(name))))
        .collect();

    for source_type in lk_core::config::SourceType::iter() {
        let wire = source_type.to_string();
        assert!(
            example.contains(&format!("type: {wire}")),
            "config.example.yaml has no `{wire}` source, which the READMEs call the reference for \
             every source"
        );
        for (name, body) in &readmes {
            assert!(
                body.contains(&wire),
                "{name} never mentions the `{wire}` source, so a shipped adapter is undiscoverable"
            );
        }
    }
}

/// Every source in the shipped example must pass its own adapter's parameter validation. The
/// example is what `install.sh` copies and what a user edits, and `Config::load` alone checks only
/// the core keys — a typo in an adapter's params sat in it until someone ran `lore validate`.
#[test]
fn every_source_in_the_example_config_validates_against_its_adapter() {
    let path = repo_root().join("config.example.yaml");
    let config = lk_core::config::Config::load(&path).expect("the shipped example must load");
    let mut checked = 0;
    // Disabled sources included: the example disables most of them, and a broken sample is
    // exactly what a user hits when they enable one.
    for (id, source) in &config.sources {
        lk_source::validate_params(source.source_type, &source.params).unwrap_or_else(|e| {
            panic!(
                "config.example.yaml sources.{id} ({}): {e}",
                source.source_type
            )
        });
        checked += 1;
    }
    assert!(checked > 0, "the example config declares no sources");
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
