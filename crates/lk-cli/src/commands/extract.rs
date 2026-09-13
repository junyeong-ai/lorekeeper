//! `lore extract status` — how far each project's knowledge extraction has fallen behind.
//!
//! `/lore-extract` writes one manifest per project under `<vault>/.lorekeeper/extracts/`, and
//! that manifest already DECLARES what it would take to judge it: the date the scan was taken,
//! the commit it was taken against, and the source patterns it read. Nothing read those
//! declarations back. So a project could go a quarter and a thousand documents past its scan
//! while every other row of `lore status` reported green — extraction was the one plane in
//! this tool that did not answer to a checker, which is exactly the gap `lore self status`
//! closed for the installation.
//!
//! What is checked here is the deterministic half. The judgment half — whether a page's prose
//! is good, whether a project identifier leaked, which concepts two projects now share —
//! stays with `/lore-extract audit`, because it needs a reader rather than a diff.
//!
//! Staleness is measured against the DECLARED source patterns rather than against the
//! repository as a whole. A quarter of commits that never touched `docs/` leaves an extraction
//! current, and saying otherwise would mark the row permanently on any active repository —
//! an alarm nobody can clear is one everybody learns to skip.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

use super::{find_config, load_config};

#[derive(clap::Subcommand)]
pub enum ExtractCommand {
    /// What each project's extraction manifest declares, and how far its repository has moved
    /// since
    Status {
        /// Emit the report as JSON — the contract a skill reads
        #[arg(long)]
        json: bool,
    },
}

/// The manifest as this reads it. Deliberately NOT `deny_unknown_fields`: the shape is owned by
/// the `/lore-extract` skill and carries operator overrides (`strip_patterns`, `domains`,
/// `concept_mapping`) this has no opinion about. Refusing a manifest over a key this does not
/// need would make a reporting command fail on a file that is doing its job.
#[derive(Deserialize)]
struct Manifest {
    project: Project,
    #[serde(default)]
    discovered_sources: Vec<DiscoveredSource>,
    #[serde(default)]
    extracted: Vec<Extracted>,
}

#[derive(Deserialize)]
struct Project {
    name: String,
    repo_path: String,
    last_scan: Option<jiff::civil::Date>,
    /// Absent for a project not under git — the skill falls back to file mtimes there, and so
    /// does the verdict below.
    #[serde(default)]
    git_head_at_scan: Option<String>,
}

#[derive(Deserialize)]
struct DiscoveredSource {
    path: String,
}

#[derive(Deserialize)]
struct Extracted {
    /// One source, or the list of sources that folded into one page.
    source: Source,
    /// `null` records a deliberate skip, which is coverage rather than a gap.
    #[serde(default)]
    vault_page: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Source {
    One(String),
    Many(Vec<String>),
}

impl Source {
    fn paths(&self) -> &[String] {
        match self {
            Source::One(_) => std::slice::from_ref(self.first()),
            Source::Many(v) => v,
        }
    }

    fn first(&self) -> &String {
        match self {
            Source::One(s) => s,
            Source::Many(v) => &v[0],
        }
    }
}

/// Where a project's repository stands against the commit its manifest names.
///
/// Each variant is a different question for the operator, so none of them collapses into
/// another: a repository that is gone needs a path fixed, a baseline that no longer resolves
/// needs a re-scan to re-anchor, and a moved one needs the extraction run again.
enum RepoState {
    Missing,
    /// No baseline commit was recorded — the project is not under git, or was not when scanned.
    NoBaseline,
    /// The recorded commit is not in this repository: a rebase, a fresh clone, or a rewritten
    /// history. The scan cannot be diffed against anything, which is not the same as unmoved.
    BaselineGone,
    Unmoved,
    Moved {
        commits: usize,
        files: usize,
    },
}

impl RepoState {
    fn is_current(&self) -> bool {
        matches!(self, RepoState::Unmoved)
    }

    fn describe(&self) -> String {
        match self {
            RepoState::Missing => "repository not found".into(),
            RepoState::NoBaseline => "no git baseline".into(),
            RepoState::BaselineGone => "baseline commit is gone".into(),
            RepoState::Unmoved => "sources unchanged".into(),
            RepoState::Moved { commits, files } => {
                format!("{files} declared source file(s) changed over {commits} commit(s)")
            }
        }
    }
}

pub(crate) struct ProjectReport {
    name: String,
    repo: PathBuf,
    last_scan: Option<jiff::civil::Date>,
    days_since: Option<i64>,
    declared: usize,
    pages: usize,
    skipped: usize,
    state: RepoState,
}

impl ProjectReport {
    fn is_current(&self) -> bool {
        self.state.is_current()
    }
}

pub async fn run(opts: &super::GlobalOptions, cmd: ExtractCommand) -> miette::Result<()> {
    let ExtractCommand::Status { json } = cmd;
    let config = load_config(&find_config(opts)?)?;
    let today = jiff::Zoned::now()
        .with_time_zone(config.vault.timezone())
        .date();
    let reports = survey(&config.vault.root_path(), today)?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&as_json(&reports)).unwrap()
        );
    } else {
        render(&reports);
    }
    Ok(())
}

/// Read every manifest under the vault's extract directory, oldest scan first.
///
/// A manifest that will not parse is reported rather than skipped — it is a project whose
/// extraction state is unknown, which is the one answer silence would be mistaken for.
pub(crate) fn survey(
    vault_root: &Path,
    today: jiff::civil::Date,
) -> miette::Result<Vec<ProjectReport>> {
    let dir = vault_root.join(".lorekeeper").join("extracts");
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(out);
    };
    for entry in entries.flatten() {
        let path = entry.path().join("manifest.yaml");
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let manifest: Manifest = serde_yaml_ng::from_str(&text)
            .map_err(|e| miette::miette!("{}: {e}", path.display()))?;
        out.push(report(manifest, today));
    }
    out.sort_by(|a, b| a.last_scan.cmp(&b.last_scan).then(a.name.cmp(&b.name)));
    Ok(out)
}

fn report(manifest: Manifest, today: jiff::civil::Date) -> ProjectReport {
    let repo = expand_home(&manifest.project.repo_path);
    let declared: Vec<String> = manifest
        .discovered_sources
        .iter()
        .map(|s| s.path.clone())
        .collect();
    let state = repo_state(
        &repo,
        manifest.project.git_head_at_scan.as_deref(),
        &declared,
    );
    let pages = manifest
        .extracted
        .iter()
        .filter(|e| e.vault_page.is_some())
        .flat_map(|e| e.source.paths())
        .cloned()
        .collect::<BTreeSet<_>>()
        .len();
    let skipped = manifest
        .extracted
        .iter()
        .filter(|e| e.vault_page.is_none())
        .count();
    ProjectReport {
        name: manifest.project.name,
        repo,
        last_scan: manifest.project.last_scan,
        days_since: manifest
            .project
            .last_scan
            .map(|d| today.since(d).map(|s| s.get_days() as i64).unwrap_or(0)),
        declared: declared.len(),
        pages,
        skipped,
        state,
    }
}

/// `~` is expanded because the manifest is written by an agent transcribing what the operator
/// typed, and both spellings appear in manifests already on disk.
fn expand_home(raw: &str) -> PathBuf {
    match raw.strip_prefix("~/") {
        Some(rest) => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(rest),
            None => PathBuf::from(raw),
        },
        None => PathBuf::from(raw),
    }
}

/// Ask git what moved under the paths the scan declared it was reading.
///
/// The declared paths go through as pathspecs, which is what they already are — the skill
/// records them verbatim from `find`/`ls`, so a glob stays a glob and a file stays a file.
/// git's pathspec wildcards reach across directory separators where the scan's would not, so
/// this can only over-report, and over-reporting a change is the direction that sends someone
/// to look rather than the one that keeps them away.
fn repo_state(repo: &Path, baseline: Option<&str>, declared: &[String]) -> RepoState {
    if !repo.join(".git").exists() {
        return RepoState::Missing;
    }
    let Some(baseline) = baseline else {
        return RepoState::NoBaseline;
    };
    if git(
        repo,
        &["rev-parse", "--verify", &format!("{baseline}^{{commit}}")],
    )
    .is_none()
    {
        return RepoState::BaselineGone;
    }
    let range = format!("{baseline}..HEAD");
    let mut args = vec!["log", "--oneline", &range, "--"];
    args.extend(declared.iter().map(String::as_str));
    let commits = git(repo, &args).map_or(0, |out| out.lines().count());
    if commits == 0 {
        return RepoState::Unmoved;
    }
    let mut args = vec!["diff", "--name-only", &range, "--"];
    args.extend(declared.iter().map(String::as_str));
    let files = git(repo, &args).map_or(0, |out| out.lines().count());
    RepoState::Moved { commits, files }
}

fn git(repo: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn render(reports: &[ProjectReport]) {
    if reports.is_empty() {
        println!("No extraction manifests — `/lore-extract scan <repo>` writes the first.");
        return;
    }
    println!("=== Project knowledge extraction ===\n");
    let width = reports
        .iter()
        .map(|r| unicode_width::UnicodeWidthStr::width(r.name.as_str()))
        .max()
        .unwrap_or(0);
    for r in reports {
        let mark = if r.is_current() { ' ' } else { '!' };
        let scanned = match (r.last_scan, r.days_since) {
            (Some(d), Some(n)) => format!("scanned {d} ({n}d ago)"),
            _ => "never scanned".to_string(),
        };
        println!(
            "{mark} {}  {scanned} · {}",
            super::pad(&r.name, width),
            r.state.describe()
        );
        println!(
            "  {}  {} source group(s) declared · {} extracted · {} skipped",
            super::pad("", width),
            r.declared,
            r.pages,
            r.skipped
        );
    }
    let behind = reports.iter().filter(|r| !r.is_current()).count();
    println!();
    if behind == 0 {
        println!("Every manifest is current with its repository.");
    } else {
        println!(
            "{behind} of {} project(s) have moved since their scan — `/lore-extract scan <repo>` \
             then `/lore-extract run <repo>`; `/lore-extract audit` judges what is already there.",
            reports.len()
        );
    }
}

fn as_json(reports: &[ProjectReport]) -> serde_json::Value {
    serde_json::json!({
        "projects": reports.iter().map(|r| serde_json::json!({
            "name": r.name,
            "repo": r.repo.to_string_lossy(),
            "last_scan": r.last_scan.map(|d| d.to_string()),
            "days_since_scan": r.days_since,
            "state": match &r.state {
                RepoState::Missing => serde_json::json!({"kind": "repo-missing"}),
                RepoState::NoBaseline => serde_json::json!({"kind": "no-baseline"}),
                RepoState::BaselineGone => serde_json::json!({"kind": "baseline-gone"}),
                RepoState::Unmoved => serde_json::json!({"kind": "current"}),
                RepoState::Moved { commits, files } => serde_json::json!({
                    "kind": "moved", "commits": commits, "files": files,
                }),
            },
            "declared_source_groups": r.declared,
            "extracted_sources": r.pages,
            "skipped_sources": r.skipped,
        })).collect::<Vec<_>>(),
        "behind": reports.iter().filter(|r| !r.is_current()).count(),
    })
}

/// How many projects have moved since their scan, for the `lore status` row.
pub(crate) fn behind(reports: &[ProjectReport]) -> usize {
    reports.iter().filter(|r| !r.is_current()).count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    const TODAY: jiff::civil::Date = jiff::civil::date(2026, 9, 13);

    fn write_manifest(vault: &Path, name: &str, body: &str) {
        let dir = vault.join(".lorekeeper").join("extracts").join(name);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("manifest.yaml"), body).expect("write");
    }

    fn git_repo(at: &Path) {
        std::fs::create_dir_all(at.join("docs")).expect("mkdir");
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "t"],
        ] {
            Command::new("git")
                .arg("-C")
                .arg(at)
                .args(args)
                .output()
                .expect("git");
        }
    }

    fn commit(at: &Path, file: &str, body: &str) -> String {
        let path = at.join(file);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, body).expect("write");
        Command::new("git")
            .arg("-C")
            .arg(at)
            .args(["add", "-A"])
            .output()
            .expect("add");
        Command::new("git")
            .arg("-C")
            .arg(at)
            .args(["commit", "-q", "-m", "c"])
            .output()
            .expect("commit");
        let out = Command::new("git")
            .arg("-C")
            .arg(at)
            .args(["rev-parse", "--short", "HEAD"])
            .output()
            .expect("rev-parse");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn manifest_for(repo: &Path, head: &str, declared: &str) -> String {
        format!(
            "project:\n  name: p\n  repo_path: {}\n  last_scan: 2026-09-01\n  \
             git_head_at_scan: {head}\ndiscovered_sources:\n  - path: \"{declared}\"\n    \
             kind: adr\n    count: 1\n    transferability_default: T1\nextracted: []\n",
            repo.display()
        )
    }

    /// The whole point of the row: a repository that moved under the paths the scan declared is
    /// behind, and one that did not is current however many commits landed elsewhere. Proven by
    /// making both happen against one baseline rather than by asserting the quiet case alone —
    /// a check that never fires and one that cannot fire read the same from a green run.
    #[test]
    fn staleness_follows_the_declared_paths_and_not_the_repository() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().join("repo");
        let vault = dir.path().join("vault");
        git_repo(&repo);
        let base = commit(&repo, "docs/a.md", "one");
        commit(&repo, "src/main.rs", "fn main() {}");

        write_manifest(&vault, "quiet", &manifest_for(&repo, &base, "docs/*.md"));
        let quiet = survey(&vault, TODAY).expect("survey");
        assert!(
            quiet[0].is_current(),
            "a commit outside the declared paths must not age an extraction: {}",
            quiet[0].state.describe()
        );

        commit(&repo, "docs/b.md", "two");
        let moved = survey(&vault, TODAY).expect("survey");
        assert!(
            !moved[0].is_current(),
            "a changed declared path must age it"
        );
        assert!(matches!(moved[0].state, RepoState::Moved { files: 1, .. }));
    }

    /// A baseline the repository no longer holds is its own state. Folding it into "unmoved"
    /// would report an un-diffable scan as a current one — the exact reading that let a
    /// manifest sit at one page of eleven without a word.
    #[test]
    fn a_baseline_the_repository_lost_is_not_read_as_unmoved() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().join("repo");
        let vault = dir.path().join("vault");
        git_repo(&repo);
        commit(&repo, "docs/a.md", "one");

        write_manifest(
            &vault,
            "rebased",
            &manifest_for(
                &repo,
                "0123456789abcdef0123456789abcdef01234567",
                "docs/*.md",
            ),
        );
        let report = survey(&vault, TODAY).expect("survey");
        assert!(matches!(report[0].state, RepoState::BaselineGone));
        assert!(!report[0].is_current());
    }

    /// One source folding into several pages and several folding into one are both the skill's
    /// documented shapes, and a count that read only the first would report coverage no
    /// manifest on disk actually has.
    #[test]
    fn a_folded_extraction_counts_every_source_it_names() {
        let dir = tempfile::tempdir().expect("tempdir");
        let vault = dir.path().join("vault");
        write_manifest(
            &vault,
            "folded",
            "project:\n  name: p\n  repo_path: /nonexistent\n  last_scan: 2026-09-01\n\
             discovered_sources: []\nextracted:\n  - source:\n      - a.md\n      - b.md\n    \
             vault_page: wiki/documents/x.md\n  - source: c.md\n    vault_page: \
             wiki/documents/y.md\n  - source: d.md\n    vault_page: null\n",
        );
        let report = survey(&vault, TODAY).expect("survey");
        assert_eq!(report[0].pages, 3, "a.md, b.md and c.md all reached a page");
        assert_eq!(report[0].skipped, 1);
        assert!(matches!(report[0].state, RepoState::Missing));
    }

    /// An absent extracts directory is an install that has never extracted, which is not a
    /// defect and must not print a row — the row exists to name work, and one that says
    /// "0 projects" on every fresh install teaches the reader to skip the column.
    #[test]
    fn a_vault_that_never_extracted_reports_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(survey(dir.path(), TODAY).expect("survey").is_empty());
    }
}
