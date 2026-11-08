use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::{Command, Stdio};

use serde::Serialize;

use super::{Extracted, Manifest};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum InputState {
    Current,
    Changed,
    Unmeasured,
    Invalid,
}

#[derive(Serialize)]
struct EntryVerification {
    source: Vec<String>,
    vault_page: Option<String>,
    baseline: Option<String>,
    status: InputState,
    detail: String,
}

#[derive(Serialize)]
struct ProjectVerification {
    name: String,
    entries: Vec<EntryVerification>,
    error: Option<String>,
}

fn git(repo: &Path, args: &[&str]) -> std::io::Result<Vec<u8>> {
    let output = Command::new("git")
        .arg("--no-replace-objects")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!("git {} failed", args[0])));
    }
    Ok(output.stdout)
}

/// Hash the opened bytes as a Git blob, without attributes, filters or index flags.
fn input_blob(repo: &Path, root: &Path, relative: &str) -> std::io::Result<String> {
    let path = lk_vault::page_path(root, Path::new(relative))?;
    let file = lk_core::fs::open_regular_file(&path)?;
    let output = Command::new("git")
        .arg("--no-replace-objects")
        .arg("-C")
        .arg(repo)
        .args(["hash-object", "--no-filters", "--stdin"])
        .stdin(Stdio::from(file))
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other("git hash-object failed"));
    }
    String::from_utf8(output.stdout)
        .map(|value| value.trim().to_owned())
        .map_err(std::io::Error::other)
}

fn baseline_blobs(
    repo: &Path,
    commit: &str,
    paths: &BTreeSet<&str>,
) -> std::io::Result<BTreeMap<String, String>> {
    let prefix = git(repo, &["rev-parse", "--show-prefix"])?;
    let prefix = std::str::from_utf8(prefix.strip_suffix(b"\n").unwrap_or(&prefix))
        .map_err(std::io::Error::other)?;
    let specs: Vec<String> = paths
        .iter()
        .map(|path| format!(":(literal){prefix}{path}"))
        .collect();
    let mut args = vec!["ls-tree", "-z", "--full-tree", commit, "--"];
    args.extend(specs.iter().map(String::as_str));
    let tree = git(repo, &args)?;
    let mut blobs = BTreeMap::new();
    for record in tree
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let record = std::str::from_utf8(record).map_err(std::io::Error::other)?;
        let (metadata, path) = record
            .split_once('\t')
            .ok_or_else(|| std::io::Error::other("invalid Git tree entry"))?;
        let fields: Vec<_> = metadata.split(' ').collect();
        if let ["100644" | "100755", "blob", blob] = fields.as_slice()
            && let Some(path) = path.strip_prefix(prefix)
        {
            blobs.insert(path.to_owned(), (*blob).to_owned());
        }
    }
    Ok(blobs)
}

fn verify_entry(vault: &Path, repo: &Path, entry: &Extracted) -> (InputState, String) {
    let unmeasured = |why: &str| (InputState::Unmeasured, why.to_owned());
    let invalid = |why: &str| (InputState::Invalid, why.to_owned());
    let Some(commit) = entry.extracted_at_commit.as_deref() else {
        return unmeasured("no verified extraction commit recorded");
    };
    if !matches!(commit.len(), 40 | 64) || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return unmeasured("extraction commit must be a full Git object ID");
    }
    if entry.inputs.is_empty() {
        return unmeasured("no explicit retained input bindings recorded");
    }
    if git(repo, &["cat-file", "-t", commit]).ok().as_deref() != Some(b"commit\n") {
        return unmeasured("recorded extraction commit is unavailable");
    }
    let paths: BTreeSet<_> = entry
        .inputs
        .iter()
        .map(|input| input.path.as_str())
        .collect();
    if paths.len() != entry.inputs.len()
        || entry.source.paths().is_empty()
        || !entry
            .source
            .paths()
            .iter()
            .all(|path| paths.contains(path.as_str()))
    {
        return invalid("retained inputs must uniquely bind every declared source and dependency");
    }
    for path in &paths {
        if let Err(error) = lk_vault::page_path(repo, Path::new(path)) {
            return invalid(&format!("invalid source address {path}: {error}"));
        }
    }
    let blobs = match baseline_blobs(repo, commit, &paths) {
        Ok(blobs) => blobs,
        Err(error) => return unmeasured(&format!("cannot inspect extraction baseline: {error}")),
    };
    for input in &entry.inputs {
        let Some(expected) = blobs.get(&input.path) else {
            return invalid(&format!(
                "not an exact regular-file blob at baseline: {}",
                input.path
            ));
        };
        match input_blob(repo, vault, &input.capture) {
            Ok(actual) if actual == *expected => {}
            Ok(_) => {
                return invalid(&format!(
                    "retained input differs from baseline: {}",
                    input.path
                ));
            }
            Err(error) => {
                return invalid(&format!(
                    "cannot verify capture for {}: {error}",
                    input.path
                ));
            }
        }
    }
    let mut changed = Vec::new();
    for input in &entry.inputs {
        match input_blob(repo, repo, &input.path) {
            Ok(actual) if actual == blobs[&input.path] => {}
            Ok(_) => changed.push(input.path.as_str()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                changed.push(input.path.as_str())
            }
            Err(error) => {
                return unmeasured(&format!(
                    "cannot read current input {}: {error}",
                    input.path
                ));
            }
        }
    }
    if changed.is_empty() {
        (
            InputState::Current,
            "all declared retained inputs match the baseline and current files".into(),
        )
    } else {
        (
            InputState::Changed,
            format!(
                "current inputs differ or are missing: {}",
                changed.join(", ")
            ),
        )
    }
}

pub(super) fn run(
    opts: &crate::commands::GlobalOptions,
    project: Option<String>,
    json: bool,
) -> miette::Result<()> {
    let config = super::load_config(&super::find_config(opts)?)?;
    let vault = config.vault.root_path();
    let directory = vault.join(".lorekeeper/extracts");
    let directories = match std::fs::read_dir(&directory) {
        Ok(entries) => entries
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| miette::miette!("read extraction manifests: {error}"))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(miette::miette!("read extraction manifests: {error}")),
    };
    let mut reports = Vec::new();
    for directory in directories {
        let file_type = directory
            .file_type()
            .map_err(|error| miette::miette!("inspect extraction directory: {error}"))?;
        if !file_type.is_dir() && !file_type.is_symlink() {
            continue;
        }
        let name = directory.file_name().to_string_lossy().into_owned();
        if project.as_ref().is_some_and(|selected| *selected != name) {
            continue;
        }
        let relative = Path::new(".lorekeeper/extracts")
            .join(&name)
            .join("manifest.yaml");
        match std::fs::symlink_metadata(vault.join(&relative)) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            _ => {}
        }
        let manifest = (|| -> Result<Manifest, String> {
            use std::io::Read;
            let path = lk_vault::page_path(&vault, &relative).map_err(|error| error.to_string())?;
            let mut file =
                lk_core::fs::open_regular_file(&path).map_err(|error| error.to_string())?;
            let mut text = String::new();
            file.read_to_string(&mut text)
                .map_err(|error| error.to_string())?;
            lk_core::yaml::from_str(&text).map_err(|error| error.to_string())
        })();
        let mut report = ProjectVerification {
            name,
            entries: Vec::new(),
            error: None,
        };
        match manifest {
            Ok(manifest) => {
                let repo = lk_core::config::expand_tilde(&manifest.project.repo_path);
                for entry in manifest.extracted {
                    let (status, detail) = verify_entry(&vault, &repo, &entry);
                    report.entries.push(EntryVerification {
                        source: entry.source.paths().to_vec(),
                        vault_page: entry.vault_page,
                        baseline: entry.extracted_at_commit,
                        status,
                        detail,
                    });
                }
            }
            Err(error) => report.error = Some(error),
        }
        reports.push(report);
    }
    reports.sort_by(|a, b| a.name.cmp(&b.name));
    if project.is_some() && reports.is_empty() {
        return Err(miette::miette!(
            "no extraction manifest for the selected project"
        ));
    }
    let count = |status| {
        reports
            .iter()
            .flat_map(|project| &project.entries)
            .filter(|entry| entry.status == status)
            .count()
    };
    let invalid = count(InputState::Invalid)
        + reports
            .iter()
            .filter(|project| project.error.is_some())
            .count();
    let unsettled = invalid + count(InputState::Changed) + count(InputState::Unmeasured);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "scope": "declared-extraction-inputs",
                "projects": reports,
                "current": count(InputState::Current),
                "changed": count(InputState::Changed),
                "unmeasured": count(InputState::Unmeasured),
                "invalid": invalid,
            }))
            .map_err(|error| miette::miette!("{error}"))?
        );
    } else {
        for report in &reports {
            if let Some(error) = &report.error {
                println!("{}: {error}", report.name);
            }
            for entry in &report.entries {
                println!(
                    "{} · {}: {}",
                    report.name,
                    entry.source.join(", "),
                    entry.detail
                );
            }
        }
    }
    if unsettled > 0 {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::extract::{RetainedInput, Source};
    use std::path::PathBuf;

    struct Fixture {
        _directory: tempfile::TempDir,
        repo: PathBuf,
        vault: PathBuf,
        entry: Extracted,
    }

    impl Fixture {
        fn build() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let repo = directory.path().join("repo");
            let vault = directory.path().join("vault");
            std::fs::create_dir_all(repo.join("docs")).unwrap();
            std::fs::create_dir_all(vault.join("retained")).unwrap();
            git(&repo, &["init", "-q"]).unwrap();
            git(&repo, &["config", "user.email", "fixture@example.invalid"]).unwrap();
            git(&repo, &["config", "user.name", "Fixture"]).unwrap();
            let mut inputs = Vec::new();
            for (path, body) in [
                ("CLAUDE.md", "@AGENTS.md\n"),
                ("AGENTS.md", "Preserve declared identities. 한글\n"),
                ("docs/a[1].md", "A decision and its conditions.\n"),
            ] {
                std::fs::write(repo.join(path), body).unwrap();
                let capture = format!("retained/{}", path.replace('/', "-"));
                std::fs::write(vault.join(&capture), body).unwrap();
                inputs.push(RetainedInput {
                    path: path.into(),
                    capture,
                });
            }
            git(&repo, &["add", "--all"]).unwrap();
            git(&repo, &["commit", "-qm", "source"]).unwrap();
            let commit = String::from_utf8(git(&repo, &["rev-parse", "HEAD"]).unwrap()).unwrap();
            Self {
                _directory: directory,
                repo,
                vault,
                entry: Extracted {
                    source: Source::Many(vec!["CLAUDE.md".into(), "docs/a[1].md".into()]),
                    vault_page: Some("wiki/documents/decision.md".into()),
                    extracted_at_commit: Some(commit.trim().into()),
                    inputs,
                },
            }
        }

        fn state(&self) -> InputState {
            verify_entry(&self.vault, &self.repo, &self.entry).0
        }
    }

    #[test]
    fn imported_instruction_drift_is_detected_when_the_shim_and_git_diff_are_unchanged() {
        let fixture = Fixture::build();
        assert!(fixture.state() == InputState::Current);
        git(
            &fixture.repo,
            &["update-index", "--assume-unchanged", "AGENTS.md"],
        )
        .unwrap();
        std::fs::write(fixture.repo.join("AGENTS.md"), "A different constraint.\n").unwrap();
        assert!(
            git(&fixture.repo, &["diff", "--name-only", "HEAD"])
                .unwrap()
                .is_empty()
        );
        assert!(fixture.state() == InputState::Changed);
    }

    #[test]
    fn captures_must_match_the_recorded_commit_and_bind_all_sources() {
        let mut fixture = Fixture::build();
        std::fs::write(
            fixture.vault.join("retained/AGENTS.md"),
            "Other revision.\n",
        )
        .unwrap();
        assert!(fixture.state() == InputState::Invalid);
        fixture.entry.inputs.remove(0);
        assert!(fixture.state() == InputState::Invalid);
    }

    #[test]
    fn absent_proof_is_unmeasured_while_a_verified_deleted_input_is_changed() {
        let mut fixture = Fixture::build();
        let commit = fixture.entry.extracted_at_commit.take().unwrap();
        assert!(fixture.state() == InputState::Unmeasured);
        fixture.entry.extracted_at_commit = Some(commit[..8].into());
        assert!(fixture.state() == InputState::Unmeasured);
        fixture.entry.extracted_at_commit = Some(commit);
        std::fs::remove_file(fixture.repo.join("AGENTS.md")).unwrap();
        assert!(fixture.state() == InputState::Changed);
        fixture.entry.inputs.clear();
        assert!(fixture.state() == InputState::Unmeasured);
    }

    #[test]
    fn literal_paths_work_from_a_repository_subdirectory_and_captures_cannot_escape() {
        let mut fixture = Fixture::build();
        fixture.repo = fixture.repo.join("docs");
        fixture.entry.source = Source::One("a[1].md".into());
        fixture
            .entry
            .inputs
            .retain(|input| input.path == "docs/a[1].md");
        fixture.entry.inputs[0].path = "a[1].md".into();
        assert!(fixture.state() == InputState::Current);
        fixture.entry.inputs[0].capture = "../outside.md".into();
        assert!(fixture.state() == InputState::Invalid);
    }
}
