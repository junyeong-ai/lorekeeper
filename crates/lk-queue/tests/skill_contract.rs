//! Pins the Rust ↔ `/lore-process` wire vocabulary. The skill documents every task
//! kind, target kind, and `llm_inputs` cache key in prose — the one cross-crate
//! agreement the compiler cannot check. These tests iterate the macro-generated
//! `EnumIter` kind space (which cannot drift from the enums) and require each
//! string to appear in the skill files — and, for the kind→key mapping, to appear
//! on the same table row — so renaming or adding a kind fails here until the skill
//! documentation is updated to match.

use std::path::PathBuf;

use lk_queue::{TargetKind, TaskKind};
use strum::IntoEnumIterator;

fn read_skill_file(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../.claude/skills/lore-process")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The kebab-case wire name `#[serde(rename_all = "kebab-case")]` produces — taken
/// from the serializer itself so the test can never drift from the real encoding.
fn wire_name(kind: TargetKind) -> String {
    serde_json::to_value(kind)
        .unwrap()
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn queue_format_documents_every_target_kind_wire_name() {
    let doc = read_skill_file("references/queue-format.md");
    for kind in TargetKind::iter() {
        let needle = format!("`{}`", wire_name(kind));
        assert!(
            doc.contains(&needle),
            "references/queue-format.md must list target.kind {needle} \
             (TargetKind::{kind:?} changed without updating the skill docs?)"
        );
    }
}

#[test]
fn queue_format_documents_every_task_kind_wire_name() {
    let doc = read_skill_file("references/queue-format.md");
    for kind in TaskKind::iter() {
        let needle = format!("`{}`", kind.as_str());
        assert!(
            doc.contains(&needle),
            "references/queue-format.md must list kind {needle} \
             (TaskKind::{kind:?} changed without updating the skill docs?)"
        );
    }
}

#[test]
fn skill_key_table_maps_every_target_kind_to_its_llm_inputs_key() {
    // Row-level check: the wire name and its key must co-occur on one line of the
    // SKILL.md kind→key table. A mere "key appears somewhere in the file" check
    // would pass even if a kind's mapping silently switched to a different,
    // already-documented key.
    let doc = read_skill_file("SKILL.md");
    for kind in TargetKind::iter() {
        let wire = format!("`{}`", wire_name(kind));
        let key = format!("`{}`", kind.llm_inputs_key());
        let mentions: Vec<&str> = doc.lines().filter(|line| line.contains(&wire)).collect();
        assert!(
            !mentions.is_empty(),
            "SKILL.md's kind→key table has no row for {wire} \
             (TargetKind::{kind:?} added/renamed without updating the skill?)"
        );
        assert!(
            mentions.iter().any(|line| line.contains(&key)),
            "SKILL.md maps {wire} to the wrong key — no line pairs it with {key}; \
             lines mentioning it: {mentions:?}"
        );
    }
}

#[test]
fn skill_documents_every_completion_marker() {
    let doc = read_skill_file("SKILL.md");
    for kind in TargetKind::iter() {
        let needle = format!("`{}`", kind.completion_key());
        assert!(
            doc.contains(&needle),
            "SKILL.md must document the completion marker {needle} \
             (completion_key for TargetKind::{kind:?} changed without updating the skill?)"
        );
    }
}
