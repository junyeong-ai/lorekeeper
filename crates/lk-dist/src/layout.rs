use std::path::{Path, PathBuf};

use crate::DistError;
use crate::embedded::{
    EmbeddedFile, config_files, pipeline_files, skill_files, skill_names, template_files,
};

/// Where a set of agent skills is deployed.
///
/// Two levels rather than one because a skill is loaded by whatever session opens the
/// directory: a machine-wide install belongs under the home directory, and a checkout that
/// wants its own copy keeps one beside itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillLevel {
    User,
    Project,
}

impl SkillLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            SkillLevel::User => "user",
            SkillLevel::Project => "project",
        }
    }
}

/// Every directory this installation owns, resolved for the machine it is running on.
///
/// The rules match `scripts/install.sh`'s, because the installer places these files and this
/// is what finds them again. They are read from the environment at construction and never
/// again, so a test builds one with [`at`](Self::at) instead of setting process-wide state.
#[derive(Debug, Clone)]
pub struct Installation {
    binary: PathBuf,
    data_dir: PathBuf,
    config_dir: PathBuf,
    home: Option<PathBuf>,
    project: Option<PathBuf>,
}

impl Installation {
    /// The installation the running binary belongs to.
    pub fn detect() -> Result<Self, DistError> {
        let binary =
            std::env::current_exe().map_err(|e| DistError::io("locating the running binary", e))?;
        // Follows a shim: a version manager puts one on `PATH` and the real file is what a
        // replacement has to land on.
        let binary = binary.canonicalize().unwrap_or(binary);
        let home = home_dir();
        Ok(Self {
            data_dir: data_dir(home.as_deref()),
            config_dir: config_dir(home.as_deref()),
            project: std::env::current_dir().ok(),
            home,
            binary,
        })
    }

    /// The installation this binary belongs to, with the data directory an installer named.
    ///
    /// An override of that one directory rather than a second construction: reading `HOME`
    /// again here is how an empty value became `Some("")`, which resolved the user skill level
    /// to a RELATIVE `.claude/skills` and deployed into whatever directory the command ran in.
    pub fn detect_with_data_dir(data_dir: PathBuf) -> Result<Self, DistError> {
        Ok(Self {
            data_dir,
            ..Self::detect()?
        })
    }

    pub fn at(
        binary: PathBuf,
        data_dir: PathBuf,
        config_dir: PathBuf,
        home: Option<PathBuf>,
        project: Option<PathBuf>,
    ) -> Self {
        Self {
            binary,
            data_dir,
            config_dir,
            home,
            project,
        }
    }

    /// Write down which data directory this installation uses.
    ///
    /// An installer's `--data-dir` reached exactly one `self deploy` and was recorded nowhere,
    /// so every later `self status` looked in the default directory, found nothing, and
    /// reported the installation coherent — while `self update` deployed a second copy there
    /// and left the scheduled job firing the first one forever.
    pub fn remember(&self) -> Result<(), DistError> {
        std::fs::create_dir_all(&self.config_dir)
            .map_err(|e| DistError::io(format!("create {}", self.config_dir.display()), e))?;
        let path = self.config_dir.join(DATA_DIR_RECORD);
        lk_core::fs::write_atomic(&path, self.data_dir.to_string_lossy().as_bytes(), None)
            .map_err(|e| DistError::io(format!("write {}", path.display()), e))
    }

    pub fn binary(&self) -> &Path {
        &self.binary
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    pub fn templates_dir(&self) -> PathBuf {
        self.data_dir.join("templates")
    }

    pub fn pipelines_dir(&self) -> PathBuf {
        self.data_dir.join("pipelines")
    }

    /// Where a level's skills live, or `None` where this installation has no such level.
    ///
    /// A project level inside the checkout this binary was BUILT from is not a level: the
    /// `.claude/skills` there is the source of the skills it carries, so deploying into it
    /// would overwrite an in-progress edit with the last build's copy, and reporting it as a
    /// deployment would describe the repository as an installation of itself.
    pub fn skills_dir(&self, level: SkillLevel) -> Option<PathBuf> {
        let base = match level {
            SkillLevel::User => self.home.as_deref()?,
            SkillLevel::Project => {
                let project = self.project.as_deref()?;
                if is_inside(project, source_checkout().as_deref()) {
                    return None;
                }
                project
            }
        };
        Some(base.join(".claude").join("skills"))
    }

    /// The levels that already hold a deployment of any skill.
    ///
    /// A deploy writes where the skills already are and creates a set nowhere else: an install
    /// that deliberately took no skills must not acquire them from an update, and a checkout
    /// that keeps its own copy must not have a second one appear under its home directory.
    pub fn deployed_skill_levels(&self) -> Vec<SkillLevel> {
        let mut seen = std::collections::BTreeSet::new();
        [SkillLevel::User, SkillLevel::Project]
            .into_iter()
            .filter(|level| {
                let Some(dir) = self.skills_dir(*level) else {
                    return false;
                };
                let deployed = skill_names()
                    .iter()
                    .any(|name| dir.join(name).join("SKILL.md").is_file());
                // Run from the home directory, both levels name the same place. Counting it
                // twice reported one drifted file as two and deployed the same tree twice.
                deployed && seen.insert(dir.canonicalize().unwrap_or(dir))
            })
            .collect()
    }

    /// Skill directories under `level` that this binary does not carry.
    ///
    /// A skill dropped or renamed between releases is invisible to a comparison that iterates
    /// what the binary HAS, so an agent keeps loading `/lore-retired` and following instructions
    /// for commands that no longer exist — the same failure the file-level prune inside a skill
    /// exists to prevent, one level up. The `lore-` prefix bounds it to this tool's namespace,
    /// so a skill someone else wrote is never touched.
    pub fn retired_skills(&self, level: SkillLevel) -> Vec<PathBuf> {
        let Some(root) = self.skills_dir(level) else {
            return Vec::new();
        };
        let carried = skill_names();
        let mut retired: Vec<PathBuf> = self
            .deployed_record(level)
            .into_iter()
            .filter(|name| !carried.contains(&name.as_str()))
            .map(|name| root.join(name))
            .filter(|path| path.join("SKILL.md").is_file())
            .collect();
        retired.sort();
        retired
    }

    /// The skill names the last deploy wrote.
    ///
    /// The only thing on disk that distinguishes a directory THIS TOOL created from one the
    /// user wrote. A name prefix cannot: `lore-` is the natural prefix for a skill someone
    /// writes about this tool, and it is the prefix the documentation trains them on — pruning
    /// on it deleted a hand-written `lore-standup/` recursively, unannounced, on the first
    /// unattended `self update`.
    ///
    /// Per LEVEL, and written only for a level actually deployed to, because the record is a
    /// claim about what happened rather than about what the binary holds. A record of the
    /// CARRIED set let a `--skills none` install assert six skills it had deployed nowhere, and
    /// let a name written at one level authorise a removal at another — the same "a prefix is
    /// not provenance" argument one step in. An installation with no record for a level prunes
    /// nothing there, which is the right answer for one this tool has not written to.
    fn deployed_record(&self, level: SkillLevel) -> Vec<String> {
        let prefix = format!("{}:", level.as_str());
        std::fs::read_to_string(self.config_dir.join(DEPLOYED_SKILLS_RECORD))
            .map(|body| {
                body.lines()
                    .filter_map(|line| line.trim().strip_prefix(prefix.as_str()))
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Replace this level's entries, leaving every other level's alone.
    fn record_deployed(&self, level: SkillLevel, names: &[&str]) -> Result<(), DistError> {
        let path = self.config_dir.join(DEPLOYED_SKILLS_RECORD);
        let prefix = format!("{}:", level.as_str());
        let mut lines: Vec<String> = std::fs::read_to_string(&path)
            .map(|body| {
                body.lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty() && !line.starts_with(prefix.as_str()))
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        lines.extend(names.iter().map(|name| format!("{prefix}{name}")));
        lines.sort();

        std::fs::create_dir_all(&self.config_dir)
            .map_err(|e| DistError::io(format!("create {}", self.config_dir.display()), e))?;
        // A line-oriented file ends with a newline, so appending to it by hand cannot merge two
        // entries into one.
        let body = lines
            .iter()
            .map(|line| format!("{line}\n"))
            .collect::<String>();
        lk_core::fs::write_atomic(&path, body.as_bytes(), None)
            .map_err(|e| DistError::io(format!("write {}", path.display()), e))
    }

    /// Write the skills this binary carries to one level, and remove any it has retired.
    pub fn deploy_skills(&self, level: SkillLevel) -> Result<Deployed, DeployFailure> {
        let prune = Prune::for_level(level);
        let root = self.skills_dir(level).ok_or_else(|| DeployFailure {
            done: Deployed::default(),
            error: DistError::Unsupported(self.no_such_level(level)),
        })?;
        let mut removed = Vec::new();
        for retired in self
            .retired_skills(level)
            .into_iter()
            .filter(|_| prune.sweeps())
        {
            if let Err(e) = std::fs::remove_dir_all(&retired) {
                return Err(DeployFailure {
                    done: Deployed {
                        written: Vec::new(),
                        removed,
                    },
                    error: DistError::io(format!("remove {}", retired.display()), e),
                });
            }
            removed.push(retired);
        }
        let mut written = Vec::new();
        for name in skill_names() {
            let dir = root.join(name);
            if let Err(error) = write_tree_with(&dir, &skill_files(name), prune) {
                return Err(DeployFailure {
                    done: Deployed { written, removed },
                    error,
                });
            }
            written.push(dir);
        }
        // Recorded only now, and only for this level: the record is a claim about what was
        // written, so a failure above leaves the previous claim standing — which the next
        // deploy re-prunes against correctly, since a name whose directory is already gone
        // falls out on its own.
        if let Err(error) = self.record_deployed(level, &skill_names()) {
            return Err(DeployFailure {
                done: Deployed { written, removed },
                error,
            });
        }
        Ok(Deployed { written, removed })
    }

    /// Why a level has no directory, which is two different answers: there is no base to build
    /// one from, or the base is the checkout this binary was built from — where the skills are
    /// the source rather than a deployment, and a caller told only "no directory" would go
    /// looking for one to create.
    pub fn skills_reason(&self, level: SkillLevel) -> String {
        self.no_such_level(level)
    }

    fn no_such_level(&self, level: SkillLevel) -> String {
        match (level, self.project.as_deref()) {
            (SkillLevel::Project, Some(project))
                if is_inside(project, source_checkout().as_deref()) =>
            {
                format!(
                    "{} is the checkout this binary was built from — the skills under it are \
                     what this binary carries, not a deployment of them",
                    project.display()
                )
            }
            _ => format!("no {} directory to deploy skills into", level.as_str()),
        }
    }

    pub fn deploy_pipelines(&self) -> Result<PathBuf, DistError> {
        let dir = self.pipelines_dir();
        write_tree(&dir, &pipeline_files())?;
        Ok(dir)
    }

    pub fn deploy_templates(&self) -> Result<PathBuf, DistError> {
        let dir = self.templates_dir();
        write_tree(&dir, &template_files())?;
        Ok(dir)
    }

    /// The config example, written beside wherever `config.yaml` is discovered.
    ///
    /// Written file by file rather than as an exact tree: the directory holds the user's
    /// own `config.yaml`, which this does not own and must not remove.
    pub fn deploy_config_example(&self) -> Result<PathBuf, DistError> {
        let dir = self.config_dir.clone();
        std::fs::create_dir_all(&dir)
            .map_err(|e| DistError::io(format!("create {}", dir.display()), e))?;
        for file in config_files() {
            let path = dir.join(file.relative);
            lk_core::fs::write_atomic(&path, file.contents.as_bytes(), Some(file.mode))
                .map_err(|e| DistError::io(format!("write {}", path.display()), e))?;
        }
        Ok(dir)
    }
}

/// Whether a deploy may delete what it did not write.
///
/// Deleting is the half that costs unrecoverable work; overwriting is what a redeploy is for
/// and is recoverable wherever the files are under version control. The project level is the
/// one place this tool cannot tell its own earlier output from a checkout's tracked files —
/// the guard that answers for a build's OWN checkout is compile-time, so a released binary can
/// never ask it — and there the write happens while the sweep does not.
///
/// It is a property of the LEVEL, never of the invocation. Keyed off the flag a caller passed,
/// `--skills project` left a directory alone that the bare `lore self deploy` inside `lore self
/// update` then swept, so the protection lasted until the next unattended update and no
/// further. And what the sweep leaves, the check must not count: an extra file reported as
/// drift by a command whose named repair declines to remove it is the same unclearable loop a
/// path comparison produced on a case-folding volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prune {
    Sweep,
    Leave,
}

impl Prune {
    pub fn for_level(level: SkillLevel) -> Self {
        match level {
            SkillLevel::Project => Prune::Leave,
            SkillLevel::User => Prune::Sweep,
        }
    }

    pub(crate) fn sweeps(self) -> bool {
        self == Prune::Sweep
    }
}

/// What one level's deploy did.
///
/// The removals are carried back so the caller can NAME them: a deploy that deletes a directory
/// and prints only what it wrote leaves the one irreversible thing it did unsaid.
#[derive(Debug, Default, Clone)]
pub struct Deployed {
    pub written: Vec<PathBuf>,
    pub removed: Vec<PathBuf>,
}

/// A deploy that stopped partway, carrying what it had already done.
///
/// The removals travel WITH the error rather than being lost to it: the failure path is the one
/// where the user most needs to know a directory is gone, and an error saying only "permission
/// denied" reads as "nothing happened".
#[derive(Debug)]
pub struct DeployFailure {
    pub done: Deployed,
    pub error: DistError,
}

impl std::fmt::Display for DeployFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

/// Bring `dir` to exactly `files`: every file written, and anything else under it removed.
///
/// Exact in both directions, because a deployed set is read as a whole. A reference file
/// dropped between releases stays readable to an agent that was told the directory is the
/// contract, and it describes a binary that no longer behaves that way — so the removal is not
/// tidying, it is the same correctness the writing is.
///
/// Removal is per-file against the set being written, never a recursive delete of the
/// directory: the paths come from the embedded manifest, so nothing an attacker or a typo
/// could name ever reaches `remove_dir_all`.
#[cfg(test)]
pub(crate) fn write_tree_for_tests(dir: &Path, files: &[EmbeddedFile]) -> Result<(), DistError> {
    write_tree(dir, files)
}

fn write_tree(dir: &Path, files: &[EmbeddedFile]) -> Result<(), DistError> {
    write_tree_with(dir, files, Prune::Sweep)
}

fn write_tree_with(dir: &Path, files: &[EmbeddedFile], prune: Prune) -> Result<(), DistError> {
    std::fs::create_dir_all(dir)
        .map_err(|e| DistError::io(format!("create {}", dir.display()), e))?;

    let mut expected = std::collections::BTreeSet::new();
    for file in files {
        let path = dir.join(file.relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| DistError::io(format!("create {}", parent.display()), e))?;
        }
        lk_core::fs::write_atomic(&path, file.contents.as_bytes(), Some(file.mode))
            .map_err(|e| DistError::io(format!("write {}", path.display()), e))?;
        expected.insert(identity(&path));
    }

    if !prune.sweeps() {
        return Ok(());
    }
    for entry in walkdir::WalkDir::new(dir).contents_first(true).into_iter() {
        let entry =
            entry.map_err(|e| DistError::io(format!("read {}", dir.display()), e.into()))?;
        let path = entry.path();
        if path == dir {
            continue;
        }
        if entry.file_type().is_dir() {
            // Only when nothing is left in it — a directory the set still uses keeps its
            // contents, and `contents_first` means its files have already been decided.
            let _ = std::fs::remove_dir(path);
        } else if !expected.contains(&identity(path)) {
            std::fs::remove_file(path)
                .map_err(|e| DistError::io(format!("remove {}", path.display()), e))?;
        }
    }
    Ok(())
}

/// The repository this binary was compiled from, where it is still on this machine.
///
/// The path is baked in at compile time, so a released binary names a build directory that does
/// not exist on the machine running it and this answers `None` — the guard fires only for a
/// build run from its own checkout, which is exactly the case it exists for.
fn source_checkout() -> Option<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent()?.parent()?;
    root.join("Cargo.toml")
        .is_file()
        .then(|| root.to_path_buf())
}

/// Whether `path` is `base` or sits under it, compared through the filesystem's own answer so a
/// symlinked or differently spelled checkout is still recognised.
fn is_inside(path: &Path, base: Option<&Path>) -> bool {
    let Some(base) = base else {
        return false;
    };
    match (path.canonicalize(), base.canonicalize()) {
        (Ok(path), Ok(base)) => path.starts_with(base),
        _ => path.starts_with(base),
    }
}

/// What the filesystem thinks a path names, so two spellings of one file compare equal.
///
/// A rename onto an entry differing only in case keeps the entry's own spelling on macOS and
/// Windows, so the path read back by the walk is not the path that was written — and the prune
/// would delete the file it had just created, report success, and leave a skill pointing at a
/// reference that is gone. `canonicalize` cannot answer this: macOS `realpath` resolves symlinks
/// and leaves case exactly as it was given. The inode does answer it, exactly, and needs no
/// probe of the volume's behaviour. CI runs a case-sensitive volume, so no gate here could have
/// caught this.
#[cfg(unix)]
pub(crate) fn identity(path: &Path) -> FileIdentity {
    use std::os::unix::fs::MetadataExt;
    // `symlink_metadata`, so a link is ITSELF rather than what it names. Following it put a
    // symlink pointing at a carried file into the expected set, where it survived the prune and
    // was then skipped by the comparison — an agent loads it exactly like a real file.
    match std::fs::symlink_metadata(path) {
        Ok(meta) => FileIdentity::OnDisk(meta.dev(), meta.ino()),
        // Only for a path nothing wrote, which the prune then removes as it should.
        Err(_) => FileIdentity::Named(path.to_path_buf()),
    }
}

/// Windows has no inode `std` exposes on stable, and NTFS folds case, so the name compared
/// without case is the same answer.
#[cfg(not(unix))]
pub(crate) fn identity(path: &Path) -> FileIdentity {
    FileIdentity::Named(PathBuf::from(path.to_string_lossy().to_lowercase()))
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum FileIdentity {
    OnDisk(u64, u64),
    Named(PathBuf),
}

/// The home directory, in the order the platform's own tools read it.
///
/// Windows first: `install.ps1` and Claude Code both use `USERPROFILE`, and a Git-for-Windows
/// or MSYS shell sets `HOME` to something else — deploying the skills there would put them
/// where no session loads them, while the installer's own message named the other directory.
fn home_dir() -> Option<PathBuf> {
    let (first, second) = if cfg!(windows) {
        ("USERPROFILE", "HOME")
    } else {
        ("HOME", "USERPROFILE")
    };
    std::env::var_os(first)
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var_os(second).filter(|h| !h.is_empty()))
        .map(PathBuf::from)
}

/// Where `self deploy` records the data directory it used, so a later command finds the same
/// one instead of the default.
const DATA_DIR_RECORD: &str = "data-dir";

/// Where `self deploy` records the skills it wrote, so a later one can tell a directory this
/// tool created from one the user did.
const DEPLOYED_SKILLS_RECORD: &str = "deployed-skills";

fn data_dir(home: Option<&Path>) -> PathBuf {
    if let Some(explicit) = std::env::var_os("LORE_INSTALL_DATA_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(explicit);
    }
    // What the last deploy actually used. Below the environment, which is an explicit
    // instruction for this run, and above the defaults, which are only a guess.
    if let Some(recorded) = std::fs::read_to_string(config_dir(home).join(DATA_DIR_RECORD))
        .ok()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
    {
        return PathBuf::from(recorded);
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        // Windows only, where it is what `install.ps1` writes into: absent everywhere else, so
        // it never displaces the XDG rule on a platform that has one.
        .or_else(|| {
            std::env::var_os("LOCALAPPDATA")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        })
        .or_else(|| home.map(|h| h.join(".local").join("share")))
        .unwrap_or_else(|| PathBuf::from(".local").join("share"));
    base.join("lorekeeper")
}

fn config_dir(home: Option<&Path>) -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.map(|h| h.join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join("lorekeeper")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(relative: &'static str, contents: &'static str) -> EmbeddedFile {
        EmbeddedFile {
            relative,
            contents,
            mode: 0o644,
        }
    }

    #[test]
    fn a_deploy_writes_every_file_it_carries() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("lore-process");
        write_tree(
            &dir,
            &[file("SKILL.md", "skill"), file("references/a.md", "ref")],
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("SKILL.md")).unwrap(),
            "skill"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("references/a.md")).unwrap(),
            "ref"
        );
    }

    /// A reference file dropped between releases still loads for an agent told the directory is
    /// the contract, and describes a binary that no longer behaves that way.
    #[test]
    fn a_deploy_removes_what_the_set_no_longer_holds() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("lore-process");
        write_tree(
            &dir,
            &[file("SKILL.md", "skill"), file("references/old.md", "gone")],
        )
        .unwrap();
        write_tree(&dir, &[file("SKILL.md", "skill")]).unwrap();
        assert!(dir.join("SKILL.md").is_file());
        assert!(!dir.join("references/old.md").exists());
        assert!(
            !dir.join("references").exists(),
            "a directory the set emptied is removed too"
        );
    }

    #[test]
    fn a_deploy_replaces_a_modified_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("lore-process");
        write_tree(&dir, &[file("SKILL.md", "skill")]).unwrap();
        std::fs::write(dir.join("SKILL.md"), "hand edited").unwrap();
        write_tree(&dir, &[file("SKILL.md", "skill")]).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("SKILL.md")).unwrap(),
            "skill"
        );
    }

    /// The prune compares files, not names. A hardlink is two names for one file on every
    /// filesystem, so it proves the mechanism without needing a case-folding volume — which CI
    /// deliberately does not run, and which is where the bug this closes only ever appeared.
    #[cfg(unix)]
    #[test]
    fn one_file_under_two_names_is_one_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("jira.md");
        let second = tmp.path().join("also-jira.md");
        std::fs::write(&first, "carried").unwrap();
        std::fs::hard_link(&first, &second).unwrap();

        assert_eq!(identity(&first), identity(&second));
        assert_ne!(identity(&first), identity(&tmp.path().join("absent.md")));
    }

    #[cfg(unix)]
    #[test]
    fn a_pipeline_is_written_executable() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("pipelines");
        write_tree(
            &dir,
            &[EmbeddedFile {
                relative: "lore-daily.sh",
                contents: "#!/usr/bin/env bash\n",
                mode: 0o755,
            }],
        )
        .unwrap();
        let mode = std::fs::metadata(dir.join("lore-daily.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
    }

    /// A prefix is not provenance. `lore-` is the natural prefix for a skill someone writes
    /// about this tool, and the one its documentation trains them on — pruning on it deleted a
    /// hand-written skill recursively on the first unattended update.
    #[test]
    fn only_a_skill_this_tool_recorded_writing_is_ever_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let config = tmp.path().join("config");
        let root = home.join(".claude").join("skills");
        for name in ["lore-retired", "lore-standup"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
            std::fs::write(root.join(name).join("SKILL.md"), "x").unwrap();
        }
        let installation = Installation::at(
            tmp.path().join("bin/lore"),
            tmp.path().join("data"),
            config.clone(),
            Some(home),
            None,
        );

        // Nothing recorded yet: an installation this tool has not written to loses nothing.
        assert!(installation.retired_skills(SkillLevel::User).is_empty());

        std::fs::create_dir_all(&config).unwrap();
        installation
            .record_deployed(
                SkillLevel::User,
                &[skill_names(), vec!["lore-retired"]].concat(),
            )
            .unwrap();
        assert_eq!(
            installation.retired_skills(SkillLevel::User),
            [root.join("lore-retired")]
        );

        // The record is per level. A name written at one level authorises nothing at another —
        // the directory there was never this tool's to remove.
        assert!(installation.retired_skills(SkillLevel::Project).is_empty());
    }

    /// The record is a claim about what was WRITTEN. Recording the carried set instead let an
    /// install that took no skills assert six it had deployed nowhere.
    #[test]
    fn an_install_that_took_no_skills_claims_none() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("config");
        let installation = Installation::at(
            tmp.path().join("bin/lore"),
            tmp.path().join("data"),
            config.clone(),
            Some(tmp.path().join("home")),
            None,
        );
        installation.remember().unwrap();
        assert!(!config.join(DEPLOYED_SKILLS_RECORD).exists());
        assert!(installation.retired_skills(SkillLevel::User).is_empty());
    }

    /// An agent loads a symlink exactly like a file, and `walkdir` does not follow one — so a
    /// link left inside a deployed skill was invisible to the prune and to the check at once.
    #[cfg(unix)]
    #[test]
    fn a_symlink_inside_a_deployed_tree_is_a_file_to_whoever_reads_it() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("skill");
        let carried = [file("SKILL.md", "skill")];
        write_tree(&dir, &carried).unwrap();
        std::os::unix::fs::symlink("SKILL.md", dir.join("EXTRA.md")).unwrap();

        assert_ne!(
            identity(&dir.join("EXTRA.md")),
            identity(&dir.join("SKILL.md")),
            "a link is itself, not what it names"
        );
        write_tree(&dir, &carried).unwrap();
        assert!(!dir.join("EXTRA.md").exists(), "and the prune reaches it");
    }

    /// A skill level with no deployment is not one a deploy may create: an install that took no
    /// skills must not acquire them, and a checkout with its own copy must not gain a second.
    #[test]
    fn only_a_level_that_already_holds_skills_is_deployed_to() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let name = skill_names()[0];
        let deployed = home.join(".claude").join("skills").join(name);
        std::fs::create_dir_all(&deployed).unwrap();
        std::fs::write(deployed.join("SKILL.md"), "x").unwrap();

        let installation = Installation::at(
            tmp.path().join("bin/lore"),
            tmp.path().join("data"),
            tmp.path().join("config"),
            Some(home),
            Some(project),
        );
        assert_eq!(installation.deployed_skill_levels(), [SkillLevel::User]);
    }

    /// The checkout is where the skills come FROM. A build run inside it would otherwise write
    /// the last build's copies over an edit in progress, and report the repository as an
    /// installation of itself.
    #[test]
    fn the_checkout_this_was_built_from_is_not_a_deployment_of_it() {
        let source = source_checkout().expect("the suite runs from the checkout");
        let tmp = tempfile::tempdir().unwrap();
        let installation = Installation::at(
            tmp.path().join("bin/lore"),
            tmp.path().join("data"),
            tmp.path().join("config"),
            Some(tmp.path().join("home")),
            Some(source.clone()),
        );
        assert_eq!(installation.skills_dir(SkillLevel::Project), None);
        assert!(installation.deploy_skills(SkillLevel::Project).is_err());

        // A directory that is not the checkout is an ordinary project level.
        let elsewhere = Installation::at(
            tmp.path().join("bin/lore"),
            tmp.path().join("data"),
            tmp.path().join("config"),
            Some(tmp.path().join("home")),
            Some(tmp.path().to_path_buf()),
        );
        assert!(elsewhere.skills_dir(SkillLevel::Project).is_some());
        assert!(source.join("Cargo.toml").is_file());
    }
}
