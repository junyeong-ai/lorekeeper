use lk_core::execution::AgentEngine;
use std::path::{Path, PathBuf};

use crate::DistError;
use crate::embedded::{EmbeddedFile, config_files, skill_files_for, skill_names, template_files};
use crate::tree::ArtifactTree;

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
    engine: AgentEngine,
}

impl Installation {
    /// The installation the running binary belongs to.
    pub fn detect() -> Result<Self, DistError> {
        Self::detect_selected(None)
    }

    fn detect_selected(override_dir: Option<PathBuf>) -> Result<Self, DistError> {
        let binary =
            std::env::current_exe().map_err(|e| DistError::io("locating the running binary", e))?;
        // Follows a shim: a version manager puts one on `PATH` and the real file is what a
        // replacement has to land on.
        let binary = binary.canonicalize().unwrap_or(binary);
        let project = std::env::current_dir()
            .map_err(|e| DistError::io("locating the installation project directory", e))?;
        let home = home_dir().map(|path| absolute_path(&path)).transpose()?;
        let config_dir = absolute_path(&config_dir(home.as_deref()))?;
        let data_dir = match override_dir {
            Some(path) => absolute_path(&path)?,
            None => absolute_path(&data_dir(home.as_deref())?)?,
        };
        Ok(Self {
            data_dir,
            config_dir,
            project: Some(project),
            home,
            binary,
            engine: AgentEngine::ClaudeCode,
        })
    }

    /// The installation this binary belongs to, with the data directory an installer named.
    ///
    /// An override of that one directory rather than a second construction: reading `HOME`
    /// again here is how an empty value became `Some("")`, which resolved the user skill level
    /// to a RELATIVE `.claude/skills` and deployed into whatever directory the command ran in.
    pub fn detect_with_data_dir(data_dir: PathBuf) -> Result<Self, DistError> {
        Self::detect_selected(Some(data_dir))
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
            engine: AgentEngine::ClaudeCode,
        }
    }

    pub fn for_engine(&self, engine: AgentEngine) -> Self {
        Self {
            engine,
            ..self.clone()
        }
    }

    pub fn with_project(mut self, project: PathBuf) -> Self {
        self.project = Some(project);
        self
    }

    pub fn engine(&self) -> AgentEngine {
        self.engine
    }

    pub fn runtime_skills_dir(&self) -> PathBuf {
        self.data_dir.join("skills").join(self.engine.as_str())
    }

    pub fn deploy_runtime_skills(&self) -> Result<PathBuf, DistError> {
        let root = self.runtime_skills_dir();
        for name in skill_names() {
            ArtifactTree::build(
                &self.data_dir,
                &root.join(name),
                &skill_files_for(self.engine, name),
            )?;
        }
        for name in skill_names() {
            write_tree_with(
                &self.data_dir,
                &root.join(name),
                &skill_files_for(self.engine, name),
                Prune::Sweep,
            )?;
        }
        Ok(root)
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

    pub(crate) fn has_deployment_record(&self) -> bool {
        !matches!(
            std::fs::symlink_metadata(self.config_dir.join(DATA_DIR_RECORD)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound
        )
    }

    pub fn templates_dir(&self) -> PathBuf {
        self.data_dir.join("templates")
    }

    /// Host discovery location for an explicitly selected installation scope.
    pub fn skills_dir(&self, level: SkillLevel) -> Option<PathBuf> {
        let base = match level {
            SkillLevel::User => self.home.as_deref()?,
            SkillLevel::Project => self.project.as_deref()?,
        };
        Some(base.join(self.engine.skills_directory()).join("skills"))
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
                let deployed = skill_names().iter().any(|name| {
                    let skill = dir.join(name);
                    match std::fs::symlink_metadata(&skill) {
                        Ok(metadata) if !metadata.is_dir() => true,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                        Err(_) => true,
                        Ok(_) => !matches!(std::fs::symlink_metadata(skill.join("SKILL.md")), Err(error) if error.kind() == std::io::ErrorKind::NotFound),
                    }
                });
                // Run from the home directory, both levels name the same place. Counting it
                // twice reported one drifted file as two and deployed the same tree twice.
                deployed && seen.insert(dir.canonicalize().unwrap_or(dir))
            })
            .collect()
    }

    /// Retired names come from this level's deployment record, never a naming prefix.
    /// Failed inspection preserves ownership and refuses cleanup.
    pub fn retired_skills(&self, level: SkillLevel) -> Result<Vec<PathBuf>, DistError> {
        let Some(root) = self.skills_dir(level) else {
            return Ok(Vec::new());
        };
        let carried = skill_names();
        let mut retired = Vec::new();
        for name in self.deployed_record(level)? {
            if carried.contains(&name.as_str()) {
                continue;
            }
            let path = root.join(name);
            match std::fs::symlink_metadata(path.join("SKILL.md")) {
                Ok(_) => retired.push(path),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(DistError::io(
                        format!("inspect retired skill {}", path.display()),
                        error,
                    ));
                }
            }
        }
        retired.sort();
        Ok(retired)
    }

    /// Ownership is recorded per engine and level. An absent legacy record grants no
    /// removal authority; an unreadable or malformed record is an error.
    fn deployed_record(&self, level: SkillLevel) -> Result<Vec<String>, DistError> {
        let prefix = format!("{}/{}:", self.engine.as_str(), level.as_str());
        let path = self.config_dir.join(DEPLOYED_SKILLS_RECORD);
        let body = match crate::tree::load_text(&path) {
            Ok(body) => body,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(DistError::io(format!("read {}", path.display()), error)),
        };
        let names: Vec<String> = body
            .lines()
            .filter_map(|line| line.trim().strip_prefix(prefix.as_str()))
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect();
        for name in &names {
            let mut parts = Path::new(name).components();
            if !matches!(parts.next(), Some(std::path::Component::Normal(_)))
                || parts.next().is_some()
            {
                return Err(DistError::Unsupported(format!(
                    "invalid deployed skill name in {}",
                    path.display()
                )));
            }
        }
        Ok(names)
    }

    /// Replace this level's entries, leaving every other level's alone.
    fn record_deployed(&self, level: SkillLevel, names: &[&str]) -> Result<(), DistError> {
        let path = self.config_dir.join(DEPLOYED_SKILLS_RECORD);
        let prefix = format!("{}/{}:", self.engine.as_str(), level.as_str());
        let body = match crate::tree::load_text(&path) {
            Ok(body) => body,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(DistError::io(format!("read {}", path.display()), error)),
        };
        let mut lines: Vec<String> = body
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with(prefix.as_str()))
            .map(str::to_owned)
            .collect();
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

    /// Deploy carried skills, pruning retired skills only at the user level.
    pub fn deploy_skills(&self, level: SkillLevel) -> Result<Deployed, DeployFailure> {
        let prune = Prune::for_level(level);
        let root = self.skills_dir(level).ok_or_else(|| DeployFailure {
            done: Deployed::default(),
            error: DistError::Unsupported(self.no_such_level(level)),
        })?;
        self.validate_skill_trees(level)
            .map_err(|error| DeployFailure {
                done: Deployed::default(),
                error,
            })?;
        let mut removed = Vec::new();
        if prune.sweeps() {
            for retired in self.retired_skills(level).map_err(|error| DeployFailure {
                done: Deployed::default(),
                error,
            })? {
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
        }
        let mut written = Vec::new();
        for name in skill_names() {
            let dir = root.join(name);
            if let Err(error) =
                write_tree_with(&root, &dir, &skill_files_for(self.engine, name), prune)
            {
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

    /// Validate every selected payload before any deployment writes or removals.
    pub fn validate_deployment(&self, levels: &[SkillLevel]) -> Result<(), DistError> {
        for &level in levels {
            self.validate_skill_trees(level)?;
        }
        let root = self.runtime_skills_dir();
        for name in skill_names() {
            ArtifactTree::build(
                &self.data_dir,
                &root.join(name),
                &skill_files_for(self.engine, name),
            )?;
        }
        ArtifactTree::build(&self.data_dir, &self.templates_dir(), &template_files())?;
        ArtifactTree::build(&self.config_dir, &self.config_dir, &config_files())?;
        Ok(())
    }

    fn validate_skill_trees(&self, level: SkillLevel) -> Result<(), DistError> {
        let root = self
            .skills_dir(level)
            .ok_or_else(|| DistError::Unsupported(self.no_such_level(level)))?;
        self.deployed_record(level)?;
        for name in skill_names() {
            ArtifactTree::build(&root, &root.join(name), &skill_files_for(self.engine, name))?;
        }
        if Prune::for_level(level).sweeps() {
            for retired in self.retired_skills(level)? {
                ArtifactTree::build(&root, &retired, &[])?;
            }
        }
        Ok(())
    }

    /// Why an installation scope cannot be resolved.
    pub fn skills_reason(&self, level: SkillLevel) -> String {
        self.no_such_level(level)
    }

    fn no_such_level(&self, level: SkillLevel) -> String {
        format!("no {} directory to deploy skills into", level.as_str())
    }

    pub fn deploy_templates(&self) -> Result<PathBuf, DistError> {
        let dir = self.templates_dir();
        write_tree_with(&self.data_dir, &dir, &template_files(), Prune::Sweep)?;
        Ok(dir)
    }

    /// Starter and reference configs, written beside the user's `config.yaml`.
    ///
    /// Written file by file rather than as an exact tree: the directory holds the user's
    /// own `config.yaml`, which this does not own and must not remove.
    pub fn deploy_config_examples(&self) -> Result<PathBuf, DistError> {
        let dir = self.config_dir.clone();
        write_tree_with(&dir, &dir, &config_files(), Prune::Leave)?;
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

#[cfg(test)]
fn write_tree(dir: &Path, files: &[EmbeddedFile]) -> Result<(), DistError> {
    write_tree_with(dir, dir, files, Prune::Sweep)
}

fn write_tree_with(
    root: &Path,
    dir: &Path,
    files: &[EmbeddedFile],
    prune: Prune,
) -> Result<(), DistError> {
    let tree = ArtifactTree::build(root, dir, files)?;
    tree.provision()?;
    let dir = tree.directory();

    let mut expected = std::collections::BTreeSet::new();
    for file in files {
        let path = dir.join(file.relative);
        lk_core::fs::write_atomic(&path, file.contents.as_bytes(), Some(file.mode))
            .map_err(|e| DistError::io(format!("write {}", path.display()), e))?;
        expected.insert(identity(&path));
    }

    if !prune.sweeps() {
        return Ok(());
    }
    for entry in walkdir::WalkDir::new(dir)
        .follow_root_links(false)
        .contents_first(true)
        .into_iter()
    {
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

fn absolute_path(path: &Path) -> Result<PathBuf, DistError> {
    std::path::absolute(path).map_err(|error| {
        DistError::io(
            format!("resolving installation path {}", path.display()),
            error,
        )
    })
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

fn data_dir(home: Option<&Path>) -> Result<PathBuf, DistError> {
    if let Some(explicit) = std::env::var_os("LORE_INSTALL_DATA_DIR").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(explicit));
    }
    // What the last deploy actually used. Below the environment, which is an explicit
    // instruction for this run, and above the defaults, which are only a guess.
    let path = config_dir(home).join(DATA_DIR_RECORD);
    match crate::tree::load_text(&path) {
        Ok(recorded) if !recorded.trim().is_empty() => return Ok(PathBuf::from(recorded.trim())),
        Ok(_) => {
            return Err(DistError::Unsupported(format!(
                "empty installation data directory record: {}",
                path.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(DistError::io(format!("read {}", path.display()), error)),
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
    Ok(base.join("lorekeeper"))
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

    #[test]
    fn installation_paths_are_absolute_without_requiring_existing_directories() {
        let relative = Path::new("relative installation").join("자료");
        assert_eq!(
            absolute_path(&relative).unwrap(),
            std::env::current_dir().unwrap().join(relative)
        );
        let temporary = tempfile::tempdir().unwrap();
        let missing = temporary.path().join("not installed").join("data");
        assert!(!missing.exists());
        assert_eq!(absolute_path(&missing).unwrap(), missing);
    }

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
        assert!(
            installation
                .retired_skills(SkillLevel::User)
                .unwrap()
                .is_empty()
        );

        std::fs::create_dir_all(&config).unwrap();
        installation
            .record_deployed(
                SkillLevel::User,
                &[skill_names(), vec!["lore-retired"]].concat(),
            )
            .unwrap();
        assert_eq!(
            installation.retired_skills(SkillLevel::User).unwrap(),
            [root.join("lore-retired")]
        );

        // The record is per level. A name written at one level authorises nothing at another —
        // the directory there was never this tool's to remove.
        assert!(
            installation
                .retired_skills(SkillLevel::Project)
                .unwrap()
                .is_empty()
        );
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
        assert!(
            installation
                .retired_skills(SkillLevel::User)
                .unwrap()
                .is_empty()
        );
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

    #[test]
    fn agent_deployments_and_ownership_are_independent() {
        let tmp = tempfile::tempdir().unwrap();
        let installation = Installation::at(
            tmp.path().join("bin/lore"),
            tmp.path().join("data"),
            tmp.path().join("config"),
            Some(tmp.path().join("home")),
            None,
        );
        let codex = installation.for_engine(AgentEngine::Codex);
        installation.deploy_skills(SkillLevel::User).unwrap();
        assert!(codex.deployed_skill_levels().is_empty());
        assert!(codex.deployed_record(SkillLevel::User).unwrap().is_empty());
        codex.deploy_skills(SkillLevel::User).unwrap();
        let root = codex.skills_dir(SkillLevel::User).unwrap();
        assert!(root.ends_with(".agents/skills"));
        let source = std::fs::read_to_string(root.join("lore-process/SKILL.md")).unwrap();
        let page = lk_core::frontmatter::parse_page(&source).unwrap();
        assert_eq!(page.frontmatter.get("name").unwrap(), "lore-process");
        assert_eq!(
            installation.deployed_record(SkillLevel::User).unwrap(),
            codex.deployed_record(SkillLevel::User).unwrap()
        );
    }

    #[test]
    fn project_deployment_preserves_canonical_skill_sources() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("skills/lore-process/SKILL.md");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, "in-progress canonical instructions").unwrap();
        let installation = Installation::at(
            tmp.path().join("bin/lore"),
            tmp.path().join("data"),
            tmp.path().join("config"),
            None,
            Some(tmp.path().to_owned()),
        );
        for engine in [AgentEngine::ClaudeCode, AgentEngine::Codex] {
            let selected = installation.for_engine(engine);
            selected.deploy_skills(SkillLevel::Project).unwrap();
            assert!(
                selected
                    .skills_dir(SkillLevel::Project)
                    .unwrap()
                    .join("lore-process/SKILL.md")
                    .is_file()
            );
            assert_eq!(
                std::fs::read_to_string(&source).unwrap(),
                "in-progress canonical instructions"
            );
        }
    }
}

#[cfg(all(test, unix))]
mod boundary_tests {
    use super::*;

    #[test]
    fn derived_links_refuse_before_writing_or_pruning_in_both_scopes() {
        let files = [
            EmbeddedFile {
                relative: "SKILL.md",
                contents: "new",
                mode: 0o644,
            },
            EmbeddedFile {
                relative: "references/a.md",
                contents: "reference",
                mode: 0o644,
            },
        ];
        for prune in [Prune::Sweep, Prune::Leave] {
            for placement in ["root", "parent", "internal"] {
                let temporary = tempfile::tempdir().unwrap();
                let root = temporary.path().join("selected");
                let dir = root.join("skill");
                let outside = temporary.path().join("outside");
                std::fs::create_dir(&root).unwrap();
                std::fs::create_dir(&outside).unwrap();
                std::fs::write(outside.join("custom.md"), "keep").unwrap();
                if placement == "root" {
                    std::os::unix::fs::symlink(&outside, &dir).unwrap();
                } else {
                    std::fs::create_dir(&dir).unwrap();
                    std::fs::write(dir.join("SKILL.md"), "previous").unwrap();
                    let target = if placement == "internal" {
                        let internal = dir.join("shared");
                        std::fs::create_dir(&internal).unwrap();
                        internal
                    } else {
                        outside.clone()
                    };
                    std::os::unix::fs::symlink(target, dir.join("references")).unwrap();
                }
                assert!(matches!(
                    write_tree_with(&root, &dir, &files, prune),
                    Err(DistError::Unsupported(_))
                ));
                assert_eq!(
                    std::fs::read_to_string(outside.join("custom.md")).unwrap(),
                    "keep"
                );
                assert!(!outside.join("SKILL.md").exists());
                if placement != "root" {
                    assert_eq!(
                        std::fs::read_to_string(dir.join("SKILL.md")).unwrap(),
                        "previous"
                    );
                    assert!(dir.join("references").is_symlink());
                }
            }
        }
    }

    #[test]
    fn selected_root_and_leaf_aliases_preserve_external_originals() {
        for hardlink in [false, true] {
            let temporary = tempfile::tempdir().unwrap();
            let root = temporary.path().join("actual");
            let alias = temporary.path().join("별칭 space");
            let dir = root.join("skill");
            let original = temporary.path().join("original.md");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(&original, "original").unwrap();
            std::os::unix::fs::symlink(&root, &alias).unwrap();
            if hardlink {
                std::fs::hard_link(&original, dir.join("SKILL.md")).unwrap();
            } else {
                std::os::unix::fs::symlink(&original, dir.join("SKILL.md")).unwrap();
            }
            write_tree_with(
                &alias,
                &alias.join("skill"),
                &[EmbeddedFile {
                    relative: "SKILL.md",
                    contents: "new",
                    mode: 0o644,
                }],
                Prune::Sweep,
            )
            .unwrap();
            assert_eq!(std::fs::read_to_string(&original).unwrap(), "original");
            assert_eq!(
                std::fs::read_to_string(dir.join("SKILL.md")).unwrap(),
                "new"
            );
            assert!(!dir.join("SKILL.md").is_symlink());
        }
    }
    #[test]
    fn project_deployment_ignores_inaccessible_retired_payloads() {
        use std::os::unix::fs::PermissionsExt;
        for engine in [AgentEngine::ClaudeCode, AgentEngine::Codex] {
            let temporary = tempfile::tempdir().unwrap();
            let installation = Installation::at(
                temporary.path().join("lore"),
                temporary.path().join("data"),
                temporary.path().join("config"),
                None,
                Some(temporary.path().join("project")),
            )
            .for_engine(engine);
            installation.deploy_skills(SkillLevel::Project).unwrap();
            let root = installation.skills_dir(SkillLevel::Project).unwrap();
            let retired = root.join("lore-retired");
            std::fs::create_dir(&retired).unwrap();
            std::fs::write(retired.join("SKILL.md"), "private project instructions").unwrap();
            installation
                .record_deployed(
                    SkillLevel::Project,
                    &[skill_names(), vec!["lore-retired"]].concat(),
                )
                .unwrap();
            let current = root.join(skill_names()[0]).join("SKILL.md");
            std::fs::write(&current, "previous").unwrap();
            std::fs::set_permissions(&retired, std::fs::Permissions::from_mode(0o000)).unwrap();
            let inaccessible = std::fs::symlink_metadata(retired.join("SKILL.md")).is_err();
            let inspection = installation.retired_skills(SkillLevel::Project);
            let deployment = installation.deploy_skills(SkillLevel::Project);
            std::fs::set_permissions(&retired, std::fs::Permissions::from_mode(0o755)).unwrap();
            if inaccessible {
                assert!(inspection.is_err());
            }
            let deployment = deployment.unwrap();
            assert!(deployment.removed.is_empty());
            assert_eq!(deployment.written.len(), skill_names().len());
            let expected = skill_files_for(engine, skill_names()[0]);
            let expected = expected
                .iter()
                .find(|file| file.relative == "SKILL.md")
                .unwrap();
            assert_eq!(std::fs::read_to_string(current).unwrap(), expected.contents);
            assert_eq!(
                std::fs::read_to_string(retired.join("SKILL.md")).unwrap(),
                "private project instructions"
            );
        }
    }

    #[test]
    fn failed_retirement_inspection_preserves_payloads_and_ownership() {
        use std::os::unix::fs::PermissionsExt;
        let temporary = tempfile::tempdir().unwrap();
        let installation = Installation::at(
            temporary.path().join("lore"),
            temporary.path().join("data"),
            temporary.path().join("config"),
            Some(temporary.path().join("home")),
            None,
        );
        installation.deploy_skills(SkillLevel::User).unwrap();
        let root = installation.skills_dir(SkillLevel::User).unwrap();
        let retired = root.join("lore-retired");
        std::fs::create_dir(&retired).unwrap();
        std::fs::write(retired.join("SKILL.md"), "old").unwrap();
        installation
            .record_deployed(
                SkillLevel::User,
                &[skill_names(), vec!["lore-retired"]].concat(),
            )
            .unwrap();
        let record = installation.config_dir.join(DEPLOYED_SKILLS_RECORD);
        let before = std::fs::read(&record).unwrap();
        let current = root.join(skill_names()[0]).join("SKILL.md");
        std::fs::write(&current, "previous").unwrap();
        std::fs::set_permissions(&retired, std::fs::Permissions::from_mode(0o000)).unwrap();
        let unavailable = std::fs::symlink_metadata(retired.join("SKILL.md")).is_err();
        let result = installation.retired_skills(SkillLevel::User);
        let deployment = if unavailable {
            Some(installation.deploy_skills(SkillLevel::User))
        } else {
            None
        };
        std::fs::set_permissions(&retired, std::fs::Permissions::from_mode(0o755)).unwrap();
        if unavailable {
            assert!(result.is_err());
            let failure = deployment.unwrap().unwrap_err();
            assert!(failure.done.written.is_empty() && failure.done.removed.is_empty());
        } else {
            assert!(result.unwrap().contains(&retired));
        }
        assert_eq!(std::fs::read(&record).unwrap(), before);
        assert_eq!(std::fs::read_to_string(current).unwrap(), "previous");
        assert!(retired.join("SKILL.md").exists());
    }

    #[test]
    fn a_dangling_derived_skill_stays_visible_but_empty_scopes_do_not_deploy() {
        let temporary = tempfile::tempdir().unwrap();
        let installation = Installation::at(
            temporary.path().join("lore"),
            temporary.path().join("data"),
            temporary.path().join("config"),
            Some(temporary.path().join("home")),
            None,
        );
        let root = installation.skills_dir(SkillLevel::User).unwrap();
        std::fs::create_dir_all(root.join("lore-day")).unwrap();
        assert!(installation.deployed_skill_levels().is_empty());
        std::os::unix::fs::symlink(temporary.path().join("missing"), root.join("lore-process"))
            .unwrap();
        assert_eq!(installation.deployed_skill_levels(), vec![SkillLevel::User]);
        let report =
            crate::InstallationReport::build(&installation, crate::SchemaState::Unconfigured);
        let artifact = report.skills[0]
            .1
            .artifacts
            .iter()
            .find(|a| a.name == "lore-process")
            .unwrap();
        assert_eq!(artifact.state, crate::ArtifactState::InvalidLayout);
        assert!(report.has_filesystem_issues());
    }
}
