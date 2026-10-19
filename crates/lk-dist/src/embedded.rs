//! Canonical skill/config bytes and host-specific entry metadata embedded in the binary.
//! Templates are embedded by lk-vault and reused here for deployment.

include!(concat!(env!("OUT_DIR"), "/embedded.rs"));

/// One file a deploy writes, addressed relative to the directory its group is deployed into.
#[derive(Debug, Clone, Copy)]
pub struct EmbeddedFile {
    pub relative: &'static str,
    pub contents: &'static str,
    /// Required Unix permission bits for the carried file.
    pub mode: u32,
}

const READ_ONLY: u32 = 0o644;

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
    skill_files_for(lk_core::execution::AgentEngine::Codex, name)
}

pub fn skill_files_for(engine: lk_core::execution::AgentEngine, name: &str) -> Vec<EmbeddedFile> {
    let prefix = format!("{name}/");
    let files = match engine {
        lk_core::execution::AgentEngine::ClaudeCode => CLAUDE_SKILL_FILES,
        lk_core::execution::AgentEngine::Codex => SKILL_FILES,
    };
    files
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

/// The starter and reference configs, addressed relative to the config directory.
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

    #[test]
    fn each_host_receives_its_own_invocation_metadata_and_the_same_procedure() {
        for name in skill_names() {
            let files = skill_files(name);
            let instructions = files
                .iter()
                .find(|file| file.relative == "SKILL.md")
                .unwrap();
            let page = lk_core::frontmatter::parse_page(instructions.contents).unwrap();
            let expected = name;
            assert_eq!(
                page.frontmatter
                    .get("name")
                    .and_then(|value| value.as_str()),
                Some(expected),
                "{name} must use a portable canonical name"
            );
            let host = skill_files_for(lk_core::execution::AgentEngine::ClaudeCode, name);
            let host = host
                .iter()
                .find(|file| file.relative == "SKILL.md")
                .unwrap();
            let host = lk_core::frontmatter::parse_page(host.contents).unwrap();
            assert_eq!(
                host.frontmatter.get("name").unwrap(),
                &format!("{name}:{name}")
            );
            assert_eq!(host.body, page.body);
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
}
