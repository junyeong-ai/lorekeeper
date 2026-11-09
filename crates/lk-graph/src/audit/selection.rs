//! Revision-bound applicability decisions for complete audit discovery inventories.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lk_core::concept::{Citation, file_revision};
use lk_core::discovery::{DiscoveryQuery, ScreeningDecision};
use lk_vault::{MutationPlan, MutationSession, ReadControl};
use serde::{Deserialize, Serialize};

use super::{AuditCandidate, AuditReviewDependency, GraphError, ReviewDependencyIndex, error};

pub const DIRECTORY: &str = ".lorekeeper/audit-selection";
const POLICY: &str = "audit-applicability-v1";
const STATE_SCHEMA: &str = "audit-selection-state-v1";
const PACKET_SCHEMA: &str = "audit-selection-packet-v1";
const MAX_REASON_JSON_BYTES: usize = 256;
const INSTRUCTIONS: &str = "Judge each candidate independently for this audit subject. Cards are bounded discovery previews, not full evidence. Inspect a potentially applicable or uncertain candidate. Use not-selected only when its declared scope is inapplicable. Defer only when an explicit unresolved prerequisite prevents deciding; later unknown candidates still require decisions. Every assigned key needs exactly one reasoned decision. Mandatory evidence and direct links remain required regardless of this selection.";

mod retention;
#[cfg(test)]
mod tests;
pub use retention::{PruneReport, present, prune};

fn revision(value: &impl Serialize) -> Result<String, GraphError> {
    serde_json::to_string(value)
        .map(|raw| file_revision(&raw))
        .map_err(error)
}
fn is_revision(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_source_revision(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapacityBoundary {
    Input,
    Result,
}

impl std::fmt::Display for CapacityBoundary {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Input => "input",
            Self::Result => "result",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateReference {
    pub subject: String,
    pub revision: String,
}
impl StateReference {
    pub fn validate(&self) -> Result<(), GraphError> {
        if !super::exact_page_address(&self.subject) || !is_revision(&self.revision) {
            return Err(error("invalid audit selection state reference"));
        }
        Ok(())
    }
    pub fn path(&self) -> Result<PathBuf, GraphError> {
        self.validate()?;
        Ok(subject_directory(&self.subject)
            .join("states")
            .join(format!("{}.json", self.revision)))
    }
}
fn subject_directory(subject: &str) -> PathBuf {
    Path::new(DIRECTORY).join(file_revision(subject))
}
fn packet_path(subject: &str, packet: &str) -> Result<PathBuf, GraphError> {
    if !super::exact_page_address(subject) || !is_revision(packet) {
        return Err(error("invalid audit screening packet reference"));
    }
    Ok(subject_directory(subject)
        .join("packets")
        .join(format!("{packet}.json")))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum SelectionPolicy {
    IndependentV1,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SelectionPlan {
    policy: SelectionPolicy,
    state: Option<StateReference>,
}
impl Default for SelectionPlan {
    fn default() -> Self {
        Self {
            policy: SelectionPolicy::IndependentV1,
            state: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CandidateInput {
    owner: String,
    path: String,
    source_revision: String,
    queries: Vec<DiscoveryQuery>,
    query_revisions: Vec<String>,
    review_revision: String,
}
impl CandidateInput {
    fn key(&self, purpose: &str) -> Result<String, GraphError> {
        revision(&(POLICY, purpose, self))
    }
    fn validate(&self) -> Result<(), GraphError> {
        if !super::exact_page_address(&self.owner)
            || !super::exact_page_address(&self.path)
            || !is_source_revision(&self.source_revision)
            || !is_revision(&self.review_revision)
            || self.queries.is_empty()
            || self.queries.len() != self.query_revisions.len()
            || self
                .query_revisions
                .iter()
                .any(|revision| !is_revision(revision))
            || self.queries.windows(2).any(|pair| pair[0] >= pair[1])
        {
            return Err(error("invalid audit candidate identity"));
        }
        for query in &self.queries {
            query.validate().map_err(error)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DecisionReference {
    packet_revision: String,
    row: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StateRow {
    input: CandidateInput,
    decision: Option<DecisionReference>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectionState {
    schema: String,
    created_at: jiff::Timestamp,
    subject: String,
    purpose_revision: String,
    rows: Vec<StateRow>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScreeningCard {
    pub candidate_key: String,
    input: CandidateInput,
    pub candidate: lk_vault::DiscoveryCandidate,
    previous: Option<DecisionReference>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScreeningRequest {
    schema: String,
    pub subject: String,
    pub subject_file_revision: String,
    pub subject_kind: super::AuditSubjectKind,
    pub subject_title: String,
    pub queries: Vec<DiscoveryQuery>,
    pub purpose_revision: String,
    pub max_result_bytes: usize,
    pub max_reason_json_bytes: usize,
    pub candidates: Vec<ScreeningCard>,
    pub instructions: String,
    pub reassess: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateDecision {
    pub candidate_key: String,
    pub decision: ScreeningDecision,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub enum ScreeningOutput {
    Record {
        path: String,
        purpose_revision: String,
        packet_revision: String,
        #[serde(default)]
        reassess: bool,
        decisions: Vec<CandidateDecision>,
    },
    Refresh {
        path: String,
        purpose_revision: String,
        inventory_revision: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PacketReceipt {
    schema: String,
    created_at: jiff::Timestamp,
    request: ScreeningRequest,
    decisions: Vec<CandidateDecision>,
}

#[derive(Clone, Serialize)]
pub struct SelectionProgress {
    pub policy: &'static str,
    pub purpose_revision: String,
    pub inventory_revision: String,
    pub total: usize,
    pub inspect: usize,
    pub not_selected: usize,
    pub deferred: usize,
    pub unknown: usize,
    pub complete: bool,
    pub needs_refresh: bool,
    pub state: Option<StateReference>,
    #[serde(skip)]
    rows: Vec<StateRow>,
    #[serde(skip)]
    decisions: BTreeMap<String, ScreeningDecision>,
    #[serde(skip)]
    pub(super) guards: BTreeMap<PathBuf, Arc<String>>,
}
impl SelectionProgress {
    pub fn is_ready(&self) -> bool {
        self.complete && !self.needs_refresh
    }
    pub(super) fn selected_owners(&self) -> Result<BTreeSet<String>, GraphError> {
        self.rows
            .iter()
            .filter_map(|row| match row.input.key(&self.purpose_revision) {
                Ok(key) if self.decisions.get(&key) == Some(&ScreeningDecision::Inspect) => {
                    Some(Ok(row.input.owner.clone()))
                }
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            })
            .collect()
    }
}

fn read_artifact<T: serde::de::DeserializeOwned>(
    root: &Path,
    path: &Path,
    expected: &str,
    read: &ReadControl,
) -> Result<(Arc<String>, T), GraphError> {
    read.check().map_err(GraphError::from)?;
    let full = lk_vault::page_path(root, path).map_err(error)?;
    let metadata = std::fs::symlink_metadata(&full).map_err(error)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(error(
            "audit selection artifacts must be direct regular files",
        ));
    }
    let raw = Arc::new(read.read_to_string(&full).map_err(GraphError::from)?);
    if file_revision(&raw) != expected {
        return Err(error("audit selection artifact revision differs"));
    }
    let value = serde_json::from_str(&raw).map_err(error)?;
    Ok((raw, value))
}

fn publish_artifact<T: Serialize>(
    root: &Path,
    subject: &str,
    directory: &str,
    value: &T,
    mutation: &mut MutationPlan,
) -> Result<String, GraphError> {
    let raw = serde_json::to_string(value).map_err(error)?;
    let revision = file_revision(&raw);
    let path = subject_directory(subject)
        .join(directory)
        .join(format!("{revision}.json"));
    let full = lk_vault::page_path(root, &path).map_err(error)?;
    match std::fs::symlink_metadata(&full) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            let before = std::fs::read_to_string(full).map_err(error)?;
            if before != raw {
                return Err(error("immutable audit selection artifact differs"));
            }
            mutation.require_unchanged(path, before);
        }
        Ok(_) => {
            return Err(error(
                "audit selection artifact is not a direct regular file",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            mutation.create(path, raw);
        }
        Err(failure) => return Err(error(failure)),
    }
    Ok(revision)
}

fn candidate_input(
    root: &Path,
    owner: &str,
    path: &str,
    source_revision: &str,
    queries: Vec<DiscoveryQuery>,
    query_revisions: Vec<String>,
    reviews: &mut ReviewDependencyIndex<'_>,
) -> Result<CandidateInput, GraphError> {
    let paths = reviews.by_subject.get(owner).cloned().unwrap_or_default();
    let mut dependencies = Vec::new();
    for path in paths {
        dependencies.push(reviews.dependency(root, owner, &path)?);
    }
    let identities = dependencies
        .iter()
        .map(AuditReviewDependency::identity)
        .collect::<Vec<_>>();
    let input = CandidateInput {
        owner: owner.into(),
        path: path.into(),
        source_revision: source_revision.into(),
        queries,
        query_revisions,
        review_revision: revision(&identities)?,
    };
    input.validate()?;
    Ok(input)
}

pub(super) struct SelectionInputs<'a> {
    pub subject: &'a str,
    pub subject_revision: &'a str,
    pub queries: &'a [DiscoveryQuery],
    pub matches: &'a [Citation],
    pub evidence: &'a [Citation],
    pub direct: &'a [Citation],
    pub scoped_reviews: &'a [AuditReviewDependency],
}
pub(super) fn project(
    root: &Path,
    inputs: SelectionInputs<'_>,
    plan: &SelectionPlan,
    discovered: &lk_vault::DiscoveryQueryMatches,
    reviews: &mut ReviewDependencyIndex<'_>,
    read: &ReadControl,
) -> Result<SelectionProgress, GraphError> {
    let SelectionInputs {
        subject,
        subject_revision,
        queries,
        matches,
        evidence,
        direct,
        scoped_reviews,
    } = inputs;
    let purpose = revision(&(
        POLICY,
        lk_vault::DISCOVERY_CAPTURE_POLICY,
        subject,
        subject_revision,
        queries,
        evidence,
        direct,
        scoped_reviews
            .iter()
            .map(AuditReviewDependency::identity)
            .collect::<Vec<_>>(),
    ))?;
    let mut roster = BTreeMap::new();
    for citation in matches {
        read.check().map_err(GraphError::from)?;
        let owner = lk_vault::resolve_page_owner(root, Path::new(&citation.path))
            .map_err(error)?
            .to_string_lossy()
            .into_owned();
        let entry = roster.entry(owner.clone()).or_insert_with(|| {
            (
                citation.path.clone(),
                citation.revision.clone(),
                BTreeSet::new(),
            )
        });
        entry.0 = lk_vault::DiscoveryCandidate::preferred_path(&owner, &entry.0, &citation.path)
            .to_owned();
        if entry.1 != citation.revision {
            return Err(error("audit discovery owner has conflicting revisions"));
        }
        for query in queries {
            if discovered.matches[query].contains(citation) {
                entry.2.insert(query.clone());
            }
        }
    }
    let rows = roster
        .into_iter()
        .map(|(owner, (path, revision, queries))| {
            let queries = queries.into_iter().collect::<Vec<_>>();
            let query_revisions = queries
                .iter()
                .map(|query| discovered.query_revisions[query].clone())
                .collect();
            Ok(StateRow {
                input: candidate_input(
                    root,
                    &owner,
                    &path,
                    &revision,
                    queries,
                    query_revisions,
                    reviews,
                )?,
                decision: None,
            })
        })
        .collect::<Result<Vec<_>, GraphError>>()?;
    let mut progress = SelectionProgress {
        policy: POLICY,
        purpose_revision: purpose.clone(),
        inventory_revision: revision(&(POLICY, &purpose, &rows))?,
        total: rows.len(),
        inspect: 0,
        not_selected: 0,
        deferred: 0,
        unknown: rows.len(),
        complete: false,
        needs_refresh: true,
        state: plan.state.clone(),
        rows,
        decisions: BTreeMap::new(),
        guards: BTreeMap::new(),
    };
    if let Some(reference) = &plan.state {
        if reference.subject != subject {
            return Err(error("audit selection belongs to another subject"));
        }
        let path = reference.path()?;
        let (raw, state): (_, SelectionState) =
            read_artifact(root, &path, &reference.revision, read)?;
        validate_state(&state)?;
        progress.guards.insert(path, raw);
        let packets = load_packets(root, &state, &mut progress.guards, read)?;
        if state.purpose_revision == purpose {
            let prior = state
                .rows
                .iter()
                .map(|row| Ok((row.input.key(&purpose)?, row)))
                .collect::<Result<BTreeMap<_, _>, GraphError>>()?;
            for row in &mut progress.rows {
                let key = row.input.key(&purpose)?;
                if let Some(previous) = prior.get(&key) {
                    row.decision = previous.decision.clone();
                    if let Some(reference) = &row.decision {
                        progress.decisions.insert(
                            key,
                            packets[&reference.packet_revision].decisions[reference.row].decision,
                        );
                    }
                }
            }
            progress.needs_refresh = progress.rows != state.rows;
        }
    }
    for decision in progress.decisions.values() {
        progress.unknown -= 1;
        match decision {
            ScreeningDecision::Inspect => progress.inspect += 1,
            ScreeningDecision::NotSelected => progress.not_selected += 1,
            ScreeningDecision::Defer => progress.deferred += 1,
        }
    }
    progress.complete = progress.unknown == 0 && progress.deferred == 0;
    Ok(progress)
}

fn validate_state(state: &SelectionState) -> Result<(), GraphError> {
    if state.schema != STATE_SCHEMA
        || !super::exact_page_address(&state.subject)
        || !is_revision(&state.purpose_revision)
        || state
            .rows
            .windows(2)
            .any(|pair| pair[0].input.owner >= pair[1].input.owner)
    {
        return Err(error("invalid audit selection state"));
    }
    for row in &state.rows {
        row.input.validate()?;
        if row
            .decision
            .as_ref()
            .is_some_and(|reference| !is_revision(&reference.packet_revision))
        {
            return Err(error("invalid audit selection decision reference"));
        }
    }
    Ok(())
}

fn validate_packet(packet: &PacketReceipt) -> Result<(), GraphError> {
    let request = &packet.request;
    if packet.schema != PACKET_SCHEMA
        || request.schema != POLICY
        || !super::exact_page_address(&request.subject)
        || !is_revision(&request.purpose_revision)
        || !is_revision(&request.subject_file_revision)
        || request.max_result_bytes == 0
        || !(3..=MAX_REASON_JSON_BYTES).contains(&request.max_reason_json_bytes)
        || request.instructions != INSTRUCTIONS
        || request.candidates.is_empty()
        || request.candidates.len() > 100
        || request.queries.is_empty()
        || request.queries.len() > 16
        || request.queries.windows(2).any(|pair| pair[0] >= pair[1])
        || request.candidates.len() != packet.decisions.len()
        || request
            .candidates
            .windows(2)
            .any(|pair| pair[0].input.owner >= pair[1].input.owner)
    {
        return Err(error("invalid audit screening packet"));
    }
    for (card, decision) in request.candidates.iter().zip(&packet.decisions) {
        card.input.validate()?;
        let queries = card
            .candidate
            .query_indexes
            .iter()
            .map(|index| request.queries.get(*index).cloned())
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| error("audit card query index is outside its request"))?;
        if card.candidate_key != card.input.key(&request.purpose_revision)?
            || decision.candidate_key != card.candidate_key
            || card.input.owner != card.candidate.owner
            || card.input.path != card.candidate.path
            || card.input.queries != queries
            || card.input.source_revision != card.candidate.source_revision
            || !is_revision(&card.candidate.file_revision)
            || decision.reason.trim().is_empty()
            || serde_json::to_vec(&decision.reason).map_err(error)?.len()
                > request.max_reason_json_bytes
            || card
                .previous
                .as_ref()
                .is_some_and(|reference| !is_revision(&reference.packet_revision))
        {
            return Err(error("invalid audit candidate decision"));
        }
    }
    let response = ScreeningOutput::Record {
        path: request.subject.clone(),
        purpose_revision: request.purpose_revision.clone(),
        packet_revision: revision(request)?,
        reassess: request.reassess,
        decisions: packet.decisions.clone(),
    };
    if serde_json::to_vec(&response).map_err(error)?.len() > request.max_result_bytes {
        return Err(error("retained audit screening response exceeds its bound"));
    }
    Ok(())
}

fn load_packets(
    root: &Path,
    state: &SelectionState,
    guards: &mut BTreeMap<PathBuf, Arc<String>>,
    read: &ReadControl,
) -> Result<BTreeMap<String, PacketReceipt>, GraphError> {
    let mut packets = BTreeMap::new();
    for row in &state.rows {
        let Some(reference) = &row.decision else {
            continue;
        };
        if !packets.contains_key(&reference.packet_revision) {
            let path = packet_path(&state.subject, &reference.packet_revision)?;
            let (raw, packet): (_, PacketReceipt) =
                read_artifact(root, &path, &reference.packet_revision, read)?;
            validate_packet(&packet)?;
            guards.insert(path, raw);
            packets.insert(reference.packet_revision.clone(), packet);
        }
        let packet = &packets[&reference.packet_revision];
        validate_decision_reference(state, row, reference, packet)?;
    }
    Ok(packets)
}

fn validate_decision_reference(
    state: &SelectionState,
    row: &StateRow,
    reference: &DecisionReference,
    packet: &PacketReceipt,
) -> Result<(), GraphError> {
    if packet.request.subject != state.subject
        || packet.request.purpose_revision != state.purpose_revision
    {
        return Err(error(
            "audit decision packet differs from its state purpose",
        ));
    }
    let assigned = packet
        .request
        .candidates
        .get(reference.row)
        .ok_or_else(|| error("audit decision row is absent"))?;
    if assigned.input != row.input {
        return Err(error("audit decision row differs from its candidate"));
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub struct ScreeningLimits {
    pub max_input_bytes: usize,
    pub max_result_bytes: usize,
    pub max_candidates: usize,
    pub max_batches: Option<usize>,
}

#[derive(Serialize)]
pub struct ScreeningAdmission {
    pub required_batches: usize,
    pub maximum_batches: usize,
}

#[derive(Serialize)]
pub struct CandidatePacket {
    pub progress: SelectionProgress,
    pub scope_batches: Option<usize>,
    pub admission: Option<ScreeningAdmission>,
    pub request_revision: Option<String>,
    pub request: Option<ScreeningRequest>,
    pub refresh: Option<ScreeningOutput>,
}

struct CardCapture {
    cards: BTreeMap<String, lk_vault::DiscoveryCandidate>,
    pages: BTreeMap<PathBuf, Arc<String>>,
    reviews: Vec<AuditReviewDependency>,
}
impl CardCapture {
    fn guard(
        &self,
        root: &Path,
        mutation: &mut MutationPlan,
        read: &ReadControl,
    ) -> Result<(), GraphError> {
        for (path, raw) in &self.pages {
            mutation.require_unchanged(path, raw.as_ref().clone());
        }
        for review in &self.reviews {
            super::guard_review_dependency_controlled(mutation, root, review, read)?;
        }
        Ok(())
    }
}

fn capture_cards(
    root: &Path,
    dirs: &lk_core::config::VaultDirs,
    candidate: &AuditCandidate,
    read: &ReadControl,
) -> Result<CardCapture, GraphError> {
    let discovery = candidate
        .discovery
        .as_ref()
        .ok_or_else(|| error("audit screening needs discovery"))?;
    let progress = discovery
        .selection
        .as_ref()
        .ok_or_else(|| error("legacy audit discovery uses its original full context"))?;
    let mut inventory =
        lk_vault::ReviewInventory::load_controlled(root, dirs, read).map_err(GraphError::from)?;
    let (mut captures, _) = lk_vault::capture_discovery_batch_with_inventory(
        root,
        dirs,
        std::slice::from_ref(&discovery.queries),
        &mut inventory,
        read,
    )
    .map_err(GraphError::from)?;
    let capture = captures.pop().expect("one selected audit recipe");
    let mut reviews = ReviewDependencyIndex::from_inventory(root, &mut inventory, read)?;
    let mut cards = BTreeMap::new();
    let subject_owner = lk_vault::resolve_page_owner(root, Path::new(&candidate.path))
        .map_err(error)?
        .to_string_lossy()
        .into_owned();
    for card in &capture.candidates {
        if card.owner == subject_owner {
            continue;
        }
        let queries: Vec<_> = card
            .query_indexes
            .iter()
            .map(|index| capture.queries[*index].clone())
            .collect();
        let input = candidate_input(
            root,
            &card.owner,
            &card.path,
            &card.source_revision,
            queries.clone(),
            queries
                .iter()
                .map(|query| capture.query_revisions[query].clone())
                .collect(),
            &mut reviews,
        )?;
        let key = input.key(&progress.purpose_revision)?;
        if !progress.rows.iter().any(|row| row.input == input) {
            return Err(GraphError::AuditScreeningChanged);
        }
        if cards.insert(key, card.clone()).is_some() {
            return Err(error("duplicate audit candidate owner"));
        }
    }
    if cards.len() != progress.total {
        return Err(GraphError::AuditScreeningChanged);
    }
    Ok(CardCapture {
        cards,
        pages: capture.guarded_pages().clone(),
        reviews: reviews.dependencies.into_values().collect(),
    })
}

fn request_for_rows(
    candidate: &AuditCandidate,
    rows: &[&StateRow],
    cards: &BTreeMap<String, lk_vault::DiscoveryCandidate>,
    reassess: bool,
    max_result_bytes: usize,
) -> Result<ScreeningRequest, GraphError> {
    let progress = candidate
        .discovery
        .as_ref()
        .and_then(|discovery| discovery.selection.as_ref())
        .ok_or_else(|| error("audit selection is unavailable"))?;
    let candidates = rows
        .iter()
        .map(|row| {
            let key = row.input.key(&progress.purpose_revision)?;
            Ok(ScreeningCard {
                candidate_key: key.clone(),
                input: row.input.clone(),
                candidate: cards
                    .get(&key)
                    .ok_or_else(|| error("assigned audit candidate is absent"))?
                    .clone(),
                previous: row.decision.clone(),
            })
        })
        .collect::<Result<Vec<_>, GraphError>>()?;
    let empty = ScreeningOutput::Record {
        path: candidate.path.clone(),
        purpose_revision: progress.purpose_revision.clone(),
        packet_revision: "0".repeat(64),
        reassess,
        decisions: candidates
            .iter()
            .map(|card| CandidateDecision {
                candidate_key: card.candidate_key.clone(),
                decision: ScreeningDecision::NotSelected,
                reason: String::new(),
            })
            .collect(),
    };
    let overhead = serde_json::to_vec(&empty).map_err(error)?.len();
    let minimum = overhead + candidates.len();
    if max_result_bytes < minimum {
        return Err(GraphError::AuditScreeningCapacity {
            boundary: CapacityBoundary::Result,
            required: minimum,
            maximum: max_result_bytes,
        });
    }
    let available = max_result_bytes - overhead;
    let max_reason_json_bytes = MAX_REASON_JSON_BYTES.min(available / candidates.len() + 2);
    Ok(ScreeningRequest {
        schema: POLICY.into(),
        subject: candidate.path.clone(),
        subject_file_revision: candidate.file_revision.clone(),
        subject_kind: candidate.kind,
        subject_title: candidate.title.clone(),
        queries: candidate
            .discovery
            .as_ref()
            .expect("screened discovery")
            .queries
            .clone(),
        purpose_revision: progress.purpose_revision.clone(),
        max_result_bytes,
        max_reason_json_bytes,
        candidates,
        instructions: INSTRUCTIONS.into(),
        reassess,
    })
}

fn build_packet(
    candidate: &AuditCandidate,
    eligible: &[StateRow],
    cards: &BTreeMap<String, lk_vault::DiscoveryCandidate>,
    limits: ScreeningLimits,
    reassess: bool,
) -> Result<Option<ScreeningRequest>, GraphError> {
    let mut rows = Vec::new();
    let mut request = None;
    for row in eligible {
        let mut next = rows.clone();
        next.push(row);
        let proposed =
            match request_for_rows(candidate, &next, cards, reassess, limits.max_result_bytes) {
                Ok(request) => request,
                Err(GraphError::AuditScreeningCapacity { .. }) if request.is_some() => break,
                Err(error) => return Err(error),
            };
        let bytes = serde_json::to_vec(&proposed).map_err(error)?.len();
        if bytes > limits.max_input_bytes {
            if rows.is_empty() {
                return Err(GraphError::AuditScreeningCapacity {
                    boundary: CapacityBoundary::Input,
                    required: bytes,
                    maximum: limits.max_input_bytes,
                });
            }
            break;
        }
        rows = next;
        request = Some(proposed);
        if rows.len() == limits.max_candidates {
            break;
        }
    }
    Ok(request)
}

pub fn candidates(
    root: &Path,
    dirs: &lk_core::config::VaultDirs,
    pages: &[crate::scan::ScannedPage],
    path: &str,
    limits: ScreeningLimits,
    reassess: bool,
    read: &ReadControl,
) -> Result<CandidatePacket, GraphError> {
    let mut selected =
        super::load_selected_inputs_controlled(root, dirs, pages, &[path.into()], read)?;
    let candidate = selected
        .pop()
        .ok_or_else(|| error("audit subject is absent"))?;
    prepare_candidate_packet(root, dirs, &candidate, limits, reassess, read)
}

pub fn prepare_candidate_packet(
    root: &Path,
    dirs: &lk_core::config::VaultDirs,
    candidate: &AuditCandidate,
    limits: ScreeningLimits,
    reassess: bool,
    read: &ReadControl,
) -> Result<CandidatePacket, GraphError> {
    if limits.max_candidates == 0 || limits.max_candidates > 100 {
        return Err(error("audit screening packet requires 1..100 candidates"));
    }
    let progress = candidate
        .discovery
        .as_ref()
        .and_then(|discovery| discovery.selection.as_ref())
        .ok_or_else(|| error("audit subject needs screened discovery"))?;
    let needs_cards = progress.unknown != 0 || reassess && progress.deferred != 0;
    let (request, planned_batches) = if needs_cards {
        let captured = capture_cards(root, dirs, candidate, read)?;
        let planned_batches = if progress.unknown != 0 && limits.max_batches.is_some() {
            let fresh = progress
                .rows
                .iter()
                .map(|row| StateRow {
                    input: row.input.clone(),
                    decision: None,
                })
                .collect::<Vec<_>>();
            let mut remaining = fresh.as_slice();
            let mut batches = 0;
            while !remaining.is_empty() {
                let packet = build_packet(candidate, remaining, &captured.cards, limits, false)?
                    .expect("a nonempty fitting inventory yields a packet");
                remaining = &remaining[packet.candidates.len()..];
                batches += 1;
            }
            Some(batches)
        } else {
            None
        };
        let eligible = progress
            .rows
            .iter()
            .filter_map(|row| match row.input.key(&progress.purpose_revision) {
                Ok(key)
                    if if reassess {
                        progress.decisions.get(&key) == Some(&ScreeningDecision::Defer)
                    } else {
                        row.decision.is_none()
                    } =>
                {
                    Some(Ok(row.clone()))
                }
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            })
            .collect::<Result<Vec<_>, GraphError>>()?;
        (
            build_packet(candidate, &eligible, &captured.cards, limits, reassess)?,
            planned_batches,
        )
    } else {
        (None, None)
    };
    let admission = planned_batches
        .zip(limits.max_batches)
        .filter(|(required, maximum)| required > maximum)
        .map(|(required_batches, maximum_batches)| ScreeningAdmission {
            required_batches,
            maximum_batches,
        });
    let request_revision = request.as_ref().map(revision).transpose()?;
    let refresh = (request.is_none() && progress.needs_refresh).then(|| ScreeningOutput::Refresh {
        path: candidate.path.clone(),
        purpose_revision: progress.purpose_revision.clone(),
        inventory_revision: progress.inventory_revision.clone(),
    });
    Ok(CandidatePacket {
        progress: progress.clone(),
        scope_batches: planned_batches,
        admission,
        request_revision,
        request,
        refresh,
    })
}

pub fn state_reference(candidate: &AuditCandidate) -> Result<Option<StateReference>, GraphError> {
    let Some(progress) = candidate
        .discovery
        .as_ref()
        .and_then(|discovery| discovery.selection.as_ref())
    else {
        return Ok(None);
    };
    if !progress.is_ready() {
        return Err(error(
            "audit discovery candidate decisions or their current state remain incomplete",
        ));
    }
    progress
        .state
        .clone()
        .map(Some)
        .ok_or_else(|| error("ready audit selection has no retained state"))
}

pub enum PacketStatus {
    Recorded,
    Changed,
    Unrecorded,
}

pub fn packet_status(
    root: &Path,
    dirs: &lk_core::config::VaultDirs,
    candidate: &AuditCandidate,
    expected: &ScreeningRequest,
    read: &ReadControl,
) -> Result<PacketStatus, GraphError> {
    let Some(progress) = candidate
        .discovery
        .as_ref()
        .and_then(|discovery| discovery.selection.as_ref())
    else {
        return Ok(PacketStatus::Changed);
    };
    if progress.purpose_revision != expected.purpose_revision {
        return Ok(PacketStatus::Changed);
    }
    let mut recorded = true;
    for card in &expected.candidates {
        let Some(row) = progress.rows.iter().find(|row| row.input == card.input) else {
            return Ok(PacketStatus::Changed);
        };
        let Some(reference) = &row.decision else {
            recorded = false;
            continue;
        };
        let path = packet_path(&candidate.path, &reference.packet_revision)?;
        let raw = progress
            .guards
            .get(&path)
            .ok_or_else(|| error("captured audit packet is absent"))?;
        let packet: PacketReceipt = serde_json::from_str(raw).map_err(error)?;
        if revision(&packet.request)? != revision(expected)? {
            recorded = false;
        }
    }
    if recorded {
        return Ok(PacketStatus::Recorded);
    }
    let captured = match capture_cards(root, dirs, candidate, read) {
        Ok(captured) => captured,
        Err(GraphError::AuditScreeningChanged) => return Ok(PacketStatus::Changed),
        Err(error) => return Err(error),
    };
    let mut rows = Vec::new();
    for card in &expected.candidates {
        let Some(row) = progress.rows.iter().find(|row| row.input == card.input) else {
            return Ok(PacketStatus::Changed);
        };
        rows.push(StateRow {
            input: row.input.clone(),
            decision: card.previous.clone(),
        });
    }
    let rows = rows.iter().collect::<Vec<_>>();
    let current = request_for_rows(
        candidate,
        &rows,
        &captured.cards,
        expected.reassess,
        expected.max_result_bytes,
    )?;
    if revision(&current)? != revision(expected)? {
        Ok(PacketStatus::Changed)
    } else {
        Ok(PacketStatus::Unrecorded)
    }
}

pub fn guard_ready(
    root: &Path,
    dirs: &lk_core::config::VaultDirs,
    candidate: &AuditCandidate,
    mutation: &mut MutationPlan,
    read: &ReadControl,
) -> Result<(), GraphError> {
    if state_reference(candidate)?.is_none() {
        return Ok(());
    }
    let captured = capture_cards(root, dirs, candidate, read)?;
    captured.guard(root, mutation, read)?;
    let progress = candidate
        .discovery
        .as_ref()
        .and_then(|discovery| discovery.selection.as_ref())
        .expect("screened discovery");
    for (path, raw) in &progress.guards {
        mutation.require_unchanged(path, raw.as_ref().clone());
    }
    Ok(())
}

pub fn record(
    session: &MutationSession,
    dirs: &lk_core::config::VaultDirs,
    pages: &[crate::scan::ScannedPage],
    output: ScreeningOutput,
    limits: ScreeningLimits,
    read: &ReadControl,
) -> Result<bool, GraphError> {
    let root = session.root();
    let (path, supplied_purpose) = match &output {
        ScreeningOutput::Record {
            path,
            purpose_revision,
            ..
        }
        | ScreeningOutput::Refresh {
            path,
            purpose_revision,
            ..
        } => (path, purpose_revision),
    };
    let (before, mut receipts) = super::load_controlled(root, read)?;
    let mut selected = super::inputs_selected_controlled(
        root,
        dirs,
        pages,
        &receipts,
        Some(&BTreeSet::from([path.as_str()])),
        read,
    )?;
    let candidate = selected
        .pop()
        .ok_or_else(|| error("audit screening requires an exact subject"))?;
    let progress = candidate
        .discovery
        .as_ref()
        .and_then(|discovery| discovery.selection.as_ref())
        .ok_or_else(|| error("audit subject has no screened discovery"))?;
    if supplied_purpose != &progress.purpose_revision {
        return Err(GraphError::AuditScreeningChanged);
    }
    let captured = capture_cards(root, dirs, &candidate, read)?;
    let mut mutation = MutationPlan::default();
    captured.guard(root, &mut mutation, read)?;
    super::guard_candidate_controlled(&mut mutation, root, &candidate, read)?;
    let mut rows = progress.rows.clone();
    match &output {
        ScreeningOutput::Refresh {
            inventory_revision, ..
        } => {
            if inventory_revision != &progress.inventory_revision {
                return Err(GraphError::AuditScreeningChanged);
            }
            if !progress.needs_refresh {
                return Ok(false);
            }
        }
        ScreeningOutput::Record {
            packet_revision,
            decisions,
            reassess,
            ..
        } => {
            if decisions.is_empty()
                || decisions.len() > limits.max_candidates
                || serde_json::to_vec(&output).map_err(error)?.len() > limits.max_result_bytes
            {
                return Err(error(
                    "audit screening response exceeds the complete configured packet bounds",
                ));
            }
            let keys = decisions
                .iter()
                .map(|decision| decision.candidate_key.as_str())
                .collect::<BTreeSet<_>>();
            if keys.len() != decisions.len() {
                return Err(error("duplicate audit candidate decision"));
            }
            let assigned = rows
                .iter()
                .filter_map(|row| match row.input.key(&progress.purpose_revision) {
                    Ok(key) if keys.contains(key.as_str()) => Some(Ok(row)),
                    Ok(_) => None,
                    Err(error) => Some(Err(error)),
                })
                .collect::<Result<Vec<_>, _>>()?;
            if assigned.len() != decisions.len() {
                return Err(error(
                    "audit decision is outside the current complete inventory",
                ));
            }
            let mut repeated = assigned.iter().all(|row| row.decision.is_some());
            for row in &assigned {
                let Some(reference) = &row.decision else {
                    continue;
                };
                let path = packet_path(path, &reference.packet_revision)?;
                let raw = progress
                    .guards
                    .get(&path)
                    .ok_or_else(|| error("captured audit receipt is absent"))?;
                let packet: PacketReceipt = serde_json::from_str(raw).map_err(error)?;
                repeated &= revision(&packet.request)? == *packet_revision
                    && packet.decisions == *decisions;
            }
            if repeated {
                return Ok(false);
            }
            for row in &assigned {
                if row.decision.is_none() && *reassess {
                    return Err(error(
                        "audit reassessment requires a deferred current decision",
                    ));
                }
                let key = row.input.key(&progress.purpose_revision)?;
                if row.decision.is_some()
                    && (!reassess
                        || progress.decisions.get(&key) != Some(&ScreeningDecision::Defer))
                {
                    return Err(error(
                        "audit decision is already recorded; deferred reassessment requires its exact current packet",
                    ));
                }
            }
            let request = request_for_rows(
                &candidate,
                &assigned,
                &captured.cards,
                *reassess,
                limits.max_result_bytes,
            )?;
            if revision(&request)? != *packet_revision
                || serde_json::to_vec(&request).map_err(error)?.len() > limits.max_input_bytes
            {
                return Err(error(
                    "audit screening request changed or exceeds its complete input bound",
                ));
            }
            let receipt = PacketReceipt {
                schema: PACKET_SCHEMA.into(),
                created_at: jiff::Timestamp::now(),
                request,
                decisions: decisions.clone(),
            };
            validate_packet(&receipt)?;
            let packet = publish_artifact(root, path, "packets", &receipt, &mut mutation)?;
            let indexes = rows
                .iter()
                .enumerate()
                .map(|(index, row)| Ok((row.input.key(&progress.purpose_revision)?, index)))
                .collect::<Result<BTreeMap<_, _>, GraphError>>()?;
            for (ordinal, decision) in decisions.iter().enumerate() {
                let row = &mut rows[indexes[&decision.candidate_key]];
                row.decision = Some(DecisionReference {
                    packet_revision: packet.clone(),
                    row: ordinal,
                });
            }
        }
    }
    let state = SelectionState {
        schema: STATE_SCHEMA.into(),
        created_at: jiff::Timestamp::now(),
        subject: path.clone(),
        purpose_revision: progress.purpose_revision.clone(),
        rows,
    };
    validate_state(&state)?;
    let state_revision = publish_artifact(root, path, "states", &state, &mut mutation)?;
    receipts
        .discovery
        .get_mut(path)
        .ok_or_else(|| error("audit discovery disappeared"))?
        .selection
        .as_mut()
        .ok_or_else(|| error("audit discovery policy changed"))?
        .state = Some(StateReference {
        subject: path.clone(),
        revision: state_revision,
    });
    mutation
        .publish_expected(
            root,
            super::STATE,
            before,
            serde_json::to_string_pretty(&receipts).map_err(error)? + "\n",
        )
        .map_err(error)?;
    read.check().map_err(GraphError::from)?;
    session.apply(mutation).map_err(error)?;
    Ok(true)
}
