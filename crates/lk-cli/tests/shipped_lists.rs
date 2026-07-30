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

/// The uninstallers are held to the same list as the installers: a skill missing from them stays
/// on disk after an uninstall, and stale skill instructions an agent still loads are worse than a
/// leftover file — they describe a binary that is no longer there.
#[test]
fn install_and_uninstall_scripts_list_every_skill() {
    let skills = shipped_skills();
    for (script, anchor) in [
        ("scripts/install.sh", "for skill in "),
        ("scripts/install.ps1", "$SkillNames = @("),
        ("scripts/uninstall.sh", "SKILL_NAMES=("),
        ("scripts/uninstall.ps1", "$SkillNames = @("),
    ] {
        assert_eq!(
            declared_skills(script, anchor),
            skills,
            "{script}'s skill list must be exactly the directories under .claude/skills"
        );
    }
}

/// The archive an installer asks for has to be one the release builds. They are separate lists in
/// separate languages, so a target renamed on either side is a 404 at install time — the first
/// thing a new user sees, and the one failure they cannot work around.
#[test]
fn every_target_an_installer_asks_for_is_one_the_release_builds() {
    let release = read(&repo_root().join(".github/workflows/release.yml"));
    let built: Vec<&str> = release
        .lines()
        .filter_map(|line| line.trim().strip_prefix("- target: "))
        .map(str::trim)
        .collect();
    assert!(!built.is_empty(), "release.yml declares no build targets");

    for script in ["scripts/install.sh", "scripts/install.ps1"] {
        let body = read(&repo_root().join(script));
        // Anchored on the architecture prefix rather than on a well-formed suffix: a rule that
        // only recognized `-musl`/`-darwin`/`-msvc` endings could not see a MISSPELLED target at
        // all, so it passed vacuously on exactly the drift it was written for.
        let named: Vec<&str> = body
            .split(['\'', '"'])
            .filter(|token| token.starts_with("x86_64-") || token.starts_with("aarch64-"))
            .collect();
        assert!(!named.is_empty(), "{script} names no build target");
        for target in named {
            assert!(
                built.contains(&target),
                "{script} can ask for `{target}`, which the release does not build"
            );
        }
    }
}

/// No skill spells out a vocabulary `AGENTS.md` already generates.
///
/// Three of them enumerated the `document_type` values in prose, which is a copy of a list the
/// schema generator emits from `DOCUMENT_TYPES` into the very file those skills are told to read
/// ("derive everything from AGENTS.md — never hardcode"). Detecting drift between the copies took
/// a windowed word search, because `data` is an ordinary English word; deleting the copies takes
/// nothing, and leaves the generated page as the only place the vocabulary appears.
#[test]
fn no_skill_restates_a_vocabulary_agents_md_generates() {
    for skill in glob_skill_markdown() {
        let body = read(&skill);
        for (at, _) in body.match_indices("document_type") {
            let line = body[..at].rfind('\n').map_or(0, |i| i + 1);
            let line = &body[line..at + body[at..].find('\n').unwrap_or(0)];
            let named: Vec<&&str> = lk_core::document::DOCUMENT_TYPES
                .iter()
                .filter(|value| names_word(line, value))
                .collect();
            // One value is a legitimate example (`document_type: note` in a sample page); two or
            // more on one line is the vocabulary restated.
            assert!(
                named.len() < 2,
                "{} restates the document_type vocabulary ({named:?}) that AGENTS.md generates \
                 from DOCUMENT_TYPES — point at it instead:\n  {line}",
                skill.display()
            );
        }
    }
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
/// pages to the vault has to re-derive ALL of them.
///
/// Listing the commands at each call site is what left `log.md` refreshed by nothing while the
/// catalog and the map were refreshed by five separate places. `lore wiki refresh` re-derives the
/// whole set in one call, so a caller cannot name a subset and a page added to the set reaches
/// every caller without any of them changing — which is why this checks for one command rather
/// than comparing lists. `lore schema` stays separate and per-page, because `AGENTS.md` derives
/// from config rather than from the vault.
#[test]
fn everything_that_writes_pages_refreshes_the_pages_derived_from_the_vault() {
    use lk_core::vault_path::Derivation;

    let vault_derived: Vec<&str> = lk_core::vault_path::GENERATED_WIKI_PAGES
        .iter()
        .filter(|(_, _, derivation)| *derivation == Derivation::VaultContents)
        .map(|(page, _, _)| *page)
        .collect();
    assert!(
        vault_derived.len() > 1,
        "one command for a single page would be indirection, not a contract"
    );

    let pipeline = read(&repo_root().join("scripts/lore-pipeline.sh"));
    assert!(
        pipeline.contains("lore_cmd wiki refresh"),
        "scripts/lore-pipeline.sh never refreshes the vault-derived pages: {vault_derived:?}"
    );
    for (page, command, derivation) in lk_core::vault_path::GENERATED_WIKI_PAGES {
        if derivation == Derivation::Config {
            assert!(
                pipeline.contains(&format!("lore_cmd {command}")),
                "scripts/lore-pipeline.sh never runs `lore {command}`, so {page} is never \
                 refreshed on a schedule"
            );
        }
    }

    // Any skill that finalizes a page-writing run must call it, and none may call the per-page
    // commands instead — a subset is what went stale.
    let mut refreshing = 0;
    for skill in glob_skill_markdown() {
        let body = read(&skill);
        if body.contains("lore wiki refresh") {
            refreshing += 1;
        }
        for (page, command, derivation) in lk_core::vault_path::GENERATED_WIKI_PAGES {
            if derivation == Derivation::VaultContents {
                assert!(
                    !body.contains(&format!("lore {command}")),
                    "{} names `lore {command}` for {page} — call `lore wiki refresh` so no caller \
                     can refresh a subset",
                    skill.display()
                );
            }
        }
    }
    assert!(refreshing > 0, "no skill refreshes the vault-derived pages");

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
