//! Pins the `version:` frontmatter of every shipped skill to the `lore` crate version.
//! The field is not part of the Agent Skills frontmatter spec — it exists as release
//! provenance for installed copies (an installed `~/.claude/skills/lore-*/SKILL.md`
//! says which release it came from without hashing it against a download). A stamp
//! that lags the release it ships in is worse than none, so the release version bump
//! fails here until every skill is restamped to match.

use std::path::PathBuf;

#[test]
fn every_skill_version_matches_the_crate_version() {
    let skills_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.claude/skills");
    let needle = format!("version: {}", env!("CARGO_PKG_VERSION"));

    let mut checked = 0;
    for entry in std::fs::read_dir(&skills_dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", skills_dir.display()))
    {
        let dir = entry.unwrap().path();
        if !dir.is_dir() {
            continue;
        }
        let path = dir.join("SKILL.md");
        let doc = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let frontmatter: Vec<&str> = doc
            .lines()
            .skip(1)
            .take_while(|line| *line != "---")
            .collect();
        assert!(
            frontmatter.iter().any(|line| line.trim() == needle),
            "{} frontmatter must carry `{needle}` \
             (release version bumped without restamping the skill?)",
            path.display()
        );
        checked += 1;
    }
    assert!(checked > 0, "no skills found under {}", skills_dir.display());
}
