//! Revision-bound candidate-card screening. Card decisions are not full-body assessments.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use lk_vault::{MutationPlan, MutationSession};
use miette::IntoDiagnostic;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

mod rejection;
pub(in crate::commands) use rejection::ScreeningRejection;

const POLICY: &str = "progressive-evidence-v1";
const CARD_SCOPE: &str = "candidate-card-only";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ScreeningCapacity {
    Input {
        required_bytes: usize,
        max_bytes: usize,
    },
    Result {
        required_bytes: usize,
        max_bytes: usize,
    },
}

impl std::fmt::Display for ScreeningCapacity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (boundary, required, maximum) = match self {
            Self::Input {
                required_bytes,
                max_bytes,
            } => ("input", required_bytes, max_bytes),
            Self::Result {
                required_bytes,
                max_bytes,
            } => ("result", required_bytes, max_bytes),
        };
        write!(
            formatter,
            "candidate screening: one complete packet requires {required} {boundary} bytes, above the configured bound {maximum}; the complete scope is retained"
        )
    }
}

impl std::error::Error for ScreeningCapacity {}
impl miette::Diagnostic for ScreeningCapacity {}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SelectionBinding {
    pub task_id: String,
    pub cache_hash: String,
    pub target: String,
    pub target_revision: String,
    pub config_revision: String,
    pub inventory_revision: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(in crate::commands) enum ScreeningDecision {
    Inspect,
    NotSelected,
    Defer,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CandidateDecision {
    pub candidate_key: String,
    pub decision: ScreeningDecision,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ScreeningOutput {
    pub selection_id: String,
    pub batch_id: String,
    pub input_revision: String,
    pub decisions: Vec<CandidateDecision>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    policy: String,
    binding: SelectionBinding,
    source_context: Value,
    source_context_revision: String,
    capture_policy: String,
    capture_revision: String,
    discovery: Vec<lk_vault::DiscoveryQuery>,
    candidates: Vec<lk_vault::DiscoveryCandidate>,
    max_input_bytes: usize,
    max_result_bytes: usize,
    max_reason_json_bytes: usize,
    batches: Vec<Vec<usize>>,
}

pub(super) struct Selection {
    pub id: String,
    directory: PathBuf,
    manifest: Manifest,
    captured_pages: BTreeMap<PathBuf, String>,
}

#[derive(Debug, Serialize)]
pub(super) struct BatchReceipt {
    pub batch_id: String,
    pub path: PathBuf,
    pub revision: String,
}

#[derive(Debug, Serialize)]
pub(super) struct SelectionProgress {
    pub selection_id: String,
    pub candidates: usize,
    pub batches: usize,
    pub completed_batches: usize,
    pub inspect: usize,
    pub card_not_selected: usize,
    pub deferred: usize,
    pub unknown: usize,
    pub complete: bool,
    pub examination_scope: &'static str,
    pub receipts: Vec<BatchReceipt>,
}

#[derive(Debug, Serialize)]
pub(super) struct SelectionLedger {
    pub progress: SelectionProgress,
    pub selected_owners: BTreeSet<String>,
    pub inventory_revision: String,
    pub decisions_revision: String,
}

#[derive(Debug, Serialize)]
pub(super) struct ScreeningCost {
    pub selection_id: String,
    pub inventory_revision: String,
    pub candidates: usize,
    pub batches: usize,
    pub completed_batches: usize,
    pub remaining_unrecorded_batches: usize,
    pub deferred_candidates: usize,
    pub unknown_candidates: usize,
    pub source_context_json_bytes: usize,
    pub candidate_inventory_json_bytes: usize,
    pub remaining_request_json_bytes: usize,
    pub queries: Vec<QueryCost>,
}

#[derive(Debug, Serialize)]
pub(super) struct QueryCost {
    pub query: lk_core::discovery::DiscoveryQuery,
    pub matched_candidates: usize,
    pub exclusive_candidates: usize,
}

impl SelectionLedger {
    pub fn binding(&self) -> miette::Result<lk_queue::ExtractionSelection> {
        if !self.progress.complete {
            return Err(fail(format!(
                "candidate screening is incomplete: {} unknown, {} deferred; not-selected covers candidate cards only",
                self.progress.unknown, self.progress.deferred,
            )));
        }
        Ok(lk_queue::ExtractionSelection {
            policy: lk_queue::ExtractionSelectionPolicy::ProgressiveV1,
            inventory_revision: self.inventory_revision.clone(),
            decisions_revision: self.decisions_revision.clone(),
        })
    }
}

struct CapturedLedger {
    ledger: SelectionLedger,
    raw: BTreeMap<PathBuf, String>,
    next: Option<usize>,
}

fn fail(message: impl std::fmt::Display) -> miette::Report {
    miette::miette!("candidate screening: {message}")
}

fn encoded(value: &impl Serialize) -> miette::Result<String> {
    Ok(serde_json::to_string(value).into_diagnostic()? + "\n")
}

fn digest(value: &impl Serialize) -> miette::Result<String> {
    Ok(lk_core::rss_original::digest(
        &serde_json::to_vec(value).into_diagnostic()?,
    ))
}

fn read_optional(root: &Path, path: &Path) -> miette::Result<Option<String>> {
    let full = lk_vault::page_path(root, path).into_diagnostic()?;
    match std::fs::symlink_metadata(&full) {
        Ok(metadata) if !metadata.is_file() || metadata.file_type().is_symlink() => {
            return Err(fail("screening artifacts must be direct regular files"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).into_diagnostic(),
    }
    std::fs::read_to_string(full).map(Some).into_diagnostic()
}

fn require_settled(root: &Path) -> miette::Result<()> {
    match std::fs::symlink_metadata(root.join(".lorekeeper/mutation/pending.json")) {
        Ok(_) => Err(fail(
            "pending publication; screening reads never recover journals",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).into_diagnostic(),
    }
}

fn source_role(context: &Value, has_candidates: bool) -> miette::Result<&str> {
    match context.get("role").and_then(Value::as_str) {
        Some(role @ ("complete-retained-input" | "complete-bounded-source-observations")) => {
            Ok(role)
        }
        Some("no-candidates")
            if !has_candidates
                && context
                    .get("source_revision")
                    .and_then(Value::as_str)
                    .is_some_and(|revision| !revision.trim().is_empty()) =>
        {
            Ok("no-candidates")
        }
        _ => Err(fail(
            "source context requires its explicit complete-read role; no-candidates requires an empty inventory and exact source revision",
        )),
    }
}

impl Selection {
    pub fn prefer_saved_configuration(
        mut self,
        root: &Path,
        config_revision: &str,
        source_revision: &str,
    ) -> miette::Result<Self> {
        if self.is_planned(root)? || self.manifest.binding.config_revision == config_revision {
            return Ok(self);
        }
        let directory =
            lk_vault::page_path(root, self.directory.parent().expect("selection owner"))
                .into_diagnostic()?;
        match std::fs::symlink_metadata(directory) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => return Err(fail("selection owner must be a direct directory")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(self),
            Err(error) => return Err(error).into_diagnostic(),
        }
        let source_before = self
            .manifest
            .source_context
            .get("source_revision")
            .cloned()
            .ok_or_else(|| {
                fail("saved configuration comparison requires an explicit source revision")
            })?;
        if encoded(&self.manifest.binding.config_revision)?.len()
            != encoded(&config_revision)?.len()
            || encoded(&source_before)?.len() != encoded(&source_revision)?.len()
        {
            return Ok(self);
        }
        let config_before = std::mem::replace(
            &mut self.manifest.binding.config_revision,
            config_revision.into(),
        );
        self.manifest.source_context["source_revision"] = json!(source_revision);
        self.refresh_identity()?;
        if self.is_planned(root)? {
            return Ok(self);
        }
        self.manifest.binding.config_revision = config_before;
        self.manifest.source_context["source_revision"] = source_before;
        self.refresh_identity()?;
        Ok(self)
    }

    fn refresh_identity(&mut self) -> miette::Result<()> {
        self.manifest.source_context_revision = digest(&self.manifest.source_context)?;
        self.id = digest(&self.manifest)?;
        self.directory = Path::new(".lorekeeper/queue/selection")
            .join(digest(&(
                &self.manifest.binding.task_id,
                &self.manifest.binding.cache_hash,
            ))?)
            .join(&self.id);
        Ok(())
    }

    pub fn cost(&self, root: &Path) -> miette::Result<ScreeningCost> {
        let progress = if self.is_planned(root)? {
            Some(self.ledger(root)?.progress)
        } else {
            None
        };
        let completed_batches = progress
            .as_ref()
            .map_or(0, |progress| progress.completed_batches);
        let mut remaining_request_json_bytes = 0;
        for (index, indexes) in self.manifest.batches.iter().enumerate() {
            if read_optional(root, &self.receipt_path(index)?)?.is_none() {
                remaining_request_json_bytes += encoded(&self.request_for(indexes, index)?)?.len();
            }
        }
        Ok(ScreeningCost {
            selection_id: self.id.clone(),
            inventory_revision: self.manifest.capture_revision.clone(),
            candidates: self.manifest.candidates.len(),
            batches: self.manifest.batches.len(),
            completed_batches,
            remaining_unrecorded_batches: self
                .manifest
                .batches
                .len()
                .saturating_sub(completed_batches),
            deferred_candidates: progress.as_ref().map_or(0, |progress| progress.deferred),
            unknown_candidates: progress
                .as_ref()
                .map_or(self.manifest.candidates.len(), |progress| progress.unknown),
            source_context_json_bytes: encoded(&self.manifest.source_context)?.len(),
            candidate_inventory_json_bytes: encoded(&self.manifest.candidates)?.len(),
            remaining_request_json_bytes,
            queries: self
                .manifest
                .discovery
                .iter()
                .enumerate()
                .map(|(index, query)| QueryCost {
                    query: query.clone(),
                    matched_candidates: self
                        .manifest
                        .candidates
                        .iter()
                        .filter(|candidate| candidate.query_indexes.contains(&index))
                        .count(),
                    exclusive_candidates: self
                        .manifest
                        .candidates
                        .iter()
                        .filter(|candidate| candidate.query_indexes.as_slice() == [index])
                        .count(),
                })
                .collect(),
        })
    }

    pub fn admission_reason(
        &self,
        config: &lk_core::config::Config,
        root: &Path,
    ) -> miette::Result<Option<String>> {
        let Some(limit) = config
            .execution
            .effective_work()
            .into_diagnostic()?
            .max_screening_batches
        else {
            return Ok(None);
        };
        if self.manifest.batches.len() <= limit.get() {
            return Ok(None);
        }
        if self.is_planned(root)? && self.ledger(root)?.progress.complete {
            return Ok(None);
        }
        Ok(Some(format!(
            "complete candidate scope needs {} screening batches, above max_screening_batches {}; review `lore work cost` and explicitly revise discovery or capacity; all candidates and paid decisions are retained",
            self.manifest.batches.len(),
            limit
        )))
    }

    pub fn is_planned(&self, root: &Path) -> miette::Result<bool> {
        require_settled(root)?;
        match read_optional(root, &self.manifest_path())? {
            None => Ok(false),
            Some(raw) if raw == encoded(&self.manifest)? => Ok(true),
            Some(_) => Err(fail(
                "selection manifest differs from its captured identity",
            )),
        }
    }

    pub fn source_context(&self) -> &Value {
        &self.manifest.source_context
    }

    pub fn source_fits(
        binding: &SelectionBinding,
        source_context: &Value,
        capture: &lk_vault::DiscoveryCapture,
        max_input_bytes: usize,
        max_result_bytes: usize,
    ) -> miette::Result<bool> {
        let role = source_role(source_context, !capture.candidates.is_empty())?;
        let minimal_context = if role == "no-candidates" {
            json!({"role":role,"source_revision":source_context["source_revision"]})
        } else {
            json!({"role":role})
        };
        let mut selection = Self::build(
            binding.clone(),
            minimal_context,
            capture,
            max_input_bytes,
            max_result_bytes,
        )?;
        selection.manifest.source_context_revision = digest(source_context)?;
        selection.manifest.source_context = source_context.clone();
        if selection.manifest.candidates.is_empty() {
            return Ok(encoded(&selection.request_for(&[], 0)?)?.len() <= max_input_bytes);
        }
        for index in 0..selection.manifest.candidates.len() {
            if encoded(&selection.request_for(&[index], 0)?)?.len() > max_input_bytes {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub fn build(
        binding: SelectionBinding,
        source_context: Value,
        capture: &lk_vault::DiscoveryCapture,
        max_input_bytes: usize,
        max_result_bytes: usize,
    ) -> miette::Result<Self> {
        for value in [
            &binding.task_id,
            &binding.cache_hash,
            &binding.target_revision,
            &binding.config_revision,
            &binding.inventory_revision,
            &capture.revision,
        ] {
            if value.trim().is_empty() {
                return Err(fail("selection identities must be nonblank"));
            }
        }
        source_role(&source_context, !capture.candidates.is_empty())?;
        let mut owners = BTreeSet::new();
        for candidate in &capture.candidates {
            if lk_core::link::resolve_dest(Path::new(""), &candidate.owner).as_deref()
                != Some(Path::new(&candidate.owner))
                || !Path::new(&candidate.owner)
                    .extension()
                    .is_some_and(|extension| extension == "md")
                || !owners.insert(candidate.owner.clone())
            {
                return Err(fail(
                    "candidate keys require distinct exact owner addresses",
                ));
            }
        }
        let mut selection = Self {
            id: "0".repeat(64),
            directory: PathBuf::new(),
            manifest: Manifest {
                policy: POLICY.into(),
                binding,
                source_context_revision: digest(&source_context)?,
                source_context,
                capture_policy: capture.policy.clone(),
                capture_revision: capture.revision.clone(),
                discovery: capture.queries.clone(),
                candidates: capture.candidates.clone(),
                max_input_bytes,
                max_result_bytes,
                max_reason_json_bytes: 256,
                batches: Vec::new(),
            },
            captured_pages: capture
                .guarded_pages()
                .iter()
                .map(|(path, raw)| (path.clone(), raw.as_ref().clone()))
                .collect(),
        };
        for index in 0..selection.manifest.candidates.len() {
            let size = encoded(&selection.output_shape(&[index], ""))?.len();
            if size >= max_result_bytes {
                return Err(ScreeningCapacity::Result {
                    required_bytes: size + 1,
                    max_bytes: max_result_bytes,
                }
                .into());
            }
            let available = max_result_bytes - size;
            selection.manifest.max_reason_json_bytes =
                selection.manifest.max_reason_json_bytes.min(available + 2);
        }
        let mut batch = Vec::new();
        for index in 0..selection.manifest.candidates.len() {
            let mut prospective = batch.clone();
            prospective.push(index);
            if let Some(capacity) = selection.capacity(&prospective)? {
                if batch.is_empty() {
                    return Err(capacity.into());
                }
                selection.manifest.batches.push(std::mem::take(&mut batch));
                prospective = vec![index];
                if let Some(capacity) = selection.capacity(&prospective)? {
                    return Err(capacity.into());
                }
            }
            batch = prospective;
        }
        if !batch.is_empty() {
            selection.manifest.batches.push(batch);
        }
        if selection.manifest.candidates.is_empty() {
            let required_bytes = encoded(&selection.request_for(&[], 0)?)?.len();
            if required_bytes > max_input_bytes {
                return Err(ScreeningCapacity::Input {
                    required_bytes,
                    max_bytes: max_input_bytes,
                }
                .into());
            }
        }
        selection.refresh_identity()?;
        Ok(selection)
    }

    fn manifest_path(&self) -> PathBuf {
        self.directory.join("manifest.json")
    }

    fn batch_id(&self, index: usize) -> miette::Result<String> {
        digest(&(POLICY, &self.id, index))
    }

    fn receipt_path(&self, index: usize) -> miette::Result<PathBuf> {
        Ok(self
            .directory
            .join("batches")
            .join(format!("{}.json", self.batch_id(index)?)))
    }

    fn request_for(&self, indexes: &[usize], batch: usize) -> miette::Result<Value> {
        let cards = indexes
            .iter()
            .map(|index| {
                let candidate = &self.manifest.candidates[*index];
                json!({
                    "candidate_key":candidate.owner,
                    "path":candidate.path,
                    "file_revision":candidate.file_revision,
                    "source_revision":candidate.source_revision,
                    "query_indexes":candidate.query_indexes,
                    "card":candidate.card,
                })
            })
            .collect::<Vec<_>>();
        let mut request = json!({
            "policy":POLICY,"selection_id":self.id,"batch_id":self.batch_id(batch)?,
            "binding":self.manifest.binding,
            "source_context":self.manifest.source_context,
            "source_context_revision":self.manifest.source_context_revision,
            "discovery":self.manifest.discovery,
            "examination_scope":CARD_SCOPE,
            "scope_limit":"not-selected is a candidate-card judgment, not a full-body negative; unresolved applicability must remain defer",
            "candidates":cards,
            "max_reason_json_bytes":self.manifest.max_reason_json_bytes,
            "max_result_bytes":self.manifest.max_result_bytes,
        });
        let revision = digest(&request)?;
        request
            .as_object_mut()
            .expect("request object")
            .insert("input_revision".into(), revision.into());
        Ok(request)
    }

    fn output_shape(&self, indexes: &[usize], reason: &str) -> ScreeningOutput {
        ScreeningOutput {
            selection_id: self.id.clone(),
            batch_id: "0".repeat(64),
            input_revision: "0".repeat(64),
            decisions: indexes
                .iter()
                .map(|index| CandidateDecision {
                    candidate_key: self.manifest.candidates[*index].owner.clone(),
                    decision: ScreeningDecision::NotSelected,
                    reason: reason.into(),
                })
                .collect(),
        }
    }

    fn capacity(&self, indexes: &[usize]) -> miette::Result<Option<ScreeningCapacity>> {
        let request = self.request_for(indexes, self.manifest.batches.len())?;
        let required_bytes = encoded(&request)?.len();
        if required_bytes > self.manifest.max_input_bytes {
            return Ok(Some(ScreeningCapacity::Input {
                required_bytes,
                max_bytes: self.manifest.max_input_bytes,
            }));
        }
        let reason = "x".repeat(self.manifest.max_reason_json_bytes - 2);
        let required_bytes = encoded(&self.output_shape(indexes, &reason))?.len();
        if required_bytes > self.manifest.max_result_bytes {
            return Ok(Some(ScreeningCapacity::Result {
                required_bytes,
                max_bytes: self.manifest.max_result_bytes,
            }));
        }
        Ok(None)
    }

    fn fits(&self, indexes: &[usize]) -> miette::Result<bool> {
        Ok(self.capacity(indexes)?.is_none())
    }

    fn guard_pages(
        &self,
        root: &Path,
        pages: &BTreeMap<PathBuf, String>,
        plan: &mut MutationPlan,
    ) -> miette::Result<()> {
        let root = root.canonicalize().into_diagnostic()?;
        let target = lk_vault::page_path(&root, Path::new(&self.manifest.binding.target))
            .and_then(|path| path.canonicalize())
            .into_diagnostic()?;
        let mut captured = BTreeMap::new();
        for (path, raw) in self.captured_pages.iter().chain(pages) {
            let owner = lk_vault::page_path(&root, path)
                .and_then(|path| path.canonicalize())
                .into_diagnostic()?;
            let relative = owner.strip_prefix(&root).into_diagnostic()?.to_path_buf();
            if captured
                .insert(relative, raw.clone())
                .is_some_and(|prior| prior != *raw)
            {
                return Err(fail("captured source pages disagree"));
            }
        }
        let target = target.strip_prefix(&root).into_diagnostic()?;
        if captured.get(target).is_none_or(|raw| {
            lk_core::concept::file_revision(raw) != self.manifest.binding.target_revision
        }) {
            return Err(fail("target read revision is absent from captured guards"));
        }
        for (path, raw) in captured {
            plan.require_unchanged(path, raw);
        }
        Ok(())
    }

    pub fn create(
        &self,
        session: &MutationSession,
        pages: &BTreeMap<PathBuf, String>,
    ) -> miette::Result<()> {
        let path = self.manifest_path();
        let after = encoded(&self.manifest)?;
        let before = read_optional(session.root(), &path)?;
        if before.as_ref().is_some_and(|before| before != &after) {
            return Err(fail(
                "selection identity is occupied by a different manifest",
            ));
        }
        let mut plan = MutationPlan::default();
        self.guard_pages(session.root(), pages, &mut plan)?;
        if let Some(before) = before {
            plan.require_unchanged(path, before);
        } else {
            plan.create(path, after);
        }
        session.apply(plan).into_diagnostic()
    }

    fn normalize(&self, batch: usize, output: ScreeningOutput) -> miette::Result<ScreeningOutput> {
        let request = self.request_for(&self.manifest.batches[batch], batch)?;
        if output.selection_id != self.id
            || request["batch_id"].as_str() != Some(output.batch_id.as_str())
            || request["input_revision"].as_str() != Some(output.input_revision.as_str())
        {
            return Err(fail(
                "screening output belongs to a different selection, batch or input",
            ));
        }
        if encoded(&output)?.len() > self.manifest.max_result_bytes {
            return Err(fail("screening output exceeds its result bound"));
        }
        let mut decisions = BTreeMap::new();
        for decision in output.decisions {
            if decision.reason.trim().is_empty() {
                return Err(fail("screening decision reason is blank"));
            }
            let bytes = serde_json::to_vec(&decision.reason)
                .into_diagnostic()?
                .len();
            if bytes > self.manifest.max_reason_json_bytes {
                return Err(fail(format!(
                    "screening decision reason uses {bytes} compact UTF-8 JSON bytes, above {}",
                    self.manifest.max_reason_json_bytes
                )));
            }
            if decisions
                .insert(decision.candidate_key.clone(), decision)
                .is_some()
            {
                return Err(fail(
                    "screening decisions contain a duplicate candidate key",
                ));
            }
        }
        let mut ordered = Vec::new();
        for index in &self.manifest.batches[batch] {
            let key = &self.manifest.candidates[*index].owner;
            ordered.push(
                decisions
                    .remove(key)
                    .ok_or_else(|| fail("assigned candidate decision is missing"))?,
            );
        }
        if !decisions.is_empty() {
            return Err(fail("screening output contains an unassigned candidate"));
        }
        Ok(ScreeningOutput {
            decisions: ordered,
            ..output
        })
    }

    fn capture_ledger(&self, root: &Path) -> miette::Result<CapturedLedger> {
        require_settled(root)?;
        let manifest_path = self.manifest_path();
        let manifest = read_optional(root, &manifest_path)?
            .ok_or_else(|| {
                fail("current selection manifest is missing; inspect `lore work context` for the required plan; earlier selections do not establish the current scope")
            })?;
        if manifest != encoded(&self.manifest)? {
            return Err(fail(
                "selection manifest differs from the current complete inventory or source context",
            ));
        }
        let mut raw = BTreeMap::from([(manifest_path, manifest)]);
        let mut progress = SelectionProgress {
            selection_id: self.id.clone(),
            candidates: self.manifest.candidates.len(),
            batches: self.manifest.batches.len(),
            completed_batches: 0,
            inspect: 0,
            card_not_selected: 0,
            deferred: 0,
            unknown: 0,
            complete: false,
            examination_scope: CARD_SCOPE,
            receipts: Vec::new(),
        };
        let mut selected_owners = BTreeSet::new();
        let mut answers = Vec::new();
        let mut next = None;
        for (batch, indexes) in self.manifest.batches.iter().enumerate() {
            let path = self.receipt_path(batch)?;
            let Some(bytes) = read_optional(root, &path)? else {
                progress.unknown += indexes.len();
                next.get_or_insert(batch);
                continue;
            };
            let output = self.normalize(batch, serde_json::from_str(&bytes).into_diagnostic()?)?;
            progress.completed_batches += 1;
            for decision in &output.decisions {
                match decision.decision {
                    ScreeningDecision::Inspect => {
                        progress.inspect += 1;
                        selected_owners.insert(decision.candidate_key.clone());
                    }
                    ScreeningDecision::NotSelected => progress.card_not_selected += 1,
                    ScreeningDecision::Defer => progress.deferred += 1,
                }
            }
            progress.receipts.push(BatchReceipt {
                batch_id: output.batch_id.clone(),
                path: path.clone(),
                revision: lk_core::concept::file_revision(&bytes),
            });
            raw.insert(path, bytes);
            answers.push(output);
        }
        if let Some((path, bytes)) = self.capture_rejections(root)? {
            raw.insert(path, bytes);
        }
        progress.complete = progress.unknown == 0 && progress.deferred == 0;
        Ok(CapturedLedger {
            ledger: SelectionLedger {
                inventory_revision: self.manifest.binding.inventory_revision.clone(),
                decisions_revision: digest(&(POLICY, &self.id, answers))?,
                selected_owners,
                progress,
            },
            raw,
            next,
        })
    }

    #[cfg(unix)]
    pub fn max_input_bytes(&self) -> usize {
        self.manifest.max_input_bytes
    }

    pub fn prepare(&self, root: &Path) -> miette::Result<(SelectionProgress, Option<Value>)> {
        let captured = self.capture_ledger(root)?;
        let request = captured
            .next
            .map(|batch| self.request_for(&self.manifest.batches[batch], batch))
            .transpose()?;
        Ok((captured.ledger.progress, request))
    }

    pub fn packet(&self, root: &Path, batch_id: &str) -> miette::Result<Value> {
        self.capture_ledger(root)?;
        let batch = (0..self.manifest.batches.len())
            .find(|index| self.batch_id(*index).is_ok_and(|id| id == batch_id))
            .ok_or_else(|| fail("unknown screening batch"))?;
        self.request_for(&self.manifest.batches[batch], batch)
    }

    pub fn record(
        &self,
        session: &MutationSession,
        pages: &BTreeMap<PathBuf, String>,
        output: ScreeningOutput,
        expected_receipt_revision: Option<&str>,
    ) -> miette::Result<SelectionProgress> {
        let batch = (0..self.manifest.batches.len())
            .find(|index| self.batch_id(*index).is_ok_and(|id| id == output.batch_id))
            .ok_or_else(|| fail("unknown screening batch"))?;
        let output = self.normalize(batch, output)?;
        let captured = self.capture_ledger(session.root())?;
        let path = self.receipt_path(batch)?;
        let before = read_optional(session.root(), &path)?;
        let after = encoded(&output)?;
        if let Some(expected) = expected_receipt_revision {
            if before
                .as_ref()
                .is_none_or(|before| lk_core::concept::file_revision(before) != expected)
            {
                return Err(fail("previous screening receipt changed or is missing"));
            }
        } else if let Some(before) = &before {
            let previous =
                self.normalize(batch, serde_json::from_str(before).into_diagnostic()?)?;
            if previous != output {
                return Err(fail(
                    "conflicting screening decisions require an explicit previous receipt revision",
                ));
            }
        }
        let mut plan = MutationPlan::default();
        self.guard_pages(session.root(), pages, &mut plan)?;
        for (guard, bytes) in captured.raw {
            if guard != path {
                plan.require_unchanged(guard, bytes);
            }
        }
        if before.as_ref() == Some(&after) {
            plan.require_unchanged(path, after);
        } else {
            plan.publish_expected(session.root(), path, before, after)
                .into_diagnostic()?;
        }
        session.apply(plan).into_diagnostic()?;
        Ok(self.capture_ledger(session.root())?.ledger.progress)
    }

    pub fn ledger(&self, root: &Path) -> miette::Result<SelectionLedger> {
        Ok(self.capture_ledger(root)?.ledger)
    }

    pub(super) fn publication_state(
        &self,
        root: &Path,
    ) -> miette::Result<Option<(SelectionLedger, BTreeMap<PathBuf, String>)>> {
        if !self.is_planned(root)? {
            return Ok(None);
        }
        let captured = self.capture_ledger(root)?;
        Ok(Some((captured.ledger, captured.raw)))
    }

    pub fn guard(
        &self,
        root: &Path,
        plan: &mut MutationPlan,
        binding: &lk_queue::ExtractionSelection,
    ) -> miette::Result<SelectionLedger> {
        if binding.inventory_revision != self.manifest.binding.inventory_revision {
            return Err(super::publication_rejection(
                super::RejectionKind::SelectionInventoryChanged,
                "candidate screening: staged selection inventory differs from current inputs; inspect `lore work context`, rescreen and regenerate the extraction result",
            ));
        }
        if !self.is_planned(root)? {
            return Err(super::publication_rejection(
                super::RejectionKind::EvidenceScopeChanged,
                "candidate screening: current selection manifest is missing; inspect `lore work context` for the required plan; earlier selections do not establish the current scope",
            ));
        }
        let captured = self.capture_ledger(root)?;
        if !captured.ledger.progress.complete {
            return Err(super::publication_rejection(
                super::RejectionKind::SelectionDecisionsChanged,
                format!(
                    "candidate screening is incomplete: {} unknown, {} deferred; not-selected covers candidate cards only",
                    captured.ledger.progress.unknown, captured.ledger.progress.deferred,
                ),
            ));
        }
        if serde_json::to_value(captured.ledger.binding()?).into_diagnostic()?
            != serde_json::to_value(binding).into_diagnostic()?
        {
            return Err(super::publication_rejection(
                super::RejectionKind::SelectionDecisionsChanged,
                "candidate screening: selection decisions changed since drafting",
            ));
        }
        for (path, raw) in captured.raw {
            plan.require_unchanged(path, raw);
        }
        Ok(captured.ledger)
    }
}

fn retention_selection(root: &Path, directory: &Path) -> miette::Result<Selection> {
    let path = directory.join("manifest.json");
    let raw = read_optional(root, &path)?.ok_or_else(|| fail("selection manifest is missing"))?;
    let manifest: Manifest = serde_json::from_str(&raw).into_diagnostic()?;
    let id = digest(&manifest)?;
    let expected = Path::new(".lorekeeper/queue/selection")
        .join(digest(&(
            &manifest.binding.task_id,
            &manifest.binding.cache_hash,
        ))?)
        .join(&id);
    if expected != directory
        || manifest.policy != POLICY
        || raw != encoded(&manifest)?
        || manifest.capture_policy != lk_vault::DISCOVERY_CAPTURE_POLICY
        || manifest.source_context_revision != digest(&manifest.source_context)?
        || manifest.max_input_bytes == 0
        || manifest.max_result_bytes == 0
        || !(3..=256).contains(&manifest.max_reason_json_bytes)
        || manifest
            .batches
            .iter()
            .flatten()
            .copied()
            .collect::<Vec<_>>()
            != (0..manifest.candidates.len()).collect::<Vec<_>>()
        || manifest.batches.iter().any(Vec::is_empty)
    {
        return Err(fail("selection retention requires an exact owned manifest"));
    }
    source_role(&manifest.source_context, !manifest.candidates.is_empty())?;
    super::discovery::validate_queries(&manifest.discovery)?;
    for identity in [
        &manifest.binding.task_id,
        &manifest.binding.cache_hash,
        &manifest.binding.target_revision,
        &manifest.binding.config_revision,
        &manifest.binding.inventory_revision,
        &manifest.capture_revision,
    ] {
        if identity.trim().is_empty() {
            return Err(fail("selection retention requires nonblank identities"));
        }
    }
    if lk_core::link::resolve_dest(Path::new(""), &manifest.binding.target).as_deref()
        != Some(Path::new(&manifest.binding.target))
    {
        return Err(fail("selection retention target is invalid"));
    }
    let mut owners = BTreeSet::new();
    for candidate in &manifest.candidates {
        if lk_core::link::resolve_dest(Path::new(""), &candidate.owner).as_deref()
            != Some(Path::new(&candidate.owner))
            || !Path::new(&candidate.owner)
                .extension()
                .is_some_and(|extension| extension == "md")
            || !owners.insert(&candidate.owner)
        {
            return Err(fail("selection retention candidate owner is invalid"));
        }
    }
    let selection = Selection {
        id,
        directory: directory.into(),
        manifest,
        captured_pages: BTreeMap::new(),
    };
    for batch in &selection.manifest.batches {
        if !selection.fits(batch)? {
            return Err(fail(
                "selection retention packet exceeds its declared bounds",
            ));
        }
    }
    Ok(selection)
}

struct QueueReferences {
    identities: BTreeSet<(String, String)>,
    snapshots: BTreeMap<PathBuf, String>,
}

fn retained_queue_references(
    root: &Path,
    pending: &BTreeSet<(String, String)>,
) -> miette::Result<QueueReferences> {
    let mut retained = BTreeSet::new();
    let mut snapshots = BTreeMap::new();
    for (directory, extension, staged) in [
        (".lorekeeper/queue", "jsonl", false),
        (".lorekeeper/queue/results", "json", true),
    ] {
        let full = lk_vault::page_path(root, Path::new(directory)).into_diagnostic()?;
        let entries = match std::fs::read_dir(full) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).into_diagnostic(),
        };
        for entry in entries {
            let entry = entry.into_diagnostic()?;
            let path = Path::new(directory).join(entry.file_name());
            if !path.extension().is_some_and(|value| value == extension) {
                continue;
            }
            let raw =
                read_optional(root, &path)?.ok_or_else(|| fail("queue reference disappeared"))?;
            if staged {
                let name = path.to_string_lossy();
                let identity = if name.ends_with(lk_queue::SECTION_RESULT_SUFFIX) {
                    let result: lk_queue::SectionResult =
                        serde_json::from_str(&raw).into_diagnostic()?;
                    (result.task_id, result.cache_hash)
                } else if name.ends_with(lk_queue::SYNTHESIS_RESULT_SUFFIX) {
                    let result: lk_queue::SynthesisResult =
                        serde_json::from_str(&raw).into_diagnostic()?;
                    (result.task_id, result.cache_hash)
                } else {
                    let result: lk_queue::TaskResult =
                        serde_json::from_str(&raw).into_diagnostic()?;
                    (result.task_id, result.cache_hash)
                };
                retained.insert(identity);
            } else {
                let (bytes, tasks) =
                    lk_queue::read_tasks_snapshot(&root.join(&path)).into_diagnostic()?;
                if bytes != raw.as_bytes() || !tasks.is_rewritable() {
                    return Err(fail(
                        "pending queue changed or is unparseable during selection retention",
                    ));
                }
                for task in tasks.tasks {
                    task.validate_retained_input().map_err(fail)?;
                    task.generation_policy().map_err(fail)?;
                    if !pending.contains(&(task.task_id.clone(), task.cache_hash.clone())) {
                        return Err(fail(
                            "pending queue identities changed during selection retention",
                        ));
                    }
                    retained.insert((task.task_id, task.cache_hash));
                }
            }
            snapshots.insert(path, raw);
        }
    }
    if !pending.is_subset(&retained) {
        return Err(fail(
            "pending queue reference disappeared during selection retention",
        ));
    }
    Ok(QueueReferences {
        identities: retained,
        snapshots,
    })
}

fn is_empty_selection_directory(directory: &Path) -> miette::Result<bool> {
    for entry in std::fs::read_dir(directory).into_diagnostic()? {
        let entry = entry.into_diagnostic()?;
        if entry.file_name() != "batches" || !entry.file_type().into_diagnostic()?.is_dir() {
            return Ok(false);
        }
        if let Some(entry) = std::fs::read_dir(entry.path()).into_diagnostic()?.next() {
            entry.into_diagnostic()?;
            return Ok(false);
        }
    }
    Ok(true)
}

fn require_directory_identity(name: &std::ffi::OsStr) -> miette::Result<()> {
    if !name.to_str().is_some_and(|name| {
        name.len() == 64
            && name
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }) {
        return Err(fail("unknown selection directory identity"));
    }
    Ok(())
}

/// The caller holds the queue claim inside its mutation session. Unknown state refuses cleanup.
pub(super) fn prune(
    session: &MutationSession,
    pending: &BTreeSet<(String, String)>,
) -> miette::Result<usize> {
    let root = session.root();
    require_settled(root)?;
    let directory = Path::new(".lorekeeper/queue/selection");
    let full = lk_vault::page_path(root, directory).into_diagnostic()?;
    let entries = match std::fs::read_dir(&full) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error).into_diagnostic(),
    };
    if std::fs::symlink_metadata(&full)
        .into_diagnostic()?
        .file_type()
        .is_symlink()
    {
        return Err(fail("selection store must be a direct directory"));
    }
    let QueueReferences {
        identities: retained,
        snapshots,
    } = retained_queue_references(root, pending)?;
    let mut mutation = MutationPlan::default();
    for (path, raw) in snapshots {
        mutation.require_unchanged(path, raw);
    }
    let mut removed = Vec::new();
    let mut empty = Vec::new();
    for entry in entries {
        let entry = entry.into_diagnostic()?;
        require_directory_identity(&entry.file_name())?;
        if !entry.file_type().into_diagnostic()?.is_dir() {
            return Err(fail("selection owner must be a direct directory"));
        }
        let owner = directory.join(entry.file_name());
        for entry in std::fs::read_dir(entry.path()).into_diagnostic()? {
            let entry = entry.into_diagnostic()?;
            require_directory_identity(&entry.file_name())?;
            if !entry.file_type().into_diagnostic()?.is_dir() {
                return Err(fail("selection identity must be a direct directory"));
            }
            let path = owner.join(entry.file_name());
            if is_empty_selection_directory(&entry.path())? {
                empty.push(path);
                continue;
            }
            let selection = retention_selection(root, &path)?;
            let captured = selection.capture_ledger(root)?;
            for entry in walkdir::WalkDir::new(entry.path())
                .max_depth(2)
                .follow_links(false)
            {
                let entry = entry.into_diagnostic()?;
                let relative = entry.path().strip_prefix(root).into_diagnostic()?;
                if relative == path
                    || (relative == path.join("batches") && entry.file_type().is_dir())
                {
                    continue;
                }
                if !entry.file_type().is_file() || !captured.raw.contains_key(relative) {
                    return Err(fail("unknown selection artifact prevents retention"));
                }
            }
            if retained.contains(&(
                selection.manifest.binding.task_id.clone(),
                selection.manifest.binding.cache_hash.clone(),
            )) {
                for (path, raw) in captured.raw {
                    mutation.require_unchanged(path, raw);
                }
            } else {
                for (path, raw) in captured.raw {
                    mutation.remove(path, raw);
                }
                removed.push(path);
            }
        }
    }
    if !removed.is_empty() {
        session.apply(mutation).into_diagnostic()?;
    }
    for path in removed.iter().chain(&empty) {
        for directory in [
            path.join("batches"),
            path.clone(),
            path.parent().expect("selection owner").to_path_buf(),
        ] {
            match std::fs::remove_dir(root.join(directory)) {
                Ok(()) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                    ) => {}
                Err(error) => return Err(error).into_diagnostic(),
            }
        }
    }
    Ok(removed.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _directory: tempfile::TempDir,
        root: PathBuf,
        source: String,
    }

    impl Fixture {
        fn new(candidates: usize) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap();
            std::fs::create_dir_all(root.join("wiki/documents")).unwrap();
            std::fs::create_dir_all(root.join("wiki/concepts")).unwrap();
            let source = "---\ntype: document\ntitle: Original source\n---\nA complete retained observation.\n".to_owned();
            std::fs::write(root.join("wiki/documents/source.md"), &source).unwrap();
            for index in 0..candidates {
                std::fs::write(
                    root.join(format!("wiki/concepts/candidate-{index}.md")),
                    format!("---\ntype: concept\ntitle: Candidate {index}\n---\n## Synthesis\nCandidate {index} has an explicit scope.\n\n## Related\n"),
                ).unwrap();
            }
            Self {
                _directory: directory,
                root,
                source,
            }
        }

        fn capture(&self) -> lk_vault::DiscoveryCapture {
            lk_vault::capture_discovery(
                &self.root,
                &Default::default(),
                &[lk_vault::DiscoveryQuery {
                    query: "Candidate".into(),
                    phrase: true,
                    spacing: false,
                }],
            )
            .unwrap()
        }

        fn selection(&self, input: usize, result: usize) -> Selection {
            let capture = self.capture();
            Selection::build(
                self.binding(&capture),
                json!({"role":"complete-retained-input","text":"A complete retained observation."}),
                &capture,
                input,
                result,
            )
            .unwrap()
        }

        fn binding(&self, capture: &lk_vault::DiscoveryCapture) -> SelectionBinding {
            SelectionBinding {
                task_id: "retained-task".into(),
                cache_hash: "input-hash".into(),
                target: "wiki/documents/source.md".into(),
                target_revision: lk_core::concept::file_revision(&self.source),
                config_revision: "selected-config-revision".into(),
                inventory_revision: capture.revision.clone(),
            }
        }

        fn pages(&self) -> BTreeMap<PathBuf, String> {
            BTreeMap::from([(
                PathBuf::from("wiki/documents/source.md"),
                self.source.clone(),
            )])
        }

        fn create(&self, selection: &Selection) {
            let session = MutationSession::acquire(&self.root).unwrap();
            selection.create(&session, &self.pages()).unwrap();
        }

        fn record(
            &self,
            selection: &Selection,
            output: ScreeningOutput,
            previous: Option<&str>,
        ) -> miette::Result<SelectionProgress> {
            let session = MutationSession::acquire(&self.root).unwrap();
            selection.record(&session, &self.pages(), output, previous)
        }
    }

    fn answer(request: &Value, decision: ScreeningDecision) -> ScreeningOutput {
        ScreeningOutput {
            selection_id: request["selection_id"].as_str().unwrap().into(),
            batch_id: request["batch_id"].as_str().unwrap().into(),
            input_revision: request["input_revision"].as_str().unwrap().into(),
            decisions: request["candidates"]
                .as_array()
                .unwrap()
                .iter()
                .map(|candidate| CandidateDecision {
                    candidate_key: candidate["candidate_key"].as_str().unwrap().into(),
                    decision,
                    reason: "The assigned card needs complete inspection.".into(),
                })
                .collect(),
        }
    }

    #[test]
    fn metadata_only_capacity_is_typed_without_clipping_the_complete_recipe() {
        let fixture = Fixture::new(0);
        let queries = (b'A'..=b'I')
            .map(|character| lk_vault::DiscoveryQuery {
                query: char::from(character).to_string().repeat(4096),
                phrase: true,
                spacing: false,
            })
            .collect::<Vec<_>>();
        let capture =
            lk_vault::capture_discovery(&fixture.root, &Default::default(), &queries).unwrap();
        assert!(capture.candidates.is_empty());
        let before = serde_json::to_vec(&capture).unwrap();
        let error = Selection::source_fits(
            &fixture.binding(&capture),
            &json!({"role":"no-candidates","source_revision":"a".repeat(64)}),
            &capture,
            32768,
            4096,
        )
        .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<ScreeningCapacity>(),
            Some(ScreeningCapacity::Input {
                required_bytes,
                max_bytes: 32768,
            }) if *required_bytes > 32768
        ));
        assert_eq!(serde_json::to_vec(&capture).unwrap(), before);
        assert_eq!(capture.queries, queries);
        assert!(!fixture.root.join(".lorekeeper").exists());
    }

    #[test]
    fn oversized_retained_context_still_requests_bounded_source_observations() {
        let fixture = Fixture::new(1);
        let capture = fixture.capture();
        let binding = fixture.binding(&capture);
        let context = json!({"role":"complete-retained-input","source_revision":"a".repeat(64),
            "text":"Complete source evidence. ".repeat(1000)});
        assert!(!Selection::source_fits(&binding, &context, &capture, 4096, 1024).unwrap());
        let bounded = json!({"role":"complete-bounded-source-observations",
            "source_revision":"a".repeat(64),"observation":{"output":{"unresolved":[]}}});
        assert!(Selection::source_fits(&binding, &bounded, &capture, 4096, 1024).unwrap());
        assert!(Selection::build(binding, bounded, &capture, 4096, 1024).is_ok());
        assert!(!fixture.root.join(".lorekeeper").exists());
    }

    #[test]
    fn indivisible_candidate_envelopes_report_input_and_result_capacity() {
        let fixture = Fixture::new(1);
        let capture = fixture.capture();
        let context = json!({"role":"complete-retained-input","source_revision":"a".repeat(64)});
        let baseline = Selection::build(
            fixture.binding(&capture),
            context.clone(),
            &capture,
            8192,
            1024,
        )
        .unwrap();
        let required = encoded(&baseline.request_for(&[0], 0).unwrap())
            .unwrap()
            .len();
        let error = Selection::build(
            fixture.binding(&capture),
            context.clone(),
            &capture,
            required - 1,
            1024,
        )
        .err()
        .expect("one complete card must not be clipped");
        assert_eq!(
            error.downcast_ref::<ScreeningCapacity>(),
            Some(&ScreeningCapacity::Input {
                required_bytes: required,
                max_bytes: required - 1,
            })
        );

        let fixture = Fixture::new(0);
        let address = format!(
            "wiki/concepts/{}candidate.md",
            format!("{}/", "owner".repeat(40)).repeat(4)
        );
        let path = fixture.root.join(&address);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            "---\ntype: concept\ntitle: Candidate\n---\nA scoped candidate.\n",
        )
        .unwrap();
        let capture = fixture.capture();
        assert_eq!(capture.candidates.len(), 1);
        let error = Selection::build(fixture.binding(&capture), context, &capture, 32768, 1024)
            .err()
            .expect("the complete exact owner must fit in the result");
        assert!(matches!(
            error.downcast_ref::<ScreeningCapacity>(),
            Some(ScreeningCapacity::Result {
                required_bytes,
                max_bytes: 1024,
            }) if *required_bytes > 1024
        ));
    }

    #[test]
    fn invalid_identity_and_stored_manifest_remain_integrity_errors() {
        let fixture = Fixture::new(1);
        let capture = fixture.capture();
        let mut binding = fixture.binding(&capture);
        binding.task_id.clear();
        let error = Selection::build(
            binding,
            json!({"role":"complete-retained-input"}),
            &capture,
            1,
            1,
        )
        .err()
        .expect("invalid identity must be refused");
        assert!(error.downcast_ref::<ScreeningCapacity>().is_none());

        let mut stored = fixture.selection(8192, 4096);
        stored.manifest.max_input_bytes = 1;
        stored.refresh_identity().unwrap();
        fixture.create(&stored);
        let error = retention_selection(&fixture.root, &stored.directory)
            .err()
            .expect("invalid saved bounds must prevent retention");
        assert!(error.downcast_ref::<ScreeningCapacity>().is_none());
    }

    #[test]
    fn legacy_configuration_adoption_requires_the_exact_saved_manifest_and_preserves_receipts() {
        let fixture = Fixture::new(7);
        let mut legacy = fixture.selection(4500, 4096);
        legacy.manifest.binding.config_revision = "a".repeat(64);
        legacy.manifest.source_context["source_revision"] = json!("b".repeat(64));
        legacy.refresh_identity().unwrap();
        fixture.create(&legacy);
        let request = legacy.prepare(&fixture.root).unwrap().1.unwrap();
        fixture
            .record(&legacy, answer(&request, ScreeningDecision::Inspect), None)
            .unwrap();
        let original = std::fs::read(fixture.root.join(legacy.manifest_path())).unwrap();
        let mut current = fixture.selection(4500, 4096);
        current.manifest.binding.config_revision = "c".repeat(64);
        current.manifest.source_context["source_revision"] = json!("d".repeat(64));
        current.refresh_identity().unwrap();
        let current_id = current.id.clone();
        let adopted = current
            .prefer_saved_configuration(&fixture.root, &"a".repeat(64), &"b".repeat(64))
            .unwrap();
        assert_eq!(adopted.id, legacy.id);
        assert_eq!(
            adopted
                .ledger(&fixture.root)
                .unwrap()
                .progress
                .completed_batches,
            1
        );
        assert_eq!(
            std::fs::read(fixture.root.join(legacy.manifest_path())).unwrap(),
            original
        );
        let mut changed = fixture.selection(4500, 4096);
        changed.manifest.binding.config_revision = "c".repeat(64);
        changed.manifest.source_context["source_revision"] = json!("d".repeat(64));
        changed.refresh_identity().unwrap();
        let unmatched = changed
            .prefer_saved_configuration(&fixture.root, &"e".repeat(64), &"b".repeat(64))
            .unwrap();
        assert_eq!(unmatched.id, current_id);
    }

    #[test]
    fn completed_screening_is_usable_under_a_lower_capacity_limit() {
        let fixture = Fixture::new(7);
        let selection = fixture.selection(4500, 4096);
        let config:lk_core::config::Config=lk_core::yaml::from_str("vault: {root: .}\nidentity: {name: Test, email: test@example.com}\nsources: {}\nexecution: {work: {max_screening_batches: 1}}\n").unwrap();
        assert!(
            selection
                .admission_reason(&config, &fixture.root)
                .unwrap()
                .is_some()
        );
        fixture.create(&selection);
        while let Some(request) = selection.prepare(&fixture.root).unwrap().1 {
            fixture
                .record(
                    &selection,
                    answer(&request, ScreeningDecision::Inspect),
                    None,
                )
                .unwrap();
        }
        assert!(
            selection
                .admission_reason(&config, &fixture.root)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn screening_cost_counts_complete_scope_and_only_unrecorded_request_bytes() {
        let fixture = Fixture::new(7);
        let selection = fixture.selection(4500, 4096);
        let before = selection.cost(&fixture.root).unwrap();
        assert_eq!(before.candidates, 7);
        assert_eq!(before.completed_batches, 0);
        assert_eq!(before.remaining_unrecorded_batches, before.batches);
        assert_eq!(before.queries[0].matched_candidates, 7);
        assert_eq!(before.queries[0].exclusive_candidates, 7);
        assert!(!fixture.root.join(".lorekeeper").exists());
        fixture.create(&selection);
        let (_, request) = selection.prepare(&fixture.root).unwrap();
        let request = request.unwrap();
        let request_bytes = encoded(&request).unwrap().len();
        fixture
            .record(&selection, answer(&request, ScreeningDecision::Defer), None)
            .unwrap();
        let after = selection.cost(&fixture.root).unwrap();
        assert_eq!(after.completed_batches, 1);
        assert_eq!(after.remaining_unrecorded_batches + 1, before.batches);
        assert_eq!(
            after.remaining_request_json_bytes + request_bytes,
            before.remaining_request_json_bytes
        );
        assert_eq!(
            after.deferred_candidates,
            request["candidates"].as_array().unwrap().len()
        );
        assert_eq!(after.candidates, before.candidates);
        assert_eq!(after.inventory_revision, before.inventory_revision);
        assert!(!selection.ledger(&fixture.root).unwrap().progress.complete);
    }

    #[test]
    fn candidate_batches_resume_with_complete_counts_and_both_byte_bounds() {
        let fixture = Fixture::new(7);
        let selection = fixture.selection(4500, 4096);
        fixture.create(&selection);
        let mut visited = BTreeSet::new();
        loop {
            let (progress, request) = selection.prepare(&fixture.root).unwrap();
            let Some(request) = request else {
                assert!(progress.complete);
                assert_eq!(progress.inspect, 7);
                assert_eq!(progress.unknown, 0);
                break;
            };
            assert!(serde_json::to_vec(&request).unwrap().len() <= 4500);
            let cards = request["candidates"].as_array().unwrap();
            assert!(
                cards.len() < 7,
                "packets must not repeat the complete catalog"
            );
            assert_eq!(request["examination_scope"], CARD_SCOPE);
            assert!(visited.insert(request["batch_id"].as_str().unwrap().to_owned()));
            let output = answer(&request, ScreeningDecision::Inspect);
            assert!(encoded(&output).unwrap().len() <= 4096);
            let after = fixture.record(&selection, output.clone(), None).unwrap();
            let repeated = fixture.record(&selection, output, None).unwrap();
            assert_eq!(after.inspect, repeated.inspect);
        }
        let ledger = selection.ledger(&fixture.root).unwrap();
        assert_eq!(ledger.selected_owners.len(), 7);
        assert!(ledger.progress.batches > 1);
        let binding = ledger.binding().unwrap();
        let session = MutationSession::acquire(&fixture.root).unwrap();
        let mut plan = MutationPlan::default();
        selection.guard(&fixture.root, &mut plan, &binding).unwrap();
        plan.create("published.md", "Verified ledger guard.\n".into());
        session.apply(plan).unwrap();
    }

    #[test]
    fn screening_requires_exact_assigned_candidate_and_input_coverage() {
        let fixture = Fixture::new(2);
        let selection = fixture.selection(8192, 8192);
        fixture.create(&selection);
        let request = selection.prepare(&fixture.root).unwrap().1.unwrap();
        let complete = answer(&request, ScreeningDecision::Inspect);
        for case in [
            "missing",
            "duplicate",
            "unknown",
            "input",
            "empty",
            "large",
            "escaped",
        ] {
            let mut output = complete.clone();
            match case {
                "missing" => {
                    output.decisions.pop();
                }
                "duplicate" => output.decisions.push(output.decisions[0].clone()),
                "unknown" => output.decisions[0].candidate_key = "wiki/concepts/absent.md".into(),
                "input" => output.input_revision = "different".into(),
                "empty" => output.decisions[0].reason = " \n".into(),
                "large" => output.decisions[0].reason = "x".repeat(257),
                "escaped" => output.decisions[0].reason = "\u{1}".repeat(60),
                _ => unreachable!(),
            }
            assert!(fixture.record(&selection, output, None).is_err(), "{case}");
            assert_eq!(selection.ledger(&fixture.root).unwrap().progress.unknown, 2);
        }
    }

    #[test]
    fn deferred_cards_remain_blocked_and_explicit_receipt_revision_allows_reassessment() {
        let fixture = Fixture::new(5);
        let selection = fixture.selection(4500, 4096);
        fixture.create(&selection);
        let first = selection.prepare(&fixture.root).unwrap().1.unwrap();
        fixture
            .record(&selection, answer(&first, ScreeningDecision::Defer), None)
            .unwrap();
        while let Some(request) = selection.prepare(&fixture.root).unwrap().1 {
            fixture
                .record(
                    &selection,
                    answer(&request, ScreeningDecision::NotSelected),
                    None,
                )
                .unwrap();
        }
        let ledger = selection.ledger(&fixture.root).unwrap();
        assert!(!ledger.progress.complete);
        assert!(ledger.progress.deferred > 0);
        assert_eq!(ledger.progress.unknown, 0);
        assert!(ledger.binding().is_err());
        let batch = first["batch_id"].as_str().unwrap();
        let previous = ledger
            .progress
            .receipts
            .iter()
            .find(|receipt| receipt.batch_id == batch)
            .unwrap()
            .revision
            .clone();
        let inspected = answer(&first, ScreeningDecision::Inspect);
        assert!(fixture.record(&selection, inspected.clone(), None).is_err());
        assert!(
            fixture
                .record(&selection, inspected.clone(), Some("different"))
                .is_err()
        );
        fixture
            .record(&selection, inspected, Some(&previous))
            .unwrap();
        let complete = selection.ledger(&fixture.root).unwrap();
        let binding = complete.binding().unwrap();
        assert!(complete.progress.complete);
        let previous = complete
            .progress
            .receipts
            .iter()
            .find(|receipt| receipt.batch_id == batch)
            .unwrap()
            .revision
            .clone();
        fixture
            .record(
                &selection,
                answer(&first, ScreeningDecision::NotSelected),
                Some(&previous),
            )
            .unwrap();
        let changed = selection.ledger(&fixture.root).unwrap();
        assert_ne!(changed.decisions_revision, complete.decisions_revision);
        assert!(
            selection
                .guard(&fixture.root, &mut MutationPlan::default(), &binding)
                .is_err()
        );
    }

    #[test]
    fn changed_target_candidate_or_inventory_cannot_relabel_prior_screening() {
        for changed in ["target", "candidate", "added"] {
            let fixture = Fixture::new(2);
            let selection = fixture.selection(8192, 8192);
            fixture.create(&selection);
            let output = answer(
                &selection.prepare(&fixture.root).unwrap().1.unwrap(),
                ScreeningDecision::Inspect,
            );
            let path = match changed {
                "target" => fixture.root.join("wiki/documents/source.md"),
                "candidate" => fixture.root.join("wiki/concepts/candidate-0.md"),
                "added" => fixture.root.join("wiki/concepts/new.md"),
                _ => unreachable!(),
            };
            let before = std::fs::read_to_string(&path).ok();
            std::fs::write(
                &path,
                "---\ntype: concept\ntitle: Candidate changed\n---\nCandidate evidence changed.\n",
            )
            .unwrap();
            if changed == "added" {
                let fresh = fixture.selection(8192, 8192);
                assert_ne!(fresh.id, selection.id);
                assert!(fixture.record(&fresh, output, None).is_err());
            } else {
                assert!(fixture.record(&selection, output, None).is_err());
            }
            assert_eq!(selection.ledger(&fixture.root).unwrap().progress.unknown, 2);
            if let Some(before) = before {
                std::fs::write(path, before).unwrap();
            }
        }
    }

    #[test]
    fn publication_reports_changed_inventory_before_reading_a_new_selection_manifest() {
        let fixture = Fixture::new(2);
        let selection = fixture.selection(8192, 8192);
        fixture.create(&selection);
        let request = selection.prepare(&fixture.root).unwrap().1.unwrap();
        fixture
            .record(
                &selection,
                answer(&request, ScreeningDecision::NotSelected),
                None,
            )
            .unwrap();
        let binding = selection.ledger(&fixture.root).unwrap().binding().unwrap();
        let retained_manifest =
            std::fs::read(fixture.root.join(selection.manifest_path())).unwrap();
        let retained_receipt =
            std::fs::read(fixture.root.join(selection.receipt_path(0).unwrap())).unwrap();
        std::fs::write(
            fixture.root.join("wiki/concepts/new.md"),
            "---\ntype: concept\ntitle: Candidate added\n---\nA newly available candidate.\n",
        )
        .unwrap();
        let current = fixture.selection(8192, 8192);
        assert_ne!(current.id, selection.id);
        assert!(!current.is_planned(&fixture.root).unwrap());
        let error = current
            .guard(&fixture.root, &mut MutationPlan::default(), &binding)
            .expect_err("changed inventory must refuse publication");
        assert_eq!(
            error
                .downcast_ref::<crate::commands::queue::application::PublicationRejection>()
                .map(|error| error.kind),
            Some(super::super::RejectionKind::SelectionInventoryChanged),
        );
        let error = error.to_string();
        assert!(
            error.contains("staged selection inventory differs from current inputs"),
            "{error}"
        );
        assert!(!current.is_planned(&fixture.root).unwrap());
        assert_eq!(
            std::fs::read(fixture.root.join(selection.manifest_path())).unwrap(),
            retained_manifest
        );
        assert_eq!(
            std::fs::read(fixture.root.join(selection.receipt_path(0).unwrap())).unwrap(),
            retained_receipt
        );
        assert!(selection.ledger(&fixture.root).unwrap().progress.complete);
    }

    #[test]
    fn publication_distinguishes_a_missing_current_manifest_from_inventory_drift() {
        let fixture = Fixture::new(0);
        let selection = fixture.selection(8192, 8192);
        fixture.create(&selection);
        let binding = selection.ledger(&fixture.root).unwrap().binding().unwrap();
        let manifest = fixture.root.join(selection.manifest_path());
        std::fs::remove_file(&manifest).unwrap();
        let error = selection
            .guard(&fixture.root, &mut MutationPlan::default(), &binding)
            .expect_err("missing current manifest must refuse publication");
        assert_eq!(
            error
                .downcast_ref::<crate::commands::queue::application::PublicationRejection>()
                .map(|error| error.kind),
            Some(super::super::RejectionKind::EvidenceScopeChanged),
        );
        let error = error.to_string();
        assert!(
            error.contains("current selection manifest is missing"),
            "{error}"
        );
        assert!(!error.contains("inventory changed"), "{error}");
        assert!(!manifest.exists());
    }

    #[test]
    fn capacity_refuses_complete_items_instead_of_clipping_or_selecting_a_prefix() {
        let fixture = Fixture::new(2);
        let capture = fixture.capture();
        let binding = fixture.selection(8192, 8192).manifest.binding;
        for (input, result, context) in [
            (
                128,
                8192,
                json!({"role":"complete-retained-input","text":"source"}),
            ),
            (
                8192,
                128,
                json!({"role":"complete-retained-input","text":"source"}),
            ),
            (
                4096,
                8192,
                json!({"role":"complete-retained-input","text":"x".repeat(5000)}),
            ),
            (8192, 8192, json!({"role":"partial-source","text":"source"})),
        ] {
            assert!(Selection::build(binding.clone(), context, &capture, input, result).is_err());
        }
        assert!(!fixture.root.join(".lorekeeper/queue/selection").exists());
    }

    #[test]
    fn source_fit_distinguishes_context_cost_from_invalid_or_oversized_packets() {
        let fixture = Fixture::new(2);
        let capture = fixture.capture();
        let binding = fixture.selection(8192, 8192).manifest.binding;
        let small = json!({"role":"complete-retained-input","text":"source"});
        assert!(Selection::source_fits(&binding, &small, &capture, 4096, 8192).unwrap());
        let large = json!({"role":"complete-retained-input","text":"x".repeat(5000)});
        assert!(!Selection::source_fits(&binding, &large, &capture, 4096, 8192).unwrap());
        assert!(Selection::source_fits(&binding, &large, &capture, 128, 8192).is_err());
        assert!(Selection::source_fits(&binding, &large, &capture, 4096, 128).is_err());
        let incomplete = json!({"role":"partial-source","text":"source"});
        assert!(Selection::source_fits(&binding, &incomplete, &capture, 4096, 8192).is_err());
    }

    #[test]
    fn empty_inventory_needs_no_screening_and_cannot_disguise_unexamined_candidates() {
        let fixture = Fixture::new(0);
        let capture = fixture.capture();
        let binding = fixture.selection(8192, 8192).manifest.binding;
        let context = json!({"role":"no-candidates","source_revision":"complete-source-revision"});
        let selection = Selection::build(binding, context.clone(), &capture, 2048, 2048).unwrap();
        fixture.create(&selection);
        let (progress, request) = selection.prepare(&fixture.root).unwrap();
        assert!(progress.complete);
        assert_eq!(progress.candidates, 0);
        assert_eq!(progress.batches, 0);
        assert!(request.is_none());
        let ledger = selection.ledger(&fixture.root).unwrap();
        assert!(ledger.selected_owners.is_empty());
        assert!(ledger.binding().is_ok());

        let populated = Fixture::new(1);
        let capture = populated.capture();
        let binding = populated.selection(8192, 8192).manifest.binding;
        assert!(Selection::build(binding, context, &capture, 4096, 4096).is_err());
    }

    #[test]
    fn retention_keeps_pending_and_staged_selection_references_then_removes_owned_ledgers() {
        let fixture = Fixture::new(2);
        let queue = fixture.root.join(".lorekeeper/queue");
        let task = lk_queue::QueueLlmClient::new(queue.clone()).build_task(
            "summary",
            lk_queue::TaskKind::Summarize,
            lk_queue::SummarizeRequest {
                text: "Retained source.".into(),
                max_sentences: 3,
                locale: lk_core::i18n::Locale::En,
                source_type: None,
                focus: None,
                target: lk_queue::TaskTarget {
                    vault_path: "wiki/documents/source.md".into(),
                    kind: lk_queue::TargetKind::DocumentSummary,
                    anchor: "## Summary".into(),
                },
            },
        );
        let capture = fixture.capture();
        let mut binding = fixture.selection(8192, 8192).manifest.binding;
        binding.task_id.clone_from(&task.task_id);
        binding.cache_hash.clone_from(&task.cache_hash);
        let selection = Selection::build(
            binding,
            json!({"role":"complete-retained-input","text":"Retained source."}),
            &capture,
            8192,
            8192,
        )
        .unwrap();
        fixture.create(&selection);
        let request = selection.prepare(&fixture.root).unwrap().1.unwrap();
        {
            let session = MutationSession::acquire(&fixture.root).unwrap();
            selection
                .record_rejection(
                    &session,
                    &fixture.pages(),
                    &request,
                    &"c".repeat(64),
                    ScreeningRejection::ReferenceCoverage {
                        missing: 1,
                        duplicate: 0,
                        unknown: 1,
                        response_revision: "d".repeat(64),
                    },
                )
                .unwrap();
        }
        fixture
            .record(
                &selection,
                answer(&request, ScreeningDecision::Inspect),
                None,
            )
            .unwrap();
        let before = std::fs::read(fixture.root.join(selection.manifest_path())).unwrap();
        lk_queue::write_tasks_atomic(&queue.join("pending.jsonl"), std::slice::from_ref(&task))
            .unwrap();
        let session = MutationSession::acquire(&fixture.root).unwrap();
        assert_eq!(
            prune(
                &session,
                &BTreeSet::from([(task.task_id.clone(), task.cache_hash.clone())])
            )
            .unwrap(),
            0
        );
        assert_eq!(
            std::fs::read(fixture.root.join(selection.manifest_path())).unwrap(),
            before
        );
        std::fs::remove_file(queue.join("pending.jsonl")).unwrap();
        std::fs::create_dir_all(queue.join("results")).unwrap();
        let staged = queue.join("results/pending.section.json");
        std::fs::write(
            &staged,
            encoded(&lk_queue::SectionResult {
                context_revision: None,
                task_id: task.task_id,
                cache_hash: task.cache_hash,
                target_revision: "captured".into(),
                body: "Staged interpretation.".into(),
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(prune(&session, &BTreeSet::new()).unwrap(), 0);
        std::fs::remove_file(staged).unwrap();
        assert_eq!(prune(&session, &BTreeSet::new()).unwrap(), 1);
        assert!(!fixture.root.join(&selection.directory).exists());
        assert_eq!(
            std::fs::read_to_string(fixture.root.join("wiki/documents/source.md")).unwrap(),
            fixture.source
        );
    }

    #[test]
    fn screening_rejections_bind_exact_requests_and_allow_explicit_valid_records() {
        let fixture = Fixture::new(2);
        let selection = fixture.selection(8192, 8192);
        fixture.create(&selection);
        let request = selection.prepare(&fixture.root).unwrap().1.unwrap();
        let manifest = std::fs::read(fixture.root.join(selection.manifest_path())).unwrap();
        let contract = "c".repeat(64);
        let changed_contract = "e".repeat(64);
        let rejected = ScreeningRejection::ReferenceCoverage {
            missing: 1,
            duplicate: 0,
            unknown: 1,
            response_revision: "d".repeat(64),
        };
        let candidate = fixture.root.join(&selection.manifest.candidates[0].owner);
        let candidate_before = std::fs::read(&candidate).unwrap();
        std::fs::write(&candidate, "Changed evidence after preparation.").unwrap();
        {
            let session = MutationSession::acquire(&fixture.root).unwrap();
            assert!(
                selection
                    .record_rejection(
                        &session,
                        &fixture.pages(),
                        &request,
                        &contract,
                        rejected.clone()
                    )
                    .is_err()
            );
            assert!(
                !fixture
                    .root
                    .join(selection.directory.join("rejections.json"))
                    .exists()
            );
        }
        std::fs::write(&candidate, candidate_before).unwrap();
        {
            let session = MutationSession::acquire(&fixture.root).unwrap();
            let mut changed = request.clone();
            changed["source_context"]["text"] = json!("Changed text.");
            assert!(
                selection
                    .record_rejection(
                        &session,
                        &fixture.pages(),
                        &changed,
                        &contract,
                        rejected.clone()
                    )
                    .is_err()
            );
            selection
                .record_rejection(
                    &session,
                    &fixture.pages(),
                    &request,
                    &contract,
                    rejected.clone(),
                )
                .unwrap();
        }
        assert!(
            selection
                .rejection_reason(&fixture.root, &request, &contract)
                .unwrap()
                .is_some()
        );
        assert!(
            selection
                .rejection_reason(&fixture.root, &request, &changed_contract)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            selection
                .ledger(&fixture.root)
                .unwrap()
                .progress
                .completed_batches,
            0
        );
        assert_eq!(
            selection.prepare(&fixture.root).unwrap().1.unwrap(),
            request
        );
        assert_eq!(
            std::fs::read(fixture.root.join(selection.manifest_path())).unwrap(),
            manifest
        );

        let path = fixture
            .root
            .join(selection.directory.join("rejections.json"));
        let raw = std::fs::read_to_string(&path).unwrap();
        let mut legacy: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(legacy["schema"], "screening-rejections-v2");
        legacy["schema"] = json!("screening-rejections-v1");
        legacy["batches"][request["batch_id"].as_str().unwrap()]["rejection"]
            .as_object_mut()
            .unwrap()
            .remove("kind");
        let legacy = encoded(&legacy).unwrap();
        std::fs::write(&path, &legacy).unwrap();
        assert!(
            selection
                .rejection_reason(&fixture.root, &request, &contract)
                .unwrap()
                .is_some()
        );
        selection.ledger(&fixture.root).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), legacy);
        std::fs::write(&path, &raw).unwrap();
        for corrupt in [json!([]), json!({}), {
            let mut value: Value = serde_json::from_str(&raw).unwrap();
            value["batches"][request["batch_id"].as_str().unwrap()]["input_revision"] =
                json!("f".repeat(64));
            value
        }] {
            std::fs::write(&path, encoded(&corrupt).unwrap()).unwrap();
            assert!(
                selection
                    .rejection_reason(&fixture.root, &request, &changed_contract)
                    .is_err()
            );
            let session = MutationSession::acquire(&fixture.root).unwrap();
            assert!(prune(&session, &BTreeSet::new()).is_err());
            assert!(fixture.root.join(selection.manifest_path()).exists());
        }
        std::fs::write(&path, &raw).unwrap();
        fixture
            .record(
                &selection,
                answer(&request, ScreeningDecision::Inspect),
                None,
            )
            .unwrap();
        assert!(selection.ledger(&fixture.root).unwrap().progress.complete);
        let session = MutationSession::acquire(&fixture.root).unwrap();
        assert!(
            selection
                .record_rejection(&session, &fixture.pages(), &request, &contract, rejected)
                .is_err()
        );
    }

    #[test]
    fn invalid_reason_receipts_preserve_scope_and_reject_corrupt_dispositions() {
        let fixture = Fixture::new(2);
        let selection = fixture.selection(8192, 8192);
        fixture.create(&selection);
        let request = selection.prepare(&fixture.root).unwrap().1.unwrap();
        let contract = "c".repeat(64);
        let session = MutationSession::acquire(&fixture.root).unwrap();
        selection
            .record_rejection(
                &session,
                &fixture.pages(),
                &request,
                &contract,
                ScreeningRejection::InvalidReasons {
                    blank: 0,
                    oversized: 1,
                    max_reason_json_bytes: 256,
                    largest_oversized_json_bytes: 260,
                    response_revision: "d".repeat(64),
                },
            )
            .unwrap();
        assert_eq!(
            selection
                .ledger(&fixture.root)
                .unwrap()
                .progress
                .completed_batches,
            0
        );
        assert!(
            selection
                .rejection_reason(&fixture.root, &request, &contract)
                .unwrap()
                .unwrap()
                .contains("260")
        );
        assert!(
            selection
                .rejection_reason(&fixture.root, &request, &"e".repeat(64))
                .unwrap()
                .is_none()
        );
        let path = fixture
            .root
            .join(selection.directory.join("rejections.json"));
        let before = std::fs::read_to_string(&path).unwrap();
        for (field, invalid) in [
            ("blank", json!(3)),
            ("oversized", json!(0)),
            ("max_reason_json_bytes", json!(257)),
            ("largest_oversized_json_bytes", json!(256)),
            ("largest_oversized_json_bytes", json!(8193)),
            ("unknown", json!(0)),
        ] {
            let mut corrupt: Value = serde_json::from_str(&before).unwrap();
            corrupt["batches"][request["batch_id"].as_str().unwrap()]["rejection"][field] = invalid;
            std::fs::write(&path, encoded(&corrupt).unwrap()).unwrap();
            assert!(
                selection
                    .rejection_reason(&fixture.root, &request, &contract)
                    .is_err(),
                "{field}"
            );
            assert!(prune(&session, &BTreeSet::new()).is_err());
            assert!(fixture.root.join(selection.manifest_path()).exists());
        }
        std::fs::write(&path, &before).unwrap();
        selection
            .record(
                &session,
                &fixture.pages(),
                answer(&request, ScreeningDecision::Inspect),
                None,
            )
            .unwrap();
        assert!(selection.ledger(&fixture.root).unwrap().progress.complete);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn rejection_receipts_refuse_duplicate_fields_and_batches_in_both_versions() {
        let fixture = Fixture::new(2);
        let selection = fixture.selection(8192, 8192);
        fixture.create(&selection);
        let request = selection.prepare(&fixture.root).unwrap().1.unwrap();
        let contract = "c".repeat(64);
        {
            let session = MutationSession::acquire(&fixture.root).unwrap();
            selection
                .record_rejection(
                    &session,
                    &fixture.pages(),
                    &request,
                    &contract,
                    ScreeningRejection::ReferenceCoverage {
                        missing: 1,
                        duplicate: 0,
                        unknown: 1,
                        response_revision: "d".repeat(64),
                    },
                )
                .unwrap();
        }
        let path = fixture
            .root
            .join(selection.directory.join("rejections.json"));
        let original: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let batch = request["batch_id"].as_str().unwrap();
        for legacy in [false, true] {
            let mut valid = original.clone();
            if legacy {
                valid["schema"] = json!("screening-rejections-v1");
                valid["batches"][batch]["rejection"]
                    .as_object_mut()
                    .unwrap()
                    .remove("kind");
            }
            let raw = serde_json::to_string(&valid).unwrap();
            std::fs::write(&path, &raw).unwrap();
            assert!(
                selection
                    .rejection_reason(&fixture.root, &request, &contract)
                    .unwrap()
                    .is_some()
            );
            let entry = &valid["batches"][batch];
            let mut fields = vec![
                ("schema", valid["schema"].clone()),
                ("selection_id", valid["selection_id"].clone()),
                ("input_revision", entry["input_revision"].clone()),
                ("request_revision", entry["request_revision"].clone()),
                ("contract_revision", entry["contract_revision"].clone()),
                ("missing", entry["rejection"]["missing"].clone()),
                (
                    "response_revision",
                    entry["rejection"]["response_revision"].clone(),
                ),
            ];
            if !legacy {
                fields.push(("kind", entry["rejection"]["kind"].clone()));
            }
            for (field, value) in fields {
                let key = serde_json::to_string(field).unwrap();
                let encoded = serde_json::to_string(&value).unwrap();
                let needle = format!("{key}:{encoded}");
                assert!(raw.contains(&needle));
                for conflicting in ["null".to_owned(), encoded.clone()] {
                    let corrupt =
                        raw.replacen(&needle, &format!("{key}:{conflicting},{needle}"), 1);
                    std::fs::write(&path, &corrupt).unwrap();
                    assert!(
                        selection
                            .rejection_reason(&fixture.root, &request, &contract)
                            .is_err(),
                        "legacy={legacy} field={field}"
                    );
                    let session = MutationSession::acquire(&fixture.root).unwrap();
                    assert!(prune(&session, &BTreeSet::new()).is_err());
                    assert_eq!(std::fs::read_to_string(&path).unwrap(), corrupt);
                }
            }
            let key = serde_json::to_string(batch).unwrap();
            let corrupt = raw.replacen(
                "\"batches\":{",
                &format!(
                    "\"batches\":{{{key}:{},",
                    serde_json::to_string(entry).unwrap()
                ),
                1,
            );
            std::fs::write(&path, &corrupt).unwrap();
            assert!(
                selection
                    .rejection_reason(&fixture.root, &request, &contract)
                    .is_err(),
                "duplicate batch legacy={legacy}"
            );
            std::fs::write(&path, &raw).unwrap();
            selection.ledger(&fixture.root).unwrap();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
        }
    }

    #[test]
    fn selection_retention_refuses_malformed_unowned_and_unknown_state_before_deletion() {
        for scenario in ["manifest", "receipt", "unowned", "unknown", "staged"] {
            let fixture = Fixture::new(1);
            let selection = fixture.selection(8192, 8192);
            fixture.create(&selection);
            let request = selection.prepare(&fixture.root).unwrap().1.unwrap();
            fixture
                .record(
                    &selection,
                    answer(&request, ScreeningDecision::Inspect),
                    None,
                )
                .unwrap();
            let mut directory = fixture.root.join(&selection.directory);
            match scenario {
                "manifest" => std::fs::write(directory.join("manifest.json"), "{}").unwrap(),
                "receipt" => {
                    std::fs::write(fixture.root.join(selection.receipt_path(0).unwrap()), "{}")
                        .unwrap()
                }
                "unowned" => {
                    let moved = directory.parent().unwrap().join("f".repeat(64));
                    std::fs::rename(&directory, &moved).unwrap();
                    directory = moved;
                }
                "unknown" => {
                    std::fs::write(directory.join("unowned.txt"), "Keep this file.").unwrap()
                }
                "staged" => {
                    std::fs::create_dir_all(fixture.root.join(".lorekeeper/queue/results"))
                        .unwrap();
                    std::fs::write(
                        fixture.root.join(".lorekeeper/queue/results/bad.json"),
                        "{}",
                    )
                    .unwrap();
                }
                _ => unreachable!(),
            }
            let session = MutationSession::acquire(&fixture.root).unwrap();
            assert!(prune(&session, &BTreeSet::new()).is_err(), "{scenario}");
            assert!(directory.join("manifest.json").exists(), "{scenario}");
            assert_eq!(
                std::fs::read_to_string(fixture.root.join("wiki/documents/source.md")).unwrap(),
                fixture.source
            );
        }
    }

    #[test]
    fn selection_retention_resumes_empty_directory_cleanup_after_journaled_file_removal() {
        let fixture = Fixture::new(1);
        let selection = fixture.selection(8192, 8192);
        fixture.create(&selection);
        let request = selection.prepare(&fixture.root).unwrap().1.unwrap();
        fixture
            .record(
                &selection,
                answer(&request, ScreeningDecision::Inspect),
                None,
            )
            .unwrap();
        let captured = selection.capture_ledger(&fixture.root).unwrap();
        let session = MutationSession::acquire(&fixture.root).unwrap();
        let mut mutation = MutationPlan::default();
        for (path, raw) in captured.raw {
            mutation.remove(path, raw);
        }
        session.apply(mutation).unwrap();
        assert!(fixture.root.join(&selection.directory).exists());
        assert_eq!(prune(&session, &BTreeSet::new()).unwrap(), 0);
        assert!(!fixture.root.join(&selection.directory).exists());
    }
}
