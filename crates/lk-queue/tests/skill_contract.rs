//! Pins the Rust ↔ `/lore-process` wire vocabulary. The skill documents every task
//! kind, target kind, and `llm_inputs` cache key in prose — the one cross-crate
//! agreement the compiler cannot check. These tests iterate the macro-generated
//! `EnumIter` kind space (which cannot drift from the enums) and require each
//! string to appear in the skill files — and, for the kind→key mapping, to appear
//! on the same table row — so renaming or adding a kind fails here until the skill
//! documentation is updated to match.

use std::path::PathBuf;

use lk_core::config::SourceType;
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

/// `queue-format.md` lists the kinds; `processing-kinds.md` is where each one's generation spec
/// lives, under a `## \`kind: …\`` heading the drain looks for. Listing a kind and specifying it
/// are different obligations, and only the first was checked — misspelling the `refine-events`
/// heading left a real task kind with no instructions, silently, since the drain then has nothing
/// to follow for it and every other kind still works.
#[test]
fn processing_kinds_specifies_every_task_kind() {
    let doc = read_skill_file("references/processing-kinds.md");
    for kind in TaskKind::iter() {
        let heading = format!("## `kind: {}`", kind.as_str());
        assert!(
            doc.contains(&heading),
            "references/processing-kinds.md has no `{heading}` section, so a drain receiving \
             TaskKind::{kind:?} has no generation spec to follow"
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

/// A task carries `input.source_type` and the skill is told to key its synthesis and
/// extraction strategy on it, so a source type with no entry leaves the skill guessing on
/// every page that source ever writes. `confluence` shipped exactly that way — added as a
/// source type in v0.11.0 and absent from both sections of the reference until a drain run
/// went looking for it.
#[test]
fn source_types_documents_every_source_type_in_both_sections() {
    let doc = read_skill_file("references/source-types.md");
    let (summarize, extract) = doc
        .split_once("## Extract-concepts")
        .expect("references/source-types.md must keep its two per-type sections");
    for source in SourceType::iter() {
        let needle = format!(
            "`{}`",
            serde_json::to_value(source).unwrap().as_str().unwrap()
        );
        for (section, name) in [(summarize, "Summarize"), (extract, "Extract-concepts")] {
            assert!(
                section.contains(&needle),
                "references/source-types.md § {name} must give a strategy for {needle} \
                 (SourceType::{source:?} added without updating the skill docs?)"
            );
        }
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

/// The result protocol is the other half of the wire agreement, and the half a compiler
/// cannot see at all: the drain writes JSON that only `lore queue apply` reads. Pinning the
/// field names and the path here means a rename in `TaskResult` fails until the skill that
/// produces it is updated — the drift that would otherwise surface as an apply step that
/// silently finds nothing.
#[test]
fn skill_documents_the_result_protocol_it_must_produce() {
    let processing = read_skill_file("references/processing-kinds.md");
    let expected_path = format!("{}/{{task_id}}.json", lk_queue::RESULTS_SUBDIR);
    assert!(
        processing.contains(&expected_path),
        "processing-kinds.md must name the result path `{expected_path}`"
    );

    // Field names as serde writes them, taken from a real value so the test cannot drift.
    let sample = lk_queue::TaskResult {
        task_id: "t".into(),
        cache_hash: "h".into(),
        target: lk_queue::TaskTarget {
            vault_path: "p".into(),
            kind: TargetKind::DailyConcepts,
            anchor: "## a".into(),
        },
        date: jiff::civil::date(2026, 1, 1),
        concepts: vec![],
    };
    let serde_json::Value::Object(fields) = serde_json::to_value(&sample).unwrap() else {
        panic!("TaskResult must serialize as an object");
    };
    for field in fields.keys() {
        assert!(
            processing.contains(&format!("\"{field}\"")),
            "processing-kinds.md must document the `{field}` result field"
        );
    }
}

/// "the concept kinds" names two kinds where three exist, and the drain reads it as covering
/// `synthesize-concept` — whose section and marker are its own. The phrase was removed from
/// three safety rules and survived in a fourth sentence, invisible to a grep because the line
/// wrapped between the two words. A prohibition that a search cannot find is one that comes
/// back, so it is a test: matching across whitespace is what the greps could not do.
#[test]
fn no_skill_file_calls_a_set_of_kinds_the_concept_kinds() {
    for name in [
        "SKILL.md",
        "references/processing-kinds.md",
        "references/queue-format.md",
    ] {
        let doc = read_skill_file(name);
        // Over the WORDS with punctuation and markup peeled off each, not the bare split:
        // measured against the file this phrase was removed from, splitting on whitespace
        // alone found ONE of its three occurrences — `concept kinds**,` and `concept\n
        // kinds,` each carry a trailing character, and a guard a comma defeats is the grep
        // it was written to replace.
        let words: Vec<String> = doc
            .split_whitespace()
            .map(|w| {
                w.trim_matches(|c: char| !c.is_alphanumeric())
                    .to_ascii_lowercase()
            })
            .collect();
        assert!(
            !words.windows(2).any(|w| w == ["concept", "kinds"]),
            "{name} says \"concept kinds\": name `extract-concepts`, or say which kinds are \
             meant — `synthesize-concept` writes a concept page's sections and stamps its own \
             marker, so the phrase reads as a rule against the work the drain is handed"
        );
    }
}

/// The whole point of routing EXTRACTED concepts through `queue apply` is that ONE
/// implementation merges them, and a skill that created concept pages would restore the
/// second one. What the rule may not do is forbid concept pages wholesale: a
/// `synthesize-concept` task writes an existing page's own two sections, so a rule stated
/// that broadly reads as forbidding the work the drain is handed, and an agent obeying it
/// stamps `synthesis_done` over a relations section nothing will ever write. The pair is
/// pinned together for that reason — narrowing the prohibition without naming the exemption
/// leaves the same contradiction one edit away.
#[test]
fn skill_forbids_creating_concept_pages_and_names_the_kind_that_writes_one() {
    let skill = read_skill_file("SKILL.md");
    assert!(
        skill.contains("Never create or merge a concept page"),
        "SKILL.md must forbid the drain from creating or merging concept pages"
    );
    assert!(
        skill.contains("queue apply"),
        "SKILL.md must point at `lore queue apply` as the materializer"
    );
    assert!(
        skill.contains("synthesize-concept"),
        "SKILL.md must name the kind that does write a concept page's own sections"
    );
}
