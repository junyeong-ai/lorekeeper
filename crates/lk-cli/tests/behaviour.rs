//! Behavioural snapshot sweep.
//!
//! The unit tests are an allowlist: each names an expectation and checks it.
//! A regression lands in the complement — behaviour nobody named, changed by a
//! fix aimed somewhere else. Three guards were reverted here on a single day,
//! each because what it actually admitted was only learned after it shipped.
//! This sweep is the complement: every command that can be a pure function of
//! the fixture vault is run against it and its whole output pinned, so a
//! guard's real population shows up as a diff before it lands rather than as a
//! revert after.
//!
//! The output of a run is a diff, never a verdict. `cargo insta review` is
//! where a diff is judged intended; nothing here accepts a snapshot on the
//! author's behalf.
//!
//! Coverage is closed against [`lore::Cli`] itself — the same clap tree the
//! binary dispatches on — so a new subcommand fails
//! [`every_command_is_swept_or_exempt`] until it is swept or written into
//! [`EXEMPT`] with its reason. A hand-kept list would be the same allowlist
//! this exists to complement.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use clap::CommandFactory;
use tempfile::TempDir;

/// Commands swept against the fixture, as the argv after `--config`.
///
/// Every entry was measured byte-identical across repeated runs before being
/// admitted: a snapshot of an unstable command is a flake, not a pin.
const SWEEP: &[&[&str]] = &[
    &["validate"],
    &["config", "vault-root"],
    &["config", "schema-path"],
    &["config", "categories"],
    &["status"],
    &["health"],
    &["doctor"],
    &["performance"],
    &["schedule"],
    &["schema"],
    &["maintenance", "--dry-run"],
    &["queue", "status"],
    &["queue", "count"],
    &["queue", "prune"],
    &["wiki", "concepts"],
    &["wiki", "index"],
    &["wiki", "log"],
    &["wiki", "map"],
    &["wiki", "refresh"],
    &["graph", "lint"],
    &["graph", "hubs"],
    &["graph", "orphans"],
    &["graph", "broken"],
    &["graph", "cluster"],
    &["graph", "export"],
    &["graph", "index-sync"],
    &["graph", "normalize"],
    &["graph", "suggest-links"],
    &["graph", "audit-candidates"],
    &["graph", "backlinks-sync"],
];

/// Leaf commands the sweep does not cover, each with the reason its answer is
/// not a function of the fixture alone. Held against the real clap tree by
/// [`every_command_is_swept_or_exempt`], so an exemption is a written decision
/// rather than an omission.
const EXEMPT: &[(&[&str], &str)] = &[
    (
        &["synthesis", "weekly"],
        "names the period it ran for from the wall clock (`week of 2026-07-30`, `for 2026-07`) and takes no `--date` override the way `ingest` does, so a pin of it goes red at the next period boundary rather than on a change to the code",
    ),
    (
        &["synthesis", "monthly"],
        "names the period it ran for from the wall clock (`week of 2026-07-30`, `for 2026-07`) and takes no `--date` override the way `ingest` does, so a pin of it goes red at the next period boundary rather than on a change to the code",
    ),
    (
        &["synthesis", "quarterly"],
        "names the period it ran for from the wall clock (`week of 2026-07-30`, `for 2026-07`) and takes no `--date` override the way `ingest` does, so a pin of it goes red at the next period boundary rather than on a change to the code",
    ),
    (
        &["synthesis", "annual"],
        "names the period it ran for from the wall clock (`week of 2026-07-30`, `for 2026-07`) and takes no `--date` override the way `ingest` does, so a pin of it goes red at the next period boundary rather than on a change to the code",
    ),
    (
        &["ingest"],
        "reaches the configured sources, so its answer is a property of the network rather than of the vault",
    ),
    (
        &["init", "credentials"],
        "prompts interactively and writes a secret; it has no non-interactive form to observe",
    ),
    (
        &["init", "schema"],
        "the same call as the swept top-level `schema`, reached by a second spelling",
    ),
    (
        &["graph", "audit-mark"],
        "takes a concept id and stamps it; the id it takes is the observation, and it belongs to a mutation corpus that pins files rather than output",
    ),
    (
        &["graph", "merge"],
        "takes two concept ids and deletes one, as `audit-mark` does",
    ),
    (
        &["queue", "apply"],
        "materialises a drain's LLM results; the fixture has no drain to apply",
    ),
];

/// Swept commands whose corpus verdict is a guard-clause answer rather than the
/// command's own work, with what the corpus would have to carry to reach it.
/// Each is a stated gap: the pin still catches a change to the guard's own
/// wording or exit code, and states that it catches nothing past it.
const GUARD_ONLY: &[(&[&str], &str)] = &[
    (
        &["status"],
        "prints one line per enabled source; the corpus enables none, so a source's last-ingest line is never reached",
    ),
    (
        &["health"],
        "classifies each enabled source fresh/stale/never against the ingest log; the corpus has neither",
    ),
    (
        &["performance"],
        "reads `me/work-log`; the corpus vault carries no work-log page, so the category distribution is never computed",
    ),
    (
        &["queue", "status"],
        "classifies pending tasks against their target pages; the corpus queue is empty, so none of the five categories is exercised",
    ),
    (
        &["queue", "prune"],
        "drops, keeps, rewrites, archives, and deletes by category; an empty queue reaches none of those branches",
    ),
    (
        &["maintenance", "--dry-run"],
        "prunes the ingest log and drained queue files past retention; the corpus has no log and no processed/ directory",
    ),
];

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/behaviour/corpus/small")
}

/// Copy the fixture into a fresh tempdir. Half the sweep writes into the vault
/// — the catalog, the timeline, the map — so each observation needs its own
/// copy or it would be reading the previous one's output.
fn stage() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    copy_tree(&corpus(), &dir.path().join("corpus"));
    dir
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create staged dir");
    for entry in std::fs::read_dir(from).expect("read fixture dir") {
        let entry = entry.expect("fixture entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy fixture file");
        }
    }
}

/// Stand-in for the staging directory. It is a fresh tempdir per observation,
/// so it is the one part of an output genuinely different every run; pinning it
/// would pin the filesystem rather than the behaviour. Nothing else is rewritten.
const STAGED: &str = "<staged>";

/// Run one command against a staged fixture. Both streams are pinned: `lore`
/// reports to stdout and stderr both, and which stream a message takes is part
/// of what a consumer depends on.
fn observe(staged: &Path, argv: &[&str]) -> String {
    let out = Command::cargo_bin("lore")
        .expect("lore binary")
        // From the staging directory, so anything a command derives from its CWD normalises
        // with everything else. `lore schedule` emits `# Working dir: <cwd>`, which run from
        // the crate would pin one developer's absolute path into the snapshot and fail for
        // everyone else.
        .current_dir(staged)
        .arg("--config")
        .arg(staged.join("corpus/config.yaml"))
        .args(argv)
        .output()
        .expect("command ran");
    let body = format!(
        "exit: {}\n--- stdout ---\n{}--- stderr ---\n{}",
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    normalise(&body, staged)
}

fn normalise(body: &str, dir: &Path) -> String {
    let mut out = body.to_string();
    let mut spellings = vec![dir.to_string_lossy().into_owned()];
    if let Ok(resolved) = dir.canonicalize() {
        spellings.push(resolved.to_string_lossy().into_owned());
    }
    // Longest first: `/private/var/…` contains `/var/…` as a suffix, and
    // replacing the shorter one first would leave a half-rewritten path.
    spellings.sort_by_key(|s| std::cmp::Reverse(s.len()));
    for s in spellings {
        out = out.replace(&s, STAGED);
    }
    out
}

/// Every file the fixture is made of must be in the REPOSITORY.
///
/// The sweep compares against snapshots recorded from whatever is on the author's disk, so a
/// fixture file git does not have makes CI stage a different corpus and fail on a diff that
/// describes the fixture rather than the behaviour. `.gitignore` carried an unanchored
/// `config.yaml` — meant for a real config at the repo root — which silently excluded this
/// corpus's own, leaving CI with a vault the binary could not read.
#[test]
fn every_fixture_file_is_tracked() {
    let corpus = corpus();
    // Run from the repository root and take `--full-name`, so a path is repo-relative rather
    // than relative to wherever this happened to run.
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let listed = std::process::Command::new("git")
        .args(["ls-files", "--full-name", "--", &corpus.to_string_lossy()])
        .current_dir(&root)
        .output()
        .expect("git ls-files");
    assert!(listed.status.success(), "git ls-files failed: {listed:?}");
    let tracked: Vec<PathBuf> = String::from_utf8_lossy(&listed.stdout)
        .lines()
        .map(|line| root.join(line))
        .filter_map(|p| p.canonicalize().ok())
        .collect();

    let mut on_disk = Vec::new();
    let mut stack = vec![corpus.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read fixture dir").flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                on_disk.push(path.canonicalize().expect("canonicalize fixture file"));
            }
        }
    }
    assert!(!on_disk.is_empty(), "the fixture corpus is empty");

    let untracked: Vec<&PathBuf> = on_disk.iter().filter(|p| !tracked.contains(p)).collect();
    assert!(
        untracked.is_empty(),
        "fixture files git does not have, so CI stages a different corpus: {untracked:?}"
    );
}

#[test]
fn behaviour_sweep() {
    for argv in SWEEP {
        let staged = stage();
        let name = argv.join("_").replace('-', "_");
        insta::assert_snapshot!(name, observe(staged.path(), argv));
    }
}

/// Every leaf of the real clap tree, as `["graph", "lint"]` style paths.
/// `help` is clap's own and dispatches to nothing of ours.
fn leaf_commands() -> Vec<Vec<String>> {
    fn walk(cmd: &clap::Command, prefix: &[String], out: &mut Vec<Vec<String>>) {
        let subs: Vec<&clap::Command> = cmd
            .get_subcommands()
            .filter(|s| s.get_name() != "help")
            .collect();
        if subs.is_empty() {
            if !prefix.is_empty() {
                out.push(prefix.to_vec());
            }
            return;
        }
        for sub in subs {
            let mut next = prefix.to_vec();
            next.push(sub.get_name().to_string());
            walk(sub, &next, out);
        }
    }
    let mut out = Vec::new();
    walk(&lore::Cli::command(), &[], &mut out);
    out
}

/// `--help` is a shipped surface, and a doc comment can only land on the wrong
/// variant by leaving its own bare — so a subcommand or flag that describes
/// nothing is also how a description ends up on the neighbour that does not own it.
#[test]
fn every_command_and_flag_describes_itself() {
    fn walk(cmd: &clap::Command, prefix: &str, out: &mut Vec<String>) {
        for arg in cmd.get_arguments() {
            if arg.get_help().is_none() && arg.get_long_help().is_none() {
                out.push(format!("{prefix} <{}>", arg.get_id()));
            }
        }
        for sub in cmd.get_subcommands().filter(|s| s.get_name() != "help") {
            let path = format!("{prefix} {}", sub.get_name());
            if sub.get_about().is_none() && sub.get_long_about().is_none() {
                out.push(path.clone());
            }
            walk(sub, &path, out);
        }
    }

    let mut undocumented = Vec::new();
    walk(&lore::Cli::command(), "lore", &mut undocumented);
    assert!(
        undocumented.is_empty(),
        "these show up in `--help` with no description: {undocumented:?}"
    );
}

#[test]
fn every_guard_only_entry_is_still_swept() {
    let orphaned: Vec<String> = GUARD_ONLY
        .iter()
        .filter(|(argv, _)| !SWEEP.iter().any(|swept| swept == argv))
        .map(|(argv, _)| argv.join(" "))
        .collect();
    assert!(
        orphaned.is_empty(),
        "these GUARD_ONLY entries name invocations the sweep no longer runs — a corpus \
         that grew past the guard drops the entry, and so does a dropped invocation: {orphaned:?}"
    );
}

#[test]
fn every_command_is_swept_or_exempt() {
    let declared = leaf_commands();

    // A swept argv starts with its command path and continues into flags, so a
    // declared path is covered when it is a prefix of some swept argv.
    let covered = |path: &[String]| {
        SWEEP
            .iter()
            .any(|argv| argv.len() >= path.len() && argv[..path.len()] == path[..])
    };
    let exempt = |path: &[String]| EXEMPT.iter().any(|(p, _)| *p == path);

    let uncovered: Vec<String> = declared
        .iter()
        .filter(|p| !covered(p) && !exempt(p))
        .map(|p| p.join(" "))
        .collect();
    assert!(
        uncovered.is_empty(),
        "these commands are neither swept nor exempt — add them to SWEEP, or to \
         EXEMPT with the reason their answer is not a function of the fixture: {uncovered:?}"
    );

    // The reverse: an exemption for a command that no longer exists reads as
    // coverage of a surface that is gone.
    let stale: Vec<String> = EXEMPT
        .iter()
        .filter(|(p, _)| !declared.iter().any(|d| d.as_slice() == *p))
        .map(|(p, _)| p.join(" "))
        .collect();
    assert!(
        stale.is_empty(),
        "these EXEMPT entries name commands the binary no longer has: {stale:?}"
    );
}
