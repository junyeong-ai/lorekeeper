//! Canonical skills declare their packaged release in metadata.version.
//! Host entry files inherit that provenance from the same source.

use std::path::PathBuf;

#[test]
fn every_skill_version_matches_the_crate_version() {
    let skills_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../skills");
    let version = env!("CARGO_PKG_VERSION");

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
        let page = lk_core::frontmatter::parse_page(&doc).unwrap();
        assert_eq!(
            page.frontmatter
                .get("metadata")
                .and_then(|value| value.get("version"))
                .and_then(|value| value.as_str()),
            Some(version),
            "{} version differs",
            path.display()
        );
        checked += 1;
    }
    assert!(
        checked > 0,
        "no skills found under {}",
        skills_dir.display()
    );
}
