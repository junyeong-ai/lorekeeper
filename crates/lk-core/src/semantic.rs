//! Exact text coverage for bounded semantic execution; coverage is not a truth verdict.

use std::collections::BTreeSet;
use std::io::Write;

use pulldown_cmark::{Event, Options, Parser};
use serde::{Deserialize, Serialize};

pub const SEMANTIC_POLICY: &str = "semantic-ranges-v1";
const DIGEST_WIDTH: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SemanticAuthority {
    Subject,
    Context,
    RetainedInput,
    Page,
    RssEvent,
    Review,
    Archive,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticResource {
    pub address: String,
    pub revision: String,
    pub authority: SemanticAuthority,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticResourceGuard {
    pub address: String,
    pub revision: String,
    pub authority: SemanticAuthority,
    pub text_digest: String,
    pub characters: usize,
    pub bytes: usize,
}

/// Half-open offsets in Unicode scalar values, never UTF-8 bytes or graphemes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticRange {
    pub resource: usize,
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticPart {
    pub id: String,
    pub ranges: Vec<SemanticRange>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticRangeInput {
    pub range: SemanticRange,
    pub address: String,
    pub revision: String,
    pub authority: SemanticAuthority,
    pub text_digest: String,
    pub total_characters: usize,
    /// A range starts or ends inside a Markdown block; it is not a whole-block read.
    pub continued: bool,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticPartInput {
    pub policy: String,
    pub plan_id: String,
    pub part_id: String,
    pub subject_revision: String,
    pub max_result_bytes: usize,
    pub ranges: Vec<SemanticRangeInput>,
    pub input_revision: String,
}

/// Derived execution metadata. Source content stays with the caller's guarded resources.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticPlan {
    pub id: String,
    pub policy: String,
    pub subject_revision: String,
    pub max_input_bytes: usize,
    pub max_result_bytes: usize,
    pub resources: Vec<SemanticResourceGuard>,
    pub parts: Vec<SemanticPart>,
}

/// One verified view of exact resources for reconstructing several bounded inputs.
pub struct SemanticInputs<'a> {
    plan: &'a SemanticPlan,
    texts: Vec<TextIndex<'a>>,
}

impl SemanticInputs<'_> {
    pub fn part_input(&self, part_id: &str) -> Result<SemanticPartInput> {
        let part = self
            .plan
            .parts
            .iter()
            .find(|part| part.id == part_id)
            .ok_or_else(|| error("unknown part id"))?;
        self.context().input(part)
    }

    fn context(&self) -> InputContext<'_> {
        InputContext {
            plan_id: &self.plan.id,
            subject_revision: &self.plan.subject_revision,
            max_input_bytes: self.plan.max_input_bytes,
            max_result_bytes: self.plan.max_result_bytes,
            resources: &self.plan.resources,
            texts: &self.texts,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("semantic evidence plan: {0}")]
pub struct SemanticError(String);

type Result<T> = std::result::Result<T, SemanticError>;

fn error(message: &str) -> SemanticError {
    SemanticError(message.into())
}

fn digest(value: &impl Serialize) -> Result<String> {
    serde_json::to_vec(value)
        .map(|bytes| blake3::hash(&bytes).to_hex().to_string())
        .map_err(|_| error("cannot serialize evidence metadata"))
}

#[derive(Default)]
struct JsonBytes(usize);

impl Write for JsonBytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .ok_or_else(|| std::io::Error::other("serialized evidence size overflow"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn json_bytes(value: &impl Serialize) -> Result<usize> {
    let mut bytes = JsonBytes::default();
    serde_json::to_writer(&mut bytes, value)
        .map_err(|_| error("cannot serialize evidence metadata"))?;
    Ok(bytes.0)
}

struct TextIndex<'a> {
    resource: &'a SemanticResource,
    offsets: Vec<usize>,
    boundaries: Vec<usize>,
    escaping: Vec<(usize, usize)>,
}

impl<'a> TextIndex<'a> {
    fn build(resource: &'a SemanticResource) -> Result<Self> {
        let mut offsets = Vec::new();
        let mut escaping = Vec::new();
        let mut extra = 0;
        for (index, (offset, character)) in resource.text.char_indices().enumerate() {
            offsets.push(offset);
            let overhead = json_bytes(&character)? - 2 - character.len_utf8();
            if overhead != 0 {
                extra += overhead;
                escaping.push((index + 1, extra));
            }
        }
        offsets.push(resource.text.len());
        let mut ends = BTreeSet::from([0, resource.text.len()]);
        let mut depth = 0usize;
        for (event, range) in
            Parser::new_ext(&resource.text, Options::ENABLE_TABLES).into_offset_iter()
        {
            match event {
                Event::Start(_) => depth += 1,
                Event::End(_) => {
                    depth -= 1;
                    if depth == 0 {
                        ends.insert(range.end);
                    }
                }
                Event::Rule if depth == 0 => {
                    ends.insert(range.end);
                }
                _ => {}
            }
        }
        let boundaries = ends
            .into_iter()
            .filter_map(|offset| offsets.binary_search(&offset).ok())
            .collect();
        Ok(Self {
            resource,
            offsets,
            boundaries,
            escaping,
        })
    }

    fn text(&self, range: &SemanticRange) -> &str {
        &self.resource.text[self.offsets[range.start]..self.offsets[range.end]]
    }

    fn continued(&self, range: &SemanticRange) -> bool {
        self.boundaries.binary_search(&range.start).is_err()
            || self.boundaries.binary_search(&range.end).is_err()
    }

    fn serialized_text_bytes(&self, range: &SemanticRange) -> usize {
        let extra = |offset| {
            let index = self.escaping.partition_point(|(end, _)| *end <= offset);
            index
                .checked_sub(1)
                .map_or(0, |index| self.escaping[index].1)
        };
        self.text(range).len() + extra(range.end) - extra(range.start)
    }
}

struct InputContext<'a> {
    plan_id: &'a str,
    subject_revision: &'a str,
    max_input_bytes: usize,
    max_result_bytes: usize,
    resources: &'a [SemanticResourceGuard],
    texts: &'a [TextIndex<'a>],
}

impl InputContext<'_> {
    fn range_input(&self, range: &SemanticRange, text: String) -> SemanticRangeInput {
        let guard = &self.resources[range.resource];
        SemanticRangeInput {
            range: range.clone(),
            address: guard.address.clone(),
            revision: guard.revision.clone(),
            authority: guard.authority,
            text_digest: guard.text_digest.clone(),
            total_characters: guard.characters,
            continued: self.texts[range.resource].continued(range),
            text,
        }
    }

    fn input(&self, part: &SemanticPart) -> Result<SemanticPartInput> {
        let ranges = part
            .ranges
            .iter()
            .map(|range| self.range_input(range, self.texts[range.resource].text(range).into()))
            .collect::<Vec<_>>();
        let input_revision = digest(&(
            SEMANTIC_POLICY,
            self.plan_id,
            &part.id,
            self.subject_revision,
            self.max_result_bytes,
            &ranges,
        ))?;
        Ok(SemanticPartInput {
            policy: SEMANTIC_POLICY.into(),
            plan_id: self.plan_id.into(),
            part_id: part.id.clone(),
            subject_revision: self.subject_revision.into(),
            max_result_bytes: self.max_result_bytes,
            ranges,
            input_revision,
        })
    }

    fn fits(&self, ranges: &[SemanticRange]) -> Result<bool> {
        let raw_bytes = ranges.iter().try_fold(0usize, |total, range| {
            total.checked_add(self.texts[range.resource].text(range).len())
        });
        if raw_bytes.is_none_or(|bytes| bytes > self.max_input_bytes) {
            return Ok(false);
        }
        Ok(self.input_bytes(ranges)? <= self.max_input_bytes)
    }

    fn input_bytes(&self, ranges: &[SemanticRange]) -> Result<usize> {
        let placeholder = "0".repeat(DIGEST_WIDTH);
        let input = SemanticPartInput {
            policy: SEMANTIC_POLICY.into(),
            plan_id: self.plan_id.into(),
            part_id: placeholder.clone(),
            subject_revision: self.subject_revision.into(),
            max_result_bytes: self.max_result_bytes,
            ranges: Vec::new(),
            input_revision: placeholder,
        };
        let mut bytes = json_bytes(&input)?;
        for (index, range) in ranges.iter().enumerate() {
            let text = &self.texts[range.resource];
            let metadata = self.range_input(range, String::new());
            bytes = bytes
                .checked_add(json_bytes(&metadata)?)
                .and_then(|bytes| bytes.checked_add(text.serialized_text_bytes(range)))
                .and_then(|bytes| bytes.checked_add(usize::from(index != 0)))
                .ok_or_else(|| error("serialized evidence size overflow"))?;
        }
        Ok(bytes)
    }
}

fn guards(resources: &[SemanticResource]) -> Result<Vec<SemanticResourceGuard>> {
    if resources.is_empty() {
        return Err(error("at least one resource is required"));
    }
    let mut keys = BTreeSet::new();
    resources
        .iter()
        .map(|resource| {
            if resource.address.trim().is_empty() || resource.revision.trim().is_empty() {
                return Err(error("resource address and revision must be nonblank"));
            }
            if !keys.insert((&resource.address, &resource.revision, resource.authority)) {
                return Err(error("duplicate resource identity"));
            }
            Ok(SemanticResourceGuard {
                address: resource.address.clone(),
                revision: resource.revision.clone(),
                authority: resource.authority,
                text_digest: blake3::hash(resource.text.as_bytes()).to_hex().to_string(),
                characters: resource.text.chars().count(),
                bytes: resource.text.len(),
            })
        })
        .collect()
}

fn appended(ranges: &[SemanticRange], next: SemanticRange) -> Vec<SemanticRange> {
    let mut ranges = ranges.to_vec();
    if let Some(last) = ranges.last_mut()
        && last.resource == next.resource
        && last.end == next.start
    {
        last.end = next.end;
    } else {
        ranges.push(next);
    }
    ranges
}

fn flush(parts: &mut Vec<SemanticPart>, ranges: &mut Vec<SemanticRange>) {
    if !ranges.is_empty() {
        parts.push(SemanticPart {
            id: String::new(),
            ranges: std::mem::take(ranges),
        });
    }
}

impl SemanticPlan {
    pub fn build(
        subject_revision: &str,
        resources: &[SemanticResource],
        max_input_bytes: usize,
        max_result_bytes: usize,
    ) -> Result<Self> {
        if subject_revision.trim().is_empty() || max_input_bytes == 0 || max_result_bytes == 0 {
            return Err(error(
                "subject revision and positive input/result bounds are required",
            ));
        }
        let resource_guards = guards(resources)?;
        let texts = resources
            .iter()
            .map(TextIndex::build)
            .collect::<Result<Vec<_>>>()?;
        let placeholder = "0".repeat(DIGEST_WIDTH);
        let context = InputContext {
            plan_id: &placeholder,
            subject_revision,
            max_input_bytes,
            max_result_bytes,
            resources: &resource_guards,
            texts: &texts,
        };
        let mut parts = Vec::new();
        let mut active = Vec::new();
        for (resource, text) in texts.iter().enumerate() {
            let units: Vec<_> = if text.resource.text.is_empty() {
                vec![(0, 0)]
            } else {
                text.boundaries
                    .windows(2)
                    .map(|pair| (pair[0], pair[1]))
                    .collect()
            };
            for (mut start, end) in units {
                let range = SemanticRange {
                    resource,
                    start,
                    end,
                };
                let candidate = appended(&active, range.clone());
                if context.fits(&candidate)? {
                    active = candidate;
                    continue;
                }
                flush(&mut parts, &mut active);
                if context.fits(std::slice::from_ref(&range))? {
                    active.push(range);
                    continue;
                }
                while start < end {
                    let mut low = start;
                    let mut high = end;
                    while low < high {
                        let middle = low + (high - low).div_ceil(2);
                        let range = SemanticRange {
                            resource,
                            start,
                            end: middle,
                        };
                        if context.fits(std::slice::from_ref(&range))? {
                            low = middle;
                        } else {
                            high = middle - 1;
                        }
                    }
                    if low == start {
                        return Err(error(
                            "input bound cannot hold resource metadata and one character",
                        ));
                    }
                    active.push(SemanticRange {
                        resource,
                        start,
                        end: low,
                    });
                    start = low;
                    if start < end {
                        flush(&mut parts, &mut active);
                    }
                }
                if end == 0 {
                    return Err(error("input bound cannot hold empty resource metadata"));
                }
            }
        }
        flush(&mut parts, &mut active);
        let mut plan = Self {
            id: String::new(),
            policy: SEMANTIC_POLICY.into(),
            subject_revision: subject_revision.into(),
            max_input_bytes,
            max_result_bytes,
            resources: resource_guards,
            parts,
        };
        plan.id = plan.identity()?;
        for part in &mut plan.parts {
            part.id = part_identity(&plan.resources, &part.ranges)?;
        }
        Ok(plan)
    }

    fn identity(&self) -> Result<String> {
        digest(&(
            &self.policy,
            &self.subject_revision,
            self.max_input_bytes,
            self.max_result_bytes,
            &self.resources,
            self.parts
                .iter()
                .map(|part| &part.ranges)
                .collect::<Vec<_>>(),
        ))
    }

    /// Binds local evidence to a caller-defined semantic purpose, not a prior judgment.
    pub fn part_content_revision(&self, part_id: &str, purpose_key: &str) -> Result<String> {
        self.verify_structure()?;
        if purpose_key.trim().is_empty() {
            return Err(error("semantic purpose key must be nonblank"));
        }
        let part = self
            .parts
            .iter()
            .find(|part| part.id == part_id)
            .ok_or_else(|| error("unknown part id"))?;
        let identity = part_identity(&self.resources, &part.ranges)?;
        if identity != part.id {
            return Err(error("part identity changed"));
        }
        digest(&(SEMANTIC_POLICY, purpose_key, identity))
    }

    fn verify_structure(&self) -> Result<()> {
        if self.policy != SEMANTIC_POLICY
            || self.subject_revision.trim().is_empty()
            || self.max_input_bytes == 0
            || self.max_result_bytes == 0
            || self.resources.is_empty()
            || self.id != self.identity()?
            || self.parts.is_empty()
        {
            return Err(error("plan metadata or exact resource guards changed"));
        }
        let mut cursor = vec![0; self.resources.len()];
        let mut counts = vec![0usize; self.resources.len()];
        let mut previous = 0;
        for part in &self.parts {
            if part.id != part_identity(&self.resources, &part.ranges)? || part.ranges.is_empty() {
                return Err(error("part identity or ranges changed"));
            }
            for range in &part.ranges {
                let Some(guard) = self.resources.get(range.resource) else {
                    return Err(error("range names an unknown resource"));
                };
                if range.resource < previous
                    || range.start != cursor[range.resource]
                    || range.end < range.start
                    || range.end > guard.characters
                    || (range.start == range.end && guard.characters != 0)
                {
                    return Err(error(
                        "resource ranges overlap, have gaps or exceed the text",
                    ));
                }
                previous = range.resource;
                cursor[range.resource] = range.end;
                counts[range.resource] += 1;
            }
        }
        if self.resources.iter().enumerate().any(|(index, guard)| {
            counts[index] == 0
                || cursor[index] != guard.characters
                || (guard.characters == 0 && counts[index] != 1)
        }) {
            return Err(error("resource coverage is incomplete"));
        }
        Ok(())
    }

    pub fn verify_resources(&self, resources: &[SemanticResource]) -> Result<()> {
        self.build_inputs(resources).map(|_| ())
    }

    pub fn build_inputs<'a>(
        &'a self,
        resources: &'a [SemanticResource],
    ) -> Result<SemanticInputs<'a>> {
        self.verify_structure()?;
        if self.resources != guards(resources)? {
            return Err(error("exact resource guards changed"));
        }
        let inputs = SemanticInputs {
            plan: self,
            texts: resources
                .iter()
                .map(TextIndex::build)
                .collect::<Result<Vec<_>>>()?,
        };
        for part in &self.parts {
            if !inputs.context().fits(&part.ranges)? {
                return Err(error("serialized part input exceeds its bound"));
            }
        }
        Ok(inputs)
    }

    /// Reconstruct one prior leaf from its exact assigned resources, not a whole-plan read.
    pub fn part_input_for_assigned_resources(
        &self,
        part_id: &str,
        resources: &[SemanticResource],
    ) -> Result<SemanticPartInput> {
        self.verify_structure()?;
        let part = self
            .parts
            .iter()
            .find(|part| part.id == part_id)
            .ok_or_else(|| error("unknown part id"))?;
        let supplied = guards(resources)?;
        let assigned: BTreeSet<_> = part.ranges.iter().map(|range| range.resource).collect();
        let positions = self
            .resources
            .iter()
            .enumerate()
            .map(|(index, expected)| {
                assigned
                    .contains(&index)
                    .then(|| supplied.iter().position(|actual| actual == expected))
                    .flatten()
            })
            .collect::<Vec<_>>();
        if part
            .ranges
            .iter()
            .any(|range| positions[range.resource].is_none())
        {
            return Err(error("an assigned resource is missing or changed"));
        }
        let empty = SemanticResource {
            address: String::new(),
            revision: String::new(),
            authority: SemanticAuthority::RetainedInput,
            text: String::new(),
        };
        let texts = positions
            .iter()
            .map(|position| TextIndex::build(position.map_or(&empty, |index| &resources[index])))
            .collect::<Result<Vec<_>>>()?;
        let input = InputContext {
            plan_id: &self.id,
            subject_revision: &self.subject_revision,
            max_input_bytes: self.max_input_bytes,
            max_result_bytes: self.max_result_bytes,
            resources: &self.resources,
            texts: &texts,
        }
        .input(part)?;
        if serde_json::to_vec(&input)
            .map_err(|_| error("cannot serialize part input"))?
            .len()
            > self.max_input_bytes
        {
            return Err(error("serialized part input exceeds its bound"));
        }
        Ok(input)
    }

    pub fn part_input(
        &self,
        part_id: &str,
        resources: &[SemanticResource],
    ) -> Result<SemanticPartInput> {
        self.build_inputs(resources)?.part_input(part_id)
    }
}

fn part_identity(resources: &[SemanticResourceGuard], ranges: &[SemanticRange]) -> Result<String> {
    let assigned = ranges
        .iter()
        .map(|range| {
            let resource = resources
                .get(range.resource)
                .ok_or_else(|| error("range names an unknown resource"))?;
            Ok((resource, range.start, range.end))
        })
        .collect::<Result<Vec<_>>>()?;
    digest(&(SEMANTIC_POLICY, assigned))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resource(address: &str, text: &str) -> SemanticResource {
        SemanticResource {
            address: address.into(),
            revision: "authored-revision".into(),
            authority: SemanticAuthority::Page,
            text: text.into(),
        }
    }

    fn reconstruct(plan: &SemanticPlan, resources: &[SemanticResource]) {
        let mut reconstructed = vec![String::new(); resources.len()];
        for part in &plan.parts {
            let input = plan.part_input(&part.id, resources).unwrap();
            assert!(serde_json::to_vec(&input).unwrap().len() <= plan.max_input_bytes);
            for range in input.ranges {
                reconstructed[range.range.resource].push_str(&range.text);
            }
        }
        assert_eq!(
            reconstructed,
            resources
                .iter()
                .map(|resource| resource.text.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn complete_ranges_reconstruct_unicode_and_empty_resources() {
        let resources = vec![
            resource("first", "가🙂e\u{301}\n\n".repeat(150).as_str()),
            resource("empty", ""),
            resource("last", "끝"),
        ];
        let plan = SemanticPlan::build("subject", &resources, 950, 300).unwrap();
        assert!(plan.parts.len() > 1);
        reconstruct(&plan, &resources);
        let encoded = serde_json::to_string(&plan).unwrap();
        assert!(!encoded.contains(&resources[0].text));
        let restored: SemanticPlan = serde_json::from_str(&encoded).unwrap();
        restored.verify_resources(&resources).unwrap();
    }

    #[test]
    fn giant_single_block_is_explicitly_continued_without_truncation() {
        let resources = vec![resource("long", &"한🙂".repeat(4000))];
        let plan = SemanticPlan::build("subject", &resources, 1200, 300).unwrap();
        assert!(plan.parts.len() > 2);
        assert!(
            plan.parts
                .iter()
                .all(|part| plan.part_input(&part.id, &resources).unwrap().ranges[0].continued)
        );
        reconstruct(&plan, &resources);
    }

    #[test]
    fn markdown_table_and_fence_are_whole_when_their_envelopes_fit() {
        let text = format!(
            "{}\n\n| A | B |\n|---|---|\n| 1 | 2 |\n\n```rust\n{}\n```\n\n{}\n",
            "before ".repeat(70),
            "x".repeat(150),
            "after ".repeat(70)
        );
        let resources = vec![resource("markdown", &text)];
        let plan = SemanticPlan::build("subject", &resources, 1100, 200).unwrap();
        let inputs = plan
            .parts
            .iter()
            .map(|part| plan.part_input(&part.id, &resources).unwrap())
            .collect::<Vec<_>>();
        assert!(
            inputs
                .iter()
                .flat_map(|input| &input.ranges)
                .any(|range| range.text.contains("| A | B |\n|---|---|\n| 1 | 2 |"))
        );
        assert!(inputs.iter().flat_map(|input| &input.ranges).any(|range| {
            range
                .text
                .contains(&format!("```rust\n{}\n```", "x".repeat(150)))
        }));
        assert!(
            inputs
                .iter()
                .flat_map(|input| &input.ranges)
                .all(|range| !range.continued)
        );
        reconstruct(&plan, &resources);
    }

    #[test]
    fn serialized_escaping_and_metadata_are_in_the_input_bound() {
        let resources = vec![resource(&"주소\"\\".repeat(20), &"\"\\\n\t".repeat(300))];
        let plan = SemanticPlan::build("subject\\\"", &resources, 1400, 100).unwrap();
        reconstruct(&plan, &resources);
        assert!(SemanticPlan::build("subject", &resources, 200, 100).is_err());
        assert!(SemanticPlan::build("subject", &[resource("empty", "")], 1, 100).is_err());
    }

    #[test]
    fn indexed_input_sizes_match_json_at_scalar_boundaries_and_exact_limits() {
        let text: String = (0..=127)
            .filter_map(char::from_u32)
            .chain("가🙂e\u{301}\u{2028}\u{2029}\u{10ffff}".chars())
            .collect();
        let resources = vec![resource("주소\"\\", &text), resource("empty", "")];
        let resource_guards = guards(&resources).unwrap();
        let texts = resources
            .iter()
            .map(TextIndex::build)
            .collect::<Result<Vec<_>>>()
            .unwrap();
        let mut context = InputContext {
            plan_id: &"0".repeat(DIGEST_WIDTH),
            subject_revision: "revision\"\\\n🙂",
            max_input_bytes: usize::MAX,
            max_result_bytes: 1000,
            resources: &resource_guards,
            texts: &texts,
        };
        for start in 0..=resource_guards[0].characters {
            for end in start..=resource_guards[0].characters {
                let ranges = vec![
                    SemanticRange {
                        resource: 0,
                        start,
                        end,
                    },
                    SemanticRange {
                        resource: 1,
                        start: 0,
                        end: 0,
                    },
                ];
                let input = context
                    .input(&SemanticPart {
                        id: "a".repeat(DIGEST_WIDTH),
                        ranges: ranges.clone(),
                    })
                    .unwrap();
                let actual = serde_json::to_vec(&input).unwrap().len();
                assert_eq!(context.input_bytes(&ranges).unwrap(), actual);
                context.max_input_bytes = actual;
                assert!(context.fits(&ranges).unwrap());
                context.max_input_bytes = actual - 1;
                assert!(!context.fits(&ranges).unwrap());
            }
        }
    }

    #[test]
    fn prepared_inputs_reject_changed_sources_incomplete_coverage_and_oversized_parts() {
        let resources = vec![resource("one", &"한🙂\"\\\n".repeat(100))];
        let plan = SemanticPlan::build("subject", &resources, 1400, 100).unwrap();
        let inputs = plan.build_inputs(&resources).unwrap();
        for part in &plan.parts {
            assert_eq!(
                inputs.part_input(&part.id).unwrap(),
                plan.part_input_for_assigned_resources(&part.id, &resources)
                    .unwrap()
            );
        }
        let mut changed = resources.clone();
        changed[0].text.replace_range(0.."한".len(), "글");
        assert!(plan.build_inputs(&changed).is_err());
        let mut incomplete = plan.clone();
        incomplete.parts.pop();
        incomplete.id = incomplete.identity().unwrap();
        assert!(incomplete.build_inputs(&resources).is_err());
        let mut oversized = plan;
        oversized.max_input_bytes = 100;
        oversized.id = oversized.identity().unwrap();
        assert!(oversized.build_inputs(&resources).is_err());
    }

    #[test]
    fn exact_guards_policy_ranges_and_bounds_are_bound_to_the_plan() {
        let resources = vec![resource("one", "first\n\nsecond")];
        let plan = SemanticPlan::build("subject", &resources, 1200, 100).unwrap();
        assert_eq!(
            plan,
            SemanticPlan::build("subject", &resources, 1200, 100).unwrap()
        );
        for changed in [
            SemanticPlan::build("other", &resources, 1200, 100).unwrap(),
            SemanticPlan::build("subject", &resources, 1201, 100).unwrap(),
            SemanticPlan::build("subject", &resources, 1200, 101).unwrap(),
        ] {
            assert_ne!(plan.id, changed.id);
        }
        let mut changed = resources.clone();
        changed[0].text.push(' ');
        assert!(plan.verify_resources(&changed).is_err());
        changed = resources.clone();
        changed[0].revision.push('x');
        assert!(plan.verify_resources(&changed).is_err());
        let mut corrupt = plan.clone();
        corrupt.parts[0].ranges[0].start = 1;
        assert!(corrupt.verify_resources(&resources).is_err());
        assert!(SemanticPlan::build("subject", &resources, 0, 100).is_err());
        assert!(SemanticPlan::build("subject", &resources, 1000, 0).is_err());
        assert!(plan.part_input("unknown", &resources).is_err());
    }

    #[test]
    fn local_content_identity_survives_unrelated_subject_changes_only_under_same_purpose() {
        let resources = vec![resource("one", "one paragraph")];
        let before = SemanticPlan::build("old-input", &resources, 1200, 100).unwrap();
        let after = SemanticPlan::build("new-input", &resources, 1200, 100).unwrap();
        assert_ne!(before.id, after.id);
        assert_eq!(before.parts[0].id, after.parts[0].id);
        assert_eq!(
            before
                .part_content_revision(&before.parts[0].id, "summary:focus:a")
                .unwrap(),
            after
                .part_content_revision(&after.parts[0].id, "summary:focus:a")
                .unwrap()
        );
        assert_ne!(
            before
                .part_content_revision(&before.parts[0].id, "summary:focus:a")
                .unwrap(),
            before
                .part_content_revision(&before.parts[0].id, "extraction:focus:a")
                .unwrap()
        );
        assert_ne!(
            before
                .part_input(&before.parts[0].id, &resources)
                .unwrap()
                .input_revision,
            after
                .part_input(&after.parts[0].id, &resources)
                .unwrap()
                .input_revision
        );
    }

    #[test]
    fn prior_leaf_input_reconstructs_without_unrelated_old_resources() {
        let resources = vec![
            resource("one", &"first ".repeat(120)),
            resource("two", &"second ".repeat(120)),
        ];
        let plan = SemanticPlan::build("subject", &resources, 1400, 100).unwrap();
        let first = &plan.parts[0];
        assert!(first.ranges.iter().all(|range| range.resource == 0));
        let full = plan.part_input(&first.id, &resources).unwrap();
        assert_eq!(
            full,
            plan.part_input_for_assigned_resources(&first.id, &resources[..1])
                .unwrap()
        );
        assert!(
            plan.part_input_for_assigned_resources(&first.id, &resources[1..])
                .is_err()
        );
        let mut changed = resources[..1].to_vec();
        changed[0].text.push('x');
        assert!(
            plan.part_input_for_assigned_resources(&first.id, &changed)
                .is_err()
        );
    }
}
