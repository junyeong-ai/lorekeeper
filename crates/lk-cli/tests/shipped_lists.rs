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
        // Anchored on the architecture prefix, not on a well-formed suffix: recognizing only
        // `-musl`/`-darwin`/`-msvc` endings would not see a MISSPELLED target at all, which is
        // the drift this exists for.
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

/// Every command a skill's own procedure spells is one its `allowed-tools` permits.
///
/// A skill runs under `claude -p` in the scheduled pipeline, where nothing can prompt: a command
/// the procedure spells and the allowlist omits is simply DENIED mid-step. `lore-setup` declared
/// nine tools and its own `references/jira.md` piped into `grep`, which was the step that finds
/// the `start_date_field` id — so config discovery stopped there, unattended, with no way to ask.
/// Two hand-kept lists in one file, which is what a gate is for.
#[test]
fn every_command_a_skill_spells_is_one_it_permits() {
    // Shell builtins and control words. A procedure that leads with one of these is not naming a
    // tool an allowlist can spell, which is itself the finding — `Bash(for *)` does not exist.
    const NOT_A_TOOL: &[&str] = &[
        "for", "do", "done", "if", "then", "fi", "else", "while", "case", "esac", "cd", "export",
    ];

    let mut denied: Vec<String> = Vec::new();
    for dir in std::fs::read_dir(repo_root().join(".claude/skills"))
        .expect("skills dir")
        .flatten()
        .filter(|e| e.path().is_dir())
    {
        let skill = dir.file_name().to_string_lossy().into_owned();
        let manifest = read(&dir.path().join("SKILL.md"));
        let frontmatter = manifest.split("---").nth(1).expect("skill frontmatter");
        let permitted: Vec<String> = frontmatter
            .lines()
            .filter_map(|line| line.trim().strip_prefix("Bash("))
            .filter_map(|rest| rest.strip_suffix(')'))
            .map(|spec| spec.split_whitespace().next().unwrap_or(spec).to_owned())
            .collect();
        if permitted.is_empty() {
            continue;
        }

        // The manifest and every reference file it ships with.
        let mut bodies = vec![manifest.clone()];
        let references = dir.path().join("references");
        if references.is_dir() {
            for entry in std::fs::read_dir(&references)
                .expect("references")
                .flatten()
            {
                bodies.push(read(&entry.path()));
            }
        }

        for body in bodies {
            for block in body.split("```bash").skip(1) {
                let Some(code) = block.split("```").next() else {
                    continue;
                };
                // A trailing `\\` continues the line, so the next one carries flags rather than
                // a command; joining first is what keeps `--limit` from reading as a tool.
                let joined = code.replace("\\\n", " ");
                for line in joined.lines() {
                    let line = line.trim().trim_start_matches("$ ");
                    // A slash-command illustration is not a shell command.
                    if line.is_empty() || line.starts_with('#') || line.starts_with('/') {
                        continue;
                    }
                    // Every stage of a pipe or a `||`/`&&` chain is its own permission decision.
                    for stage in line.split(["|", "&"].map(|s| s.chars().next().unwrap())) {
                        let Some(argv0) = stage.split_whitespace().next() else {
                            continue;
                        };
                        let argv0 = argv0.trim_start_matches('(');
                        if NOT_A_TOOL.contains(&argv0) || argv0.contains('=') {
                            denied.push(format!("{skill}: `{argv0}` is a shell builtin, which no allowlist entry can name — spell the step as a command"));
                            continue;
                        }
                        if !argv0
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || "-_./[".contains(c))
                        {
                            continue;
                        }
                        if !permitted.iter().any(|tool| tool == argv0) {
                            denied.push(format!(
                                "{skill}: spells `{argv0}`, which its allowed-tools does not permit"
                            ));
                        }
                    }
                }
            }
        }
    }
    denied.sort();
    denied.dedup();
    assert!(
        denied.is_empty(),
        "under `claude -p` an unpermitted step is denied, never prompted:\n  {}",
        denied.join("\n  ")
    );
}

/// The pipeline's `--allowedTools` covers every command the skills it runs declare needing.
///
/// Nothing in `claude -p` can prompt, so a command the skill's protocol spells and this list
/// omits is simply DENIED — the drain then fails on a step it was told to perform, every night,
/// for work it may already have committed. Two hand-kept lists that must agree is exactly the
/// shape the skill-packaging lists were in when a skill shipped uninstallable.
#[test]
fn the_pipeline_permits_every_tool_the_skills_it_runs_declare() {
    let pipeline = read(&repo_root().join("scripts/lore-pipeline.sh"));
    let allowed: Vec<String> = pipeline
        .split('"')
        .filter_map(|token| token.strip_prefix("Bash("))
        .filter_map(|token| token.strip_suffix(":*)"))
        .map(str::to_owned)
        .collect();
    assert!(!allowed.is_empty(), "the pipeline permits no Bash command");

    // The skills the pipelines actually invoke, read off the scripts rather than listed here.
    let weekly = read(&repo_root().join("scripts/lore-weekly.sh"));
    let invoked: Vec<String> = format!("{pipeline}{weekly}")
        .split('"')
        .filter_map(|token| token.strip_prefix("$SKILL_DIR/"))
        .map(str::to_owned)
        .collect();
    assert!(
        !invoked.is_empty(),
        "no skill invocation found in the pipeline scripts"
    );

    for skill in invoked {
        let body = read(
            &repo_root()
                .join(".claude/skills")
                .join(&skill)
                .join("SKILL.md"),
        );
        let frontmatter = body.split("---").nth(1).expect("skill frontmatter");
        for line in frontmatter.lines() {
            let Some(cmd) = line
                .trim()
                .strip_prefix("Bash(")
                .and_then(|rest| rest.strip_suffix(" *)"))
            else {
                continue;
            };
            assert!(
                allowed.iter().any(|a| a == cmd),
                "{skill} declares it runs `{cmd}`, which the pipeline's --allowedTools denies — \
                 nothing in `claude -p` can prompt, so the step is refused unattended"
            );
        }
    }
}

/// Every asset the installer downloads is checksummed, and the release publishes the checksum.
///
/// The pipelines were the exception both ways: `install.sh` fetched them and ran them without
/// verifying anything, and `release.yml` published no `.sha256` beside them. They are the most
/// dangerous asset of the three — a scheduler fires them unattended with the user's shell
/// environment — and they were the only one taken on trust.
#[test]
fn every_downloaded_pipeline_is_checksummed_on_both_sides() {
    let install = read(&repo_root().join("scripts/install.sh"));
    let release = read(&repo_root().join(".github/workflows/release.yml"));

    let pipelines: Vec<String> = install
        .lines()
        .find_map(|line| line.trim().strip_prefix("PIPELINES=\"").map(str::to_owned))
        .expect("install.sh declares a PIPELINES list")
        .trim_end_matches('"')
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    assert!(!pipelines.is_empty(), "install.sh declares no pipelines");

    for name in &pipelines {
        assert!(
            release.contains(name),
            "release.yml never packages `{name}`, which install.sh downloads"
        );
    }
    assert!(
        release.contains(r#"sha256sum "$name" > "${name}.sha256""#),
        "release.yml packages the pipelines without publishing their checksums"
    );
    assert!(
        install.contains("Pipeline '${name}' checksum mismatch"),
        "install.sh downloads a pipeline without verifying it"
    );
}

/// The two scripts agree about where an install lives.
///
/// `install.sh --install-dir /opt/bin` put the binary somewhere `uninstall.sh` had no way to be
/// told about, so it reported "Nothing to uninstall" and left it there.
#[test]
fn the_uninstaller_takes_every_path_flag_the_installer_does() {
    let install = read(&repo_root().join("scripts/install.sh"));
    let uninstall = read(&repo_root().join("scripts/uninstall.sh"));
    for flag in ["--install-dir", "--data-dir"] {
        assert!(install.contains(flag), "install.sh lost {flag}");
        assert!(
            uninstall.contains(flag),
            "uninstall.sh cannot be told about {flag}, so an install made with it is invisible"
        );
    }
}

/// No skill spells out a vocabulary `AGENTS.md` already generates.
///
/// Enumerating the `document_type` values in a skill copies a list the schema generator emits
/// from `DOCUMENT_TYPES` into the very file those skills are told to read ("derive everything
/// from AGENTS.md — never hardcode"). The generated page is the only place the vocabulary
/// belongs; a copy has to be deleted rather than kept in sync, since `data` is an ordinary
/// English word and detecting drift between the copies takes a windowed search.
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
    // commands instead — a subset is what went stale. The GENERATED page is held to the same rule:
    // it is the authoritative procedure every skill is told to read, and it went on naming
    // `wiki index` then `wiki map` after the skills had stopped.
    let mut refreshing = 0;
    for skill in glob_skill_markdown()
        .into_iter()
        .chain([repo_root().join("crates/lk-cli/src/commands/schema.rs")])
    {
        let body = read(&skill);
        if body.contains("lore wiki refresh") {
            refreshing += 1;
        }
        for (page, command, derivation) in lk_core::vault_path::GENERATED_WIKI_PAGES {
            if derivation != Derivation::VaultContents {
                continue;
            }
            // The full invocation only. `index`, `log` and `map` are also ordinary English
            // words, so banning the bare form fails the build on "write the reason to the run
            // `log`" — a check that cannot tell prose from a command list is worse than the
            // drift it looks for.
            let named = format!("lore {command}");
            assert!(
                !body.contains(&named),
                "{} names `{named}` for {page} — say `lore wiki refresh` so no caller can \
                 refresh or report on a subset",
                skill.display()
            );
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
                source_table(body).contains(&wire),
                "{name}'s source table has no `{wire}` row, so a shipped adapter is undiscoverable"
            );
        }
    }

    // Where a source's pages LAND is one branch in the pipeline — `manual` writes documents,
    // everything else writes a daily page — and `lore-ingest` stated it as a list of six
    // adapters. `confluence` reached config.example.yaml, both READMEs and the setup skill's
    // config reference while that sentence stayed at six, so the skill offered a source and then
    // described a routing without it. A rule cannot fall behind; a list has to be gated, so the
    // gate is that it stays a rule.
    let routing = read(&root.join(".claude/skills/lore-ingest/SKILL.md"));
    let sentence = routing
        .lines()
        .zip(routing.lines().skip(1))
        .find(|(line, next)| {
            line.contains("writes `<wiki>/documents/{slug}.md`")
                || next.contains("writes `<daily>/{source-id}/DATE.md`")
        })
        .map(|(line, next)| format!("{line} {next}"))
        .expect("lore-ingest states where each source's pages land");
    let enumerated: Vec<String> = lk_core::config::SourceType::iter()
        .map(|t| t.to_string())
        .filter(|wire| wire != "manual")
        .filter(|wire| sentence.to_lowercase().contains(wire.as_str()))
        .collect();
    assert!(
        enumerated.is_empty(),
        "lore-ingest's page-routing sentence names {enumerated:?} instead of stating the rule \
         (`manual` writes documents, every other type writes a daily page) — a list of adapters \
         goes stale the moment one is added, which is how `confluence` was left out"
    );
}

/// The type names in the first column of the README's source table.
///
/// Located by its heading and bounded by the end of the table, not collected from every line in
/// the file that happens to start like a row: a standalone `| `x` | … |` line elsewhere satisfied
/// the looser rule, and a legitimate row whose cell is not backticked failed it. Both directions
/// were wrong, in a gate whose whole purpose is to say where it looked.
fn source_table(body: &str) -> Vec<String> {
    let table = body
        .lines()
        .skip_while(|line| !is_sources_heading(line))
        .skip(1)
        .skip_while(|line| !line.trim_start().starts_with('|'))
        .take_while(|line| line.trim_start().starts_with('|'));
    table
        .filter_map(|row| row.trim().trim_start_matches('|').split('|').next())
        .map(|cell| cell.trim().trim_matches('`').trim().to_owned())
        .filter(|cell| !cell.is_empty() && !cell.starts_with('-'))
        .collect()
}

/// The heading the source table sits under, in either README's language.
fn is_sources_heading(line: &str) -> bool {
    let heading = line.trim();
    heading.starts_with("## ") && matches!(&heading[3..], "Sources" | "소스")
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

/// `rust-version` is the one the toolchain enforces, and the READMEs are the only other place it
/// belongs: they are what a person reads to decide whether they can build this. CI reads it out of
/// the manifest, so a restatement there would be a fourth copy of one number and there is nothing
/// in the workflow to compare.
#[test]
fn both_readmes_state_the_msrv_the_manifest_declares() {
    let root = repo_root();
    let manifest = read(&root.join("Cargo.toml"));
    let msrv = manifest
        .lines()
        .find_map(|line| line.strip_prefix("rust-version = "))
        .map(|value| value.trim().trim_matches('"').to_string())
        .expect("workspace.package must declare rust-version");

    for readme in ["README.md", "README.en.md"] {
        let body = read(&root.join(readme));
        assert!(
            body.contains(&msrv),
            "{readme} must state MSRV {msrv} (badge and prose)"
        );
    }
    let ci = read(&root.join(".github/workflows/ci.yml"));
    assert!(
        !ci.contains(&msrv),
        "the CI workflow must not restate MSRV {msrv} — it reads `rust-version` from the manifest"
    );
}
