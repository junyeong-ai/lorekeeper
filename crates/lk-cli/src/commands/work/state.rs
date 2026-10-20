//! Resumable derived work. Structural coverage never certifies semantic truth.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use lk_core::semantic::{SemanticPartInput, SemanticPlan, SemanticRange, SemanticResource};
use lk_vault::{MutationPlan, MutationSession};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

mod retention;
pub(crate) use retention::prune_published;

#[derive(Clone, Copy)]
pub(crate) struct WorkLimits {
    pub max_input_bytes: usize,
    pub max_result_bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Work {
    pub plan: SemanticPlan,
    pub context: Value,
    pub max_input_bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkFinding {
    pub statement: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_time: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribution: Option<String>,
    pub evidence: Vec<SemanticRange>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkUnresolved {
    pub id: String,
    pub question: String,
    pub evidence: Vec<SemanticRange>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkResolution {
    pub id: String,
    pub reason: String,
    pub evidence: Vec<SemanticRange>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkOutput {
    pub work_id: String,
    pub node_id: String,
    pub input_revision: String,
    pub summary: String,
    #[serde(default)]
    pub findings: Vec<WorkFinding>,
    #[serde(default)]
    pub unresolved: Vec<WorkUnresolved>,
    #[serde(default)]
    pub resolutions: Vec<WorkResolution>,
}

#[derive(Serialize)]
pub(crate) struct WorkProgress {
    pub root_node: String,
    pub ready: Vec<String>,
    pub completed: usize,
    pub total: usize,
    pub complete: bool,
}

#[derive(Clone)]
enum NodeKind {
    Inspect { part_id: String },
    Reduce { children: [String; 2] },
}

#[derive(Clone)]
struct Node {
    id: String,
    kind: NodeKind,
    ranges: Vec<SemanticRange>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredOutput {
    output: WorkOutput,
    output_revision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reused_from: Option<Reuse>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reuse {
    work_id: String,
    node_id: String,
    output_revision: String,
    content_revision: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Seal {
    work_id: String,
    root_node: String,
    root_output_revision: String,
    final_result_revision: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Publication {
    work_id: String,
    final_result_revision: String,
    published_at: jiff::Timestamp,
}

struct Evaluation {
    nodes: Vec<Node>,
    current: BTreeMap<String, StoredOutput>,
    raw: BTreeMap<PathBuf, String>,
    ready: Vec<String>,
}

enum NodeInputError {
    Bound,
    Other(miette::Report),
}

impl NodeInputError {
    fn report(self) -> miette::Report {
        match self {
            Self::Bound => fail("serialized worker input including context exceeds its bound"),
            Self::Other(error) => error,
        }
    }
}

impl From<miette::Report> for NodeInputError {
    fn from(error: miette::Report) -> Self {
        Self::Other(error)
    }
}

fn fail(message: impl std::fmt::Display) -> miette::Report {
    miette::miette!("{message}")
}

fn encoded(value: &impl Serialize) -> miette::Result<String> {
    serde_json::to_string(value).map_err(fail)
}

fn revision(value: &impl Serialize) -> miette::Result<String> {
    let value = serde_json::to_value(value).map_err(fail)?;
    fn sort(value: Value) -> Value {
        match value {
            Value::Object(map) => Value::Object(
                map.into_iter()
                    .map(|(key, value)| (key, sort(value)))
                    .collect::<BTreeMap<_, _>>()
                    .into_iter()
                    .collect(),
            ),
            Value::Array(values) => Value::Array(values.into_iter().map(sort).collect()),
            other => other,
        }
    }
    let bytes = serde_json::to_vec(&sort(value)).map_err(fail)?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

fn require_id(id: &str) -> miette::Result<()> {
    if id.len() != 64
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(fail(
            "work/node id must be 64 lowercase hexadecimal characters",
        ));
    }
    Ok(())
}

fn work_path(id: &str, suffix: &str) -> miette::Result<PathBuf> {
    require_id(id)?;
    Ok(PathBuf::from(".lorekeeper/work").join(id).join(suffix))
}

fn output_path(work: &str, node: &str) -> miette::Result<PathBuf> {
    require_id(node)?;
    work_path(work, &format!("nodes/{node}.json"))
}

fn read_optional(root: &Path, path: &Path) -> miette::Result<Option<String>> {
    let full = lk_vault::page_path(root, path).map_err(fail)?;
    match std::fs::symlink_metadata(&full) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(fail("work artifact must be a direct regular file"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(fail(error)),
    }
    std::fs::read_to_string(full).map(Some).map_err(fail)
}

fn require_settled(root: &Path) -> miette::Result<()> {
    match std::fs::symlink_metadata(root.join(".lorekeeper/mutation/pending.json")) {
        Ok(_) => Err(fail(
            "pending mutation journal; read-only work inspection never recovers",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(fail(error)),
    }
}

fn nonblank(value: &str) -> miette::Result<()> {
    if value.trim().is_empty() {
        return Err(fail(
            "work observations require nonblank statements and identifiers",
        ));
    }
    Ok(())
}

fn context_overhead(context: &Value) -> miette::Result<usize> {
    if !context.is_object() || context.get("purpose").is_none_or(Value::is_null) {
        return Err(fail(
            "work context requires an object with an explicit semantic purpose",
        ));
    }
    let placeholder = "0".repeat(64);
    Ok(encoded(&json!({
        "work_id": placeholder, "node_id": placeholder, "kind": "inspect",
        "context": context, "input": null, "input_revision": placeholder,
    }))?
    .len()
        - 4)
}

fn validate_references(
    references: &[SemanticRange],
    coverage: &[SemanticRange],
) -> miette::Result<()> {
    if references.is_empty() {
        return Err(fail(
            "a finding, concern or resolution requires exact evidence references",
        ));
    }
    let mut ordered = coverage.to_vec();
    ordered.sort_by_key(|range| (range.resource, range.start, range.end));
    let mut covered: Vec<SemanticRange> = Vec::new();
    for range in ordered {
        if let Some(last) = covered.last_mut()
            && last.resource == range.resource
            && last.end >= range.start
        {
            last.end = last.end.max(range.end);
        } else {
            covered.push(range);
        }
    }
    for reference in references {
        if reference.start > reference.end
            || !covered.iter().any(|range| {
                range.resource == reference.resource
                    && range.start <= reference.start
                    && range.end >= reference.end
                    && (reference.start < reference.end || range.start == range.end)
            })
        {
            return Err(fail(
                "observation evidence is outside the node's assigned ranges",
            ));
        }
    }
    Ok(())
}

fn validate_output(
    output: &WorkOutput,
    node: &Node,
    children: &[&WorkOutput],
) -> miette::Result<()> {
    nonblank(&output.summary)?;
    for finding in &output.findings {
        nonblank(&finding.statement)?;
        for value in [
            &finding.scope,
            &finding.effective_time,
            &finding.attribution,
        ]
        .into_iter()
        .flatten()
        {
            nonblank(value)?;
        }
        validate_references(&finding.evidence, &node.ranges)?;
    }
    let mut unresolved = BTreeSet::new();
    for concern in &output.unresolved {
        nonblank(&concern.id)?;
        nonblank(&concern.question)?;
        validate_references(&concern.evidence, &node.ranges)?;
        if !unresolved.insert(concern.id.as_str()) {
            return Err(fail("duplicate unresolved concern id"));
        }
    }
    let mut resolved = BTreeSet::new();
    for resolution in &output.resolutions {
        nonblank(&resolution.id)?;
        nonblank(&resolution.reason)?;
        validate_references(&resolution.evidence, &node.ranges)?;
        if !resolved.insert(resolution.id.as_str()) || unresolved.contains(resolution.id.as_str()) {
            return Err(fail(
                "concern ids must have one unresolved or resolved disposition",
            ));
        }
    }
    let mut inherited = BTreeMap::<&str, &WorkUnresolved>::new();
    for child in children {
        for concern in &child.unresolved {
            if let Some(prior) = inherited.insert(&concern.id, concern)
                && prior != concern
            {
                return Err(fail(
                    "child concern ids collide; preserve distinct namespaced ids",
                ));
            }
        }
    }
    for (id, concern) in &inherited {
        if let Some(current) = output.unresolved.iter().find(|current| current.id == *id) {
            if current != *concern {
                return Err(fail(
                    "an inherited open concern must remain unchanged or be explicitly resolved",
                ));
            }
        } else if !resolved.contains(id) {
            return Err(fail("reduction omitted an inherited open concern"));
        }
    }
    if !children.is_empty() && resolved.iter().any(|id| !inherited.contains_key(id)) {
        return Err(fail("reduction resolves an unknown inherited concern"));
    }
    if children.is_empty() && !output.resolutions.is_empty() {
        return Err(fail("inspection cannot resolve concerns from another part"));
    }
    Ok(())
}

impl Work {
    pub fn build(
        resources: &[SemanticResource],
        subject_revision: &str,
        context: Value,
        limits: WorkLimits,
    ) -> miette::Result<Self> {
        let overhead = context_overhead(&context)?;
        let inner_limit = limits
            .max_input_bytes
            .checked_sub(overhead)
            .filter(|limit| *limit > 0)
            .ok_or_else(|| fail("work context and envelope exceed the input bound"))?;
        let plan = SemanticPlan::build(
            subject_revision,
            resources,
            inner_limit,
            limits.max_result_bytes,
        )
        .map_err(fail)?;
        let work = Self {
            plan,
            context,
            max_input_bytes: limits.max_input_bytes,
        };
        work.plan.verify_resources(resources).map_err(fail)?;
        Ok(work)
    }

    pub fn create(
        session: &MutationSession,
        resources: &[SemanticResource],
        subject_revision: &str,
        context: Value,
        limits: WorkLimits,
    ) -> miette::Result<Self> {
        require_settled(session.root())?;
        let work = Self::build(resources, subject_revision, context, limits)?;
        let path = work_path(&work.plan.id, "plan.json")?;
        let before = read_optional(session.root(), &path)?;
        let after = encoded(&work)?;
        if let Some(prior) = &before
            && prior != &after
        {
            return Err(fail(
                "work id is occupied by a different manifest or context",
            ));
        }
        let mut mutation = MutationPlan::default();
        mutation
            .publish_expected(session.root(), path, before, after)
            .map_err(fail)?;
        session.apply(mutation).map_err(fail)?;
        Ok(work)
    }

    pub fn load(root: &Path, id: &str) -> miette::Result<Self> {
        require_settled(root)?;
        let path = work_path(id, "plan.json")?;
        let raw = read_optional(root, &path)?.ok_or_else(|| fail("work manifest is missing"))?;
        let work: Self = serde_json::from_str(&raw).map_err(fail)?;
        if work.plan.id != id {
            return Err(fail("work manifest id differs from its address"));
        }
        work.verify_context()?;
        Ok(work)
    }

    fn verify_context(&self) -> miette::Result<()> {
        if self
            .max_input_bytes
            .checked_sub(context_overhead(&self.context)?)
            != Some(self.plan.max_input_bytes)
        {
            return Err(fail("work context wrapper or input bound changed"));
        }
        Ok(())
    }

    fn manifest(&self, root: &Path) -> miette::Result<(PathBuf, String)> {
        let path = work_path(&self.plan.id, "plan.json")?;
        let raw = read_optional(root, &path)?.ok_or_else(|| fail("work manifest disappeared"))?;
        let current: Self = serde_json::from_str(&raw).map_err(fail)?;
        if &current != self {
            return Err(fail("work manifest changed"));
        }
        Ok((path, raw))
    }

    fn nodes(&self) -> miette::Result<Vec<Node>> {
        fn collect(
            work: &Work,
            parts: &[lk_core::semantic::SemanticPart],
            nodes: &mut Vec<Node>,
        ) -> miette::Result<String> {
            if parts.len() == 1 {
                let part = &parts[0];
                nodes.push(Node {
                    id: part.id.clone(),
                    kind: NodeKind::Inspect {
                        part_id: part.id.clone(),
                    },
                    ranges: part.ranges.clone(),
                });
                return Ok(part.id.clone());
            }
            let middle = parts.len() / 2;
            let left = collect(work, &parts[..middle], nodes)?;
            let right = collect(work, &parts[middle..], nodes)?;
            let id = revision(&json!(["reduce", work.plan.id, left, right]))?;
            nodes.push(Node {
                id: id.clone(),
                kind: NodeKind::Reduce {
                    children: [left, right],
                },
                ranges: parts
                    .iter()
                    .flat_map(|part| part.ranges.iter().cloned())
                    .collect(),
            });
            Ok(id)
        }
        if self.plan.parts.is_empty() {
            return Err(fail("work plan has no inspect parts"));
        }
        let mut nodes = Vec::new();
        collect(self, &self.plan.parts, &mut nodes)?;
        let unique: BTreeSet<_> = nodes.iter().map(|node| &node.id).collect();
        if unique.len() != nodes.len() {
            return Err(fail("work tree contains duplicate node ids"));
        }
        Ok(nodes)
    }

    fn node_input(
        &self,
        node: &Node,
        resources: &[SemanticResource],
        outputs: &BTreeMap<String, StoredOutput>,
    ) -> Result<Value, NodeInputError> {
        let mut input = match &node.kind {
            NodeKind::Inspect { part_id } => {
                return self.inspect_input(
                    &node.id,
                    self.plan
                        .part_input_for_assigned_resources(part_id, resources)
                        .map_err(fail)?,
                );
            }
            NodeKind::Reduce { children } => {
                let outputs = children
                    .iter()
                    .map(|id| {
                        outputs
                            .get(id)
                            .ok_or_else(|| fail("reduction has an incomplete or stale child"))
                    })
                    .collect::<miette::Result<Vec<_>>>()?;
                let evidence = self.evidence_guards(
                    &outputs
                        .iter()
                        .map(|stored| &stored.output)
                        .collect::<Vec<_>>(),
                );
                let children = children.iter().zip(outputs).map(|(id, child)| {
                    json!({"node_id": id, "result_revision": child.output_revision, "output": child.output})
                }).collect::<Vec<_>>();
                json!({
                    "work_id": self.plan.id,
                    "node_id": node.id,
                    "kind": "reduce",
                    "max_result_bytes": self.plan.max_result_bytes,
                    "context": self.context,
                    "children": children,
                    "evidence": evidence,
                })
            }
        };
        let input_revision = revision(&input)?;
        input
            .as_object_mut()
            .expect("input object")
            .insert("input_revision".into(), input_revision.into());
        if encoded(&input)?.len() > self.max_input_bytes {
            return Err(NodeInputError::Bound);
        }
        Ok(input)
    }

    fn evidence_guards(&self, outputs: &[&WorkOutput]) -> Vec<Value> {
        let indices: BTreeSet<_> = outputs
            .iter()
            .flat_map(|output| {
                output
                    .findings
                    .iter()
                    .flat_map(|finding| &finding.evidence)
                    .chain(
                        output
                            .unresolved
                            .iter()
                            .flat_map(|concern| &concern.evidence),
                    )
                    .chain(
                        output
                            .resolutions
                            .iter()
                            .flat_map(|resolution| &resolution.evidence),
                    )
                    .map(|range| range.resource)
            })
            .collect();
        indices
            .into_iter()
            .map(|index| json!({"resource": index, "guard": self.plan.resources[index]}))
            .collect()
    }

    fn inspect_input(
        &self,
        node_id: &str,
        part: SemanticPartInput,
    ) -> Result<Value, NodeInputError> {
        let mut input = json!({
            "work_id": self.plan.id, "node_id": node_id, "kind": "inspect",
            "context": self.context, "input": part,
        });
        let input_revision = revision(&input)?;
        input
            .as_object_mut()
            .expect("input object")
            .insert("input_revision".into(), input_revision.into());
        if encoded(&input)?.len() > self.max_input_bytes {
            return Err(NodeInputError::Bound);
        }
        Ok(input)
    }

    fn evaluate(&self, root: &Path, resources: &[SemanticResource]) -> miette::Result<Evaluation> {
        require_settled(root)?;
        self.verify_context()?;
        self.manifest(root)?;
        self.plan.verify_resources(resources).map_err(fail)?;
        let nodes = self.nodes()?;
        let mut current: BTreeMap<String, StoredOutput> = BTreeMap::new();
        let mut raw = BTreeMap::new();
        let mut ready = Vec::new();
        for node in &nodes {
            let path = output_path(&self.plan.id, &node.id)?;
            let stored_raw = read_optional(root, &path)?;
            let stored = stored_raw
                .as_ref()
                .map(|text| serde_json::from_str::<StoredOutput>(text))
                .transpose()
                .map_err(fail)?;
            if let (Some(stored_raw), Some(stored)) = (&stored_raw, &stored) {
                if stored.output.work_id != self.plan.id
                    || stored.output.node_id != node.id
                    || revision(&stored.output)? != stored.output_revision
                    || encoded(&stored.output)?.len() > self.plan.max_result_bytes
                {
                    return Err(fail(
                        "stored work output identity, checksum or bound is invalid",
                    ));
                }
                raw.insert(path, stored_raw.clone());
            }
            let children = match &node.kind {
                NodeKind::Inspect { .. } => Some(Vec::new()),
                NodeKind::Reduce { children } => children
                    .iter()
                    .map(|id| current.get(id))
                    .collect::<Option<Vec<_>>>(),
            };
            let Some(children) = children else { continue };
            let input = match self.node_input(node, resources, &current) {
                Ok(input) => input,
                Err(NodeInputError::Bound) => {
                    ready.push(node.id.clone());
                    continue;
                }
                Err(error) => return Err(error.report()),
            };
            if let Some(stored) = stored
                && input["input_revision"].as_str() == Some(stored.output.input_revision.as_str())
            {
                let child_outputs = children
                    .iter()
                    .map(|child| &child.output)
                    .collect::<Vec<_>>();
                validate_output(&stored.output, node, &child_outputs)?;
                current.insert(node.id.clone(), stored);
            } else {
                ready.push(node.id.clone());
            }
        }
        Ok(Evaluation {
            nodes,
            current,
            raw,
            ready,
        })
    }

    pub fn progress(
        &self,
        root: &Path,
        resources: &[SemanticResource],
    ) -> miette::Result<WorkProgress> {
        let evaluated = self.evaluate(root, resources)?;
        let root_node = evaluated.nodes.last().expect("nonempty tree").id.clone();
        Ok(WorkProgress {
            complete: evaluated.current.contains_key(&root_node),
            root_node,
            ready: evaluated.ready,
            completed: evaluated.current.len(),
            total: evaluated.nodes.len(),
        })
    }

    pub fn input(
        &self,
        root: &Path,
        node_id: &str,
        resources: &[SemanticResource],
    ) -> miette::Result<Value> {
        require_id(node_id)?;
        let evaluated = self.evaluate(root, resources)?;
        let node = evaluated
            .nodes
            .iter()
            .find(|node| node.id == node_id)
            .ok_or_else(|| fail("unknown work node"))?;
        self.node_input(node, resources, &evaluated.current)
            .map_err(NodeInputError::report)
    }

    pub fn whole_input_bytes(&self, resources: &[SemanticResource]) -> miette::Result<usize> {
        if self.plan.parts.len() != 1 {
            return Err(fail("whole input requires one inspection part"));
        }
        let part = &self.plan.parts[0];
        let input = self
            .plan
            .part_input_for_assigned_resources(&part.id, resources)
            .map_err(fail)?;
        let request = self
            .inspect_input(&part.id, input)
            .map_err(NodeInputError::report)?;
        Ok(encoded(&request)?.len())
    }

    pub fn result(&self, root: &Path, resources: &[SemanticResource]) -> miette::Result<Value> {
        let evaluated = self.evaluate(root, resources)?;
        let node = evaluated.nodes.last().expect("nonempty tree");
        let output = evaluated
            .current
            .get(&node.id)
            .ok_or_else(|| fail("work reduction is incomplete"))?;
        let mut result = json!({
            "kind": "finalize", "work_id": self.plan.id, "node_id": node.id,
            "max_result_bytes": self.plan.max_result_bytes,
            "context": self.context, "result_revision": output.output_revision,
            "output": output.output, "evidence": self.evidence_guards(&[&output.output]),
        });
        let input_revision = revision(&result)?;
        result
            .as_object_mut()
            .expect("result object")
            .insert("input_revision".into(), input_revision.into());
        if encoded(&result)?.len() > self.max_input_bytes {
            return Err(fail("serialized finalizer input exceeds its input bound"));
        }
        Ok(result)
    }

    pub fn record(
        &self,
        session: &MutationSession,
        output: WorkOutput,
        resources: &[SemanticResource],
    ) -> miette::Result<WorkProgress> {
        self.record_with_reuse(session, output, resources, None, Vec::new())
    }

    fn record_with_reuse(
        &self,
        session: &MutationSession,
        output: WorkOutput,
        resources: &[SemanticResource],
        reused_from: Option<Reuse>,
        additional_guards: Vec<(PathBuf, String)>,
    ) -> miette::Result<WorkProgress> {
        let mutation =
            self.plan_record(session, output, resources, reused_from, additional_guards)?;
        session.apply(mutation).map_err(fail)?;
        self.progress(session.root(), resources)
    }

    fn plan_record(
        &self,
        session: &MutationSession,
        output: WorkOutput,
        resources: &[SemanticResource],
        reused_from: Option<Reuse>,
        additional_guards: Vec<(PathBuf, String)>,
    ) -> miette::Result<MutationPlan> {
        if output.work_id != self.plan.id {
            return Err(fail("output belongs to another work"));
        }
        require_id(&output.node_id)?;
        let evaluated = self.evaluate(session.root(), resources)?;
        let node = evaluated
            .nodes
            .iter()
            .find(|node| node.id == output.node_id)
            .ok_or_else(|| fail("unknown output node"))?;
        let input = self
            .node_input(node, resources, &evaluated.current)
            .map_err(NodeInputError::report)?;
        if input["input_revision"].as_str() != Some(output.input_revision.as_str()) {
            return Err(fail("output was written from stale or different inputs"));
        }
        if encoded(&output)?.len() > self.plan.max_result_bytes {
            return Err(fail("serialized work output exceeds its result bound"));
        }
        let children = match &node.kind {
            NodeKind::Inspect { .. } => Vec::new(),
            NodeKind::Reduce { children } => children
                .iter()
                .map(|id| &evaluated.current[id].output)
                .collect(),
        };
        validate_output(&output, node, &children)?;
        let stored = StoredOutput {
            output_revision: revision(&output)?,
            output,
            reused_from,
        };
        let path = output_path(&self.plan.id, &node.id)?;
        let before = read_optional(session.root(), &path)?;
        let mut mutation = MutationPlan::default();
        let (manifest_path, manifest) = self.manifest(session.root())?;
        mutation.require_unchanged(manifest_path, manifest);
        // The node edit owns its before/after guard. A separate unchanged guard
        // would reject our own replacement when an interrupted journal resumes.
        for (dependency, raw) in evaluated.raw {
            if dependency != path {
                mutation.require_unchanged(dependency, raw);
            }
        }
        for (dependency, raw) in additional_guards {
            if dependency != path {
                mutation.require_unchanged(dependency, raw);
            }
        }
        mutation
            .publish_expected(session.root(), path, before, encoded(&stored)?)
            .map_err(fail)?;
        Ok(mutation)
    }

    pub fn reuse_from(
        &self,
        session: &MutationSession,
        prior: &Work,
        old_node_id: &str,
        new_node_id: &str,
        resources: &[SemanticResource],
    ) -> miette::Result<WorkProgress> {
        fn purpose(context: &Value) -> miette::Result<Value> {
            let mut context = context
                .as_object()
                .cloned()
                .ok_or_else(|| fail("invalid work context"))?;
            if !context.contains_key("purpose") {
                return Err(fail("work has no semantic purpose"));
            }
            context.remove("subject");
            Ok(Value::Object(context))
        }
        prior.verify_context()?;
        let purpose_value = purpose(&self.context)?;
        if purpose_value != purpose(&prior.context)? {
            return Err(fail(
                "prior observation has a different semantic purpose or output contract",
            ));
        }
        let purpose_key = encoded(&purpose_value)?;
        self.plan.verify_resources(resources).map_err(fail)?;
        let content_revision = self
            .plan
            .part_content_revision(new_node_id, &purpose_key)
            .map_err(fail)?;
        if content_revision
            != prior
                .plan
                .part_content_revision(old_node_id, &purpose_key)
                .map_err(fail)?
        {
            return Err(fail("prior leaf has different assigned exact evidence"));
        }
        let old_node = prior
            .nodes()?
            .into_iter()
            .find(|node| node.id == old_node_id)
            .ok_or_else(|| fail("unknown prior node"))?;
        let NodeKind::Inspect { part_id } = &old_node.kind else {
            return Err(fail("only local inspection observations can be reused"));
        };
        let old_input = prior
            .inspect_input(
                old_node_id,
                prior
                    .plan
                    .part_input_for_assigned_resources(part_id, resources)
                    .map_err(fail)?,
            )
            .map_err(NodeInputError::report)?;
        let old_path = output_path(&prior.plan.id, old_node_id)?;
        let old_raw = read_optional(session.root(), &old_path)?
            .ok_or_else(|| fail("prior leaf observation is missing"))?;
        let old: StoredOutput = serde_json::from_str(&old_raw).map_err(fail)?;
        if old.output.work_id != prior.plan.id
            || old.output.node_id != old_node_id
            || old_input["input_revision"].as_str() != Some(old.output.input_revision.as_str())
            || revision(&old.output)? != old.output_revision
            || encoded(&old.output)?.len() > prior.plan.max_result_bytes
        {
            return Err(fail(
                "prior leaf observation is not a verified matching output",
            ));
        }
        validate_output(&old.output, &old_node, &[])?;
        let mut output = old.output.clone();
        output.work_id = self.plan.id.clone();
        output.node_id = new_node_id.into();
        let new_input = self.input(session.root(), new_node_id, resources)?;
        output.input_revision = new_input["input_revision"]
            .as_str()
            .ok_or_else(|| fail("missing input revision"))?
            .into();
        let remap = |ranges: &mut Vec<SemanticRange>| -> miette::Result<()> {
            for range in ranges {
                let guard = prior
                    .plan
                    .resources
                    .get(range.resource)
                    .ok_or_else(|| fail("invalid prior resource"))?;
                range.resource = self
                    .plan
                    .resources
                    .iter()
                    .position(|resource| resource == guard)
                    .ok_or_else(|| fail("prior evidence owner is absent from current work"))?;
            }
            Ok(())
        };
        for finding in &mut output.findings {
            remap(&mut finding.evidence)?;
        }
        for concern in &mut output.unresolved {
            remap(&mut concern.evidence)?;
        }
        let (manifest_path, manifest_raw) = prior.manifest(session.root())?;
        self.record_with_reuse(
            session,
            output,
            resources,
            Some(Reuse {
                work_id: prior.plan.id.clone(),
                node_id: old_node_id.into(),
                output_revision: old.output_revision,
                content_revision,
            }),
            vec![(manifest_path, manifest_raw), (old_path, old_raw)],
        )
    }

    pub fn seal(
        &self,
        session: &MutationSession,
        resources: &[SemanticResource],
        final_result: Value,
    ) -> miette::Result<()> {
        let evaluated = self.evaluate(session.root(), resources)?;
        let node = evaluated.nodes.last().expect("nonempty tree");
        let output = evaluated
            .current
            .get(&node.id)
            .ok_or_else(|| fail("work reduction is incomplete"))?;
        let seal = Seal {
            work_id: self.plan.id.clone(),
            root_node: node.id.clone(),
            root_output_revision: output.output_revision.clone(),
            final_result_revision: revision(&final_result)?,
        };
        let path = work_path(&self.plan.id, "seal.json")?;
        let before = read_optional(session.root(), &path)?;
        let mut mutation = MutationPlan::default();
        let (manifest_path, manifest) = self.manifest(session.root())?;
        mutation.require_unchanged(manifest_path, manifest);
        for (path, raw) in evaluated.raw {
            mutation.require_unchanged(path, raw);
        }
        mutation
            .publish_expected(session.root(), path, before, encoded(&seal)?)
            .map_err(fail)?;
        session.apply(mutation).map_err(fail)
    }

    pub fn guard_completion(
        &self,
        mutation: &mut MutationPlan,
        root: &Path,
        resources: &[SemanticResource],
        final_result: &Value,
    ) -> miette::Result<()> {
        let evaluated = self.evaluate(root, resources)?;
        let node = evaluated.nodes.last().expect("nonempty tree");
        let output = evaluated
            .current
            .get(&node.id)
            .ok_or_else(|| fail("work reduction is incomplete"))?;
        let seal_path = work_path(&self.plan.id, "seal.json")?;
        let raw = read_optional(root, &seal_path)?
            .ok_or_else(|| fail("work final result is not sealed"))?;
        let seal: Seal = serde_json::from_str(&raw).map_err(fail)?;
        if seal.work_id != self.plan.id
            || seal.root_node != node.id
            || seal.root_output_revision != output.output_revision
            || seal.final_result_revision != revision(final_result)?
        {
            return Err(fail("sealed result or completed reduction changed"));
        }
        let (manifest_path, manifest) = self.manifest(root)?;
        mutation.require_unchanged(manifest_path, manifest);
        for (path, raw) in evaluated.raw {
            mutation.require_unchanged(path, raw);
        }
        mutation.require_unchanged(seal_path, raw);
        Ok(())
    }

    pub fn plan_publication(
        &self,
        mutation: &mut MutationPlan,
        root: &Path,
        result: &Value,
    ) -> miette::Result<()> {
        let path = work_path(&self.plan.id, "publication.json")?;
        let digest = revision(result)?;
        if let Some(raw) = read_optional(root, &path)? {
            let receipt: Publication = serde_json::from_str(&raw).map_err(fail)?;
            if receipt.work_id != self.plan.id || receipt.final_result_revision != digest {
                return Err(fail(
                    "work publication receipt differs from the sealed result",
                ));
            }
            mutation.require_unchanged(path, raw);
        } else {
            let receipt = Publication {
                work_id: self.plan.id.clone(),
                final_result_revision: digest,
                published_at: jiff::Timestamp::now(),
            };
            mutation
                .publish_expected(root, path, None, encoded(&receipt)?)
                .map_err(fail)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lk_core::semantic::SemanticAuthority;

    pub(super) fn resources() -> Vec<SemanticResource> {
        vec![SemanticResource {
            address: "wiki/documents/source.md".into(),
            revision: "source-revision".into(),
            authority: SemanticAuthority::Page,
            text: "source paragraph ".repeat(350),
        }]
    }

    pub(super) fn context(subject: &str) -> Value {
        json!({"subject": subject, "purpose": {"kind": "synthesis", "locale": "ko"}, "output_contract": "synthesis"})
    }

    pub(super) fn limits() -> WorkLimits {
        WorkLimits {
            max_input_bytes: 3000,
            max_result_bytes: 1500,
        }
    }

    fn draft(work: &Work, root: &Path, id: &str, resources: &[SemanticResource]) -> WorkOutput {
        let input = work.input(root, id, resources).unwrap();
        let unresolved = input
            .get("children")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .flat_map(|child| {
                serde_json::from_value::<WorkOutput>(child["output"].clone())
                    .unwrap()
                    .unresolved
            })
            .collect();
        WorkOutput {
            work_id: work.plan.id.clone(),
            node_id: id.into(),
            input_revision: input["input_revision"].as_str().unwrap().into(),
            summary: "Read the assigned range; source-scoped observation.".into(),
            findings: Vec::new(),
            unresolved,
            resolutions: Vec::new(),
        }
    }

    pub(super) fn finish(work: &Work, session: &MutationSession, resources: &[SemanticResource]) {
        loop {
            let progress = work.progress(session.root(), resources).unwrap();
            if progress.complete {
                return;
            }
            assert!(!progress.ready.is_empty());
            for id in progress.ready {
                work.record(
                    session,
                    draft(work, session.root(), &id, resources),
                    resources,
                )
                .unwrap();
            }
        }
    }

    #[test]
    fn complete_inspection_reduction_and_exact_sealed_publication() {
        let root = tempfile::tempdir().unwrap();
        let source = resources();
        let session = MutationSession::acquire(root.path()).unwrap();
        let work = Work::create(&session, &source, "subject", context("one"), limits()).unwrap();
        assert!(work.plan.parts.len() > 1);
        assert_eq!(work, Work::load(root.path(), &work.plan.id).unwrap());
        let initial = work.progress(root.path(), &source).unwrap();
        assert_eq!(initial.completed, 0);
        assert_eq!(initial.total, work.plan.parts.len() * 2 - 1);
        let final_result = json!({"task_id": "task", "body": "final typed body"});
        assert!(work.seal(&session, &source, final_result.clone()).is_err());
        assert!(work.result(root.path(), &source).is_err());
        finish(&work, &session, &source);
        let finalizer = work.result(root.path(), &source).unwrap();
        assert_eq!(finalizer["kind"], "finalize");
        assert!(encoded(&finalizer).unwrap().len() <= work.max_input_bytes);
        assert_eq!(finalizer["context"], work.context);
        work.seal(&session, &source, final_result.clone()).unwrap();
        let mut mutation = MutationPlan::default();
        work.guard_completion(&mut mutation, root.path(), &source, &final_result)
            .unwrap();
        session.apply(mutation).unwrap();
        assert!(
            work.guard_completion(
                &mut MutationPlan::default(),
                root.path(),
                &source,
                &json!({"body": "different"})
            )
            .is_err()
        );
    }

    #[test]
    fn incomplete_or_out_of_range_or_forged_outputs_never_complete_a_part() {
        let root = tempfile::tempdir().unwrap();
        let source = resources();
        let session = MutationSession::acquire(root.path()).unwrap();
        let work = Work::create(&session, &source, "subject", context("one"), limits()).unwrap();
        let id = work.plan.parts[0].id.clone();
        let original = draft(&work, root.path(), &id, &source);
        let mut forged = original.clone();
        forged.input_revision = "0".repeat(64);
        assert!(work.record(&session, forged, &source).is_err());
        forged = original.clone();
        forged.findings.push(WorkFinding {
            statement: "Unsupported range".into(),
            scope: None,
            effective_time: None,
            attribution: None,
            evidence: vec![SemanticRange {
                resource: 0,
                start: 0,
                end: source[0].text.chars().count(),
            }],
        });
        assert!(work.record(&session, forged, &source).is_err());
        forged = original;
        forged.summary = "x".repeat(limits().max_result_bytes);
        assert!(work.record(&session, forged, &source).is_err());
        assert_eq!(work.progress(root.path(), &source).unwrap().completed, 0);
        assert!(work.input(root.path(), "../escape", &source).is_err());
        assert!(
            Work::build(
                &source,
                "subject",
                json!({"purpose": "x".repeat(4000)}),
                limits()
            )
            .is_err()
        );
    }

    #[test]
    fn reductions_preserve_or_explicitly_resolve_inherited_open_concerns() {
        let root = tempfile::tempdir().unwrap();
        let source = resources();
        let session = MutationSession::acquire(root.path()).unwrap();
        let work = Work::create(&session, &source, "subject", context("one"), limits()).unwrap();
        for (index, part) in work.plan.parts.iter().enumerate() {
            let mut output = draft(&work, root.path(), &part.id, &source);
            if index == 0 {
                output.unresolved.push(WorkUnresolved {
                    id: "scope:source:count".into(),
                    question: "The source count is inconsistent.".into(),
                    evidence: vec![part.ranges[0].clone()],
                });
            }
            work.record(&session, output, &source).unwrap();
        }
        let id = work.progress(root.path(), &source).unwrap().ready[0].clone();
        let mut output = draft(&work, root.path(), &id, &source);
        if output.unresolved.is_empty() {
            work.record(&session, output, &source).unwrap();
            let id = work.progress(root.path(), &source).unwrap().ready[0].clone();
            output = draft(&work, root.path(), &id, &source);
        }
        assert!(!output.unresolved.is_empty());
        let concern = output.unresolved.remove(0);
        assert!(work.record(&session, output.clone(), &source).is_err());
        output.resolutions.push(WorkResolution {
            id: concern.id,
            reason: "The source explicitly supplies the missing denominator.".into(),
            evidence: concern.evidence,
        });
        work.record(&session, output, &source).unwrap();
    }

    #[test]
    fn revised_leaf_invalidates_ancestors_and_seal_without_deleting_history() {
        let root = tempfile::tempdir().unwrap();
        let source = resources();
        let session = MutationSession::acquire(root.path()).unwrap();
        let work = Work::create(&session, &source, "subject", context("one"), limits()).unwrap();
        finish(&work, &session, &source);
        let final_result = json!({"body": "result"});
        work.seal(&session, &source, final_result.clone()).unwrap();
        let id = work.plan.parts[0].id.clone();
        let mut output = draft(&work, root.path(), &id, &source);
        output.summary = "Corrected source-scoped observation.".into();
        work.record(&session, output, &source).unwrap();
        assert!(!work.progress(root.path(), &source).unwrap().complete);
        assert!(
            work.guard_completion(
                &mut MutationPlan::default(),
                root.path(),
                &source,
                &final_result
            )
            .is_err()
        );
        finish(&work, &session, &source);
        assert!(
            work.guard_completion(
                &mut MutationPlan::default(),
                root.path(),
                &source,
                &final_result
            )
            .is_err()
        );
        work.seal(&session, &source, final_result).unwrap();
    }

    #[test]
    fn revised_observation_recovery_accepts_own_edit_and_preserves_other_guards() {
        for additional_own_guard in [false, true] {
            for edit_receipted in [false, true] {
                for damaged_dependency in [false, true] {
                    let root = tempfile::tempdir().unwrap();
                    let source = resources();
                    let session = MutationSession::acquire(root.path()).unwrap();
                    let work = Work::create(&session, &source, "subject", context("one"), limits())
                        .unwrap();
                    assert!(work.plan.parts.len() > 1);
                    let id = &work.plan.parts[0].id;
                    let other = &work.plan.parts[1].id;
                    for node in [id, other] {
                        work.record(&session, draft(&work, root.path(), node, &source), &source)
                            .unwrap();
                    }
                    let own_path = output_path(&work.plan.id, id).unwrap();
                    let other_path = output_path(&work.plan.id, other).unwrap();
                    let other_raw = read_optional(root.path(), &other_path).unwrap().unwrap();
                    let additional = if additional_own_guard {
                        vec![(
                            own_path.clone(),
                            read_optional(root.path(), &own_path).unwrap().unwrap(),
                        )]
                    } else {
                        Vec::new()
                    };
                    let mut replacement = draft(&work, root.path(), id, &source);
                    replacement.summary =
                        "Corrected observation after rereading the assigned evidence.".into();
                    let plan = work
                        .plan_record(&session, replacement.clone(), &source, None, additional)
                        .unwrap();
                    let plan_revision = lk_core::concept::file_revision(&encoded(&plan).unwrap());
                    let plan_value = serde_json::to_value(&plan).unwrap();
                    let steps = plan_value["steps"].as_array().unwrap();
                    let edit = steps.last().unwrap();
                    assert_eq!(edit["path"], own_path.to_str().unwrap());
                    assert_ne!(edit["before"], edit["after"]);
                    let after = edit["after"].as_str().unwrap().to_owned();
                    let completed = steps.len() - usize::from(!edit_receipted);
                    drop(session);

                    // Native journal/progress wire shapes from lk-vault::mutation.
                    // Simulate interruption after the node write, on either side
                    // of its progress receipt, without a timing race.
                    let directory = root.path().join(".lorekeeper/mutation");
                    let pending = directory.join("pending.json");
                    let progress = directory.join("progress.json");
                    let journal = encoded(&json!({"version":1,"plan":plan_value})).unwrap();
                    std::fs::write(&pending, &journal).unwrap();
                    std::fs::write(root.path().join(&own_path), &after).unwrap();
                    std::fs::write(&progress, encoded(&json!({
                        "plan_revision":plan_revision,
                        "completed":completed,
                        "checksum":lk_core::concept::file_revision(&format!("{plan_revision}:{completed}")),
                    })).unwrap()).unwrap();
                    if damaged_dependency {
                        std::fs::write(root.path().join(&other_path), "external dependency edit")
                            .unwrap();
                        assert!(MutationSession::acquire(root.path()).is_err());
                        assert_eq!(std::fs::read_to_string(&pending).unwrap(), journal);
                        assert_eq!(
                            std::fs::read_to_string(root.path().join(&own_path)).unwrap(),
                            after
                        );
                        std::fs::write(root.path().join(&other_path), &other_raw).unwrap();
                    }
                    let _recovered = MutationSession::acquire(root.path()).unwrap();
                    assert!(!pending.exists());
                    assert!(!progress.exists());
                    let stored: StoredOutput = serde_json::from_str(
                        &read_optional(root.path(), &own_path).unwrap().unwrap(),
                    )
                    .unwrap();
                    assert_eq!(stored.output, replacement);
                    assert_eq!(work.progress(root.path(), &source).unwrap().completed, 2);
                }
            }
        }
    }

    #[test]
    fn corruption_pending_journal_and_changed_sources_remain_explicit() {
        let root = tempfile::tempdir().unwrap();
        let source = resources();
        let session = MutationSession::acquire(root.path()).unwrap();
        let work = Work::create(&session, &source, "subject", context("one"), limits()).unwrap();
        let id = work.plan.parts[0].id.clone();
        work.record(&session, draft(&work, root.path(), &id, &source), &source)
            .unwrap();
        let path = root.path().join(output_path(&work.plan.id, &id).unwrap());
        let original = std::fs::read_to_string(&path).unwrap();
        let mut corrupt: Value = serde_json::from_str(&original).unwrap();
        corrupt["output"]["summary"] = "unverified edit".into();
        std::fs::write(&path, encoded(&corrupt).unwrap()).unwrap();
        assert!(work.progress(root.path(), &source).is_err());
        std::fs::write(&path, original).unwrap();
        let mut changed = source.clone();
        changed[0].text.push('!');
        assert!(work.progress(root.path(), &changed).is_err());
        let journal = root.path().join(".lorekeeper/mutation/pending.json");
        std::fs::write(&journal, "{}").unwrap();
        assert!(work.progress(root.path(), &source).is_err());
        assert_eq!(std::fs::read_to_string(&journal).unwrap(), "{}");
        std::fs::remove_file(journal).unwrap();
        assert_eq!(work.progress(root.path(), &source).unwrap().completed, 1);
    }

    #[test]
    fn reuse_rebinds_only_exact_local_evidence_and_records_provenance() {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let mut source = resources();
        source.insert(
            0,
            SemanticResource {
                address: "other".into(),
                revision: "other".into(),
                authority: SemanticAuthority::Page,
                text: "unrelated paragraph ".repeat(300),
            },
        );
        let old = Work::create(&session, &source, "old-subject", context("old"), limits()).unwrap();
        let part = old
            .plan
            .parts
            .iter()
            .find(|part| part.ranges.iter().all(|range| range.resource == 1))
            .unwrap();
        let mut output = draft(&old, root.path(), &part.id, &source);
        output.findings.push(WorkFinding {
            statement: "The stable source states this scoped claim.".into(),
            scope: Some("one source".into()),
            effective_time: None,
            attribution: Some("authored report".into()),
            evidence: vec![part.ranges[0].clone()],
        });
        old.record(&session, output, &source).unwrap();
        let mut changed = source.clone();
        changed[0].text.push('!');
        changed.insert(
            0,
            SemanticResource {
                address: "added".into(),
                revision: "added".into(),
                authority: SemanticAuthority::Review,
                text: "added context ".repeat(300),
            },
        );
        let new =
            Work::create(&session, &changed, "new-subject", context("new"), limits()).unwrap();
        let new_part = new
            .plan
            .parts
            .iter()
            .find(|candidate| candidate.id == part.id)
            .unwrap();
        new.reuse_from(&session, &old, &part.id, &new_part.id, &changed)
            .unwrap();
        let raw = read_optional(
            root.path(),
            &output_path(&new.plan.id, &new_part.id).unwrap(),
        )
        .unwrap()
        .unwrap();
        let stored: StoredOutput = serde_json::from_str(&raw).unwrap();
        assert_eq!(stored.output.findings[0].evidence[0].resource, 2);
        assert_eq!(stored.reused_from.unwrap().work_id, old.plan.id);
        let different = Work::create(
            &session,
            &changed,
            "new-purpose",
            json!({"subject": "new", "purpose": "extraction"}),
            limits(),
        )
        .unwrap();
        assert!(
            different
                .reuse_from(&session, &old, &part.id, &new_part.id, &changed)
                .is_err()
        );
    }
    #[test]
    fn finalizer_guard_metadata_is_counted_in_the_actual_envelope_bound() {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let source = vec![SemanticResource {
            address: format!("wiki/documents/{}.md", "x".repeat(1000)),
            revision: "source-revision".into(),
            authority: SemanticAuthority::Page,
            text: "tiny source".into(),
        }];
        let work = Work::create(
            &session,
            &source,
            "subject",
            context("one"),
            WorkLimits {
                max_input_bytes: 3000,
                max_result_bytes: 2500,
            },
        )
        .unwrap();
        assert_eq!(work.plan.parts.len(), 1);
        let part = &work.plan.parts[0];
        let mut output = draft(&work, root.path(), &part.id, &source);
        output.findings.push(WorkFinding {
            statement: "f".repeat(1500),
            scope: None,
            effective_time: None,
            attribution: None,
            evidence: part.ranges.clone(),
        });
        work.record(&session, output, &source).unwrap();
        assert!(work.progress(root.path(), &source).unwrap().complete);
        assert!(work.result(root.path(), &source).is_err());
    }
    #[test]
    fn oversized_reducer_remains_read_blocked_but_leaf_can_be_revised() {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let source = resources();
        let context =
            json!({"subject":"one", "purpose":{"kind":"synthesis", "padding":"p".repeat(2000)}});
        let work = Work::create(
            &session,
            &source,
            "subject",
            context,
            WorkLimits {
                max_input_bytes: 4096,
                max_result_bytes: 1024,
            },
        )
        .unwrap();
        assert!(work.plan.parts.len() > 1);
        finish(&work, &session, &source);
        assert!(work.progress(root.path(), &source).unwrap().complete);
        for part in &work.plan.parts {
            let mut output = draft(&work, root.path(), &part.id, &source);
            output.summary = "n".repeat(430);
            output.findings.push(WorkFinding {
                statement: "Source-scoped observation.".into(),
                scope: None,
                effective_time: None,
                attribution: None,
                evidence: part.ranges.clone(),
            });
            assert!(encoded(&output).unwrap().len() <= work.plan.max_result_bytes);
            work.record(&session, output, &source).unwrap();
        }
        let status = work.progress(root.path(), &source).unwrap();
        assert!(!status.complete);
        assert!(!status.ready.is_empty());
        assert!(work.input(root.path(), &status.ready[0], &source).is_err());
        for part in &work.plan.parts {
            let replacement = draft(&work, root.path(), &part.id, &source);
            work.record(&session, replacement, &source).unwrap();
        }
        finish(&work, &session, &source);
        assert!(work.progress(root.path(), &source).unwrap().complete);
    }
}
