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
    /// Absent for a project not under git. There is then no anchor to measure against and
    /// nothing here substitutes one: an mtime moves on a checkout, a copy or a formatter
    /// without changing a word the source says, so the state is reported unmeasured.
    #[serde(default)]
    git_head_at_scan: Option<String>,
}

#[derive(Deserialize)]
struct DiscoveredSource {
    path: String,
    /// How many files the pattern matched when the scan ran. Reported beside the extracted
    /// count so the two sides share a unit; a pattern count and a file count do not. Absent on
    /// a manifest written before the field existed, and absent is not zero — a zero beside a
    /// non-zero extracted count states an absence where the truth is that nothing recorded it.
    count: Option<usize>,
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
    /// Not a git repository, so there is no commit to measure against and nothing a re-scan
    /// would change. Reported and left alone.
    NoBaseline,
    /// A git repository whose manifest records no baseline. Unlike [`RepoState::NoBaseline`]
    /// this HAS a repair — a re-scan anchors it — so it is marked. The two are told apart by
    /// asking git what the directory is, never by reading what the manifest says about it.
    Unanchored,
    /// The recorded commit is not in this repository: a rebase, a fresh clone, or a rewritten
    /// history. The scan cannot be diffed against anything, which is not the same as unmoved.
    BaselineGone,
    Unmoved,
    Moved {
        commits: Option<usize>,
        files: usize,
    },
    /// The question could not be put. Apart from the rest because a question that failed is
    /// not an answer of "nothing changed".
    Unanswered(String),
    /// The manifest declares no source, so there is nothing to measure and no repair that
    /// changes the answer. Reported and left alone, like a directory that is not a repository.
    NothingDeclared,
}

/// What a project's row amounts to, decided once.
///
/// Three surfaces report this set — the terminal, the JSON contract and the `lore status` row
/// — and each used to partition it with its own condition. Two lists that happen to agree are
/// not a partition: a state left out of one and forgotten by the other reads as measured and
/// current, which is the one answer none of them may give by accident.
#[derive(PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Something for a person to do.
    Behind,
    /// Measured, and the declared sources have not moved.
    Current,
    /// No question could be put, and no repair changes that. Reported and left unmarked,
    /// because marking a row nobody can clear teaches the reader to skip the column.
    Unmeasured,
}

impl Verdict {
    fn name(&self) -> &'static str {
        match self {
            Verdict::Behind => "behind",
            Verdict::Current => "current",
            Verdict::Unmeasured => "unmeasured",
        }
    }
}

impl RepoState {
    /// Spelled arm by arm rather than with a wildcard, so a state added later cannot inherit
    /// a verdict nobody chose for it.
    fn verdict(&self) -> Verdict {
        match self {
            RepoState::Unmoved => Verdict::Current,
            RepoState::NoBaseline | RepoState::NothingDeclared => Verdict::Unmeasured,
            RepoState::Missing(_)
            | RepoState::Unanchored
            | RepoState::BaselineGone
            | RepoState::Moved { .. }
            | RepoState::Unanswered(_) => Verdict::Behind,
        }
    }

    fn describe(&self) -> String {
        match self {
            RepoState::Missing(at) => format!("no repository at {at}"),
            RepoState::NoBaseline => "not measured — not a git repository".into(),
            RepoState::Unanchored => "no baseline recorded — a re-scan anchors it".into(),
            RepoState::BaselineGone => "baseline commit is gone".into(),
            RepoState::Unmoved => "sources unchanged".into(),
            RepoState::Moved { commits, files } => match commits {
                Some(n) => format!("{files} declared source file(s) differ, over {n} commit(s)"),
                None => format!("{files} declared source file(s) differ"),
            },
            RepoState::Unanswered(why) => format!("cannot be measured — {why}"),
            RepoState::NothingDeclared => "not measured — the manifest declares no source".into(),
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
    declared_files: Option<usize>,
    pages: usize,
    skipped: usize,
    state: RepoState,
}

impl ProjectState {
    /// A manifest nobody can read is a project whose extraction state is unknown, which is
    /// work rather than the absence of it.
    fn verdict(&self) -> Verdict {
        match &self.detail {
            Detail::Read(f) => f.state.verdict(),
            Detail::Unreadable(_) => Verdict::Behind,
        }
    }

    fn needs_attention(&self) -> bool {
        self.verdict() == Verdict::Behind
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
    if tally(&reports).behind > 0 {
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
    let declared_files: Option<usize> = manifest
        .discovered_sources
        .iter()
        .map(|s| s.count)
        .try_fold(0usize, |acc, n| n.map(|n| acc + n));
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
    // Asked before anything about git: a manifest naming no source has nothing to measure,
    // and a baseline would not change that. Asked after git, such a manifest was told to
    // anchor a baseline that would anchor nothing.
    if declared.is_empty() {
        return RepoState::NothingDeclared;
    }
    // Whether a directory is a repository is git's question, not the filesystem's: a linked
    // worktree carries `.git` as a FILE, and a subdirectory of a repository carries none
    // while git answers for it. Probing for a `.git` directory disagreed with the scan, which
    // records a baseline through `git -C <path> rev-parse`.
    let is_repo = git(repo, &["rev-parse", "--git-dir"]).is_some();
    let Some(baseline) = baseline else {
        return if is_repo {
            RepoState::Unanchored
        } else {
            RepoState::NoBaseline
        };
    };
    if !is_repo {
        return RepoState::Unanswered("git does not answer for this directory".into());
    }
    // git refuses a pathspec that leaves the repository and that refusal reaches `Unanswered`
    // below. A `~` prefix is the one it does NOT refuse: nothing expands it, so it names a
    // directory called `~`, matches nothing, and answers zero with exit 0 — a green row over a
    // source that has moved. Only what git would silently accept is checked here.
    if let Some(bad) = declared.iter().find(|p| p.starts_with('~')) {
        return RepoState::Unanswered(format!(
            "`{bad}` is home-relative; a declared path is relative to the repository root"
        ));
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
    // `git diff` lists tracked paths only, and a scan reads the working tree — so a source
    // written since and not yet staged differs from what was scanned while the diff is silent.
    // `--exclude-standard` keeps the repository's own ignore rules. What it costs: a declared
    // path git ignores is reached by neither probe, so it reads unchanged however it moves —
    // which is why the scan lists its candidates through git rather than the filesystem, and
    // refuses to declare an ignored file. Dropping the flag is not the alternative: every
    // build artefact under a declared directory would then answer, and the row would be red
    // for good.
    let mut args = vec!["ls-files", "--others", "--exclude-standard", "--"];
    args.extend(specs.iter().map(String::as_str));
    let Some(untracked) = git(repo, &args) else {
        return RepoState::Unanswered("git refused the declared paths".into());
    };
    // A union rather than a sum: a path dropped from the index but left on disk answers to
    // BOTH probes, and counting it twice states a measurement no repository holds.
    let files = diff
        .lines()
        .chain(untracked.lines())
        .collect::<BTreeSet<_>>()
        .len();
    if files == 0 {
        // Nothing DIFFERED, which is the same as "nothing changed" only where git knows the
        // files the declared patterns name. So the population is asked for rather than
        // assumed: `--cached --others --exclude-standard` is exactly what the two probes
        // above can answer for, and the scan lists its candidates the same way.
        let mut args = vec![
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
        ];
        args.extend(specs.iter().map(String::as_str));
        let Some(known) = git(repo, &args) else {
            return RepoState::Unanswered("git refused the declared paths".into());
        };
        if known.lines().next().is_some() {
            return RepoState::Unmoved;
        }
        // Git knows nothing under these patterns, so "unchanged" would be a statement about
        // files nothing looked at. A pattern that also happens to cover an ignored draft is
        // NOT this case — its population is the files git knows, and the draft was never a
        // declared source — which is why the population is asked before the ignore list.
        let mut args = vec![
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--",
        ];
        args.extend(specs.iter().map(String::as_str));
        let Some(ignored) = git(repo, &args) else {
            return RepoState::Unanswered("git refused the declared paths".into());
        };
        let n = ignored.lines().count();
        return RepoState::Unanswered(if n > 0 {
            format!("the declared paths reach {n} file(s), every one of them ignored by git")
        } else {
            "the declared paths match no file this repository holds — a submodule's files \
             belong to its own"
                .into()
        });
    }
    // Commits are context beside the file count, asked as a symmetric difference so a rolled
    // back checkout counts what it lost as well as what it gained. A count that did not answer
    // is absent rather than zero — the files are the verdict either way. No repository shape
    // is known to reach that branch once `rev-parse` and `diff` have both answered, so no test
    // pins it; it is here because a parse can fail, not because a shape produces it.
    let range = format!("{baseline}...HEAD");
    let mut args = vec!["rev-list", "--count", &range, "--"];
    args.extend(specs.iter().map(String::as_str));
    let commits = git(repo, &args).and_then(|o| o.trim().parse().ok());
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
                let declared = match f.declared_files {
                    Some(n) => format!("{n} file(s) declared in {} pattern(s)", f.declared),
                    None => format!(
                        "{} pattern(s) declared, file count not recorded",
                        f.declared
                    ),
                };
                println!(
                    "  {}  {declared} · {} extracted · {} skipped",
                    super::pad("", width),
                    f.pages,
                    f.skipped
                );
            }
        }
    }
    let t = tally(reports);
    println!();
    print!("{}", coverage(&t));
    if t.behind == 0 {
        println!();
    } else {
        println!(
            " — `/lore-extract scan <repo>` then `/lore-extract run <repo>`; \
             `/lore-extract audit` judges what is already there."
        );
    }
}

/// The state as the contract spells it. Apart from `as_json` so every state can be
/// enumerated by a test — a `kind` is a NAME for one of eight situations while the `verdict`
/// beside it says what to do, and one word serving as both would let a reader take either for
/// the other and be right by accident.
fn state_json(state: &RepoState) -> serde_json::Value {
    match state {
        RepoState::Missing(at) => {
            serde_json::json!({"kind": "repo-missing", "at": at})
        }
        RepoState::NoBaseline => serde_json::json!({"kind": "not-a-repository"}),
        // Never the vacated `no-baseline`: that string meant "nothing to do" and
        // this state is its opposite, so a consumer keyed on the old spelling
        // would read an actionable project as safe.
        RepoState::Unanchored => {
            serde_json::json!({"kind": "baseline-not-recorded"})
        }
        RepoState::NothingDeclared => {
            serde_json::json!({"kind": "nothing-declared"})
        }
        RepoState::BaselineGone => serde_json::json!({"kind": "baseline-gone"}),
        // Named for the state, not for the verdict beside it: `kind` says which
        // of eight situations this is and `verdict` says what to do about it, so
        // one word for both would invite a reader to take either for the other.
        RepoState::Unmoved => serde_json::json!({"kind": "unmoved"}),
        RepoState::Unanswered(why) => {
            serde_json::json!({"kind": "unanswered", "why": why})
        }
        RepoState::Moved { commits, files } => serde_json::json!({
            "kind": "moved", "commits": commits, "files": files,
        }),
    }
}

fn as_json(reports: &[ProjectState]) -> serde_json::Value {
    let t = tally(reports);
    serde_json::json!({
        "projects": reports.iter().map(|r| match &r.detail {
            Detail::Unreadable(why) => serde_json::json!({
                "name": r.name,
                "verdict": r.verdict().name(),
                "state": {"kind": "manifest-unreadable", "why": why},
            }),
            Detail::Read(f) => serde_json::json!({
                "name": r.name,
                // What a reader acts on. The `kind` beside it says WHICH state, and a reader
                // that had to enumerate kinds would read every state added later as safe.
                "verdict": r.verdict().name(),
                "repo": f.repo.to_string_lossy(),
                "last_scan": f.last_scan.map(|d| d.to_string()),
                "days_since_scan": f.days_since,
                "state": state_json(&f.state),
                "declared_patterns": f.declared,
                "declared_files": f.declared_files,
                "extracted_sources": f.pages,
                "skipped_sources": f.skipped,
            }),
        }).collect::<Vec<_>>(),
        "behind": t.behind,
        "current": t.current,
        // Beside it because the two are a different answer: a project nothing could ask a
        // question about is neither behind nor current, and a reader given only `behind`
        // would report the rest current — the fold the terminal row stopped making.
        "unmeasured": t.unmeasured,
    })
}

/// How many projects have moved since their scan, for the `lore status` row.
/// The three counts, summing to the number of projects by construction.
pub(crate) struct Tally {
    pub behind: usize,
    pub current: usize,
    pub unmeasured: usize,
}

pub(crate) fn tally(reports: &[ProjectState]) -> Tally {
    let mut t = Tally {
        behind: 0,
        current: 0,
        unmeasured: 0,
    };
    for r in reports {
        match r.verdict() {
            Verdict::Behind => t.behind += 1,
            Verdict::Current => t.current += 1,
            Verdict::Unmeasured => t.unmeasured += 1,
        }
    }
    t
}

/// The one phrase both the terminal and the `lore status` row state their coverage with.
/// Written from the tally rather than from the project count, because a project no question
/// reached must never be spoken for by the ones that were measured.
pub(crate) fn coverage(t: &Tally) -> String {
    let mut parts = Vec::new();
    if t.behind > 0 {
        parts.push(format!("{} need attention", t.behind));
    }
    if t.current > 0 {
        parts.push(format!("{} current", t.current));
    }
    if t.unmeasured > 0 {
        parts.push(format!("{} not measured", t.unmeasured));
    }
    parts.join(" · ")
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

    /// A source the scan has never seen is a difference, and `git diff` does not list one:
    /// it compares tracked content, while a scan reads the working tree. So the new ADR
    /// nobody has staged yet — the single likeliest thing to be waiting for extraction —
    /// was the one change the row could not see.
    #[test]
    fn a_source_written_since_the_scan_and_never_staged_is_seen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().join("repo");
        let vault = dir.path().join("vault");
        git_repo(&repo);
        let base = commit(&repo, "docs/a.md", "one");

        write_manifest(&vault, "p", &manifest_for(&repo, &base, "docs/*.md"));
        assert!(
            !survey(&vault, TODAY).expect("survey")[0].needs_attention(),
            "nothing has moved yet"
        );

        std::fs::write(repo.join("docs/b.md"), "new").expect("write");
        let r = survey(&vault, TODAY).expect("survey");
        assert!(
            r[0].needs_attention(),
            "an unstaged new source is a source the scan never read"
        );
        let Detail::Read(f) = &r[0].detail else {
            panic!("expected a read manifest")
        };
        assert!(matches!(f.state, RepoState::Moved { files: 1, .. }));
    }

    /// "Nothing differed" is the same as "nothing changed" only where git knows the files the
    /// patterns name. Three shapes, and only the middle one is a finding: a pattern whose
    /// population git knows is measured however many ignored drafts it also covers; a pattern
    /// whose every file git ignores can never be measured at all; and one matching nothing is
    /// a manifest describing a repository that no longer holds what it declared. Reporting
    /// the last two as unchanged is a statement about files nothing looked at.
    #[test]
    fn unchanged_is_said_only_of_files_git_can_answer_for() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().join("repo");
        git_repo(&repo);
        // The ignored draft sits UNDER the pattern the first case declares. Outside it, the
        // first assertion holds for any gate that asks the ignore list at all, including the
        // one this replaced — a fixture that cannot tell the two apart proves neither.
        std::fs::write(repo.join(".gitignore"), "docs/draft-*.md\ndrafts/\n").expect("write");
        let base = commit(&repo, "docs/a.md", "one");
        std::fs::write(repo.join("docs/draft-x.md"), "draft").expect("write");
        std::fs::create_dir_all(repo.join("drafts")).expect("mkdir");
        std::fs::write(repo.join("drafts/d.md"), "draft").expect("write");

        let state = |name: &str, declared: &str| {
            let vault = dir.path().join(name);
            write_manifest(&vault, name, &manifest_for(&repo, &base, declared));
            let r = survey(&vault, TODAY).expect("survey");
            let Detail::Read(f) = &r[0].detail else {
                panic!("expected a read manifest")
            };
            (f.state.describe(), r[0].needs_attention())
        };

        // An ignored file merely COVERED by a pattern was never a declared source: the
        // population is what git knows, and marking this row would be an alarm no repair
        // clears, since the ignore rule is deliberate.
        assert_eq!(
            state("incidental", "docs/*.md"),
            ("sources unchanged".into(), false)
        );

        let (why, marked) = state("all-ignored", "drafts/*.md");
        assert!(why.contains("every one of them ignored by git"), "{why}");
        assert!(marked);

        let (why, marked) = state("matches-nothing", "adr/*.md");
        assert!(why.contains("match no file"), "{why}");
        assert!(marked);
    }

    /// A manifest with no baseline AND no declared source used to be told to anchor a
    /// baseline that would anchor nothing, because the baseline was asked about first. What
    /// is declared decides whether there is anything to measure at all, so it is asked first.
    #[test]
    fn a_manifest_declaring_nothing_is_not_told_to_fix_its_baseline() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().join("repo");
        let vault = dir.path().join("vault");
        git_repo(&repo);
        commit(&repo, "docs/a.md", "one");

        write_manifest(
            &vault,
            "empty",
            &format!(
                "project:\n  repo_path: {}\n  last_scan: \
                 2026-09-01\ndiscovered_sources: []\nextracted: []\n",
                repo.display()
            ),
        );
        let r = survey(&vault, TODAY).expect("survey");
        let Detail::Read(f) = &r[0].detail else {
            panic!("expected a read manifest")
        };
        assert_eq!(
            f.state.describe(),
            "not measured — the manifest declares no source"
        );
        assert!(!r[0].needs_attention());
    }

    /// The JSON is what a skill reads, and a `kind` is a name rather than a slot: `no-baseline`
    /// meant "nothing to do" before `Unanchored` existed, so reusing it would make an
    /// actionable project read safe to anything keyed on the old spelling. The verdict beside
    /// it is what a reader acts on, so no consumer has to enumerate kinds at all.
    #[test]
    fn the_contract_names_a_verdict_and_never_reuses_a_kind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().join("repo");
        let vault = dir.path().join("vault");
        git_repo(&repo);
        commit(&repo, "docs/a.md", "one");
        write_manifest(
            &vault,
            "unanchored",
            &format!(
                "project:\n  repo_path: {}\n  last_scan: 2026-09-01\ndiscovered_sources:\n  \
                 - path: \"docs/*.md\"\n    count: 1\nextracted: []\n",
                repo.display()
            ),
        );
        let json = as_json(&survey(&vault, TODAY).expect("survey"));
        let p = &json["projects"][0];
        assert_eq!(p["verdict"], "behind");
        assert_eq!(p["state"]["kind"], "baseline-not-recorded");
        // The two fields answer different questions, so no state's kind may be spelled with a
        // verdict's word.
        let verdicts = ["behind", "current", "unmeasured"];
        for state in [
            RepoState::Missing("x".into()),
            RepoState::NoBaseline,
            RepoState::Unanchored,
            RepoState::BaselineGone,
            RepoState::Unmoved,
            RepoState::Moved {
                commits: None,
                files: 1,
            },
            RepoState::Unanswered("x".into()),
            RepoState::NothingDeclared,
        ] {
            let kind = state_json(&state)["kind"]
                .as_str()
                .expect("kind")
                .to_string();
            assert!(
                !verdicts.contains(&kind.as_str()),
                "`{kind}` names a verdict rather than the state it is"
            );
        }
        assert_eq!(json["behind"], 1);
        assert_eq!(json["current"], 0);
        assert_eq!(json["unmeasured"], 0);
    }

    /// The closing line of the command's own terminal made the fold the `lore status` row
    /// had stopped making: with nothing behind it said every manifest was current, over
    /// projects no question had reached. Both surfaces now speak from the same tally, so the
    /// phrase is tested once and neither can drift from it.
    #[test]
    fn coverage_never_speaks_for_a_project_no_question_reached() {
        let t = Tally {
            behind: 0,
            current: 0,
            unmeasured: 2,
        };
        assert_eq!(coverage(&t), "2 not measured");
        let t = Tally {
            behind: 1,
            current: 2,
            unmeasured: 3,
        };
        assert_eq!(
            coverage(&t),
            "1 need attention · 2 current · 3 not measured"
        );
        let t = Tally {
            behind: 0,
            current: 4,
            unmeasured: 0,
        };
        assert_eq!(coverage(&t), "4 current");
    }

    /// Which count each state falls into, stated once. That the three SUM to the project
    /// count proves nothing — `tally` increments exactly one counter per project whatever
    /// `verdict` answers — so the tuple is the assertion, and the fixture carries one project
    /// of each kind so that moving any state between counts fails here.
    #[test]
    fn every_project_falls_into_exactly_one_of_the_three_counts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let vault = dir.path().join("vault");
        let plain = dir.path().join("plain");
        std::fs::create_dir_all(&plain).expect("mkdir");
        let repo = dir.path().join("repo");
        git_repo(&repo);
        let base = commit(&repo, "docs/a.md", "one");

        write_manifest(&vault, "unmoved", &manifest_for(&repo, &base, "docs/*.md"));
        write_manifest(
            &vault,
            "gone",
            &manifest_for(Path::new("/nonexistent"), "a", "d/*"),
        );
        write_manifest(&vault, "not-a-repo", &manifest_for(&plain, "a", "d/*"));
        write_manifest(&vault, "broken", "project: [this is not a manifest\n");
        write_manifest(
            &vault,
            "plain-dir",
            &format!(
                "project:\n  repo_path: {}\n  last_scan: 2026-09-01\ndiscovered_sources:\n  \
                 - path: \"d/*\"\n    count: 1\nextracted: []\n",
                plain.display()
            ),
        );
        let r = survey(&vault, TODAY).expect("survey");
        let t = tally(&r);
        assert_eq!((t.behind, t.current, t.unmeasured), (3, 1, 1));
    }

    /// The other half of `:(glob)`, and the half that only a test states: `*` covers one
    /// directory level. Git's DEFAULT pathspec magic lets a bare `*` cross `/`, so
    /// `docs/*.md` would answer for `docs/sub/b.md` — a file the scan never read, since the
    /// scan matched the same pattern as a shell glob. Under default magic the row marks a
    /// project behind over a source it does not have, and the manifest's own `count` could
    /// never reproduce the number.
    #[test]
    fn a_single_star_covers_one_directory_level_the_way_the_scan_read_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().join("repo");
        let vault = dir.path().join("vault");
        git_repo(&repo);
        commit(&repo, "docs/a.md", "one");
        let base = commit(&repo, "docs/sub/b.md", "one");
        commit(&repo, "docs/sub/b.md", "two");

        write_manifest(&vault, "flat", &manifest_for(&repo, &base, "docs/*.md"));
        assert!(
            !survey(&vault, TODAY).expect("survey")[0].needs_attention(),
            "a nested file is not what `docs/*.md` declared"
        );

        let deep = dir.path().join("deep-vault");
        write_manifest(&deep, "deep", &manifest_for(&repo, &base, "docs/**/*.md"));
        assert!(
            survey(&deep, TODAY).expect("survey")[0].needs_attention(),
            "`**` is what reaches beneath, and it must still reach"
        );
    }

    /// One path can answer to both probes: dropped from the index and left on disk, it is a
    /// deletion to `git diff` and an untracked file to `git ls-files`. Summing the two lists
    /// states a file count no repository holds, which is the same class of defect — a number
    /// asserted past what was measured — this command exists to report.
    #[test]
    fn a_path_both_probes_name_is_counted_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().join("repo");
        let vault = dir.path().join("vault");
        git_repo(&repo);
        let base = commit(&repo, "docs/a.md", "one");
        std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["rm", "-q", "--cached", "docs/a.md"])
            .output()
            .expect("rm --cached");

        write_manifest(&vault, "p", &manifest_for(&repo, &base, "docs/*.md"));
        let r = survey(&vault, TODAY).expect("survey");
        let Detail::Read(f) = &r[0].detail else {
            panic!("expected a read manifest")
        };
        assert!(
            matches!(f.state, RepoState::Moved { files: 1, .. }),
            "one path, one file: {}",
            f.state.describe()
        );
    }

    /// Every manifest written before `count` existed carries none, and a defaulted zero made
    /// each of them report "0 file(s) declared" beside a non-zero extracted count — an
    /// absence stated as a measurement, which is the defect this whole command exists to
    /// report. A count nothing recorded is absent, and one recorded is summed.
    #[test]
    fn a_file_count_no_manifest_recorded_is_absent_rather_than_zero() {
        let dir = tempfile::tempdir().expect("tempdir");
        let vault = dir.path().join("vault");
        let repo = dir.path().join("repo");
        git_repo(&repo);
        let base = commit(&repo, "docs/a.md", "one");

        write_manifest(
            &vault,
            "unrecorded",
            &format!(
                "project:\n  repo_path: {}\n  last_scan: 2026-09-01\n  git_head_at_scan: \
                 {base}\ndiscovered_sources:\n  - path: \"docs/*.md\"\n  - path: \
                 \"adr/*.md\"\nextracted: []\n",
                repo.display()
            ),
        );
        write_manifest(
            &vault,
            "recorded",
            &format!(
                "project:\n  repo_path: {}\n  last_scan: 2026-09-02\n  git_head_at_scan: \
                 {base}\ndiscovered_sources:\n  - path: \"docs/*.md\"\n    count: 4\n  \
                 - path: \"adr/*.md\"\n    count: 3\nextracted: []\n",
                repo.display()
            ),
        );
        write_manifest(
            &vault,
            "mixed",
            &format!(
                "project:\n  repo_path: {}\n  last_scan: 2026-09-03\n  git_head_at_scan: \
                 {base}\ndiscovered_sources:\n  - path: \"docs/*.md\"\n    count: 4\n  \
                 - path: \"adr/*.md\"\nextracted: []\n",
                repo.display()
            ),
        );
        let r = survey(&vault, TODAY).expect("survey");
        let count = |n: &str| {
            let p = r.iter().find(|x| x.name == n).expect("project");
            let Detail::Read(f) = &p.detail else {
                panic!("expected a read manifest")
            };
            f.declared_files
        };
        assert_eq!(count("unrecorded"), None);
        assert_eq!(count("recorded"), Some(7));
        // One pattern counted and one not is not a smaller total — it is a total nobody has.
        assert_eq!(count("mixed"), None);
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

        // A git repository whose manifest records no baseline. It reads like the plain
        // directory above from the manifest alone, and the two differ in whether a re-scan
        // would fix them — which only git can say.
        let unanchored = dir.path().join("unanchored");
        git_repo(&unanchored);
        commit(&unanchored, "docs/a.md", "one");
        write_manifest(
            &vault,
            "unanchored",
            &format!(
                "project:\n  repo_path: {}\n  last_scan: 2026-09-01\ndiscovered_sources:\n  \
                 - path: \"docs/*.md\"\n    count: 1\nextracted: []\n",
                unanchored.display()
            ),
        );
        write_manifest(
            &vault,
            "declares-nothing",
            &format!(
                "project:\n  repo_path: {}\n  last_scan: 2026-09-01\n  git_head_at_scan: \
                 abc\ndiscovered_sources: []\nextracted: []\n",
                unanchored.display()
            ),
        );

        let by = |n: &str, r: &[ProjectState]| {
            let p = r.iter().find(|x| x.name == n).expect("project");
            let Detail::Read(f) = &p.detail else {
                panic!("expected a read manifest")
            };
            f.state.describe()
        };
        let marked = |n: &str, r: &[ProjectState]| {
            r.iter()
                .find(|x| x.name == n)
                .expect("project")
                .needs_attention()
        };
        let r = survey(&vault, TODAY).expect("survey");
        assert!(by("gone", &r).contains("no repository at"));
        assert_eq!(by("no-baseline", &r), "not measured — not a git repository");
        assert!(
            by("not-a-repo", &r).contains("git does not answer"),
            "a directory git does not answer for is not a missing one: {}",
            by("not-a-repo", &r)
        );
        assert_eq!(
            by("unanchored", &r),
            "no baseline recorded — a re-scan anchors it"
        );
        assert!(
            marked("unanchored", &r) && !marked("no-baseline", &r),
            "a baseline a re-scan would supply is work; one nothing would supply is not"
        );
        assert_eq!(
            by("declares-nothing", &r),
            "not measured — the manifest declares no source"
        );
        assert!(!marked("declares-nothing", &r));
        assert_eq!(
            tally(&r).unmeasured,
            2,
            "the two states no question reaches are counted apart from the current ones"
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
