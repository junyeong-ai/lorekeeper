//! Resumable derived work. Structural coverage never certifies semantic truth.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use lk_core::semantic::{
    SemanticInputs, SemanticPartInput, SemanticPlan, SemanticRange, SemanticResource,
};
use lk_vault::{MutationPlan, MutationSession};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

mod frontier;
mod inspection;
mod output;
mod retention;
use frontier::{FrontierProof, FrontierSeal};
pub(crate) use inspection::OutputCapacity;
#[cfg(unix)]
pub(crate) use output::result_bytes as output_result_bytes;
#[cfg(unix)]
pub(crate) use output::retention_overhead as question_retention_overhead;
pub(crate) use retention::prune_published;

#[derive(Debug)]
pub(super) enum CompletionRefusal {
    IncompleteReduction,
    UnsealedResult,
    ChangedResult,
    ChangedObservations,
}

impl std::fmt::Display for CompletionRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::IncompleteReduction => "work reduction is incomplete",
            Self::UnsealedResult => "work final result is not sealed",
            Self::ChangedResult => "sealed result or completed reduction changed",
            Self::ChangedObservations => {
                "sealed frontier observations changed; a fresh final result is required"
            }
        })
    }
}

impl std::error::Error for CompletionRefusal {}
impl miette::Diagnostic for CompletionRefusal {}

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
    #[serde(default, skip_serializing_if = "ReductionPolicy::is_binary")]
    reduction: ReductionPolicy,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum ReductionPolicy {
    #[default]
    BinaryV1,
    ByteBudgetedV1,
}

impl ReductionPolicy {
    fn is_binary(&self) -> bool {
        *self == Self::BinaryV1
    }
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
pub(crate) struct WorkQuestion {
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_questions: Vec<WorkQuestion>,
    #[serde(default)]
    pub unresolved: Vec<WorkQuestion>,
    #[serde(default)]
    pub resolutions: Vec<WorkResolution>,
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum QuestionKind {
    Source,
    Processing,
}

impl WorkOutput {
    fn questions(&self) -> impl Iterator<Item = (QuestionKind, &WorkQuestion)> {
        self.source_questions
            .iter()
            .map(|question| (QuestionKind::Source, question))
            .chain(
                self.unresolved
                    .iter()
                    .map(|question| (QuestionKind::Processing, question)),
            )
    }
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
    Reduce { children: Vec<String> },
}

#[derive(Clone)]
struct Node {
    id: String,
    kind: NodeKind,
    ranges: Vec<SemanticRange>,
}

#[derive(Clone)]
struct StoredOutput {
    output: WorkOutput,
    output_revision: String,
    reused_from: Option<Reuse>,
    retained: Option<output::RetainedQuestions>,
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

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Consumption {
    work_id: String,
    root_node: String,
    root_output_revision: String,
    task_id: String,
    cache_hash: String,
    target: String,
    target_revision: String,
    result_revision: String,
    consumed_at: jiff::Timestamp,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FinalizationAction {
    schema: String,
    work_id: String,
    root_node: String,
    completion_revision: String,
    request_revision: String,
    reason: String,
}

impl FinalizationAction {
    fn decode(raw: &str, work: &Work) -> miette::Result<Self> {
        let action: Self = crate::commands::work::receipt::decode(raw).map_err(fail)?;
        let root = work.nodes()?.last().expect("nonempty work tree").id.clone();
        if !matches!(
            action.schema.as_str(),
            "section-finalization-action-v1"
                | "bounded-finalization-action-v1"
                | "frontier-finalization-action-v1"
        ) || action.work_id != work.plan.id
            || action.root_node != root
            || action.reason.trim().is_empty()
        {
            return Err(fail(
                "invalid finalization action receipt identity or schema",
            ));
        }
        require_id(&action.completion_revision)?;
        require_id(&action.request_revision)?;
        Ok(action)
    }
}

struct Evaluation<'a> {
    inputs: SemanticInputs<'a>,
    nodes: Vec<Node>,
    current: BTreeMap<String, StoredOutput>,
    raw: BTreeMap<PathBuf, String>,
    ready: Vec<String>,
}

pub(super) struct CompletedWork {
    work_id: String,
    root_node: String,
    authority: CompletionAuthority,
    result: Value,
    guards: BTreeMap<PathBuf, String>,
    absent: BTreeSet<PathBuf>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum CompletionKind {
    Root,
    Frontier,
}

enum CompletionAuthority {
    Root(String),
    Frontier(FrontierProof),
}

impl CompletedWork {
    pub fn result(&self) -> &Value {
        &self.result
    }

    pub fn plan_read_guards(self, mutation: &mut MutationPlan) {
        for (path, raw) in self.guards {
            mutation.require_unchanged(path, raw);
        }
        for path in self.absent {
            mutation.require_absent(path);
        }
    }

    fn kind(&self) -> CompletionKind {
        match self.authority {
            CompletionAuthority::Root(_) => CompletionKind::Root,
            CompletionAuthority::Frontier(_) => CompletionKind::Frontier,
        }
    }

    fn completion_revision(&self) -> miette::Result<String> {
        match &self.authority {
            CompletionAuthority::Root(revision) => Ok(revision.clone()),
            CompletionAuthority::Frontier(proof) => revision(proof),
        }
    }

    pub fn plan_seal(
        self,
        work: &Work,
        mutation: &mut MutationPlan,
        root: &Path,
        final_result: &Value,
    ) -> miette::Result<Option<String>> {
        if work.plan.id != self.work_id {
            return Err(fail("completion belongs to another work"));
        }
        let prior_final = read_optional(root, &work_path(&self.work_id, "final.json")?)?;
        let (name, other, after) = match self.authority {
            CompletionAuthority::Root(root_output_revision) => (
                "seal.json",
                "frontier-seal.json",
                encoded(&Seal {
                    work_id: self.work_id.clone(),
                    root_node: self.root_node.clone(),
                    root_output_revision,
                    final_result_revision: revision(final_result)?,
                })?,
            ),
            CompletionAuthority::Frontier(proof) => (
                "frontier-seal.json",
                "seal.json",
                encoded(&FrontierSeal {
                    schema: "work-frontier-seal-v1".into(),
                    proof,
                    final_result_revision: revision(final_result)?,
                })?,
            ),
        };
        let path = work_path(&self.work_id, name)?;
        let before = read_optional(root, &path)?;
        if name == "frontier-seal.json"
            && let Some(raw) = &before
        {
            let prior = FrontierSeal::decode(raw, work)?;
            let final_result: Value = serde_json::from_str(
                prior_final
                    .as_deref()
                    .ok_or_else(|| fail("sealed final result is missing"))?,
            )
            .map_err(fail)?;
            if prior.final_result_revision != revision(&final_result)? {
                return Err(fail("prior frontier seal differs from its final result"));
            }
        }
        let other_path = work_path(&self.work_id, other)?;
        if let Some(raw) = read_optional(root, &other_path)? {
            if before.is_some() {
                return Err(fail("work has conflicting completion seals"));
            }
            let prior: Value = serde_json::from_str(
                prior_final
                    .as_deref()
                    .ok_or_else(|| fail("sealed final result is missing"))?,
            )
            .map_err(fail)?;
            let (work_id, root_node, final_revision) = if other == "seal.json" {
                let seal: Seal = serde_json::from_str(&raw).map_err(fail)?;
                require_id(&seal.root_output_revision)?;
                (seal.work_id, seal.root_node, seal.final_result_revision)
            } else {
                let seal = FrontierSeal::decode(&raw, work)?;
                (
                    seal.proof.work_id().into(),
                    seal.proof.root_node().into(),
                    seal.final_result_revision,
                )
            };
            if work_id != self.work_id
                || root_node != self.root_node
                || final_revision != revision(&prior)?
            {
                return Err(fail(
                    "prior completion authority differs from its final result",
                ));
            }
            let publication_path = work_path(&self.work_id, "publication.json")?;
            if read_optional(root, &publication_path)?.is_some() {
                return Err(fail("published completion authority cannot be replaced"));
            }
            mutation.require_absent(publication_path);
            mutation.remove(other_path, raw);
        } else {
            mutation.require_absent(other_path);
        }
        for path in self.absent {
            mutation.require_absent(path);
        }
        for (path, raw) in self.guards {
            mutation.require_unchanged(path, raw);
        }
        if before.as_ref() == Some(&after) {
            mutation.require_unchanged(path, after);
        } else {
            mutation
                .publish_expected(root, path, before, after)
                .map_err(fail)?;
        }
        Ok(prior_final)
    }
}

impl Evaluation<'_> {
    fn progress(&self) -> WorkProgress {
        let root_node = self.nodes.last().expect("nonempty tree").id.clone();
        WorkProgress {
            complete: self.current.contains_key(&root_node),
            root_node,
            ready: self.ready.clone(),
            completed: self.current.len(),
            total: self.nodes.len(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct InputCapacity {
    pub actual_bytes: usize,
    pub max_bytes: usize,
}

impl std::fmt::Display for InputCapacity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "complete worker input requires {} bytes, above its {} byte allowance",
            self.actual_bytes, self.max_bytes
        )
    }
}

impl std::error::Error for InputCapacity {}
impl miette::Diagnostic for InputCapacity {}

enum NodeInputError {
    Bound(InputCapacity),
    Other(miette::Report),
}

impl NodeInputError {
    fn report(self) -> miette::Report {
        match self {
            Self::Bound(capacity) => capacity.into(),
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

pub(super) fn read_optional(root: &Path, path: &Path) -> miette::Result<Option<String>> {
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

pub(crate) fn inherited_questions<'a>(
    children: &[&'a WorkOutput],
) -> miette::Result<BTreeMap<&'a str, (QuestionKind, &'a WorkQuestion)>> {
    let mut inherited = BTreeMap::new();
    for child in children {
        for (kind, question) in child.questions() {
            if let Some(prior) = inherited.insert(question.id.as_str(), (kind, question))
                && prior != (kind, question)
            {
                return Err(fail(
                    "child question ids collide; preserve distinct namespaced ids",
                ));
            }
        }
    }
    Ok(inherited)
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
    let mut open = BTreeSet::new();
    for (_, question) in output.questions() {
        nonblank(&question.id)?;
        nonblank(&question.question)?;
        validate_references(&question.evidence, &node.ranges)?;
        if !open.insert(question.id.as_str()) {
            return Err(fail("duplicate open question id"));
        }
    }
    let mut resolved = BTreeSet::new();
    for resolution in &output.resolutions {
        nonblank(&resolution.id)?;
        nonblank(&resolution.reason)?;
        validate_references(&resolution.evidence, &node.ranges)?;
        if !resolved.insert(resolution.id.as_str()) || open.contains(resolution.id.as_str()) {
            return Err(fail(
                "question ids must have one open or resolved disposition",
            ));
        }
    }
    let inherited = inherited_questions(children)?;
    for (id, prior) in &inherited {
        if let Some(current) = output.questions().find(|(_, question)| question.id == *id) {
            if current != *prior {
                return Err(fail(
                    "an inherited open question and its kind must remain unchanged or be explicitly resolved",
                ));
            }
        } else if !resolved.contains(id) {
            return Err(fail("reduction omitted an inherited open question"));
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
        Ok(Self {
            plan,
            context,
            max_input_bytes: limits.max_input_bytes,
            reduction: ReductionPolicy::ByteBudgetedV1,
        })
    }

    pub fn create(
        session: &MutationSession,
        resources: &[SemanticResource],
        subject_revision: &str,
        context: Value,
        limits: WorkLimits,
    ) -> miette::Result<Self> {
        require_settled(session.root())?;
        let work = Self::build(resources, subject_revision, context, limits)?
            .with_saved_policy(session.root())?;
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
        if before.is_some() {
            return Ok(work);
        }
        let mut mutation = MutationPlan::default();
        mutation
            .publish_expected(session.root(), path, before, after)
            .map_err(fail)?;
        session.apply(mutation).map_err(fail)?;
        Ok(work)
    }

    pub fn with_saved_policy(mut self, root: &Path) -> miette::Result<Self> {
        require_settled(root)?;
        let Some(raw) = read_optional(root, &work_path(&self.plan.id, "plan.json")?)? else {
            return Ok(self);
        };
        let saved: Self = serde_json::from_str(&raw).map_err(fail)?;
        self.reduction = saved.reduction;
        if self != saved {
            return Err(fail(
                "work id is occupied by a different manifest or context",
            ));
        }
        saved.verify_context()?;
        Ok(saved)
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

    pub fn is_planned(&self, root: &Path) -> miette::Result<bool> {
        require_settled(root)?;
        let Some(raw) = read_optional(root, &work_path(&self.plan.id, "plan.json")?)? else {
            return Ok(false);
        };
        let current: Self = serde_json::from_str(&raw).map_err(fail)?;
        if current != *self {
            return Err(fail("work manifest changed"));
        }
        Ok(true)
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
        if self.reduction == ReductionPolicy::ByteBudgetedV1 {
            return self.budgeted_nodes(self.max_input_bytes);
        }
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
                    children: vec![left, right],
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

    fn budgeted_nodes(&self, max_input_bytes: usize) -> miette::Result<Vec<Node>> {
        if self.plan.parts.is_empty() {
            return Err(fail("work plan has no inspect parts"));
        }
        let mut nodes = self
            .plan
            .parts
            .iter()
            .map(|part| Node {
                id: part.id.clone(),
                kind: NodeKind::Inspect {
                    part_id: part.id.clone(),
                },
                ranges: part.ranges.clone(),
            })
            .collect::<Vec<_>>();
        let mut level = nodes.clone();
        while level.len() > 1 {
            let mut next = Vec::new();
            let mut at = 0;
            while at < level.len() {
                if at + 1 == level.len() {
                    next.push(level[at].clone());
                    break;
                }
                let mut count = 2;
                while at + count < level.len()
                    && self.reserved_reduce_bytes(&level[at..at + count + 1])? <= max_input_bytes
                {
                    count += 1;
                }
                let group = &level[at..at + count];
                let children = group.iter().map(|node| node.id.clone()).collect::<Vec<_>>();
                let node = Node {
                    id: revision(&json!(["reduce-byte-budgeted-v1", self.plan.id, children]))?,
                    kind: NodeKind::Reduce { children },
                    ranges: group.iter().flat_map(|node| node.ranges.clone()).collect(),
                };
                nodes.push(node.clone());
                next.push(node);
                at += count;
            }
            level = next;
        }
        let unique: BTreeSet<_> = nodes.iter().map(|node| &node.id).collect();
        if unique.len() != nodes.len() {
            return Err(fail("work tree contains duplicate node ids"));
        }
        Ok(nodes)
    }

    fn reduction_request(
        &self,
        node_id: &str,
        children: Vec<Value>,
        evidence: Vec<Value>,
    ) -> Value {
        json!({
            "work_id": self.plan.id,
            "node_id": node_id,
            "kind": "reduce",
            "max_result_bytes": self.plan.max_result_bytes,
            "context": self.context,
            "children": children,
            "evidence": evidence,
        })
    }

    fn reserved_reduce_bytes(&self, children: &[Node]) -> miette::Result<usize> {
        let placeholder = "0".repeat(64);
        let evidence = self.resource_guards(
            children
                .iter()
                .flat_map(|node| node.ranges.iter().map(|range| range.resource)),
        )?;
        let mut request = self.reduction_request(
            &placeholder,
            children
                .iter()
                .map(|child| {
                    json!({"node_id": child.id, "result_revision": placeholder, "output": null})
                })
                .collect(),
            evidence,
        );
        request["input_revision"] = placeholder.into();
        encoded(&request)?
            .len()
            .checked_sub(
                children
                    .len()
                    .checked_mul(4)
                    .ok_or_else(|| fail("reduction size overflow"))?,
            )
            .and_then(|overhead| {
                children
                    .len()
                    .checked_mul(self.plan.max_result_bytes)
                    .and_then(|outputs| overhead.checked_add(outputs))
            })
            .ok_or_else(|| fail("reduction size overflow"))
    }

    fn node_input(
        &self,
        node: &Node,
        inputs: &SemanticInputs<'_>,
        outputs: &BTreeMap<String, StoredOutput>,
    ) -> Result<Value, NodeInputError> {
        let mut input = match &node.kind {
            NodeKind::Inspect { part_id } => {
                return self.inspect_input(&node.id, inputs.part_input(part_id).map_err(fail)?);
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
                )?;
                let children = children.iter().zip(outputs).map(|(id, child)| {
                    json!({"node_id": id, "result_revision": child.output_revision, "output": child.output})
                }).collect::<Vec<_>>();
                self.reduction_request(&node.id, children, evidence)
            }
        };
        let input_revision = revision(&input)?;
        input
            .as_object_mut()
            .expect("input object")
            .insert("input_revision".into(), input_revision.into());
        let actual_bytes = encoded(&input)?.len();
        if actual_bytes > self.max_input_bytes {
            return Err(NodeInputError::Bound(InputCapacity {
                actual_bytes,
                max_bytes: self.max_input_bytes,
            }));
        }
        Ok(input)
    }

    fn evidence_guards(&self, outputs: &[&WorkOutput]) -> miette::Result<Vec<Value>> {
        let indices = outputs.iter().flat_map(|output| {
            output
                .findings
                .iter()
                .flat_map(|finding| &finding.evidence)
                .chain(
                    output
                        .questions()
                        .flat_map(|(_, question)| &question.evidence),
                )
                .chain(
                    output
                        .resolutions
                        .iter()
                        .flat_map(|resolution| &resolution.evidence),
                )
                .map(|range| range.resource)
        });
        self.resource_guards(indices)
    }

    fn resource_guards(
        &self,
        indices: impl IntoIterator<Item = usize>,
    ) -> miette::Result<Vec<Value>> {
        let indices = indices.into_iter().collect::<BTreeSet<_>>();
        indices
            .into_iter()
            .map(|index| {
                let guard = self
                    .plan
                    .resources
                    .get(index)
                    .ok_or_else(|| fail("work range names an unknown resource"))?;
                Ok(json!({"resource": index, "guard": guard}))
            })
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
        let actual_bytes = encoded(&input)?.len();
        if actual_bytes > self.max_input_bytes {
            return Err(NodeInputError::Bound(InputCapacity {
                actual_bytes,
                max_bytes: self.max_input_bytes,
            }));
        }
        Ok(input)
    }

    fn evaluate<'a>(
        &'a self,
        root: &Path,
        resources: &'a [SemanticResource],
    ) -> miette::Result<Evaluation<'a>> {
        require_settled(root)?;
        self.verify_context()?;
        let manifest = self.manifest(root)?;
        let inputs = self.plan.build_inputs(resources).map_err(fail)?;
        let nodes = self.nodes()?;
        let mut current: BTreeMap<String, StoredOutput> = BTreeMap::new();
        let mut raw = BTreeMap::from([manifest]);
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
                {
                    return Err(fail(
                        "stored work output identity, checksum or bound is invalid",
                    ));
                }
                stored.validate_bounds(self)?;
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
            let Some(stored) = stored else {
                ready.push(node.id.clone());
                continue;
            };
            let input = match self.node_input(node, &inputs, &current) {
                Ok(input) => input,
                Err(NodeInputError::Bound(_)) => {
                    ready.push(node.id.clone());
                    continue;
                }
                Err(error) => return Err(error.report()),
            };
            if input["input_revision"].as_str() == Some(stored.output.input_revision.as_str()) {
                let child_outputs = children
                    .iter()
                    .map(|child| &child.output)
                    .collect::<Vec<_>>();
                validate_output(&stored.output, node, &child_outputs)?;
                stored.validate_inheritance(&child_outputs)?;
                current.insert(node.id.clone(), stored);
            } else {
                ready.push(node.id.clone());
            }
        }
        Ok(Evaluation {
            inputs,
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
        Ok(self.evaluate(root, resources)?.progress())
    }

    #[cfg(unix)]
    pub(super) fn has_root_output(&self, root: &Path) -> miette::Result<bool> {
        let nodes = self.nodes()?;
        let node = nodes.last().expect("nonempty work tree");
        Ok(read_optional(root, &output_path(&self.plan.id, &node.id)?)?.is_some())
    }

    pub(super) fn capture_progress(
        &self,
        root: &Path,
        resources: &[SemanticResource],
    ) -> miette::Result<(WorkProgress, BTreeMap<PathBuf, String>)> {
        let evaluated = self.evaluate(root, resources)?;
        Ok((evaluated.progress(), evaluated.raw))
    }

    pub fn prepare_inspection(
        &self,
        root: &Path,
        resources: &[SemanticResource],
    ) -> miette::Result<(WorkProgress, Option<Value>)> {
        let (progress, mut inputs) = self.prepare_inspections(root, resources, 1)?;
        Ok((progress, inputs.pop()))
    }

    pub fn prepare_inspections(
        &self,
        root: &Path,
        resources: &[SemanticResource],
        limit: usize,
    ) -> miette::Result<(WorkProgress, Vec<Value>)> {
        debug_assert!(limit > 0);
        let evaluated = self.evaluate(root, resources)?;
        let progress = evaluated.progress();
        let mut inputs = Vec::new();
        if !progress.complete {
            for id in progress.ready.iter().take(limit) {
                let node = evaluated
                    .nodes
                    .iter()
                    .find(|node| &node.id == id)
                    .expect("ready node belongs to the evaluated tree");
                match self.node_input(node, &evaluated.inputs, &evaluated.current) {
                    Ok(input) => inputs.push(input),
                    Err(NodeInputError::Bound(_)) if !inputs.is_empty() => break,
                    Err(error) => return Err(error.report()),
                }
            }
        }
        Ok((progress, inputs))
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
        self.node_input(node, &evaluated.inputs, &evaluated.current)
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

    pub fn load_finalization_action(
        &self,
        root: &Path,
        request: &Value,
    ) -> miette::Result<Option<String>> {
        self.load_finalization_action_for(root, request, &request["work"]["result_revision"])
    }

    pub fn load_finalization_action_for(
        &self,
        root: &Path,
        request: &Value,
        completion_revision: &Value,
    ) -> miette::Result<Option<String>> {
        let Some(raw) =
            read_optional(root, &work_path(&self.plan.id, "finalization-action.json")?)?
        else {
            return Ok(None);
        };
        let action = FinalizationAction::decode(&raw, self)?;
        let request_revision =
            lk_core::rss_original::digest(&serde_json::to_vec(request).map_err(fail)?);
        if action.request_revision != request_revision {
            return Ok(None);
        }
        if completion_revision.as_str() != Some(action.completion_revision.as_str()) {
            return Err(fail(
                "finalization action completion differs from its request",
            ));
        }
        Ok(Some(action.reason))
    }

    pub fn plan_finalization_action(
        &self,
        root: &Path,
        resources: &[SemanticResource],
        expected: &Value,
        request: &Value,
        reason: String,
    ) -> miette::Result<MutationPlan> {
        self.plan_finalization_action_for(
            root,
            resources,
            expected,
            request,
            reason,
            CompletionKind::Root,
        )
    }

    pub(super) fn plan_finalization_action_for(
        &self,
        root: &Path,
        resources: &[SemanticResource],
        expected: &Value,
        request: &Value,
        reason: String,
        kind: CompletionKind,
    ) -> miette::Result<MutationPlan> {
        let completed = self.capture_native_completion(root, resources, kind)?;
        if completed.result() != expected {
            return Err(fail(
                "completed observations changed before recording required finalization action",
            ));
        }
        let action = FinalizationAction {
            schema: match completed.kind() {
                CompletionKind::Root => "bounded-finalization-action-v1",
                CompletionKind::Frontier => "frontier-finalization-action-v1",
            }
            .into(),
            work_id: self.plan.id.clone(),
            root_node: completed.root_node.clone(),
            completion_revision: completed.completion_revision()?,
            request_revision: lk_core::rss_original::digest(
                &serde_json::to_vec(request).map_err(fail)?,
            ),
            reason,
        };
        let raw = encoded(&action)?;
        FinalizationAction::decode(&raw, self)?;
        let path = work_path(&self.plan.id, "finalization-action.json")?;
        let before = read_optional(root, &path)?;
        if let Some(before) = &before {
            FinalizationAction::decode(before, self)?;
        }
        let mut plan = MutationPlan::default();
        completed.plan_read_guards(&mut plan);
        plan.publish_expected(root, path, before, raw)
            .map_err(fail)?;
        Ok(plan)
    }

    pub fn result(&self, root: &Path, resources: &[SemanticResource]) -> miette::Result<Value> {
        self.result_with_guards(root, resources)
            .map(|(result, _)| result)
    }

    pub fn result_with_guards(
        &self,
        root: &Path,
        resources: &[SemanticResource],
    ) -> miette::Result<(Value, BTreeMap<PathBuf, String>)> {
        let completed = self.capture_completion(root, resources)?;
        Ok((completed.result, completed.guards))
    }

    pub(super) fn result_if_complete_with_guards(
        &self,
        root: &Path,
        resources: &[SemanticResource],
    ) -> miette::Result<Option<(Value, BTreeMap<PathBuf, String>)>> {
        let evaluated = self.evaluate(root, resources)?;
        if !evaluated.progress().complete {
            return Ok(None);
        }
        let result = self.build_finalizer_result(&evaluated)?;
        Ok(Some((result, evaluated.raw)))
    }

    pub(super) fn capture_completion(
        &self,
        root: &Path,
        resources: &[SemanticResource],
    ) -> miette::Result<CompletedWork> {
        let evaluated = self.evaluate(root, resources)?;
        let result = self.build_finalizer_result(&evaluated)?;
        let node = evaluated.nodes.last().expect("nonempty tree");
        let output = evaluated
            .current
            .get(&node.id)
            .expect("finalizer result requires a complete root");
        Ok(CompletedWork {
            work_id: self.plan.id.clone(),
            root_node: node.id.clone(),
            authority: CompletionAuthority::Root(output.output_revision.clone()),
            result,
            guards: evaluated.raw,
            absent: BTreeSet::new(),
        })
    }

    fn build_finalizer_result(&self, evaluated: &Evaluation<'_>) -> miette::Result<Value> {
        let node = evaluated.nodes.last().expect("nonempty tree");
        let output = evaluated
            .current
            .get(&node.id)
            .ok_or_else(|| fail("work reduction is incomplete"))?;
        let mut result = json!({
            "kind": "finalize", "work_id": self.plan.id, "node_id": node.id,
            "max_result_bytes": self.plan.max_result_bytes,
            "context": self.context, "result_revision": output.output_revision,
            "output": output.output, "evidence": self.evidence_guards(&[&output.output])?,
        });
        let input_revision = revision(&result)?;
        result
            .as_object_mut()
            .expect("result object")
            .insert("input_revision".into(), input_revision.into());
        let actual_bytes = encoded(&result)?.len();
        if actual_bytes > self.max_input_bytes {
            return Err(InputCapacity {
                actual_bytes,
                max_bytes: self.max_input_bytes,
            }
            .into());
        }
        Ok(result)
    }

    pub fn record(
        &self,
        session: &MutationSession,
        output: WorkOutput,
        resources: &[SemanticResource],
    ) -> miette::Result<WorkProgress> {
        let mutation = self.plan_record(session, output, resources, None, Vec::new())?;
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
        let evaluated = self.evaluate(session.root(), resources)?;
        self.plan_evaluated_record(
            session.root(),
            output,
            evaluated,
            reused_from,
            additional_guards,
        )
    }

    fn plan_evaluated_record(
        &self,
        root: &Path,
        output: WorkOutput,
        evaluated: Evaluation<'_>,
        reused_from: Option<Reuse>,
        additional_guards: Vec<(PathBuf, String)>,
    ) -> miette::Result<MutationPlan> {
        if output.work_id != self.plan.id {
            return Err(fail("output belongs to another work"));
        }
        require_id(&output.node_id)?;
        let node = evaluated
            .nodes
            .iter()
            .find(|node| node.id == output.node_id)
            .ok_or_else(|| fail("unknown output node"))?;
        let input = self
            .node_input(node, &evaluated.inputs, &evaluated.current)
            .map_err(NodeInputError::report)?;
        if input["input_revision"].as_str() != Some(output.input_revision.as_str()) {
            return Err(fail("output was written from stale or different inputs"));
        }
        let children = match &node.kind {
            NodeKind::Inspect { .. } => Vec::new(),
            NodeKind::Reduce { children } => children
                .iter()
                .map(|id| &evaluated.current[id].output)
                .collect(),
        };
        validate_output(&output, node, &children)?;
        let stored =
            StoredOutput::build(output, &children, self.plan.max_result_bytes, reused_from)?;
        stored.validate_bounds(self)?;
        stored.validate_inheritance(&children)?;
        let path = output_path(&self.plan.id, &node.id)?;
        let before = evaluated.raw.get(&path).cloned();
        let mut mutation = MutationPlan::default();
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
            .publish_expected(root, path, before, encoded(&stored)?)
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
        let evaluated = self.evaluate(session.root(), resources)?;
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
        {
            return Err(fail(
                "prior leaf observation is not a verified matching output",
            ));
        }
        old.validate_bounds(prior)?;
        old.validate_inheritance(&[])?;
        validate_output(&old.output, &old_node, &[])?;
        let mut output = old.output.clone();
        output.work_id = self.plan.id.clone();
        output.node_id = new_node_id.into();
        let new_node = evaluated
            .nodes
            .iter()
            .find(|node| node.id == new_node_id)
            .ok_or_else(|| fail("unknown work node"))?;
        let new_input = self
            .node_input(new_node, &evaluated.inputs, &evaluated.current)
            .map_err(NodeInputError::report)?;
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
        for question in output
            .source_questions
            .iter_mut()
            .chain(&mut output.unresolved)
        {
            remap(&mut question.evidence)?;
        }
        let (manifest_path, manifest_raw) = prior.manifest(session.root())?;
        let mutation = self.plan_evaluated_record(
            session.root(),
            output,
            evaluated,
            Some(Reuse {
                work_id: prior.plan.id.clone(),
                node_id: old_node_id.into(),
                output_revision: old.output_revision,
                content_revision,
            }),
            vec![(manifest_path, manifest_raw), (old_path, old_raw)],
        )?;
        session.apply(mutation).map_err(fail)?;
        self.progress(session.root(), resources)
    }

    #[cfg(test)]
    pub fn seal(
        &self,
        session: &MutationSession,
        resources: &[SemanticResource],
        final_result: Value,
    ) -> miette::Result<()> {
        let mut mutation = MutationPlan::default();
        self.capture_completion(session.root(), resources)?
            .plan_seal(self, &mut mutation, session.root(), &final_result)?;
        session.apply(mutation).map_err(fail)
    }

    pub fn guard_completion(
        &self,
        mutation: &mut MutationPlan,
        root: &Path,
        resources: &[SemanticResource],
        final_result: &Value,
    ) -> miette::Result<()> {
        if let Some(raw) = read_optional(root, &work_path(&self.plan.id, "frontier-seal.json")?)? {
            if read_optional(root, &work_path(&self.plan.id, "seal.json")?)?.is_some() {
                return Err(fail("work has conflicting completion seals"));
            }
            return self.guard_frontier_completion(mutation, root, resources, final_result, raw);
        }
        let evaluated = self.evaluate(root, resources)?;
        let node = evaluated.nodes.last().expect("nonempty tree");
        let output = evaluated
            .current
            .get(&node.id)
            .ok_or_else(|| miette::Report::new(CompletionRefusal::IncompleteReduction))?;
        let seal_path = work_path(&self.plan.id, "seal.json")?;
        let raw = read_optional(root, &seal_path)?
            .ok_or_else(|| miette::Report::new(CompletionRefusal::UnsealedResult))?;
        let seal: Seal = serde_json::from_str(&raw).map_err(fail)?;
        if seal.work_id != self.plan.id
            || seal.root_node != node.id
            || seal.root_output_revision != output.output_revision
            || seal.final_result_revision != revision(final_result)?
        {
            return Err(miette::Report::new(CompletionRefusal::ChangedResult));
        }
        for (path, raw) in evaluated.raw {
            mutation.require_unchanged(path, raw);
        }
        mutation.require_unchanged(seal_path, raw);
        mutation.require_absent(work_path(&self.plan.id, "frontier-seal.json")?);
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

    pub fn plan_consumption(
        &self,
        mutation: &mut MutationPlan,
        root: &Path,
        resources: &[SemanticResource],
        result: &lk_queue::TaskResult,
    ) -> miette::Result<()> {
        if self.context["purpose"]["extraction_scope"]["policy"] != "source-proposals-v1"
            || self.context["purpose"]["task_kind"] != "extract-concepts"
            || result.selection.is_none()
            || !matches!(
                result.target.kind,
                lk_queue::TargetKind::DailyConcepts | lk_queue::TargetKind::DocumentConcepts
            )
        {
            return Err(fail("only progressive source observations can be consumed"));
        }
        let subject: super::resources::Subject = serde_json::from_value(
            self.context
                .get("subject")
                .cloned()
                .ok_or_else(|| fail("source observations have no queue subject"))?,
        )
        .map_err(fail)?;
        let super::resources::Subject::Queue {
            task_id,
            cache_hash,
            target,
            target_revision,
        } = subject
        else {
            return Err(fail("source observations require a queue subject"));
        };
        if result.task_id != task_id
            || result.cache_hash != cache_hash
            || result.target.vault_path != target
            || result.target_revision != target_revision
        {
            return Err(fail("source observations belong to another extraction"));
        }
        let evaluated = self.evaluate(root, resources)?;
        let node = evaluated.nodes.last().expect("nonempty tree");
        let output = evaluated
            .current
            .get(&node.id)
            .ok_or_else(|| fail("source observations are incomplete"))?;
        if !output.output.unresolved.is_empty() {
            return Err(fail("source observations retain processing blockers"));
        }
        let result_revision = revision(&serde_json::to_value(result).map_err(fail)?)?;
        let path = work_path(&self.plan.id, "consumption.json")?;
        let receipt = Consumption {
            work_id: self.plan.id.clone(),
            root_node: node.id.clone(),
            root_output_revision: output.output_revision.clone(),
            task_id,
            cache_hash,
            target,
            target_revision,
            result_revision,
            consumed_at: jiff::Timestamp::now(),
        };
        for (path, raw) in evaluated.raw {
            mutation.require_unchanged(path, raw);
        }
        if let Some(raw) = read_optional(root, &path)? {
            let prior: Consumption = serde_json::from_str(&raw).map_err(fail)?;
            if prior.work_id != receipt.work_id
                || prior.root_node != receipt.root_node
                || prior.root_output_revision != receipt.root_output_revision
                || prior.task_id != receipt.task_id
                || prior.cache_hash != receipt.cache_hash
                || prior.target != receipt.target
                || prior.target_revision != receipt.target_revision
                || prior.result_revision != receipt.result_revision
            {
                return Err(fail(
                    "source observation consumption differs from the extraction",
                ));
            }
            mutation.require_unchanged(path, raw);
        } else {
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
        let children: Vec<WorkOutput> = input
            .get("children")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|child| serde_json::from_value(child["output"].clone()).unwrap())
            .collect();
        let source_questions = children
            .iter()
            .flat_map(|child| child.source_questions.clone())
            .collect();
        let unresolved = children
            .into_iter()
            .flat_map(|child| child.unresolved)
            .collect();
        WorkOutput {
            work_id: work.plan.id.clone(),
            node_id: id.into(),
            input_revision: input["input_revision"].as_str().unwrap().into(),
            summary: "Read the assigned range; source-scoped observation.".into(),
            findings: Vec::new(),
            source_questions,
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
    fn finalization_actions_bind_the_exact_request_and_refuse_corrupt_receipts() {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let mut source = resources();
        source[0].text = "One guarded source observation.".into();
        let work =
            Work::create(&session, &source, "subject", context("subject"), limits()).unwrap();
        finish(&work, &session, &source);
        let completed = work.result(root.path(), &source).unwrap();
        let request = json!({"work":completed,"instructions":"exact current generation rules"});
        let plan = work
            .plan_finalization_action(
                root.path(),
                &source,
                &completed,
                &request,
                "Record a scoped review first.".into(),
            )
            .unwrap();
        session.apply(plan).unwrap();
        assert_eq!(
            work.load_finalization_action(root.path(), &request)
                .unwrap()
                .as_deref(),
            Some("Record a scoped review first.")
        );
        let mut changed = request.clone();
        changed["instructions"] = json!("changed generation rules");
        assert!(
            work.load_finalization_action(root.path(), &changed)
                .unwrap()
                .is_none()
        );
        let path = root
            .path()
            .join(work_path(&work.plan.id, "finalization-action.json").unwrap());
        let original = std::fs::read(&path).unwrap();
        let raw =
            serde_json::to_string(&serde_json::from_slice::<Value>(&original).unwrap()).unwrap();
        let needle = "\"reason\":\"Record a scoped review first.\"";
        assert!(raw.contains(needle));
        let duplicate = raw.replacen(needle, &format!("\"reason\":null,{needle}"), 1);
        std::fs::write(&path, &duplicate).unwrap();
        assert!(
            work.load_finalization_action(root.path(), &request)
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), duplicate);
        std::fs::write(&path, &original).unwrap();
        for corrupt in [b"{}".as_slice(), b"[]".as_slice()] {
            std::fs::write(&path, corrupt).unwrap();
            assert!(
                work.load_finalization_action(root.path(), &changed)
                    .is_err()
            );
            assert!(
                work.plan_finalization_action(
                    root.path(),
                    &source,
                    &completed,
                    &changed,
                    "Still needs review.".into()
                )
                .is_err()
            );
            assert_eq!(std::fs::read(&path).unwrap(), corrupt);
        }
        std::fs::write(&path, original).unwrap();
    }

    #[test]
    fn finalization_action_publication_guards_the_completed_observation_and_manifest() {
        for changed_artifact in ["plan.json", "observation"] {
            let root = tempfile::tempdir().unwrap();
            let session = MutationSession::acquire(root.path()).unwrap();
            let mut source = resources();
            source[0].text = "One guarded source observation.".into();
            let work =
                Work::create(&session, &source, "subject", context("subject"), limits()).unwrap();
            finish(&work, &session, &source);
            let completed = work.result(root.path(), &source).unwrap();
            let request = json!({"work":completed});
            let mutation = work
                .plan_finalization_action(
                    root.path(),
                    &source,
                    &completed,
                    &request,
                    "Review required.".into(),
                )
                .unwrap();
            let path = if changed_artifact == "observation" {
                output_path(&work.plan.id, &work.plan.parts[0].id).unwrap()
            } else {
                work_path(&work.plan.id, changed_artifact).unwrap()
            };
            let path = root.path().join(path);
            let before = std::fs::read_to_string(&path).unwrap();
            std::fs::write(&path, format!("{before}\n")).unwrap();
            assert!(session.apply(mutation).is_err());
            assert!(
                !root
                    .path()
                    .join(work_path(&work.plan.id, "finalization-action.json").unwrap())
                    .exists()
            );
        }
    }

    #[test]
    fn interrupted_finalization_actions_recover_only_with_unchanged_dependencies() {
        for edit_receipted in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let session = MutationSession::acquire(root.path()).unwrap();
            let mut source = resources();
            source[0].text = "One guarded source observation.".into();
            let work =
                Work::create(&session, &source, "subject", context("subject"), limits()).unwrap();
            finish(&work, &session, &source);
            let completed = work.result(root.path(), &source).unwrap();
            let request = json!({"work":completed});
            let plan = work
                .plan_finalization_action(
                    root.path(),
                    &source,
                    &completed,
                    &request,
                    "Review required.".into(),
                )
                .unwrap();
            let plan_revision = lk_core::concept::file_revision(&encoded(&plan).unwrap());
            let plan = serde_json::to_value(plan).unwrap();
            let steps = plan["steps"].as_array().unwrap();
            let edit = steps.last().unwrap();
            let action = root.path().join(edit["path"].as_str().unwrap());
            let completed = steps.len() - usize::from(!edit_receipted);
            drop(session);
            let directory = root.path().join(".lorekeeper/mutation");
            let pending = directory.join("pending.json");
            let progress = directory.join("progress.json");
            let journal = encoded(&json!({"version":1,"plan":plan})).unwrap();
            std::fs::write(&pending, &journal).unwrap();
            std::fs::write(&action, edit["after"].as_str().unwrap()).unwrap();
            std::fs::write(&progress, encoded(&json!({
                "plan_revision":plan_revision,"completed":completed,
                "checksum":lk_core::concept::file_revision(&format!("{plan_revision}:{completed}")),
            })).unwrap()).unwrap();
            assert!(work.result(root.path(), &source).is_err());
            assert_eq!(std::fs::read_to_string(&pending).unwrap(), journal);
            let dependency = root
                .path()
                .join(output_path(&work.plan.id, &work.plan.parts[0].id).unwrap());
            let original = std::fs::read(&dependency).unwrap();
            std::fs::write(&dependency, "external edit").unwrap();
            assert!(MutationSession::acquire(root.path()).is_err());
            assert_eq!(std::fs::read_to_string(&pending).unwrap(), journal);
            std::fs::write(&dependency, original).unwrap();
            let _session = MutationSession::acquire(root.path()).unwrap();
            assert!(!pending.exists());
            assert!(!progress.exists());
            assert_eq!(
                work.load_finalization_action(root.path(), &request)
                    .unwrap()
                    .as_deref(),
                Some("Review required.")
            );
        }
    }

    fn evidence_pages(count: usize, repetitions: usize) -> Vec<SemanticResource> {
        (0..count)
            .map(|index| SemanticResource {
                address: format!("wiki/documents/evidence-{index}.md"),
                revision: format!("revision-{index}"),
                authority: SemanticAuthority::Page,
                text: "evidence ".repeat(repetitions),
            })
            .collect()
    }

    #[test]
    fn built_work_reconstructs_exact_unicode_and_empty_resources() {
        let root = tempfile::tempdir().unwrap();
        let source = vec![
            SemanticResource {
                address: "wiki/documents/원본.md".into(),
                revision: "source-revision".into(),
                authority: SemanticAuthority::Page,
                text: "## 범위\r\n\r\n한글🙂 \"quoted\" \\\t\n\n".repeat(130),
            },
            SemanticResource {
                address: "wiki/documents/empty.md".into(),
                revision: "empty-revision".into(),
                authority: SemanticAuthority::Review,
                text: String::new(),
            },
        ];
        let session = MutationSession::acquire(root.path()).unwrap();
        let work = Work::create(&session, &source, "subject", context("one"), limits()).unwrap();
        let loaded = Work::load(root.path(), &work.plan.id).unwrap();
        assert_eq!(encoded(&work).unwrap(), encoded(&loaded).unwrap());
        let mut reconstructed = vec![String::new(); source.len()];
        let mut empty_ranges = 0;
        for part in &work.plan.parts {
            let fresh = work.input(root.path(), &part.id, &source).unwrap();
            let restored = loaded.input(root.path(), &part.id, &source).unwrap();
            assert_eq!(fresh, restored);
            assert!(encoded(&fresh).unwrap().len() <= work.max_input_bytes);
            for range in fresh["input"]["ranges"].as_array().unwrap() {
                let resource = range["range"]["resource"].as_u64().unwrap() as usize;
                let text = range["text"].as_str().unwrap();
                reconstructed[resource].push_str(text);
                empty_ranges += usize::from(text.is_empty());
            }
        }
        assert_eq!(
            reconstructed,
            source
                .iter()
                .map(|resource| resource.text.clone())
                .collect::<Vec<_>>()
        );
        assert_eq!(empty_ranges, 1);
        let mut changed = source.clone();
        changed[0].text.push('!');
        assert!(loaded.progress(root.path(), &changed).is_err());
    }

    #[test]
    fn budgeted_topology_covers_every_leaf_once_without_unary_reductions() {
        for count in [1, 3, 5, 8] {
            let source = evidence_pages(count, 1100);
            let work = Work::build(
                &source,
                "subject",
                context("one"),
                WorkLimits {
                    max_input_bytes: 12_000,
                    max_result_bytes: 1024,
                },
            )
            .unwrap();
            assert_eq!(work.plan.parts.len(), count);
            let nodes = work.nodes().unwrap();
            let mut children = BTreeSet::new();
            for node in &nodes {
                if let NodeKind::Reduce { children: ids } = &node.kind {
                    assert!(ids.len() >= 2);
                    for id in ids {
                        assert!(children.insert(id));
                    }
                }
            }
            assert_eq!(children.len(), nodes.len() - 1);
            assert!(!children.contains(&nodes.last().unwrap().id));
            assert_eq!(
                nodes
                    .iter()
                    .filter(|node| matches!(node.kind, NodeKind::Inspect { .. }))
                    .count(),
                count
            );
            if count > 2 {
                assert!(nodes.iter().any(
                    |node| matches!(&node.kind, NodeKind::Reduce { children } if children.len() > 2)
                ));
            }
        }
    }

    #[test]
    fn reduction_reservation_counts_exact_envelope_and_full_child_bounds() {
        let source = evidence_pages(3, 1100);
        let mut purpose = context("one");
        purpose["purpose"]["notes"] = "quoted \" value\n\\".repeat(20).into();
        let mut work = Work::build(
            &source,
            "subject",
            purpose,
            WorkLimits {
                max_input_bytes: 12_000,
                max_result_bytes: 1024,
            },
        )
        .unwrap();
        let leaves = work
            .nodes()
            .unwrap()
            .into_iter()
            .filter(|node| matches!(node.kind, NodeKind::Inspect { .. }))
            .collect::<Vec<_>>();
        assert_eq!(leaves.len(), 3);
        let reserved = work.reserved_reduce_bytes(&leaves).unwrap();
        let placeholder = "0".repeat(64);
        let outputs = leaves
            .iter()
            .map(|node| {
                let range = &node.ranges[0];
                let mut output = WorkOutput {
                    work_id: work.plan.id.clone(),
                    node_id: node.id.clone(),
                    input_revision: placeholder.clone(),
                    summary: String::new(),
                    findings: vec![WorkFinding {
                        statement: "A source-scoped observation".into(),
                        scope: None,
                        effective_time: None,
                        attribution: None,
                        evidence: vec![SemanticRange {
                            resource: range.resource,
                            start: range.start,
                            end: range.start + 1,
                        }],
                    }],
                    source_questions: vec![],
                    unresolved: vec![],
                    resolutions: vec![],
                };
                output.summary =
                    "x".repeat(work.plan.max_result_bytes - encoded(&output).unwrap().len());
                assert_eq!(encoded(&output).unwrap().len(), work.plan.max_result_bytes);
                output
            })
            .collect::<Vec<_>>();
        let mut request = work.reduction_request(&placeholder,
            outputs.iter().map(|output| json!({"node_id": output.node_id, "result_revision": placeholder, "output": output})).collect(),
            work.evidence_guards(&outputs.iter().collect::<Vec<_>>()).unwrap(),
        );
        request["input_revision"] = placeholder.into();
        assert_eq!(encoded(&request).unwrap().len(), reserved);
        assert_eq!(work.budgeted_nodes(reserved).unwrap().len(), 4);
        let below = work.budgeted_nodes(reserved - 1).unwrap();
        assert_eq!(below.len(), 5);
        assert!(below.iter().all(
            |node| !matches!(&node.kind, NodeKind::Reduce { children } if children.len() > 2)
        ));
        work.plan.max_result_bytes = usize::MAX;
        assert!(work.reserved_reduce_bytes(&leaves).is_err());
    }

    #[test]
    fn legacy_manifest_retains_binary_nodes_seal_and_exact_bytes() {
        let root = tempfile::tempdir().unwrap();
        let source = resources();
        let session = MutationSession::acquire(root.path()).unwrap();
        let mut legacy = Work::build(&source, "subject", context("one"), limits()).unwrap();
        legacy.reduction = ReductionPolicy::BinaryV1;
        let raw = encoded(&legacy).unwrap();
        assert!(
            !serde_json::from_str::<Value>(&raw)
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("reduction")
        );
        let path = work_path(&legacy.plan.id, "plan.json").unwrap();
        let mut mutation = MutationPlan::default();
        mutation
            .publish_expected(root.path(), &path, None, raw.clone())
            .unwrap();
        session.apply(mutation).unwrap();
        let first = legacy.plan.parts[0].id.clone();
        legacy
            .record(
                &session,
                draft(&legacy, root.path(), &first, &source),
                &source,
            )
            .unwrap();
        let resumed = Work::create(&session, &source, "subject", context("one"), limits()).unwrap();
        assert_eq!(resumed, legacy);
        assert_eq!(
            std::fs::read_to_string(root.path().join(&path)).unwrap(),
            raw
        );
        assert_eq!(
            resumed.progress(root.path(), &source).unwrap().total,
            resumed.plan.parts.len() * 2 - 1
        );
        finish(&resumed, &session, &source);
        let result = json!({"task_id":"task", "body":"sealed legacy result"});
        resumed.seal(&session, &source, result.clone()).unwrap();
        let expected = Work::build(&source, "subject", context("one"), limits())
            .unwrap()
            .with_saved_policy(root.path())
            .unwrap();
        expected
            .guard_completion(&mut MutationPlan::default(), root.path(), &source, &result)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.path().join(&path)).unwrap(),
            raw
        );
        let mut unknown: Value = serde_json::from_str(&raw).unwrap();
        unknown["reduction"] = "unknown-policy".into();
        std::fs::write(root.path().join(&path), encoded(&unknown).unwrap()).unwrap();
        assert!(Work::load(root.path(), &legacy.plan.id).is_err());
        assert!(
            Work::build(&source, "subject", context("one"), limits())
                .unwrap()
                .with_saved_policy(root.path())
                .is_err()
        );
    }

    #[test]
    fn five_inspections_reduce_once_and_preserve_all_source_questions() {
        let root = tempfile::tempdir().unwrap();
        let source = evidence_pages(5, 25_000);
        let session = MutationSession::acquire(root.path()).unwrap();
        let work = Work::create(
            &session,
            &source,
            "subject",
            context("one"),
            WorkLimits {
                max_input_bytes: 262_144,
                max_result_bytes: 32_768,
            },
        )
        .unwrap();
        assert_eq!(work.plan.parts.len(), 5);
        assert_eq!(work.nodes().unwrap().len(), 6);
        let mut questions = Vec::new();
        for (index, part) in work.plan.parts.iter().enumerate() {
            let mut output = draft(&work, root.path(), &part.id, &source);
            let range = &part.ranges[0];
            for question in 0..if index == 0 { 10 } else { 8 } {
                output.source_questions.push(WorkQuestion {
                    id: format!("{}:{question}", part.id),
                    question: "The source leaves its authority and effective scope unconfirmed. "
                        .repeat(4),
                    evidence: vec![SemanticRange {
                        resource: range.resource,
                        start: range.start,
                        end: range.start + 1,
                    }],
                });
            }
            questions.extend(output.source_questions.clone());
            work.record(&session, output, &source).unwrap();
        }
        let progress = work.progress(root.path(), &source).unwrap();
        assert_eq!(progress.completed, 5);
        assert_eq!(progress.ready.len(), 1);
        let request = work
            .input(root.path(), &progress.root_node, &source)
            .unwrap();
        assert_eq!(request["children"].as_array().unwrap().len(), 5);
        assert!(encoded(&request).unwrap().len() <= work.max_input_bytes);
        let output = draft(&work, root.path(), &progress.root_node, &source);
        assert_eq!(output.source_questions, questions);
        assert_eq!(questions.len(), 42);
        let mut omitted = output.clone();
        omitted.source_questions.pop();
        assert!(work.record(&session, omitted, &source).is_err());
        let mut oversized = output.clone();
        oversized.summary = "x".repeat(work.plan.max_result_bytes);
        assert!(work.record(&session, oversized, &source).is_err());
        assert!(!work.progress(root.path(), &source).unwrap().complete);
        work.record(&session, output, &source).unwrap();
        assert!(work.progress(root.path(), &source).unwrap().complete);
    }

    #[test]
    fn prepared_inspection_and_record_progress_match_the_revision_pinned_read_contract() {
        let root = tempfile::tempdir().unwrap();
        let source = resources();
        let session = MutationSession::acquire(root.path()).unwrap();
        let work = Work::create(&session, &source, "subject", context("one"), limits()).unwrap();
        let mut kinds = BTreeSet::new();
        loop {
            let (before, input) = work.prepare_inspection(root.path(), &source).unwrap();
            assert_eq!(
                serde_json::to_value(&before).unwrap(),
                serde_json::to_value(work.progress(root.path(), &source).unwrap()).unwrap(),
            );
            if before.complete {
                assert!(input.is_none());
                break;
            }
            let node = before.ready.first().unwrap();
            let input = input.unwrap();
            assert_eq!(input, work.input(root.path(), node, &source).unwrap());
            assert_eq!(input["node_id"].as_str(), Some(node.as_str()));
            kinds.insert(input["kind"].as_str().unwrap().to_owned());
            let after = work
                .record(&session, draft(&work, root.path(), node, &source), &source)
                .unwrap();
            assert!(after.completed > before.completed);
            assert!(!after.ready.contains(node));
            assert_eq!(
                serde_json::to_value(after).unwrap(),
                serde_json::to_value(work.progress(root.path(), &source).unwrap()).unwrap(),
            );
        }
        assert_eq!(kinds, BTreeSet::from(["inspect".into(), "reduce".into()]));
    }

    #[test]
    fn prepared_inspections_preserve_ready_order_and_exclude_dependent_nodes() {
        let root = tempfile::tempdir().unwrap();
        let source = resources();
        let session = MutationSession::acquire(root.path()).unwrap();
        let work = Work::create(&session, &source, "subject", context("one"), limits()).unwrap();
        let (before, requests) = work.prepare_inspections(root.path(), &source, 3).unwrap();
        assert!(before.ready.len() > 1);
        assert_eq!(requests.len(), before.ready.len().min(3));
        assert_eq!(
            serde_json::to_value(&before).unwrap(),
            serde_json::to_value(work.progress(root.path(), &source).unwrap()).unwrap()
        );
        let (_, single) = work.prepare_inspection(root.path(), &source).unwrap();
        assert_eq!(single.as_ref(), requests.first());
        let ready = before
            .ready
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        for (request, id) in requests.iter().zip(&before.ready) {
            assert_eq!(request["node_id"].as_str(), Some(id.as_str()));
            assert_eq!(request, &work.input(root.path(), id, &source).unwrap());
            for child in request["children"].as_array().into_iter().flatten() {
                assert!(!ready.contains(child["node_id"].as_str().unwrap()));
            }
        }
        let first = &before.ready[0];
        work.record(&session, draft(&work, root.path(), first, &source), &source)
            .unwrap();
        let (after, _) = work.prepare_inspections(root.path(), &source, 3).unwrap();
        assert_eq!(after.completed, 1);
        assert!(!after.ready.contains(first));
        let parent = work
            .nodes()
            .unwrap()
            .into_iter()
            .find(|node| matches!(&node.kind, NodeKind::Reduce { children } if children.contains(first)))
            .unwrap();
        assert!(!after.ready.contains(&parent.id));
        finish(&work, &session, &source);
        let (complete, requests) = work.prepare_inspections(root.path(), &source, 3).unwrap();
        assert!(complete.complete);
        assert!(requests.is_empty());
    }

    #[test]
    fn prepared_inspections_keep_fitting_prefix_before_capacity_and_refuse_corruption() {
        let root = tempfile::tempdir().unwrap();
        let source = evidence_pages(8, 250);
        let session = MutationSession::acquire(root.path()).unwrap();
        let work = Work::create(
            &session,
            &source,
            "subject",
            context("one"),
            WorkLimits {
                max_input_bytes: 3000,
                max_result_bytes: 1500,
            },
        )
        .unwrap();
        assert!(work.plan.parts.len() >= 8);
        for part in work.plan.parts.iter().take(work.plan.parts.len() - 1) {
            let mut output = draft(&work, root.path(), &part.id, &source);
            output.source_questions.push(WorkQuestion {
                id: format!("{}:source:0", part.id),
                question: "The source does not establish effective scope. ".repeat(10),
                evidence: vec![part.ranges[0].clone()],
            });
            work.record(&session, output, &source).unwrap();
        }
        loop {
            let ready = work.progress(root.path(), &source).unwrap().ready;
            let mut recorded = false;
            for id in ready {
                let input = match work.input(root.path(), &id, &source) {
                    Ok(input) => input,
                    Err(error) if error.downcast_ref::<InputCapacity>().is_some() => continue,
                    Err(error) => panic!("{error:?}"),
                };
                if input["kind"] == "reduce" {
                    work.record(&session, draft(&work, root.path(), &id, &source), &source)
                        .unwrap();
                    recorded = true;
                }
            }
            if !recorded {
                break;
            }
        }
        let (before, single) = work.prepare_inspection(root.path(), &source).unwrap();
        assert!(before.ready.len() >= 2);
        let single = single.unwrap();
        assert_eq!(single["kind"], "inspect");
        assert!(
            work.input(root.path(), &before.ready[1], &source)
                .unwrap_err()
                .downcast_ref::<InputCapacity>()
                .is_some()
        );
        let (_, requests) = work.prepare_inspections(root.path(), &source, 2).unwrap();
        assert_eq!(requests, vec![single]);
        let path = root
            .path()
            .join(output_path(&work.plan.id, &work.plan.parts[0].id).unwrap());
        let raw = std::fs::read(&path).unwrap();
        let mut corrupt: Value = serde_json::from_slice(&raw).unwrap();
        corrupt["output"]["summary"] = "Changed without matching checksum.".into();
        std::fs::write(&path, encoded(&corrupt).unwrap()).unwrap();
        let error = work
            .prepare_inspections(root.path(), &source, 2)
            .err()
            .unwrap();
        assert!(error.downcast_ref::<InputCapacity>().is_none());
        std::fs::write(path, raw).unwrap();
        let mut deferred = false;
        for _ in 0..before.total {
            match work.prepare_inspection(root.path(), &source) {
                Ok((_, Some(request))) => {
                    let id = request["node_id"].as_str().unwrap();
                    work.record(&session, draft(&work, root.path(), id, &source), &source)
                        .unwrap();
                }
                Ok(_) => panic!("oversized reduction must remain incomplete"),
                Err(error) => {
                    assert!(error.downcast_ref::<InputCapacity>().is_some());
                    deferred = true;
                    break;
                }
            }
        }
        assert!(deferred);
    }

    #[test]
    fn completed_observation_result_keeps_exact_derived_read_guards_without_a_seal() {
        let root = tempfile::tempdir().unwrap();
        let source = resources();
        let session = MutationSession::acquire(root.path()).unwrap();
        let work = Work::create(&session, &source, "subject", context("one"), limits()).unwrap();
        assert!(work.result_with_guards(root.path(), &source).is_err());
        assert!(
            work.result_if_complete_with_guards(root.path(), &source)
                .unwrap()
                .is_none()
        );
        let first = &work.plan.parts[0].id;
        work.record(&session, draft(&work, root.path(), first, &source), &source)
            .unwrap();
        assert!(
            work.result_if_complete_with_guards(root.path(), &source)
                .unwrap()
                .is_none()
        );
        finish(&work, &session, &source);
        let (result, guards) = work.result_with_guards(root.path(), &source).unwrap();
        assert_eq!(
            work.result_if_complete_with_guards(root.path(), &source)
                .unwrap(),
            Some((result.clone(), guards.clone()))
        );
        assert_eq!(result, work.result(root.path(), &source).unwrap());
        assert_eq!(
            guards.len(),
            work.progress(root.path(), &source).unwrap().total + 1
        );
        let manifest = work_path(&work.plan.id, "plan.json").unwrap();
        let node = output_path(&work.plan.id, &work.plan.parts[0].id).unwrap();
        assert!(guards.contains_key(&manifest));
        assert!(guards.contains_key(&node));
        assert!(
            !root
                .path()
                .join(work_path(&work.plan.id, "seal.json").unwrap())
                .exists()
        );

        let guarded = || {
            let mut mutation = MutationPlan::default();
            for (path, raw) in &guards {
                mutation.require_unchanged(path, raw.clone());
            }
            mutation
        };
        session.apply(guarded()).unwrap();
        for path in [&node, &manifest] {
            let raw = guards.get(path).unwrap();
            std::fs::write(root.path().join(path), format!("{raw}\n")).unwrap();
            let mut mutation = guarded();
            mutation.create("guarded-publication.md", "must not publish".into());
            assert!(session.apply(mutation).is_err());
            assert!(!root.path().join("guarded-publication.md").exists());
            assert_eq!(
                std::fs::read_to_string(root.path().join(path)).unwrap(),
                format!("{raw}\n")
            );
            assert!(
                !root
                    .path()
                    .join(".lorekeeper/mutation/pending.json")
                    .exists()
            );
            std::fs::write(root.path().join(path), raw).unwrap();
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
        let seal_path = root
            .path()
            .join(work_path(&work.plan.id, "seal.json").unwrap());
        let seal_before = std::fs::read_to_string(&seal_path).unwrap();
        let mut retry = MutationPlan::default();
        work.capture_completion(root.path(), &source)
            .unwrap()
            .plan_seal(&work, &mut retry, root.path(), &final_result)
            .unwrap();
        retry.create("retry-publication.md", "must not publish".into());
        std::fs::write(&seal_path, format!("{seal_before}\n")).unwrap();
        assert!(session.apply(retry).is_err());
        assert!(!root.path().join("retry-publication.md").exists());
        assert!(
            !root
                .path()
                .join(".lorekeeper/mutation/pending.json")
                .exists()
        );
        std::fs::write(seal_path, seal_before).unwrap();
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
    fn manual_reduction_at_the_result_limit_keeps_short_inherited_questions() {
        let root = tempfile::tempdir().unwrap();
        let source = resources();
        let session = MutationSession::acquire(root.path()).unwrap();
        let work = Work::create(
            &session,
            &source,
            "subject",
            context("one"),
            WorkLimits {
                max_input_bytes: 4096,
                max_result_bytes: 1024,
            },
        )
        .unwrap();
        assert!(work.plan.parts.len() > 1);
        for (index, part) in work.plan.parts.iter().enumerate() {
            let mut output = draft(&work, root.path(), &part.id, &source);
            if index == 0 {
                let range = &part.ranges[0];
                output.unresolved.push(WorkQuestion {
                    id: "legacy:q".into(),
                    question: "Why?".into(),
                    evidence: vec![SemanticRange {
                        resource: range.resource,
                        start: range.start,
                        end: range.start + 1,
                    }],
                });
            }
            work.record(&session, output, &source).unwrap();
        }
        let before = work.progress(root.path(), &source).unwrap();
        let mut output = before
            .ready
            .iter()
            .map(|node| draft(&work, root.path(), node, &source))
            .find(|output| !output.unresolved.is_empty())
            .unwrap();
        output.summary.clear();
        output.summary = "s".repeat(work.plan.max_result_bytes - encoded(&output).unwrap().len());
        assert_eq!(encoded(&output).unwrap().len(), work.plan.max_result_bytes);
        let node = output.node_id.clone();
        let expected = output.clone();
        let after = work.record(&session, output, &source).unwrap();
        assert_eq!(after.completed, before.completed + 1);
        let raw = read_optional(root.path(), &output_path(&work.plan.id, &node).unwrap())
            .unwrap()
            .unwrap();
        let stored: StoredOutput = serde_json::from_str(&raw).unwrap();
        assert_eq!(stored.output, expected);
        assert!(stored.retained.is_none());
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
                output.unresolved.push(WorkQuestion {
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

    #[cfg(unix)]
    #[test]
    fn native_reduction_resumes_legacy_closed_history_without_rewriting_paid_nodes() {
        use crate::commands::execution::observation::{ObservationResponse, decode_observation};
        let root = tempfile::tempdir().unwrap();
        let source = resources();
        let session = MutationSession::acquire(root.path()).unwrap();
        let mut work = Work::build(&source, "subject", context("one"), limits()).unwrap();
        work.reduction = ReductionPolicy::BinaryV1;
        assert!(work.plan.parts.len() >= 3);
        let path = work_path(&work.plan.id, "plan.json").unwrap();
        let mut plan = MutationPlan::default();
        plan.publish_expected(root.path(), &path, None, encoded(&work).unwrap())
            .unwrap();
        session.apply(plan).unwrap();
        for (index, part) in work.plan.parts.iter().enumerate() {
            let mut output = draft(&work, root.path(), &part.id, &source);
            if index + 1 == work.plan.parts.len() {
                output.source_questions.push(WorkQuestion {
                    id: format!("{}:legacy-question", part.id),
                    question: "Was the release actually verified?".into(),
                    evidence: vec![part.ranges[0].clone()],
                });
            }
            work.record(&session, output, &source).unwrap();
        }
        let mut saved =
            BTreeMap::from([(path.clone(), std::fs::read(root.path().join(path)).unwrap())]);
        let root_children = match &work.nodes().unwrap().last().unwrap().kind {
            NodeKind::Reduce { children } => children.clone(),
            NodeKind::Inspect { .. } => panic!("expected a reduction"),
        };
        let mut closed = None;
        loop {
            let progress = work.progress(root.path(), &source).unwrap();
            if progress.ready == [progress.root_node.clone()] {
                break;
            }
            for id in progress.ready {
                let mut output = draft(&work, root.path(), &id, &source);
                if root_children.contains(&id) && !output.source_questions.is_empty() {
                    let question = output.source_questions.remove(0);
                    let resolution = WorkResolution {
                        id: question.id,
                        reason: "The cited release record supplies verification.".into(),
                        evidence: question.evidence,
                    };
                    closed = Some(resolution.clone());
                    output.resolutions.push(resolution);
                }
                work.record(&session, output, &source).unwrap();
            }
        }
        let progress = work.progress(root.path(), &source).unwrap();
        for node in work
            .nodes()
            .unwrap()
            .into_iter()
            .filter(|node| node.id != progress.root_node)
        {
            let path = output_path(&work.plan.id, &node.id).unwrap();
            saved.insert(path.clone(), std::fs::read(root.path().join(path)).unwrap());
        }
        let request = work
            .input(root.path(), &progress.root_node, &source)
            .unwrap();
        assert!(request["children"].as_array().unwrap().iter().any(|child| {
            !child["output"]["resolutions"]
                .as_array()
                .unwrap()
                .is_empty()
        }));
        let mut rejected = draft(&work, root.path(), &progress.root_node, &source);
        rejected.resolutions.push(closed.unwrap());
        assert!(
            work.record(&session, rejected, &source)
                .err()
                .unwrap()
                .to_string()
                .contains("unknown inherited concern")
        );
        let canonical = encoded(&request).unwrap();
        let reply = json!({"status":"complete","observation":{"summary":"All child observations inspected; closed history is retained in the children.","findings":[],"source_questions":[],"unresolved":[],"resolutions":[]},"action":null});
        let ObservationResponse::Complete(output) =
            decode_observation(reply, &request, work.plan.max_result_bytes).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            encoded(
                &work
                    .input(root.path(), &progress.root_node, &source)
                    .unwrap()
            )
            .unwrap(),
            canonical
        );
        work.record(&session, output, &source).unwrap();
        assert!(work.progress(root.path(), &source).unwrap().complete);
        for (path, before) in saved {
            assert_eq!(std::fs::read(root.path().join(path)).unwrap(), before);
        }
    }

    #[cfg(unix)]
    #[test]
    fn inherited_questions_survive_intermediate_and_root_capacity_without_rewriting_paid_nodes() {
        use crate::commands::execution::observation::{ObservationResponse, decode_observation};
        let root = tempfile::tempdir().unwrap();
        let source = evidence_pages(8, 1100);
        let session = MutationSession::acquire(root.path()).unwrap();
        let mut purpose = context("one");
        purpose["subject"] = json!(crate::commands::work::resources::Subject::Queue {
            task_id: "task".into(),
            cache_hash: "cache".into(),
            target: "wiki/concepts/subject.md".into(),
            target_revision: "target-revision".into(),
        });
        purpose["purpose"]["task_kind"] = json!(lk_queue::TaskKind::SynthesizeConcept);
        let work = Work::create(
            &session,
            &source,
            "subject",
            purpose,
            WorkLimits {
                max_input_bytes: 12_000,
                max_result_bytes: 1500,
            },
        )
        .unwrap();
        assert!(work.plan.parts.len() >= 8);
        for part in &work.plan.parts {
            let mut output = draft(&work, root.path(), &part.id, &source);
            output.source_questions.push(WorkQuestion {
                id: format!("{}:source:0", part.id),
                question: "Which source supplies the missing scope? ".repeat(15),
                evidence: vec![part.ranges[0].clone()],
            });
            assert!(encoded(&output).unwrap().len() < work.plan.max_result_bytes);
            work.record(&session, output, &source).unwrap();
        }
        let manifest = work_path(&work.plan.id, "plan.json").unwrap();
        let saved = std::iter::once(manifest)
            .chain(
                work.plan
                    .parts
                    .iter()
                    .map(|part| output_path(&work.plan.id, &part.id).unwrap()),
            )
            .map(|path| (path.clone(), std::fs::read(root.path().join(path)).unwrap()))
            .collect::<BTreeMap<_, _>>();
        let root_id = work.progress(root.path(), &source).unwrap().root_node;
        let mut packed = BTreeSet::new();
        let mut frontier_verified = false;
        loop {
            let progress = work.progress(root.path(), &source).unwrap();
            if progress.complete {
                break;
            }
            if progress.ready == [root_id.clone()] {
                let completed = work
                    .capture_frontier(root.path(), &source)
                    .unwrap()
                    .unwrap();
                let CompletionAuthority::Frontier(proof) = completed.authority else {
                    panic!()
                };
                let proof_value = serde_json::to_value(&proof).unwrap();
                assert!(
                    proof_value["nodes"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|node| packed.contains(node["node_id"].as_str().unwrap()))
                );
                let seal = FrontierSeal {
                    schema: "work-frontier-seal-v1".into(),
                    proof,
                    final_result_revision: "0".repeat(64),
                };
                let seal = FrontierSeal::decode(&encoded(&seal).unwrap(), &work).unwrap();
                seal.verify_stored(root.path(), &work).unwrap();
                frontier_verified = true;
            }
            for id in progress.ready {
                let request = work.input(root.path(), &id, &source).unwrap();
                assert!(encoded(&request).unwrap().len() <= work.max_input_bytes);
                let ObservationResponse::Complete(output) = decode_observation(json!({
                    "status":"complete", "observation":{"summary":"Every child and its evidence mapping were inspected; source unknowns remain open.","findings":[],"source_questions":[],"unresolved":[],"resolutions":[]}, "action":null
                }), &request, work.plan.max_result_bytes).unwrap() else { panic!() };
                let full = output.clone();
                if encoded(&output).unwrap().len() > work.plan.max_result_bytes {
                    packed.insert(id.clone());
                }
                work.record(&session, output, &source).unwrap();
                let raw = std::fs::read_to_string(
                    root.path().join(output_path(&work.plan.id, &id).unwrap()),
                )
                .unwrap();
                let stored: StoredOutput = serde_json::from_str(&raw).unwrap();
                assert_eq!(stored.output, full);
                assert_eq!(stored.output_revision, revision(&full).unwrap());
                stored.validate_bounds(&work).unwrap();
                if packed.contains(&id) {
                    let wire: Value = serde_json::from_str(&raw).unwrap();
                    assert_eq!(wire["schema"], "work-output-retained-questions-v1");
                    assert_eq!(wire["output"]["source_questions"], Value::Null);
                    assert_eq!(
                        wire["retained_questions"]["source_questions"]
                            .as_array()
                            .unwrap()
                            .len(),
                        full.source_questions.len()
                    );
                    #[derive(Deserialize)]
                    #[serde(deny_unknown_fields)]
                    struct OldReader {
                        #[serde(rename = "output")]
                        _output: WorkOutput,
                        #[serde(rename = "output_revision")]
                        _revision: String,
                    }
                    assert!(serde_json::from_str::<OldReader>(&raw).is_err());
                }
            }
        }
        assert!(packed.contains(&root_id));
        assert!(packed.iter().any(|id| id != &root_id));
        assert!(frontier_verified);
        let result = work.result(root.path(), &source).unwrap();
        assert_eq!(
            result["output"]["source_questions"]
                .as_array()
                .unwrap()
                .len(),
            work.plan.parts.len()
        );
        let final_result = json!({"body":"Source uncertainties remain explicitly attributed."});
        work.seal(&session, &source, final_result.clone()).unwrap();
        for (path, before) in saved {
            assert_eq!(std::fs::read(root.path().join(path)).unwrap(), before);
        }
        let mut publication = MutationPlan::default();
        work.guard_completion(&mut publication, root.path(), &source, &final_result)
            .unwrap();
        work.plan_publication(&mut publication, root.path(), &final_result)
            .unwrap();
        publication
            .publish_expected(
                root.path(),
                work_path(&work.plan.id, "final.json").unwrap(),
                None,
                encoded(&final_result).unwrap(),
            )
            .unwrap();
        session.apply(publication).unwrap();
        assert_eq!(
            prune_published(root.path(), None, i64::MAX)
                .unwrap()
                .removed,
            1
        );
        assert!(
            root.path()
                .join(work_path(&work.plan.id, "plan.json").unwrap())
                .exists()
        );
        assert_eq!(
            prune_published(root.path(), Some(&session), i64::MAX)
                .unwrap()
                .removed,
            1
        );
        assert!(
            !root
                .path()
                .join(work_path(&work.plan.id, "").unwrap())
                .exists()
        );
    }

    #[test]
    fn source_questions_keep_their_kind_and_evidence_through_reduction() {
        let range = SemanticRange {
            resource: 0,
            start: 0,
            end: 10,
        };
        let node = Node {
            id: "parent".into(),
            kind: NodeKind::Reduce {
                children: vec!["a".into(), "b".into()],
            },
            ranges: vec![range.clone()],
        };
        let question = WorkQuestion {
            id: "a:authority".into(),
            question: "Which policy is approved?".into(),
            evidence: vec![range.clone()],
        };
        let child: WorkOutput = serde_json::from_value(json!({
            "work_id":"w", "node_id":"a", "input_revision":"r", "summary":"Sources disagree; no deciding authority is supplied.",
            "findings":[], "source_questions":[question], "unresolved":[], "resolutions":[]
        })).unwrap();
        let mut parent = child.clone();
        parent.node_id = "parent".into();
        assert!(validate_output(&parent, &node, &[&child]).is_ok());
        parent.source_questions.clear();
        assert!(validate_output(&parent, &node, &[&child]).is_err());
        parent.unresolved.push(question.clone());
        assert!(validate_output(&parent, &node, &[&child]).is_err());
        let blocker = parent.clone();
        parent.unresolved.clear();
        parent.source_questions.push(question.clone());
        assert!(validate_output(&parent, &node, &[&blocker]).is_err());
        parent.unresolved.push(question.clone());
        assert!(validate_output(&parent, &node, &[]).is_err());
        parent.unresolved.clear();
        parent.source_questions[0].evidence[0].end += 1;
        assert!(validate_output(&parent, &node, &[]).is_err());
        parent.source_questions.clear();
        parent.resolutions.push(WorkResolution {
            id: question.id,
            reason: "The signed decision explicitly supersedes both drafts.".into(),
            evidence: vec![range],
        });
        assert!(validate_output(&parent, &node, &[&child]).is_ok());
    }

    #[test]
    fn adding_source_questions_preserves_existing_observation_revisions() {
        let encoded = r#"{"work_id":"w","node_id":"n","input_revision":"r","summary":"s","findings":[],"unresolved":[],"resolutions":[]}"#;
        let output: WorkOutput = serde_json::from_str(encoded).unwrap();
        assert!(output.source_questions.is_empty());
        assert_eq!(serde_json::to_string(&output).unwrap(), encoded);
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
        assert!(
            work.result_if_complete_with_guards(root.path(), &source)
                .is_err()
        );
        std::fs::write(&path, original).unwrap();
        let mut changed = source.clone();
        changed[0].text.push('!');
        assert!(work.progress(root.path(), &changed).is_err());
        assert!(
            work.result_if_complete_with_guards(root.path(), &changed)
                .is_err()
        );
        let journal = root.path().join(".lorekeeper/mutation/pending.json");
        std::fs::write(&journal, "{}").unwrap();
        assert!(work.progress(root.path(), &source).is_err());
        assert!(
            work.result_if_complete_with_guards(root.path(), &source)
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(&journal).unwrap(), "{}");
        std::fs::remove_file(journal).unwrap();
        assert_eq!(work.progress(root.path(), &source).unwrap().completed, 1);
    }

    #[test]
    fn captured_record_matches_fresh_planning_and_refuses_changed_artifacts() {
        for artifact in [
            "observation",
            "absent-observation",
            "other-observation",
            "manifest",
        ] {
            let root = tempfile::tempdir().unwrap();
            let source = resources();
            let session = MutationSession::acquire(root.path()).unwrap();
            let work =
                Work::create(&session, &source, "subject", context("one"), limits()).unwrap();
            let id = &work.plan.parts[0].id;
            let other = &work.plan.parts[1].id;
            for node in [id, other] {
                if artifact != "absent-observation" || node != id {
                    work.record(&session, draft(&work, root.path(), node, &source), &source)
                        .unwrap();
                }
            }
            let mut output = draft(&work, root.path(), id, &source);
            output.summary = "A corrected observation from the exact assigned source.".into();
            let fresh = work
                .plan_record(&session, output.clone(), &source, None, Vec::new())
                .unwrap();
            let captured = work.evaluate(root.path(), &source).unwrap();
            let equivalent = work
                .plan_evaluated_record(root.path(), output.clone(), captured, None, Vec::new())
                .unwrap();
            assert_eq!(encoded(&fresh).unwrap(), encoded(&equivalent).unwrap());

            let captured = work.evaluate(root.path(), &source).unwrap();
            let changed = match artifact {
                "manifest" => work_path(&work.plan.id, "plan.json").unwrap(),
                "other-observation" => output_path(&work.plan.id, other).unwrap(),
                _ => output_path(&work.plan.id, id).unwrap(),
            };
            let before = read_optional(root.path(), &changed).unwrap();
            let after = format!("{}\n", before.as_deref().unwrap_or("external observation"));
            std::fs::write(root.path().join(&changed), &after).unwrap();
            if let Ok(mutation) =
                work.plan_evaluated_record(root.path(), output, captured, None, Vec::new())
            {
                assert!(session.apply(mutation).is_err());
            }
            assert_eq!(
                read_optional(root.path(), &changed).unwrap().as_deref(),
                Some(after.as_str())
            );
            assert!(
                !root
                    .path()
                    .join(".lorekeeper/mutation/pending.json")
                    .exists()
            );
        }
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
        output.source_questions.push(WorkQuestion {
            id: format!("{}:authority", part.id),
            question: "Who approved the scoped claim?".into(),
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
        let reused = new
            .reuse_from(&session, &old, &part.id, &new_part.id, &changed)
            .unwrap();
        assert_eq!(
            serde_json::to_value(reused).unwrap(),
            serde_json::to_value(new.progress(root.path(), &changed).unwrap()).unwrap(),
        );
        let raw = read_optional(
            root.path(),
            &output_path(&new.plan.id, &new_part.id).unwrap(),
        )
        .unwrap()
        .unwrap();
        let stored: StoredOutput = serde_json::from_str(&raw).unwrap();
        assert_eq!(stored.output.findings[0].evidence[0].resource, 2);
        assert_eq!(stored.output.source_questions[0].evidence[0].resource, 2);
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

        let other = &new
            .plan
            .parts
            .iter()
            .find(|candidate| candidate.id != new_part.id)
            .unwrap()
            .id;
        new.record(
            &session,
            draft(&new, root.path(), other, &changed),
            &changed,
        )
        .unwrap();
        let other_path = output_path(&new.plan.id, other).unwrap();
        let mut corrupt: Value =
            serde_json::from_str(&read_optional(root.path(), &other_path).unwrap().unwrap())
                .unwrap();
        corrupt["output"]["summary"] = "An unverified external replacement.".into();
        std::fs::write(root.path().join(other_path), encoded(&corrupt).unwrap()).unwrap();
        let before = read_optional(
            root.path(),
            &output_path(&new.plan.id, &new_part.id).unwrap(),
        )
        .unwrap();
        assert!(
            new.reuse_from(&session, &old, &part.id, &new_part.id, &changed)
                .is_err()
        );
        assert_eq!(
            read_optional(
                root.path(),
                &output_path(&new.plan.id, &new_part.id).unwrap()
            )
            .unwrap(),
            before
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
    fn frontier_fixture(session: &MutationSession) -> (Work, Vec<SemanticResource>) {
        let source = evidence_pages(5, 25_000);
        let context = json!({"subject":{"kind":"queue","task_id":"task","cache_hash":"hash","target":"wiki/concepts/test.md","target_revision":"target"},
            "purpose":{"task_kind":"synthesize-concept","target":{"vault_path":"wiki/concepts/test.md","kind":"concept-synthesis","anchor":"## Synthesis"}}});
        let work = Work::create(
            session,
            &source,
            "subject",
            context,
            WorkLimits {
                max_input_bytes: 262144,
                max_result_bytes: 32768,
            },
        )
        .unwrap();
        assert_eq!(work.plan.parts.len(), 5);
        (work, source)
    }

    fn inspect_frontier(work: &Work, session: &MutationSession, source: &[SemanticResource]) {
        for part in &work.plan.parts {
            let mut output = draft(work, session.root(), &part.id, source);
            output.source_questions.push(WorkQuestion {
                id: format!("{}:source", part.id),
                question: "Whether the source's claim was verified remains unknown.".into(),
                evidence: vec![part.ranges[0].clone()],
            });
            work.record(session, output, source).unwrap();
        }
    }

    fn seal_frontier(
        work: &Work,
        session: &MutationSession,
        source: &[SemanticResource],
        result: &Value,
    ) {
        let mut plan = MutationPlan::default();
        let prior = work
            .capture_frontier(session.root(), source)
            .unwrap()
            .unwrap()
            .plan_seal(work, &mut plan, session.root(), result)
            .unwrap();
        let path = work_path(&work.plan.id, "final.json").unwrap();
        plan.publish_expected(session.root(), path, prior, encoded(result).unwrap())
            .unwrap();
        session.apply(plan).unwrap();
    }

    #[test]
    fn frontier_requires_current_complete_nonoverlapping_observations_without_blockers() {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let (work, source) = frontier_fixture(&session);
        assert!(
            work.capture_frontier(root.path(), &source)
                .unwrap()
                .is_none()
        );
        inspect_frontier(&work, &session, &source);
        let completed = work
            .capture_frontier(root.path(), &source)
            .unwrap()
            .unwrap();
        assert_eq!(
            completed.result()["observations"].as_array().unwrap().len(),
            5
        );
        assert!(!work.progress(root.path(), &source).unwrap().complete);
        let mut proof: FrontierProof =
            serde_json::from_value(completed.result()["proof"].clone()).unwrap();
        let mut value = serde_json::to_value(&proof).unwrap();
        let duplicate = value["nodes"][0].clone();
        value["nodes"].as_array_mut().unwrap().push(duplicate);
        proof = serde_json::from_value(value).unwrap();
        let mut bad = serde_json::to_value(&proof).unwrap();
        bad["work_id"] = json!("f".repeat(64));
        let seal = json!({"schema":"work-frontier-seal-v1","proof":bad,"final_result_revision":"a".repeat(64)});
        assert!(FrontierSeal::decode(&encoded(&seal).unwrap(), &work).is_err());
        let part = &work.plan.parts[0];
        let mut blocked = draft(&work, root.path(), &part.id, &source);
        blocked.unresolved.push(WorkQuestion {
            id: format!("{}:blocker", part.id),
            question: "Required processing input is missing.".into(),
            evidence: vec![part.ranges[0].clone()],
        });
        work.record(&session, blocked, &source).unwrap();
        assert!(
            work.capture_frontier(root.path(), &source)
                .unwrap()
                .is_none()
        );
        let mut changed = source.clone();
        changed[0].revision = "changed".into();
        assert!(work.capture_frontier(root.path(), &changed).is_err());
    }

    #[test]
    fn frontier_seals_refuse_changed_nodes_and_allow_fresh_resealing_and_root_transition() {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let (work, source) = frontier_fixture(&session);
        inspect_frontier(&work, &session, &source);
        let first = json!({"synthesis":"Source claim; unknown deployment."});
        seal_frontier(&work, &session, &source, &first);
        work.guard_completion(&mut MutationPlan::default(), root.path(), &source, &first)
            .unwrap();
        let part = &work.plan.parts[0];
        let mut corrected = draft(&work, root.path(), &part.id, &source);
        corrected.summary = "Corrected source attribution.".into();
        work.record(&session, corrected, &source).unwrap();
        let error = work
            .guard_completion(&mut MutationPlan::default(), root.path(), &source, &first)
            .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<CompletionRefusal>(),
            Some(CompletionRefusal::ChangedObservations)
        ));
        let second = json!({"synthesis":"Fresh result after corrected source observation."});
        seal_frontier(&work, &session, &source, &second);
        work.guard_completion(&mut MutationPlan::default(), root.path(), &source, &second)
            .unwrap();
        let mut forged = second.clone();
        forged["synthesis"] = json!("changed without a seal");
        assert!(matches!(
            work.guard_completion(&mut MutationPlan::default(), root.path(), &source, &forged)
                .unwrap_err()
                .downcast_ref::<CompletionRefusal>(),
            Some(CompletionRefusal::ChangedResult)
        ));
        finish(&work, &session, &source);
        let seal_path = root
            .path()
            .join(work_path(&work.plan.id, "frontier-seal.json").unwrap());
        let original = std::fs::read_to_string(&seal_path).unwrap();
        for field in ["schema", "nodes", "revision"] {
            let mut corrupted: Value = serde_json::from_str(&original).unwrap();
            match field {
                "schema" => corrupted["proof"]["schema"] = json!("unknown"),
                "nodes" => corrupted["proof"]["nodes"] = json!([]),
                _ => corrupted["proof"]["nodes"][0]["output_revision"] = json!("not-a-revision"),
            }
            std::fs::write(&seal_path, encoded(&corrupted).unwrap()).unwrap();
            assert!(
                work.capture_completion(root.path(), &source)
                    .unwrap()
                    .plan_seal(&work, &mut MutationPlan::default(), root.path(), &second)
                    .is_err()
            );
            assert_eq!(
                std::fs::read_to_string(&seal_path).unwrap(),
                encoded(&corrupted).unwrap()
            );
        }
        std::fs::write(&seal_path, original).unwrap();
        let mut mutation = MutationPlan::default();
        work.capture_completion(root.path(), &source)
            .unwrap()
            .plan_seal(&work, &mut mutation, root.path(), &second)
            .unwrap();
        session.apply(mutation).unwrap();
        assert!(
            !root
                .path()
                .join(work_path(&work.plan.id, "frontier-seal.json").unwrap())
                .exists()
        );
        work.guard_completion(&mut MutationPlan::default(), root.path(), &source, &second)
            .unwrap();
    }

    #[test]
    fn frontier_capture_guards_missing_nodes_and_all_paid_observations_before_sealing() {
        for change in ["node", "root"] {
            let root = tempfile::tempdir().unwrap();
            let session = MutationSession::acquire(root.path()).unwrap();
            let (work, source) = frontier_fixture(&session);
            inspect_frontier(&work, &session, &source);
            let mut mutation = MutationPlan::default();
            work.capture_frontier(root.path(), &source)
                .unwrap()
                .unwrap()
                .plan_seal(
                    &work,
                    &mut mutation,
                    root.path(),
                    &json!({"synthesis":"source"}),
                )
                .unwrap();
            let id = if change == "node" {
                work.plan.parts[0].id.clone()
            } else {
                work.progress(root.path(), &source).unwrap().root_node
            };
            let mut output = draft(&work, root.path(), &id, &source);
            output.summary = "Changed after capture.".into();
            work.record(&session, output, &source).unwrap();
            assert!(session.apply(mutation).is_err());
            assert!(
                !root
                    .path()
                    .join(work_path(&work.plan.id, "frontier-seal.json").unwrap())
                    .exists()
            );
        }
    }

    #[test]
    fn published_frontier_retention_validates_dependencies_before_removing_expired_work() {
        for corrupt in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let session = MutationSession::acquire(root.path()).unwrap();
            let (work, source) = frontier_fixture(&session);
            inspect_frontier(&work, &session, &source);
            let result = json!({"synthesis":"Guarded source claim."});
            seal_frontier(&work, &session, &source, &result);
            let mut mutation = MutationPlan::default();
            work.plan_publication(&mut mutation, root.path(), &result)
                .unwrap();
            session.apply(mutation).unwrap();
            if corrupt {
                let path = output_path(&work.plan.id, &work.plan.parts[0].id).unwrap();
                std::fs::write(root.path().join(path), "{}").unwrap();
                assert!(prune_published(root.path(), None, i64::MAX).is_err());
            } else {
                let report = prune_published(root.path(), Some(&session), i64::MAX).unwrap();
                assert_eq!(report.removed, 1);
                assert!(
                    !root
                        .path()
                        .join(work_path(&work.plan.id, "plan.json").unwrap())
                        .exists()
                );
            }
        }
    }
    #[test]
    fn frontier_authority_transition_refuses_conflicting_seals_and_late_publication() {
        for changed in ["dual", "publication"] {
            let root = tempfile::tempdir().unwrap();
            let session = MutationSession::acquire(root.path()).unwrap();
            let (work, source) = frontier_fixture(&session);
            inspect_frontier(&work, &session, &source);
            let result = json!({"synthesis":"Guarded source claim."});
            seal_frontier(&work, &session, &source, &result);
            finish(&work, &session, &source);
            if changed == "dual" {
                let seal = Seal {
                    work_id: work.plan.id.clone(),
                    root_node: work.progress(root.path(), &source).unwrap().root_node,
                    root_output_revision: "a".repeat(64),
                    final_result_revision: revision(&result).unwrap(),
                };
                std::fs::write(
                    root.path()
                        .join(work_path(&work.plan.id, "seal.json").unwrap()),
                    encoded(&seal).unwrap(),
                )
                .unwrap();
                assert!(
                    work.capture_completion(root.path(), &source)
                        .unwrap()
                        .plan_seal(&work, &mut MutationPlan::default(), root.path(), &result)
                        .is_err()
                );
            } else {
                let mut mutation = MutationPlan::default();
                work.capture_completion(root.path(), &source)
                    .unwrap()
                    .plan_seal(&work, &mut mutation, root.path(), &result)
                    .unwrap();
                let mut publication = MutationPlan::default();
                work.plan_publication(&mut publication, root.path(), &result)
                    .unwrap();
                session.apply(publication).unwrap();
                assert!(session.apply(mutation).is_err());
                assert!(
                    root.path()
                        .join(work_path(&work.plan.id, "frontier-seal.json").unwrap())
                        .exists()
                );
            }
        }
    }

    #[test]
    fn oversized_frontiers_keep_normal_reduction_and_exclude_source_only_work() {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let source = evidence_pages(4, 1000);
        let context = json!({"subject":{"kind":"queue","task_id":"task","cache_hash":"hash","target":"wiki/concepts/test.md","target_revision":"target"},"purpose":{"task_kind":"synthesize-concept"}});
        let work = Work::create(
            &session,
            &source,
            "subject",
            context,
            WorkLimits {
                max_input_bytes: 5000,
                max_result_bytes: 1500,
            },
        )
        .unwrap();
        for part in &work.plan.parts {
            let mut output = draft(&work, root.path(), &part.id, &source);
            output.summary = "Exact assigned observation. ".repeat(20);
            work.record(&session, output, &source).unwrap();
        }
        assert!(
            work.capture_frontier(root.path(), &source)
                .unwrap()
                .is_none()
        );
        assert!(
            !work
                .progress(root.path(), &source)
                .unwrap()
                .ready
                .is_empty()
        );
        let mut unsupported = work.clone();
        unsupported.context["purpose"]["task_kind"] = json!("summarize");
        assert!(
            unsupported
                .capture_frontier(root.path(), &source)
                .unwrap()
                .is_none()
        );
        unsupported.context["purpose"]["task_kind"] = json!("extract-concepts");
        unsupported.context["purpose"]["extraction_scope"] =
            json!({"policy":"source-proposals-v1"});
        assert!(
            unsupported
                .capture_frontier(root.path(), &source)
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn frontier_authority_replacement_uses_one_prior_final_read() {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let (work, source) = frontier_fixture(&session);
        inspect_frontier(&work, &session, &source);
        let original = json!({"synthesis":"Original guarded source claim."});
        seal_frontier(&work, &session, &source, &original);
        finish(&work, &session, &source);
        let next = json!({"synthesis":"A newly generated complete root result."});
        let mut mutation = MutationPlan::default();
        let before = work
            .capture_completion(root.path(), &source)
            .unwrap()
            .plan_seal(&work, &mut mutation, root.path(), &next)
            .unwrap();
        assert_eq!(
            before.as_deref(),
            Some(encoded(&original).unwrap().as_str())
        );
        let path = work_path(&work.plan.id, "final.json").unwrap();
        std::fs::write(
            root.path().join(&path),
            encoded(&json!({"synthesis":"External changed final result."})).unwrap(),
        )
        .unwrap();
        mutation
            .publish_expected(root.path(), path, before, encoded(&next).unwrap())
            .unwrap();
        assert!(session.apply(mutation).is_err());
        assert!(
            root.path()
                .join(work_path(&work.plan.id, "frontier-seal.json").unwrap())
                .exists()
        );
    }
}
