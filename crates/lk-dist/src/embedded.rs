//! The files this binary carries and writes out.
//!
//! An agent skill describes the commands this binary exposes and a pipeline script composes
//! them into a scheduled run, so both are true only of the version they shipped with. Carrying
//! them here rather than publishing them beside the binary is what makes "the deployed skill
//! matches the binary" hold by construction instead of by remembering — the two cannot be
//! different versions when they are one file.
//!
//! `include_str!` reaches the repository through a generated manifest (`build.rs`) rather than
//! a list written here, because the set is a directory tree: a seventh skill added under
//! `.claude/skills` is carried without anything being remembered, and editing one rebuilds the
//! binary that carries it.

include!(concat!(env!("OUT_DIR"), "/embedded.rs"));

/// One file a deploy writes, addressed relative to the directory its group is deployed into.
#[derive(Debug, Clone, Copy)]
pub struct EmbeddedFile {
    pub relative: &'static str,
    pub contents: &'static str,
    /// Unix permissions. A pipeline is fired by a system scheduler, which executes it rather
    /// than reading it, so its bit is part of the artifact and not of the writing.
    pub mode: u32,
}

const READ_ONLY: u32 = 0o644;
const EXECUTABLE: u32 = 0o755;

/// Every skill name this binary carries, sorted.
pub fn skill_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = SKILL_FILES
        .iter()
        .filter_map(|(path, _)| path.split('/').next())
        .collect();
    names.dedup();
    names
}

/// The files of one skill, addressed relative to that skill's own directory.
pub fn skill_files(name: &str) -> Vec<EmbeddedFile> {
    let prefix = format!("{name}/");
    SKILL_FILES
        .iter()
        .filter_map(|(path, contents)| {
            path.strip_prefix(&prefix).map(|relative| EmbeddedFile {
                relative,
                contents,
                mode: READ_ONLY,
            })
        })
        .collect()
}

/// The scheduled pipeline scripts, addressed relative to the pipelines directory.
pub fn pipeline_files() -> Vec<EmbeddedFile> {
    PIPELINE_FILES
        .iter()
        .map(|(relative, contents)| EmbeddedFile {
            relative,
            contents,
            mode: EXECUTABLE,
        })
        .collect()
}

/// The rendering templates, addressed relative to the templates directory.
///
/// Read from `lk-vault`, which embeds them to render with: the copy on disk exists so a
/// `--template-dir` run has one to start from, and two embeddings of one set is the drift this
/// module exists to remove.
pub fn template_files() -> Vec<EmbeddedFile> {
    lk_vault::embedded_templates()
        .iter()
        .map(|(relative, contents)| EmbeddedFile {
            relative,
            contents,
            mode: READ_ONLY,
        })
        .collect()
}

/// The config example, addressed relative to the config directory.
///
/// A starting point rather than a setting: the file a user edits is `config.yaml` beside it,
/// which nothing here writes. Carried for the same reason as the rest — an example naming keys
/// a release ago is read as current by whoever copies it.
pub fn config_files() -> Vec<EmbeddedFile> {
    CONFIG_FILES
        .iter()
        .map(|(relative, contents)| EmbeddedFile {
            relative,
            contents,
            mode: READ_ONLY,
        })
        .collect()
}

/// Pipeline file names, in the order a deploy writes them.
pub fn pipeline_names() -> Vec<&'static str> {
    PIPELINE_FILES.iter().map(|(name, _)| *name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_skill_carries_its_own_instructions() {
        let names = skill_names();
        assert!(!names.is_empty(), "no skills are embedded");
        for name in names {
            let files = skill_files(name);
            assert!(
                files.iter().any(|f| f.relative == "SKILL.md"),
                "skill `{name}` carries no SKILL.md, so deploying it would install a directory \
                 no agent can load"
            );
        }
    }

    /// The generated manifest addresses a skill's files by a path whose first segment is the
    /// skill. A file directly under `.claude/skills` would have no such segment and would be
    /// silently deployed as a skill named after itself.
    #[test]
    fn every_embedded_skill_file_is_inside_a_skill() {
        for (path, _) in SKILL_FILES {
            assert!(
                path.contains('/'),
                "`{path}` sits directly under .claude/skills, which names no skill"
            );
        }
    }

    #[test]
    fn the_pipelines_are_executable_and_the_rest_is_not() {
        assert!(pipeline_files().iter().all(|f| f.mode == EXECUTABLE));
        assert!(
            skill_files(skill_names()[0])
                .iter()
                .all(|f| f.mode == READ_ONLY)
        );
    }

    /// The shared library is sourced by both entry scripts; publishing the entries without it
    /// installs two scripts that abort on their first line under a scheduler nobody is watching.
    #[test]
    fn the_pipeline_entries_ship_with_the_library_they_source() {
        let names = pipeline_names();
        assert!(names.contains(&"lore-pipeline.sh"), "{names:?}");
        for entry in ["lore-daily.sh", "lore-weekly.sh"] {
            assert!(names.contains(&entry), "{names:?}");
        }
    }
}
