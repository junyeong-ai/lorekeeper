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
    /// How many files the pattern matched when the scan ran. Reported beside the extracted
    /// count so the two sides share a unit; a pattern count and a file count do not.
    #[serde(default)]
    count: usize,
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
            Source::One(s) => std::slice::from_ref(s),
            Source::Many(v) => v,
        }
    }
}

/// Where a project's repository stands against the commit its manifest names.
///
/// Each variant is a different question for the operator, so none of them collapses into
/// another: a repository that is gone needs a path fixed, a baseline that no longer resolves
/// needs a re-scan to re-anchor, and a moved one needs the extraction run again.
enum RepoState {
    /// Carries the path it looked at: a manifest written with a `~` this could not expand
    /// resolves nowhere, and "not found" without the path reads as a repository that moved.
    Missing(String),
    /// The manifest recorded no baseline commit, so there is nothing to diff against. The
    /// scan writes one for every git repository it reads, so this is a project that was not
    /// one when it was scanned.
    NoBaseline,
    /// The recorded commit is not in this repository: a rebase, a fresh clone, or a rewritten
    /// history. The scan cannot be diffed against anything, which is not the same as unmoved.
    BaselineGone,
    Unmoved,
    Moved {
        commits: usize,
        files: usize,
    },
    /// The question could not be put. Apart from the rest because a question that failed is
    /// not an answer of "nothing changed".
    Unanswered(String),
}

impl RepoState {
    /// Is there something here for a person to do? A project whose staleness cannot be
    /// measured is not one — it is reported and left alone, because marking a row nobody can
    /// clear teaches the reader to skip the column.
    fn needs_attention(&self) -> bool {
        !matches!(self, RepoState::Unmoved | RepoState::NoBaseline)
    }

    fn describe(&self) -> String {
        match self {
            RepoState::Missing(at) => format!("no repository at {at}"),
            RepoState::NoBaseline => "not measured — no git baseline".into(),
            RepoState::BaselineGone => "baseline commit is gone".into(),
            RepoState::Unmoved => "sources unchanged".into(),
            RepoState::Moved { commits, files } => {
                format!("{files} declared source file(s) changed over {commits} commit(s)")
            }
            RepoState::Unanswered(why) => format!("cannot be measured — {why}"),
        }
    }
}

pub(crate) struct ProjectState {
    name: String,
    detail: Detail,
}

/// A manifest that is present and unusable is REPORTED, neither dropped nor fatal — the same
/// answer `lore queue status` gives a target page that will not parse. Dropping it would say
/// a project has no extraction when it has one nobody can read; failing the command would let
/// one bad file decide what is said about the other eight.
enum Detail {
    Read(Facts),
    Unreadable(String),
}

struct Facts {
    repo: PathBuf,
    last_scan: Option<jiff::civil::Date>,
    days_since: Option<i64>,
    declared: usize,
    declared_files: usize,
    pages: usize,
    skipped: usize,
    state: RepoState,
}

impl ProjectState {
    fn needs_attention(&self) -> bool {
        match &self.detail {
            Detail::Read(f) => f.state.needs_attention(),
            Detail::Unreadable(_) => true,
        }
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
    // The command that owns a subsystem is the one whose exit code answers for it, so a
    // project a person has to act on fails this the way an overdue source fails `lore health`.
    // `lore status` composes the verdict without gating on it.
    if behind(&reports) > 0 {
        std::process::exit(1);
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
) -> miette::Result<Vec<ProjectState>> {
    let dir = vault_root.join(".lorekeeper").join("extracts");
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        // An install that has never extracted has no directory. Any other failure is a read
        // that did not happen, and reporting it as "no manifests" would be the same defect
        // this command exists to report.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(miette::miette!("{}: {e}", dir.display())),
    };
    for entry in entries.flatten() {
        let path = entry.path().join("manifest.yaml");
        let name = entry.file_name().to_string_lossy().into_owned();
        // A directory under `extracts/` holding no manifest is not a project — the only
        // absence this treats as an absence, because the file system answered it.
        let detail = match std::fs::read_to_string(&path) {
            Ok(text) => match serde_yaml_ng::from_str::<Manifest>(&text) {
                Ok(manifest) => Detail::Read(build_facts(manifest, today)),
                Err(e) => Detail::Unreadable(e.to_string()),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => Detail::Unreadable(e.to_string()),
        };
        out.push(ProjectState { name, detail });
    }
    out.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
    Ok(out)
}

impl ProjectState {
    /// Unreadable manifests sort first: they are the ones a person has to act on before any
    /// other row about them means anything.
    fn sort_key(&self) -> (Option<jiff::civil::Date>, &str) {
        match &self.detail {
            Detail::Read(f) => (f.last_scan, self.name.as_str()),
            Detail::Unreadable(_) => (None, self.name.as_str()),
        }
    }
}

fn build_facts(manifest: Manifest, today: jiff::civil::Date) -> Facts {
    let repo = lk_core::config::expand_tilde(&manifest.project.repo_path);
    let declared: Vec<String> = manifest
        .discovered_sources
        .iter()
        .map(|s| s.path.clone())
        .collect();
    let declared_files: usize = manifest.discovered_sources.iter().map(|s| s.count).sum();
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
    Facts {
        repo,
        last_scan: manifest.project.last_scan,
        days_since: manifest
            .project
            .last_scan
            .and_then(|d| today.since(d).ok().map(|s| i64::from(s.get_days()))),
        declared: declared.len(),
        declared_files,
        pages,
        skipped,
        state,
    }
}

/// Ask git whether the declared sources differ from what the scan read.
///
/// The question is a CONTENT difference against the working tree, not a commit range. A range
/// is one-directional: a checkout rolled back to an ancestor of the baseline makes
/// `baseline..HEAD` empty while the files genuinely differ, and uncommitted edits are outside
/// a range entirely. Both report "unchanged" over a source that has moved.
///
/// Declared paths carry explicit glob magic. git's DEFAULT pathspec syntax is not the one the
/// scan wrote its patterns in: `**/CLAUDE.md` sent bare matches an unrelated set of files and
/// says nothing about the ones it was meant to name, which is a wrong answer rather than a
/// refusal. `:(glob)` is the magic that gives `*` and `**` the meaning a scan means by them.
fn repo_state(repo: &Path, baseline: Option<&str>, declared: &[String]) -> RepoState {
    if !repo.is_dir() {
        return RepoState::Missing(repo.display().to_string());
    }
    // Whether a directory is a repository is git's question, not the filesystem's: a linked
    // worktree carries `.git` as a FILE, and a subdirectory of a repository carries none
    // while git answers for it. Probing for a `.git` directory disagreed with the scan, which
    // records a baseline through `git -C <path> rev-parse`.
    let Some(baseline) = baseline else {
        return RepoState::NoBaseline;
    };
    // git refuses a pathspec that leaves the repository and that refusal reaches `Unanswered`
    // below. A `~` prefix is the one it does NOT refuse: nothing expands it, so it names a
    // directory called `~`, matches nothing, and answers zero with exit 0 — a green row over a
    // source that has moved. Only what git would silently accept is checked here.
    if let Some(bad) = declared.iter().find(|p| p.starts_with('~')) {
        return RepoState::Unanswered(format!(
            "`{bad}` is home-relative; a declared path is relative to the repository root"
        ));
    }
    if declared.is_empty() {
        return RepoState::Unanswered("the manifest declares no source to measure".into());
    }
    if git(repo, &["rev-parse", "--git-dir"]).is_none() {
        return RepoState::Unanswered("git does not answer for this directory".into());
    }
    if git(
        repo,
        &["rev-parse", "--verify", &format!("{baseline}^{{commit}}")],
    )
    .is_none()
    {
        return RepoState::BaselineGone;
    }
    let specs: Vec<String> = declared.iter().map(|p| format!(":(glob){p}")).collect();
    let mut args = vec!["diff", "--name-only", baseline, "--"];
    args.extend(specs.iter().map(String::as_str));
    let Some(diff) = git(repo, &args) else {
        return RepoState::Unanswered("git refused the declared paths".into());
    };
    let files = diff.lines().count();
    if files == 0 {
        return RepoState::Unmoved;
    }
    // Commits are context beside the file count, asked as a symmetric difference so a rolled
    // back checkout counts what it lost as well as what it gained.
    let range = format!("{baseline}...HEAD");
    let mut args = vec!["rev-list", "--count", &range, "--"];
    args.extend(specs.iter().map(String::as_str));
    let commits = git(repo, &args)
        .and_then(|o| o.trim().parse().ok())
        .unwrap_or(0);
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

fn render(reports: &[ProjectState]) {
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
        let mark = if r.needs_attention() { '!' } else { ' ' };
        let name = super::pad(&r.name, width);
        match &r.detail {
            Detail::Unreadable(why) => {
                println!("{mark} {name}  manifest unreadable · {why}");
            }
            Detail::Read(f) => {
                let scanned = match (f.last_scan, f.days_since) {
                    (Some(d), Some(n)) => format!("scanned {d} ({n}d ago)"),
                    (Some(d), None) => format!("scanned {d}"),
                    _ => "never scanned".to_string(),
                };
                println!("{mark} {name}  {scanned} · {}", f.state.describe());
                println!(
                    "  {}  {} file(s) declared in {} pattern(s) · {} extracted · {} skipped",
                    super::pad("", width),
                    f.declared_files,
                    f.declared,
                    f.pages,
                    f.skipped
                );
            }
        }
    }
    let behind = behind(reports);
    println!();
    if behind == 0 {
        println!("Every manifest is current with its repository.");
    } else {
        println!(
            "{behind} of {} project(s) need attention — `/lore-extract scan <repo>` then \
             `/lore-extract run <repo>`; `/lore-extract audit` judges what is already there.",
            reports.len()
        );
    }
}

fn as_json(reports: &[ProjectState]) -> serde_json::Value {
    serde_json::json!({
        "projects": reports.iter().map(|r| match &r.detail {
            Detail::Unreadable(why) => serde_json::json!({
                "name": r.name,
                "state": {"kind": "manifest-unreadable", "why": why},
            }),
            Detail::Read(f) => serde_json::json!({
                "name": r.name,
                "repo": f.repo.to_string_lossy(),
                "last_scan": f.last_scan.map(|d| d.to_string()),
                "days_since_scan": f.days_since,
                "state": match &f.state {
                    RepoState::Missing(at) => {
                        serde_json::json!({"kind": "repo-missing", "at": at})
                    }
                    RepoState::NoBaseline => serde_json::json!({"kind": "no-baseline"}),
                    RepoState::BaselineGone => serde_json::json!({"kind": "baseline-gone"}),
                    RepoState::Unmoved => serde_json::json!({"kind": "current"}),
                    RepoState::Unanswered(why) => {
                        serde_json::json!({"kind": "unanswered", "why": why})
                    }
                    RepoState::Moved { commits, files } => serde_json::json!({
                        "kind": "moved", "commits": commits, "files": files,
                    }),
                },
                "declared_patterns": f.declared,
                "declared_files": f.declared_files,
                "extracted_sources": f.pages,
                "skipped_sources": f.skipped,
            }),
        }).collect::<Vec<_>>(),
        "behind": behind(reports),
    })
}

/// How many projects have moved since their scan, for the `lore status` row.
pub(crate) fn behind(reports: &[ProjectState]) -> usize {
    reports.iter().filter(|r| r.needs_attention()).count()
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
            !quiet[0].needs_attention(),
            "a commit outside the declared paths must not age an extraction"
        );

        commit(&repo, "docs/b.md", "two");
        let moved = survey(&vault, TODAY).expect("survey");
        assert!(
            moved[0].needs_attention(),
            "a changed declared path must age it"
        );
        assert!(matches!(
            moved[0].detail,
            Detail::Read(ref f) if matches!(f.state, RepoState::Moved { files: 1, .. })
        ));
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
        assert!(matches!(
            report[0].detail,
            Detail::Read(ref f) if matches!(f.state, RepoState::BaselineGone)
        ));
        assert!(report[0].needs_attention());
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
        let Detail::Read(f) = &report[0].detail else {
            panic!("expected a read manifest")
        };
        assert_eq!(f.pages, 3, "a.md, b.md and c.md all reached a page");
        assert_eq!(f.skipped, 1);
        assert!(matches!(f.state, RepoState::Missing(_)));
    }

    /// A git call that fails must not read as "nothing changed". Proven by making it fail:
    /// a declared path git refuses as a pathspec is the one input that reaches this without
    /// breaking the repository, and before this variant existed it reported the project
    /// current.
    #[test]
    fn a_git_call_that_did_not_answer_is_not_read_as_unchanged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().join("repo");
        let vault = dir.path().join("vault");
        git_repo(&repo);
        let base = commit(&repo, "docs/a.md", "one");
        commit(&repo, "docs/b.md", "two");

        write_manifest(&vault, "ok", &manifest_for(&repo, &base, "docs/*.md"));
        assert!(matches!(
            survey(&vault, TODAY).expect("survey")[0].detail,
            Detail::Read(ref f) if matches!(f.state, RepoState::Moved { .. })
        ));

        // A path outside the repository is one git refuses, so the call fails rather than
        // answering zero.
        write_manifest(&vault, "ok", &manifest_for(&repo, &base, "../outside/*.md"));
        let report = survey(&vault, TODAY).expect("survey");
        assert!(
            matches!(report[0].detail, Detail::Read(ref f) if matches!(f.state, RepoState::Unanswered(_))),
            "a refused pathspec must not report the project unchanged"
        );
        assert!(report[0].needs_attention());
    }

    /// A recursive glob means what the scan meant by it. Under git's DEFAULT pathspec magic
    /// `**/` requires at least one directory, so `**/NOTES.md` silently skips the one at the
    /// repository root — a wrong answer rather than a refusal, over a pattern a discovery pass
    /// would plausibly record. Proven by moving the root file and requiring it to be seen; the
    /// nested one is there so the pattern matches something either way and only the root file
    /// decides the verdict.
    #[test]
    fn a_recursive_glob_reaches_the_root_the_scan_meant_to_include() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().join("repo");
        let vault = dir.path().join("vault");
        git_repo(&repo);
        commit(&repo, "NOTES.md", "one");
        commit(&repo, "crates/a/NOTES.md", "nested");
        let base = commit(&repo, "unrelated/other.txt", "x");
        commit(&repo, "NOTES.md", "two");

        write_manifest(&vault, "p", &manifest_for(&repo, &base, "**/NOTES.md"));
        let r = survey(&vault, TODAY).expect("survey");
        let Detail::Read(f) = &r[0].detail else {
            panic!("expected a read manifest")
        };
        assert!(
            matches!(f.state, RepoState::Moved { files: 1, .. }),
            "the root file the pattern names moved and was not seen: {}",
            f.state.describe()
        );
    }

    /// A checkout rolled back below the baseline, and an edit never committed, are both
    /// differences a forward commit range cannot see. The question is what the sources say
    /// now against what the scan read, so it is asked as a content difference.
    #[test]
    fn a_rollback_and_an_uncommitted_edit_are_both_seen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().join("repo");
        let vault = dir.path().join("vault");
        git_repo(&repo);
        commit(&repo, "docs/a.md", "one");
        let base = commit(&repo, "docs/a.md", "two");

        std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["checkout", "-q", "HEAD~1"])
            .output()
            .expect("checkout");
        write_manifest(
            &vault,
            "rolled-back",
            &manifest_for(&repo, &base, "docs/*.md"),
        );
        let r = survey(&vault, TODAY).expect("survey");
        assert!(
            r[0].needs_attention(),
            "a checkout below the baseline differs from what was scanned"
        );

        std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["checkout", "-q", "-"])
            .output()
            .expect("checkout back");
        let vault2 = dir.path().join("vault2");
        write_manifest(&vault2, "dirty", &manifest_for(&repo, &base, "docs/*.md"));
        assert!(
            !survey(&vault2, TODAY).expect("survey")[0].needs_attention(),
            "restored to the baseline, nothing differs"
        );
        std::fs::write(repo.join("docs/a.md"), "three").expect("write");
        assert!(
            survey(&vault2, TODAY).expect("survey")[0].needs_attention(),
            "an uncommitted edit to a declared source is a difference"
        );
    }

    /// A manifest that is present and unusable is reported, not dropped and not fatal. Both
    /// halves matter: dropping says a project has no extraction when it has one nobody can
    /// read, and failing lets one bad file decide what is said about the others.
    #[test]
    fn an_unusable_manifest_is_reported_beside_the_ones_that_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let vault = dir.path().join("vault");
        write_manifest(
            &vault,
            "broken",
            "project:\n  name: [this is not a string\n",
        );
        write_manifest(
            &vault,
            "sound",
            "project:\n  name: p\n  repo_path: /nonexistent\n  last_scan: 2026-09-01\n\
             discovered_sources: []\nextracted: []\n",
        );
        std::fs::create_dir_all(vault.join(".lorekeeper/extracts/not-a-project")).expect("mkdir");

        let report = survey(&vault, TODAY).expect("a bad manifest must not fail the command");
        assert_eq!(
            report.len(),
            2,
            "a directory holding no manifest is not a project"
        );
        assert_eq!(report[0].name, "broken");
        assert!(matches!(report[0].detail, Detail::Unreadable(_)));
        assert!(report[0].needs_attention());
        assert!(matches!(report[1].detail, Detail::Read(_)));
    }

    /// `source: []` is a shape YAML accepts and the manifest schema does not forbid.
    #[test]
    fn an_extraction_naming_no_source_counts_none_rather_than_panicking() {
        let dir = tempfile::tempdir().expect("tempdir");
        let vault = dir.path().join("vault");
        write_manifest(
            &vault,
            "empty",
            "project:\n  name: p\n  repo_path: /nonexistent\n  last_scan: 2026-09-01\n\
             discovered_sources: []\nextracted:\n  - source: []\n    vault_page: x.md\n",
        );
        let report = survey(&vault, TODAY).expect("survey");
        assert!(matches!(report[0].detail, Detail::Read(ref f) if f.pages == 0));
    }

    /// The four repo states each name a different repair, so none may absorb another. Proven
    /// by producing each: a path that is not there, a manifest that recorded no baseline, a
    /// directory git does not answer for, and a baseline the repository no longer holds.
    /// Before this, a `.git` probe ahead of the baseline check made every non-git project
    /// read "repository not found" and told the operator to fix a path that was correct.
    #[test]
    fn each_repo_state_is_reachable_and_none_absorbs_another() {
        let dir = tempfile::tempdir().expect("tempdir");
        let vault = dir.path().join("vault");
        let plain = dir.path().join("plain");
        std::fs::create_dir_all(&plain).expect("mkdir");

        write_manifest(
            &vault,
            "gone",
            &manifest_for(Path::new("/nonexistent"), "abc", "d/*"),
        );
        write_manifest(
            &vault,
            "no-baseline",
            &format!(
                "project:\n  repo_path: {}\n  last_scan: 2026-09-01\ndiscovered_sources:\n  \
                 - path: \"d/*\"\n    count: 1\nextracted: []\n",
                plain.display()
            ),
        );
        write_manifest(&vault, "not-a-repo", &manifest_for(&plain, "abc", "d/*"));

        let by = |n: &str, r: &[ProjectState]| {
            let p = r.iter().find(|x| x.name == n).expect("project");
            let Detail::Read(f) = &p.detail else {
                panic!("expected a read manifest")
            };
            f.state.describe()
        };
        let r = survey(&vault, TODAY).expect("survey");
        assert!(by("gone", &r).contains("no repository at"));
        assert_eq!(by("no-baseline", &r), "not measured — no git baseline");
        assert!(
            by("not-a-repo", &r).contains("git does not answer"),
            "a directory git does not answer for is not a missing one: {}",
            by("not-a-repo", &r)
        );
    }

    /// A declared path git reads as something outside the repository answers zero with exit
    /// 0 — green over a source that has moved. The one failure `Unanswered` cannot catch by
    /// asking git, so it is refused before git is asked.
    #[test]
    fn a_declared_path_outside_the_repository_is_refused_rather_than_answered_green() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().join("repo");
        let vault = dir.path().join("vault");
        git_repo(&repo);
        let base = commit(&repo, "docs/a.md", "one");
        commit(&repo, "docs/b.md", "two");

        for bad in ["~/docs/*.md", "/etc/*.md", "../elsewhere/*.md"] {
            write_manifest(&vault, "p", &manifest_for(&repo, &base, bad));
            let r = survey(&vault, TODAY).expect("survey");
            assert!(
                r[0].needs_attention(),
                "`{bad}` must not report the project current"
            );
            let Detail::Read(f) = &r[0].detail else {
                panic!("expected a read manifest")
            };
            assert!(
                matches!(f.state, RepoState::Unanswered(_)),
                "`{bad}`: {}",
                f.state.describe()
            );
        }
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
