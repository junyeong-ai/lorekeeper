use super::*;

const SCHEMA: &str = "screening-computed-output-v1";
const HEAD_SCHEMA: &str = "screening-selection-head-v1";

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(super) enum ReceiptPolicy {
    ComputedPacketsV1,
}

#[derive(Deserialize, Serialize)]
enum PacketPolicy {
    #[serde(rename = "progressive-evidence-v1")]
    Legacy,
    #[serde(rename = "progressive-evidence-v2")]
    Computed,
}

pub(in crate::commands) fn is_computed_request(request: &Value) -> miette::Result<bool> {
    let Some(value) = request.get("policy") else {
        return Ok(false);
    };
    let policy: PacketPolicy = serde_json::from_value(value.clone()).into_diagnostic()?;
    Ok(matches!(policy, PacketPolicy::Computed))
}

pub(super) fn request_policy(policy: Option<ReceiptPolicy>) -> Value {
    json!(if policy.is_some() {
        PacketPolicy::Computed
    } else {
        PacketPolicy::Legacy
    })
}

#[cfg(unix)]
pub(in crate::commands::work) fn resolved_contract(
    config: &lk_core::config::Config,
) -> miette::Result<String> {
    let (profile, _) = config
        .execution
        .resolve_stage_profile(lk_core::execution::ExecutionStage::Screening)
        .into_diagnostic()?;
    digest(&(
        "resolved-screening-packet-v1",
        profile,
        crate::commands::execution::observation::screening_packet_contract_revision(),
    ))
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Origin {
    selection_id: String,
    batch_id: String,
    input_revision: String,
    receipt_revision: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    schema: String,
    output: ScreeningOutput,
    dependency_revision: String,
    contract_revision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reused_from: Option<Origin>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Head {
    schema: String,
    task_id: String,
    cache_hash: String,
    selection_id: String,
}

#[derive(Debug)]
pub(super) struct ChangedAuthority;

impl std::fmt::Display for ChangedAuthority {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("reused screening authority was reassessed; explicitly reassess this current packet before publication")
    }
}

impl std::error::Error for ChangedAuthority {}
impl miette::Diagnostic for ChangedAuthority {}

#[derive(Default)]
pub(super) struct ProofCapture {
    pub snapshots: BTreeMap<PathBuf, String>,
    selections: BTreeMap<String, Selection>,
}

fn is_revision(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

pub(super) struct CapturedHead {
    pub path: PathBuf,
    pub raw: String,
    pub task_id: String,
    pub cache_hash: String,
    pub selection_id: String,
}

pub(super) fn capture_head(root: &Path, owner: &Path) -> miette::Result<Option<CapturedHead>> {
    let path = owner.join("current.json");
    let Some(raw) = read_optional(root, &path)? else {
        return Ok(None);
    };
    let head: Head = serde_json::from_str(&raw).into_diagnostic()?;
    if head.schema != HEAD_SCHEMA
        || !is_revision(&head.selection_id)
        || owner.file_name().and_then(|name| name.to_str())
            != Some(digest(&(&head.task_id, &head.cache_hash))?.as_str())
        || raw != encoded(&head)?
    {
        return Err(fail("invalid current screening selection head"));
    }
    let selection = retention_selection(root, &owner.join(&head.selection_id))?;
    if selection.manifest.binding.task_id != head.task_id
        || selection.manifest.binding.cache_hash != head.cache_hash
        || selection.manifest.receipt_policy != Some(ReceiptPolicy::ComputedPacketsV1)
    {
        return Err(fail("screening head refers to another task"));
    }
    Ok(Some(CapturedHead {
        path,
        raw,
        task_id: head.task_id,
        cache_hash: head.cache_hash,
        selection_id: head.selection_id,
    }))
}

impl Selection {
    pub(in crate::commands::work) fn prefer_computed_receipts(
        mut self,
        root: &Path,
    ) -> miette::Result<Self> {
        if self.is_planned(root)? {
            return Ok(self);
        }
        self.manifest.receipt_policy = Some(ReceiptPolicy::ComputedPacketsV1);
        self.repack()?;
        self.refresh_identity()?;
        Ok(self)
    }

    pub(in crate::commands::work) fn create_native(
        &self,
        session: &MutationSession,
        pages: &BTreeMap<PathBuf, String>,
        contract_revision: &str,
    ) -> miette::Result<usize> {
        if self.manifest.receipt_policy.is_none() {
            self.create(session, pages)?;
            return Ok(0);
        }
        let root = session.root();
        let path = self.manifest_path();
        let after = encoded(&self.manifest)?;
        let before = read_optional(root, &path)?;
        if before.as_ref().is_some_and(|raw| raw != &after) {
            return Err(fail(
                "selection identity is occupied by a different manifest",
            ));
        }
        let owner = self.directory.parent().expect("selection owner");
        let head = capture_head(root, owner)?;
        let mut plan = MutationPlan::default();
        let mut reused = 0;
        self.guard_pages(root, pages, &mut plan)?;
        if let Some(raw) = &before {
            plan.require_unchanged(path, raw.clone());
            if head
                .as_ref()
                .is_none_or(|head| head.selection_id != self.id)
            {
                let captured = self.capture_ledger(root)?;
                for (path, raw) in captured.raw.into_iter().chain(captured.dependencies) {
                    plan.require_unchanged(path, raw);
                }
            }
        } else {
            plan.create(path, after);
        }
        if let Some(head) = &head
            && head.selection_id != self.id
        {
            let prior = retention_selection(root, &owner.join(&head.selection_id))?;
            let captured = prior.capture_ledger(root)?;
            let mut available = BTreeMap::new();
            for batch in 0..prior.manifest.batches.len() {
                let path = prior.receipt_path(batch)?;
                let Some(raw) = captured.raw.get(&path) else {
                    continue;
                };
                let (_, Some(value)) = decode(&prior, batch, raw)? else {
                    continue;
                };
                if value.contract_revision == contract_revision {
                    let usable = captured
                        .ledger
                        .progress
                        .receipts
                        .iter()
                        .any(|receipt| receipt.path == path);
                    let origin = value.reused_from.clone().unwrap_or_else(|| Origin {
                        selection_id: prior.id.clone(),
                        batch_id: value.output.batch_id.clone(),
                        input_revision: value.output.input_revision.clone(),
                        receipt_revision: lk_core::concept::file_revision(raw),
                    });
                    if available
                        .insert(value.dependency_revision.clone(), (value, origin, usable))
                        .is_some()
                    {
                        return Err(fail("ambiguous prior screening computation"));
                    }
                }
            }
            for batch in 0..self.manifest.batches.len() {
                let path = self.receipt_path(batch)?;
                if let Some(raw) = read_optional(root, &path)? {
                    plan.require_unchanged(path, raw);
                    continue;
                }
                let request = self.request_for(&self.manifest.batches[batch], batch)?;
                let key = dependency_revision(
                    &request,
                    self.manifest.max_input_bytes,
                    contract_revision,
                )?;
                let Some((prior, origin, usable)) = available.get(&key) else {
                    continue;
                };
                let output = self.normalize(
                    batch,
                    ScreeningOutput {
                        selection_id: self.id.clone(),
                        batch_id: self.batch_id(batch)?,
                        input_revision: request["input_revision"]
                            .as_str()
                            .expect("packet revision")
                            .into(),
                        decisions: prior.output.decisions.clone(),
                    },
                )?;
                let after = encoded(&Receipt {
                    schema: SCHEMA.into(),
                    output,
                    dependency_revision: key,
                    contract_revision: contract_revision.into(),
                    reused_from: Some(origin.clone()),
                })?;
                if after.len() > self.manifest.max_result_bytes {
                    return Err(fail(
                        "complete reused screening receipt exceeds its result bound",
                    ));
                }
                plan.create(path, after);
                reused += usize::from(*usable);
            }
            for (path, raw) in captured.raw.into_iter().chain(captured.dependencies) {
                plan.require_unchanged(path, raw);
            }
        }
        let head_after = encoded(&Head {
            schema: HEAD_SCHEMA.into(),
            task_id: self.manifest.binding.task_id.clone(),
            cache_hash: self.manifest.binding.cache_hash.clone(),
            selection_id: self.id.clone(),
        })?;
        let head_path = owner.join("current.json");
        match head {
            Some(head) if head.raw == head_after => plan.require_unchanged(head_path, head.raw),
            head => plan
                .publish_expected(root, head_path, head.map(|head| head.raw), head_after)
                .into_diagnostic()?,
        }
        session.apply(plan).into_diagnostic()?;
        Ok(reused)
    }

    #[cfg(unix)]
    pub(in crate::commands::work) fn native_capacity(
        &self,
        request: &Value,
    ) -> miette::Result<Option<ScreeningCapacity>> {
        let batch = (0..self.manifest.batches.len())
            .find(|index| {
                self.batch_id(*index)
                    .is_ok_and(|id| request["batch_id"] == id)
            })
            .ok_or_else(|| fail("unknown screening packet capacity"))?;
        let required_bytes = encoded(&self.output_shape(&self.manifest.batches[batch], "x"))?.len()
            + self.receipt_overhead();
        Ok(
            (required_bytes > self.manifest.max_result_bytes).then_some(
                ScreeningCapacity::Result {
                    required_bytes,
                    max_bytes: self.manifest.max_result_bytes,
                },
            ),
        )
    }
}

pub(in crate::commands) fn receipt_overhead() -> usize {
    let output = ScreeningOutput {
        selection_id: "0".repeat(64),
        batch_id: "0".repeat(64),
        input_revision: "0".repeat(64),
        decisions: Vec::new(),
    };
    let before = encoded(&output).expect("screening shape serializes").len();
    encoded(&Receipt {
        schema: SCHEMA.into(),
        output,
        dependency_revision: "0".repeat(64),
        contract_revision: "0".repeat(64),
        reused_from: Some(Origin {
            selection_id: "0".repeat(64),
            batch_id: "0".repeat(64),
            input_revision: "0".repeat(64),
            receipt_revision: "0".repeat(64),
        }),
    })
    .expect("screening shape serializes")
    .len()
        - before
}

fn decode(
    selection: &Selection,
    batch: usize,
    raw: &str,
) -> miette::Result<(ScreeningOutput, Option<Receipt>)> {
    let value: Value = serde_json::from_str(raw).into_diagnostic()?;
    if value.get("schema").is_none() {
        return Ok((
            selection.normalize(batch, serde_json::from_value(value).into_diagnostic()?)?,
            None,
        ));
    }
    let receipt: Receipt = serde_json::from_value(value).into_diagnostic()?;
    if receipt.schema != SCHEMA
        || selection.manifest.receipt_policy != Some(ReceiptPolicy::ComputedPacketsV1)
        || !is_revision(&receipt.dependency_revision)
        || !is_revision(&receipt.contract_revision)
        || encoded(&receipt)? != raw
        || raw.len() > selection.manifest.max_result_bytes
        || receipt.dependency_revision
            != dependency_revision(
                &selection.request_for(&selection.manifest.batches[batch], batch)?,
                selection.manifest.max_input_bytes,
                &receipt.contract_revision,
            )?
    {
        return Err(fail(
            "computed screening receipt does not match its complete packet",
        ));
    }
    if let Some(origin) = &receipt.reused_from
        && (![
            &origin.selection_id,
            &origin.batch_id,
            &origin.input_revision,
            &origin.receipt_revision,
        ]
        .into_iter()
        .all(|value| is_revision(value))
            || origin.selection_id == selection.id)
    {
        return Err(fail("invalid authored screening origin"));
    }
    let output = selection.normalize(batch, receipt.output.clone())?;
    Ok((output, Some(receipt)))
}

pub(super) fn read_output(
    selection: &Selection,
    root: &Path,
    batch: usize,
    raw: &str,
    capture: &mut ProofCapture,
) -> miette::Result<ScreeningOutput> {
    let (output, receipt) = decode(selection, batch, raw)?;
    let Some(receipt) = receipt else {
        return Ok(output);
    };
    let Some(origin) = receipt.reused_from else {
        return Ok(output);
    };
    let directory = selection
        .directory
        .parent()
        .expect("selection owner")
        .join(&origin.selection_id);
    if !capture.selections.contains_key(&origin.selection_id) {
        let prior = retention_selection(root, &directory)?;
        capture
            .snapshots
            .insert(prior.manifest_path(), encoded(&prior.manifest)?);
        capture
            .selections
            .insert(origin.selection_id.clone(), prior);
    }
    let prior = &capture.selections[&origin.selection_id];
    let batch = (0..prior.manifest.batches.len())
        .find(|index| {
            prior
                .batch_id(*index)
                .is_ok_and(|value| value == origin.batch_id)
        })
        .ok_or_else(|| fail("authored screening packet is absent from its manifest"))?;
    let path = prior.receipt_path(batch)?;
    if !capture.snapshots.contains_key(&path) {
        let raw = read_optional(root, &path)?
            .ok_or_else(|| fail("authored screening receipt is missing"))?;
        capture.snapshots.insert(path.clone(), raw);
    }
    let original_raw = &capture.snapshots[&path];
    let (original, authored) = decode(prior, batch, original_raw)?;
    if authored
        .as_ref()
        .is_some_and(|value| value.reused_from.is_some())
    {
        return Err(fail("screening origin must be directly authored"));
    }
    if lk_core::concept::file_revision(original_raw) != origin.receipt_revision {
        return Err(ChangedAuthority.into());
    }
    let authored =
        authored.ok_or_else(|| fail("screening origin is not an authored computation"))?;
    if original.input_revision != origin.input_revision
        || authored.dependency_revision != receipt.dependency_revision
        || authored.contract_revision != receipt.contract_revision
        || original.decisions != output.decisions
    {
        return Err(fail("computed screening has a conflicting authored origin"));
    }
    Ok(output)
}

pub(super) fn read_for_reassessment(
    selection: &Selection,
    batch: usize,
    raw: &str,
) -> miette::Result<ScreeningOutput> {
    decode(selection, batch, raw).map(|(output, _)| output)
}

pub(super) fn encode_output(
    selection: &Selection,
    batch: usize,
    output: ScreeningOutput,
    contract_revision: &str,
) -> miette::Result<String> {
    if encoded(&output)?.len() + selection.receipt_overhead() > selection.manifest.max_result_bytes
    {
        return Err(fail(
            "computed screening output leaves no room for bounded reuse provenance",
        ));
    }
    let after = encoded(&Receipt {
        schema: SCHEMA.into(),
        dependency_revision: dependency_revision(
            &selection.request_for(&selection.manifest.batches[batch], batch)?,
            selection.manifest.max_input_bytes,
            contract_revision,
        )?,
        contract_revision: contract_revision.into(),
        output,
        reused_from: None,
    })?;
    if after.len() > selection.manifest.max_result_bytes {
        return Err(fail(
            "complete computed screening receipt exceeds its result bound",
        ));
    }
    Ok(after)
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    task_id: String,
    cache_hash: String,
    target: String,
    target_revision: String,
    config_revision: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Candidate {
    candidate_key: String,
    path: String,
    query_indexes: Vec<usize>,
    card: lk_vault::DiscoveryCandidateCard,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PacketInput {
    binding: Binding,
    source_context: Value,
    source_context_revision: String,
    policy: String,
    discovery: Vec<lk_vault::DiscoveryQuery>,
    examination_scope: String,
    scope_limit: String,
    max_input_bytes: usize,
    max_result_bytes: usize,
    max_reason_json_bytes: usize,
    candidates: Vec<Candidate>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestCandidate {
    candidate_key: String,
    path: String,
    #[serde(rename = "file_revision")]
    _file_revision: String,
    #[serde(rename = "source_revision")]
    _source_revision: String,
    query_indexes: Vec<usize>,
    card: lk_vault::DiscoveryCandidateCard,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    #[serde(rename = "selection_id")]
    _selection_id: String,
    #[serde(rename = "batch_id")]
    _batch_id: String,
    #[serde(rename = "input_revision")]
    _input_revision: String,
    binding: SelectionBinding,
    source_context: Value,
    source_context_revision: String,
    policy: String,
    discovery: Vec<lk_vault::DiscoveryQuery>,
    examination_scope: String,
    scope_limit: String,
    max_result_bytes: usize,
    max_reason_json_bytes: usize,
    candidates: Vec<RequestCandidate>,
}

pub(super) fn dependency_revision(
    request: &Value,
    max_input_bytes: usize,
    contract_revision: &str,
) -> miette::Result<String> {
    let request: Request = serde_json::from_value(request.clone()).into_diagnostic()?;
    let binding = request.binding;
    let packet = PacketInput {
        binding: Binding {
            task_id: binding.task_id,
            cache_hash: binding.cache_hash,
            target: binding.target,
            target_revision: binding.target_revision,
            config_revision: binding.config_revision,
        },
        source_context: request.source_context,
        source_context_revision: request.source_context_revision,
        policy: request.policy,
        discovery: request.discovery,
        examination_scope: request.examination_scope,
        scope_limit: request.scope_limit,
        max_input_bytes,
        max_result_bytes: request.max_result_bytes,
        max_reason_json_bytes: request.max_reason_json_bytes,
        candidates: request
            .candidates
            .into_iter()
            .map(|candidate| Candidate {
                candidate_key: candidate.candidate_key,
                path: candidate.path,
                query_indexes: candidate.query_indexes,
                card: candidate.card,
            })
            .collect(),
    };
    if packet.source_context_revision != digest(&packet.source_context)? {
        return Err(fail(
            "packet source context revision does not match its input",
        ));
    }
    digest(&("screening-packet-dependency-v1", contract_revision, packet))
}

#[cfg(test)]
mod tests {
    use super::super::tests::{Fixture, answer};
    use super::*;

    fn current_selection(fixture: &Fixture, input: usize, result: usize) -> Selection {
        fixture
            .selection(input, result)
            .prefer_computed_receipts(&fixture.root)
            .unwrap()
    }

    fn seed(fixture: &Fixture) -> (Selection, String, String) {
        let selection = current_selection(fixture, 8192, 4096);
        let contract = "c".repeat(64);
        let session = MutationSession::acquire(&fixture.root).unwrap();
        selection
            .create_native(&session, &fixture.pages(), &contract)
            .unwrap();
        let (_, request) = selection.prepare(&fixture.root).unwrap();
        let request = request.unwrap();
        selection
            .record_native(
                &session,
                &fixture.pages(),
                answer(&request, ScreeningDecision::Inspect),
                &contract,
            )
            .unwrap();
        let raw =
            std::fs::read_to_string(fixture.root.join(selection.receipt_path(0).unwrap())).unwrap();
        (selection, contract, raw)
    }

    fn change_file_without_changing_projection(fixture: &Fixture) -> String {
        let path = fixture.root.join("wiki/concepts/candidate-0.md");
        let before = std::fs::read_to_string(&path).unwrap();
        std::fs::write(path, format!("{before}\nAn additional unrelated note.\n")).unwrap();
        before
    }

    #[test]
    fn fresh_complete_inventory_reuses_whole_computation_without_rewriting_paid_receipts() {
        let fixture = Fixture::new(1);
        let (prior, contract, before) = seed(&fixture);
        let manifest_before = std::fs::read(fixture.root.join(prior.manifest_path())).unwrap();
        change_file_without_changing_projection(&fixture);
        let current = current_selection(&fixture, 8192, 4096);
        assert_ne!(current.id, prior.id);
        let session = MutationSession::acquire(&fixture.root).unwrap();
        current
            .create_native(&session, &fixture.pages(), &contract)
            .unwrap();
        let (progress, request) = current.prepare(&fixture.root).unwrap();
        assert!(request.is_none());
        assert!(progress.complete);
        assert_eq!(progress.inspect, 1);
        assert_eq!(progress.completed_batches, 1);
        let raw =
            std::fs::read_to_string(fixture.root.join(current.receipt_path(0).unwrap())).unwrap();
        let receipt: Receipt = serde_json::from_str(&raw).unwrap();
        assert_eq!(receipt.reused_from.unwrap().selection_id, prior.id);
        assert!(raw.len() <= 4096);
        assert!(
            serde_json::from_str::<ScreeningOutput>(&raw).is_err(),
            "legacy reader must refuse new provenance"
        );
        assert_eq!(
            std::fs::read_to_string(fixture.root.join(prior.receipt_path(0).unwrap())).unwrap(),
            before
        );
        assert_eq!(
            std::fs::read(fixture.root.join(prior.manifest_path())).unwrap(),
            manifest_before
        );
    }

    #[test]
    fn explicit_origin_reassessment_holds_only_dependent_current_packets_and_can_be_reassessed() {
        let fixture = Fixture::new(1);
        let (prior, contract, before) = seed(&fixture);
        let original_page = change_file_without_changing_projection(&fixture);
        let current = current_selection(&fixture, 8192, 4096);
        {
            let session = MutationSession::acquire(&fixture.root).unwrap();
            current
                .create_native(&session, &fixture.pages(), &contract)
                .unwrap();
        }
        let changed_page =
            std::fs::read(fixture.root.join("wiki/concepts/candidate-0.md")).unwrap();
        std::fs::write(
            fixture.root.join("wiki/concepts/candidate-0.md"),
            original_page,
        )
        .unwrap();
        let request = prior.request_for(&[0], 0).unwrap();
        let session = MutationSession::acquire(&fixture.root).unwrap();
        prior
            .record(
                &session,
                &fixture.pages(),
                answer(&request, ScreeningDecision::NotSelected),
                Some(&lk_core::concept::file_revision(&before)),
            )
            .unwrap();
        drop(session);
        std::fs::write(
            fixture.root.join("wiki/concepts/candidate-0.md"),
            &changed_page,
        )
        .unwrap();
        let (progress, request) = current.prepare(&fixture.root).unwrap();
        assert!(!progress.complete);
        assert_eq!(progress.processing_held_batches, 1);
        assert_eq!(progress.unknown, 1);
        assert_eq!(
            progress.deferred, 0,
            "a hold must not fabricate a Defer decision"
        );
        assert!(
            request.is_none(),
            "unchanged held authority must not spend another host call"
        );
        change_file_without_changing_projection(&fixture);
        let next = current_selection(&fixture, 8192, 4096);
        {
            let session = MutationSession::acquire(&fixture.root).unwrap();
            assert_eq!(
                next.create_native(&session, &fixture.pages(), &contract)
                    .unwrap(),
                0,
                "held receipt composition is not usable computation reuse"
            );
        }
        let (held, request) = next.prepare(&fixture.root).unwrap();
        assert_eq!(held.processing_held_batches, 1);
        assert!(
            request.is_none(),
            "inventory churn must retain the explicit reassessment hold"
        );
        std::fs::write(
            fixture.root.join("wiki/concepts/candidate-0.md"),
            &changed_page,
        )
        .unwrap();
        let raw =
            std::fs::read_to_string(fixture.root.join(current.receipt_path(0).unwrap())).unwrap();
        let request = current.request_for(&[0], 0).unwrap();
        let session = MutationSession::acquire(&fixture.root).unwrap();
        let fixed = current
            .record(
                &session,
                &fixture.pages(),
                answer(&request, ScreeningDecision::NotSelected),
                Some(&lk_core::concept::file_revision(&raw)),
            )
            .unwrap();
        assert!(fixed.complete);
        assert_eq!(fixed.processing_held_batches, 0);
    }

    #[test]
    fn missing_or_corrupt_origin_is_fatal_instead_of_an_authority_hold() {
        let fixture = Fixture::new(1);
        let (prior, contract, before) = seed(&fixture);
        change_file_without_changing_projection(&fixture);
        let current = current_selection(&fixture, 8192, 4096);
        let session = MutationSession::acquire(&fixture.root).unwrap();
        current
            .create_native(&session, &fixture.pages(), &contract)
            .unwrap();
        let path = fixture.root.join(prior.receipt_path(0).unwrap());
        std::fs::remove_file(&path).unwrap();
        assert!(current.prepare(&fixture.root).is_err());
        std::fs::write(&path, "{broken}").unwrap();
        assert!(current.prepare(&fixture.root).is_err());
        std::fs::write(path, before).unwrap();
        assert!(current.prepare(&fixture.root).unwrap().0.complete);
    }

    #[test]
    fn unknown_heads_and_non_direct_origins_are_fatal() {
        let fixture = Fixture::new(1);
        let (prior, contract, original) = seed(&fixture);
        change_file_without_changing_projection(&fixture);
        let current = current_selection(&fixture, 8192, 4096);
        let session = MutationSession::acquire(&fixture.root).unwrap();
        current
            .create_native(&session, &fixture.pages(), &contract)
            .unwrap();
        let current_path = fixture.root.join(current.receipt_path(0).unwrap());
        let raw = std::fs::read_to_string(&current_path).unwrap();
        for selection_id in [current.id.clone(), "f".repeat(64)] {
            let mut value: Receipt = serde_json::from_str(&raw).unwrap();
            value.reused_from.as_mut().unwrap().selection_id = selection_id;
            std::fs::write(&current_path, encoded(&value).unwrap()).unwrap();
            let error = current.prepare(&fixture.root).unwrap_err();
            assert!(error.downcast_ref::<ChangedAuthority>().is_none());
        }
        std::fs::write(&current_path, &raw).unwrap();
        let mut value: Receipt = serde_json::from_str(&original).unwrap();
        value.reused_from = Some(Origin {
            selection_id: current.id.clone(),
            batch_id: current.batch_id(0).unwrap(),
            input_revision: "e".repeat(64),
            receipt_revision: lk_core::concept::file_revision(&raw),
        });
        std::fs::write(
            fixture.root.join(prior.receipt_path(0).unwrap()),
            encoded(&value).unwrap(),
        )
        .unwrap();
        let error = current.prepare(&fixture.root).unwrap_err();
        assert!(error.downcast_ref::<ChangedAuthority>().is_none());
        std::fs::write(fixture.root.join(prior.receipt_path(0).unwrap()), original).unwrap();
        let head_path = fixture
            .root
            .join(current.directory.parent().unwrap().join("current.json"));
        let mut head: Head = serde_json::from_slice(&std::fs::read(&head_path).unwrap()).unwrap();
        head.schema = "screening-selection-head-future".into();
        std::fs::write(head_path, encoded(&head).unwrap()).unwrap();
        assert!(
            current
                .create_native(&session, &fixture.pages(), &contract)
                .is_err()
        );
    }

    #[test]
    fn input_drift_refuses_atomic_manifest_receipt_and_head_publication() {
        let fixture = Fixture::new(1);
        let (_, contract, _) = seed(&fixture);
        change_file_without_changing_projection(&fixture);
        let current = current_selection(&fixture, 8192, 4096);
        let pages = fixture.pages();
        std::fs::write(fixture.root.join("wiki/documents/source.md"), "changed").unwrap();
        let session = MutationSession::acquire(&fixture.root).unwrap();
        assert!(current.create_native(&session, &pages, &contract).is_err());
        assert!(!fixture.root.join(current.manifest_path()).exists());
        assert!(!fixture.root.join(current.receipt_path(0).unwrap()).exists());
        let head = capture_head(&fixture.root, current.directory.parent().unwrap())
            .unwrap()
            .unwrap();
        assert_ne!(head.selection_id, current.id);
    }

    #[test]
    fn legacy_receipts_and_changed_contracts_do_not_seed_computations() {
        for legacy in [true, false] {
            let fixture = Fixture::new(1);
            let (prior, contract, before) = seed(&fixture);
            if legacy {
                let value: Receipt = serde_json::from_str(&before).unwrap();
                let session = MutationSession::acquire(&fixture.root).unwrap();
                prior
                    .record(
                        &session,
                        &fixture.pages(),
                        value.output,
                        Some(&lk_core::concept::file_revision(&before)),
                    )
                    .unwrap();
            }
            change_file_without_changing_projection(&fixture);
            let current = current_selection(&fixture, 8192, 4096);
            let contract = if legacy { contract } else { "d".repeat(64) };
            let session = MutationSession::acquire(&fixture.root).unwrap();
            current
                .create_native(&session, &fixture.pages(), &contract)
                .unwrap();
            let (progress, request) = current.prepare(&fixture.root).unwrap();
            assert_eq!(progress.completed_batches, 0);
            assert!(request.is_some());
        }
    }

    #[test]
    fn planned_legacy_selection_keeps_its_identity_layout_receipts_and_native_mode() {
        let fixture = Fixture::new(1);
        let legacy = fixture.selection(8192, 4096);
        let session = MutationSession::acquire(&fixture.root).unwrap();
        legacy.create(&session, &fixture.pages()).unwrap();
        let manifest = std::fs::read(fixture.root.join(legacy.manifest_path())).unwrap();
        let current = fixture
            .selection(8192, 4096)
            .prefer_computed_receipts(&fixture.root)
            .unwrap();
        assert_eq!(current.id, legacy.id);
        assert!(current.manifest.receipt_policy.is_none());
        assert_eq!(
            current
                .create_native(&session, &fixture.pages(), &"c".repeat(64))
                .unwrap(),
            0
        );
        let request = current.prepare(&fixture.root).unwrap().1.unwrap();
        assert!(!is_computed_request(&request).unwrap());
        current
            .record_native(
                &session,
                &fixture.pages(),
                answer(&request, ScreeningDecision::Inspect),
                &"c".repeat(64),
            )
            .unwrap();
        let raw =
            std::fs::read_to_string(fixture.root.join(current.receipt_path(0).unwrap())).unwrap();
        assert!(serde_json::from_str::<ScreeningOutput>(&raw).is_ok());
        assert_eq!(
            std::fs::read(fixture.root.join(current.manifest_path())).unwrap(),
            manifest
        );
        assert!(
            !fixture
                .root
                .join(current.directory.parent().unwrap().join("current.json"))
                .exists()
        );
    }

    #[test]
    fn reused_defer_remains_incomplete_without_a_new_request() {
        let fixture = Fixture::new(1);
        let prior = current_selection(&fixture, 8192, 4096);
        let contract = "c".repeat(64);
        {
            let session = MutationSession::acquire(&fixture.root).unwrap();
            prior
                .create_native(&session, &fixture.pages(), &contract)
                .unwrap();
            let request = prior.prepare(&fixture.root).unwrap().1.unwrap();
            prior
                .record_native(
                    &session,
                    &fixture.pages(),
                    answer(&request, ScreeningDecision::Defer),
                    &contract,
                )
                .unwrap();
        }
        change_file_without_changing_projection(&fixture);
        let current = current_selection(&fixture, 8192, 4096);
        let session = MutationSession::acquire(&fixture.root).unwrap();
        assert_eq!(
            current
                .create_native(&session, &fixture.pages(), &contract)
                .unwrap(),
            1
        );
        assert_eq!(
            current
                .create_native(&session, &fixture.pages(), &contract)
                .unwrap(),
            0
        );
        let (progress, request) = current.prepare(&fixture.root).unwrap();
        assert_eq!(progress.deferred, 1);
        assert_eq!(progress.unknown, 0);
        assert!(!progress.complete);
        assert!(request.is_none());
    }

    #[test]
    fn staged_references_protect_head_and_origins_then_completed_state_is_removable() {
        let fixture = Fixture::new(1);
        let (prior, contract, _) = seed(&fixture);
        change_file_without_changing_projection(&fixture);
        let current = current_selection(&fixture, 8192, 4096);
        let session = MutationSession::acquire(&fixture.root).unwrap();
        current
            .create_native(&session, &fixture.pages(), &contract)
            .unwrap();
        let path = fixture
            .root
            .join(".lorekeeper/queue/results/live.section.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            encoded(&lk_queue::SectionResult {
                task_id: current.manifest.binding.task_id.clone(),
                cache_hash: current.manifest.binding.cache_hash.clone(),
                target_revision: current.manifest.binding.target_revision.clone(),
                context_revision: None,
                body: String::new(),
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(prune(&session, &BTreeSet::new()).unwrap(), 0);
        assert!(fixture.root.join(prior.manifest_path()).exists());
        assert!(
            fixture
                .root
                .join(current.directory.parent().unwrap().join("current.json"))
                .exists()
        );
        std::fs::remove_file(path).unwrap();
        assert_eq!(prune(&session, &BTreeSet::new()).unwrap(), 2);
        assert!(
            !fixture
                .root
                .join(current.directory.parent().unwrap())
                .exists()
        );
    }

    #[test]
    fn native_storage_reserves_the_complete_derived_receipt_envelope() {
        let fixture = Fixture::new(1);
        let selection = current_selection(&fixture, 8192, 4096);
        let request = selection.request_for(&[0], 0).unwrap();
        let output = answer(&request, ScreeningDecision::Inspect);
        let size = encoded(&output).unwrap().len();
        let mut exact = selection;
        exact.manifest.max_result_bytes = size + receipt_overhead();
        let value = encode_output(&exact, 0, output.clone(), &"c".repeat(64)).unwrap();
        assert!(value.len() <= exact.manifest.max_result_bytes);
        exact.manifest.max_result_bytes -= 1;
        assert!(encode_output(&exact, 0, output, &"c".repeat(64)).is_err());
    }

    #[test]
    fn packet_dependencies_separate_current_closure_from_semantic_inputs() {
        let fixture = Fixture::new(2);
        let selection = current_selection(&fixture, 32768, 4096);
        let request = selection.request_for(&[0, 1], 0).unwrap();
        let revision = dependency_revision(&request, 32768, "contract").unwrap();
        let mut closure = request.clone();
        closure["selection_id"] = json!("other-selection");
        closure["batch_id"] = json!("other-batch");
        closure["input_revision"] = json!("other-input");
        closure["binding"]["inventory_revision"] = json!("other-inventory");
        closure["candidates"][0]["file_revision"] = json!("new-captured-file");
        closure["candidates"][0]["source_revision"] = json!("new-captured-source");
        assert_eq!(
            dependency_revision(&closure, 32768, "contract").unwrap(),
            revision
        );
        for field in [
            "task_id",
            "cache_hash",
            "target",
            "target_revision",
            "config_revision",
        ] {
            let mut changed = request.clone();
            changed["binding"][field] = json!("changed");
            assert_ne!(
                dependency_revision(&changed, 32768, "contract").unwrap(),
                revision
            );
        }
        let mut changed = request.clone();
        changed["candidates"].as_array_mut().unwrap().reverse();
        assert_ne!(
            dependency_revision(&changed, 32768, "contract").unwrap(),
            revision
        );
        let mut changed = request.clone();
        changed["candidates"][0]["card"]["title"]["text"] = json!("Changed title");
        assert_ne!(
            dependency_revision(&changed, 32768, "contract").unwrap(),
            revision
        );
        for field in ["policy", "examination_scope", "scope_limit"] {
            let mut changed = request.clone();
            changed[field] = json!("changed");
            assert_ne!(
                dependency_revision(&changed, 32768, "contract").unwrap(),
                revision
            );
        }
        for field in ["max_result_bytes", "max_reason_json_bytes"] {
            let mut changed = request.clone();
            changed[field] = json!(request[field].as_u64().unwrap() + 1);
            assert_ne!(
                dependency_revision(&changed, 32768, "contract").unwrap(),
                revision
            );
        }
        let mut changed = request.clone();
        changed["discovery"][0]["query"] = json!("Another query");
        assert_ne!(
            dependency_revision(&changed, 32768, "contract").unwrap(),
            revision
        );
        let mut changed = request.clone();
        changed["source_context"]["text"] = json!("Changed source");
        changed["source_context_revision"] = json!(digest(&changed["source_context"]).unwrap());
        assert_ne!(
            dependency_revision(&changed, 32768, "contract").unwrap(),
            revision
        );
        assert_ne!(
            dependency_revision(&request, 32769, "contract").unwrap(),
            revision
        );
        assert_ne!(
            dependency_revision(&request, 32768, "other-contract").unwrap(),
            revision
        );
        let mut malformed = request;
        malformed["source_context"]["text"] = json!("Changed source");
        assert!(dependency_revision(&malformed, 32768, "contract").is_err());
        for path in ["request", "binding", "candidate", "card"] {
            let mut future = closure.clone();
            match path {
                "request" => future["new_semantics"] = json!(true),
                "binding" => future["binding"]["new_semantics"] = json!(true),
                "candidate" => future["candidates"][0]["new_semantics"] = json!(true),
                "card" => future["candidates"][0]["card"]["new_semantics"] = json!(true),
                _ => unreachable!(),
            }
            assert!(
                dependency_revision(&future, 32768, "contract").is_err(),
                "{path}"
            );
        }
    }
}
