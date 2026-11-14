use super::*;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub(in crate::commands) enum ScreeningRejection {
    ReferenceCoverage {
        missing: usize,
        duplicate: usize,
        unknown: usize,
        response_revision: String,
    },
    InvalidReasons {
        blank: usize,
        oversized: usize,
        max_reason_json_bytes: usize,
        largest_oversized_json_bytes: usize,
        response_revision: String,
    },
}

impl ScreeningRejection {
    fn response_revision(&self) -> &str {
        match self {
            Self::ReferenceCoverage {
                response_revision, ..
            }
            | Self::InvalidReasons {
                response_revision, ..
            } => response_revision,
        }
    }

    fn is_valid(&self, candidates: usize, reason_limit: usize, result_limit: usize) -> bool {
        match self {
            Self::ReferenceCoverage {
                missing,
                duplicate,
                unknown,
                ..
            } => *missing <= candidates && (*missing != 0 || *duplicate != 0 || *unknown != 0),
            Self::InvalidReasons {
                blank,
                oversized,
                max_reason_json_bytes,
                largest_oversized_json_bytes,
                ..
            } => {
                blank
                    .checked_add(*oversized)
                    .is_some_and(|count| count > 0 && count <= candidates)
                    && *max_reason_json_bytes == reason_limit
                    && if *oversized == 0 {
                        *largest_oversized_json_bytes == 0
                    } else {
                        *largest_oversized_json_bytes > reason_limit
                            && *largest_oversized_json_bytes <= result_limit
                    }
            }
        }
    }
}

impl std::fmt::Display for ScreeningRejection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReferenceCoverage {
                missing,
                duplicate,
                unknown,
                ..
            } => write!(
                formatter,
                "screening references do not cover the assigned candidates: {missing} missing, {duplicate} duplicate, {unknown} unknown; no decisions were recorded"
            ),
            Self::InvalidReasons {
                blank,
                oversized,
                max_reason_json_bytes,
                largest_oversized_json_bytes,
                ..
            } => write!(
                formatter,
                "screening decision reasons are invalid: {blank} blank, {oversized} exceed {max_reason_json_bytes} compact UTF-8 JSON bytes (largest {largest_oversized_json_bytes}); no decisions were recorded"
            ),
        }
    }
}

impl std::error::Error for ScreeningRejection {}
impl miette::Diagnostic for ScreeningRejection {}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Rejections {
    schema: String,
    selection_id: String,
    #[serde(deserialize_with = "crate::commands::work::receipt::unique_entries")]
    batches: BTreeMap<String, Rejection>,
    #[serde(deserialize_with = "crate::commands::work::receipt::unique_entries")]
    attempts: BTreeMap<String, FailedAttempt>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FailedAttempt {
    canonical_revision: String,
    request_revision: String,
    contract_revision: String,
    failure: super::super::state::InvocationFailure,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviousRejections {
    schema: String,
    selection_id: String,
    #[serde(deserialize_with = "crate::commands::work::receipt::unique_entries")]
    batches: BTreeMap<String, Rejection>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Rejection {
    input_revision: String,
    request_revision: String,
    contract_revision: String,
    rejection: ScreeningRejection,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyRejections {
    schema: String,
    selection_id: String,
    #[serde(deserialize_with = "crate::commands::work::receipt::unique_entries")]
    batches: BTreeMap<String, LegacyRejection>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyRejection {
    input_revision: String,
    request_revision: String,
    contract_revision: String,
    rejection: LegacyCoverage,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyCoverage {
    missing: usize,
    duplicate: usize,
    unknown: usize,
    response_revision: String,
}

fn require_revision(revision: &str) -> miette::Result<()> {
    require_directory_identity(std::ffi::OsStr::new(revision))
}

impl Rejections {
    fn decode(raw: &str, selection: &Selection) -> miette::Result<Self> {
        if raw.len() > (selection.manifest.batches.len() + 1).saturating_mul(2048) {
            return Err(fail(
                "screening attempts exceed their declared storage bound",
            ));
        }
        #[derive(Deserialize)]
        struct Header {
            schema: String,
        }
        let header: Header = crate::commands::work::receipt::decode(raw).into_diagnostic()?;
        let receipt: Self = match header.schema.as_str() {
            "screening-rejections-v1" => {
                let legacy: LegacyRejections =
                    crate::commands::work::receipt::decode(raw).into_diagnostic()?;
                if legacy.schema != "screening-rejections-v1" {
                    return Err(fail("invalid legacy screening rejection schema"));
                }
                Self {
                    schema: "screening-rejections-v3".into(),
                    selection_id: legacy.selection_id,
                    batches: legacy
                        .batches
                        .into_iter()
                        .map(|(batch, entry)| {
                            (
                                batch,
                                Rejection {
                                    input_revision: entry.input_revision,
                                    request_revision: entry.request_revision,
                                    contract_revision: entry.contract_revision,
                                    rejection: ScreeningRejection::ReferenceCoverage {
                                        missing: entry.rejection.missing,
                                        duplicate: entry.rejection.duplicate,
                                        unknown: entry.rejection.unknown,
                                        response_revision: entry.rejection.response_revision,
                                    },
                                },
                            )
                        })
                        .collect(),
                    attempts: BTreeMap::new(),
                }
            }
            "screening-rejections-v2" => {
                let previous: PreviousRejections =
                    crate::commands::work::receipt::decode(raw).into_diagnostic()?;
                if previous.schema != "screening-rejections-v2" {
                    return Err(fail("invalid prior screening rejection schema"));
                }
                Self {
                    schema: "screening-rejections-v3".into(),
                    selection_id: previous.selection_id,
                    batches: previous.batches,
                    attempts: BTreeMap::new(),
                }
            }
            "screening-rejections-v3" => {
                crate::commands::work::receipt::decode(raw).into_diagnostic()?
            }
            _ => return Err(fail("invalid screening rejection schema")),
        };
        if receipt.schema != "screening-rejections-v3"
            || receipt.selection_id != selection.id
            || receipt.batches.is_empty() && receipt.attempts.is_empty()
        {
            return Err(fail("invalid screening rejection identity or schema"));
        }
        for (batch, attempt) in &receipt.attempts {
            let index = (0..selection.manifest.batches.len())
                .find(|index| selection.batch_id(*index).is_ok_and(|id| &id == batch))
                .ok_or_else(|| fail("screening attempt names an unknown batch"))?;
            let request = selection.request_for(&selection.manifest.batches[index], index)?;
            require_revision(&attempt.contract_revision)?;
            attempt.failure.validate()?;
            if attempt.canonical_revision != digest(&request)?
                || attempt.request_revision != digest(&(&request, &attempt.contract_revision))?
            {
                return Err(fail("invalid screening attempt binding"));
            }
        }
        for (batch, rejected) in &receipt.batches {
            let index = (0..selection.manifest.batches.len())
                .find(|index| selection.batch_id(*index).is_ok_and(|id| &id == batch))
                .ok_or_else(|| fail("screening rejection names an unknown batch"))?;
            let request = selection.request_for(&selection.manifest.batches[index], index)?;
            require_revision(&rejected.contract_revision)?;
            require_revision(rejected.rejection.response_revision())?;
            if rejected.input_revision != request["input_revision"]
                || rejected.request_revision != digest(&(&request, &rejected.contract_revision))?
                || !rejected.rejection.is_valid(
                    selection.manifest.batches[index].len(),
                    selection.manifest.max_reason_json_bytes,
                    selection.manifest.max_result_bytes,
                )
            {
                return Err(fail("invalid screening rejection binding or disposition"));
            }
        }
        Ok(receipt)
    }
}

impl Selection {
    fn rejection_path(&self) -> PathBuf {
        self.directory.join("rejections.json")
    }

    pub(super) fn capture_rejections(
        &self,
        root: &Path,
    ) -> miette::Result<Option<(PathBuf, String)>> {
        let path = self.rejection_path();
        let Some(raw) = read_optional(root, &path)? else {
            return Ok(None);
        };
        Rejections::decode(&raw, self)?;
        Ok(Some((path, raw)))
    }

    pub fn rejection_reason(
        &self,
        root: &Path,
        request: &Value,
        contract: &str,
    ) -> miette::Result<Option<String>> {
        let Some((_, raw)) = self.capture_rejections(root)? else {
            return Ok(None);
        };
        let receipt = Rejections::decode(&raw, self)?;
        let Some(batch) = request["batch_id"].as_str() else {
            return Err(fail("screening request has no batch"));
        };
        let revision = digest(&(request, contract))?;
        let canonical = digest(request)?;
        Ok(receipt
            .batches
            .get(batch)
            .filter(|rejected| rejected.request_revision == revision)
            .map(|rejected| rejected.rejection.to_string())
            .or_else(|| receipt.attempts.get(batch)
                .filter(|attempt| attempt.canonical_revision == canonical)
                .map(|attempt| format!("{}; diagnose the host, then use lore work select-retry --task-id {} --batch-id {batch}", attempt.failure, self.manifest.binding.task_id))))
    }

    pub fn record_rejection(
        &self,
        session: &MutationSession,
        pages: &BTreeMap<PathBuf, String>,
        request: &Value,
        contract: &str,
        rejection: ScreeningRejection,
    ) -> miette::Result<()> {
        let batch = request["batch_id"]
            .as_str()
            .ok_or_else(|| fail("screening request has no batch"))?;
        let plan = self.plan_update(session.root(), pages, request, |receipt| {
            receipt.batches.insert(
                batch.into(),
                Rejection {
                    input_revision: request["input_revision"]
                        .as_str()
                        .expect("validated request")
                        .into(),
                    request_revision: digest(&(request, contract))?,
                    contract_revision: contract.into(),
                    rejection,
                },
            );
            Ok(())
        })?;
        session.apply(plan).into_diagnostic()
    }

    #[cfg(unix)]
    pub fn record_failed_attempt(
        &self,
        session: &MutationSession,
        pages: &BTreeMap<PathBuf, String>,
        request: &Value,
        contract: &str,
        failure: super::super::state::InvocationFailure,
    ) -> miette::Result<()> {
        let batch = request["batch_id"]
            .as_str()
            .ok_or_else(|| fail("screening request has no batch"))?;
        let plan = self.plan_update(session.root(), pages, request, |receipt| {
            if receipt.attempts.contains_key(batch) {
                return Err(fail("screening packet already has an operational hold"));
            }
            receipt.attempts.insert(
                batch.into(),
                FailedAttempt {
                    canonical_revision: digest(request)?,
                    request_revision: digest(&(request, contract))?,
                    contract_revision: contract.into(),
                    failure,
                },
            );
            Ok(())
        })?;
        session.apply(plan).into_diagnostic()
    }

    #[cfg(unix)]
    pub fn plan_retry(
        &self,
        root: &Path,
        pages: &BTreeMap<PathBuf, String>,
        batch: &str,
    ) -> miette::Result<MutationPlan> {
        let request = self.packet(root, batch)?;
        self.plan_update(root, pages, &request, |receipt| {
            if receipt.attempts.remove(batch).is_none() {
                return Err(fail("screening packet has no current operational hold"));
            }
            Ok(())
        })
    }

    fn plan_update(
        &self,
        root: &Path,
        pages: &BTreeMap<PathBuf, String>,
        request: &Value,
        update: impl FnOnce(&mut Rejections) -> miette::Result<()>,
    ) -> miette::Result<MutationPlan> {
        let captured = self.capture_ledger(root)?;
        let batch = request["batch_id"]
            .as_str()
            .ok_or_else(|| fail("screening request has no batch"))?;
        if self.packet(root, batch)? != *request {
            return Err(fail(
                "screening request changed before recording its attempt",
            ));
        }
        let index = (0..self.manifest.batches.len())
            .find(|index| self.batch_id(*index).is_ok_and(|id| id == batch))
            .expect("validated batch");
        let output_path = self.receipt_path(index)?;
        if captured.raw.contains_key(&output_path) {
            return Err(fail("screening attempt cannot replace recorded decisions"));
        }
        let path = self.rejection_path();
        let before = captured.raw.get(&path).cloned();
        let mut receipt = before
            .as_ref()
            .map(|raw| Rejections::decode(raw, self))
            .transpose()?
            .unwrap_or_else(|| Rejections {
                schema: "screening-rejections-v3".into(),
                selection_id: self.id.clone(),
                batches: BTreeMap::new(),
                attempts: BTreeMap::new(),
            });
        update(&mut receipt)?;
        let after = if receipt.batches.is_empty() && receipt.attempts.is_empty() {
            None
        } else {
            let after = encoded(&receipt)?;
            Rejections::decode(&after, self)?;
            Some(after)
        };
        let mut plan = MutationPlan::default();
        self.guard_pages(root, pages, &mut plan)?;
        plan.require_absent(output_path);
        for (guard, bytes) in captured.raw {
            if guard != path {
                plan.require_unchanged(guard, bytes);
            }
        }
        if let Some(after) = after {
            plan.publish_expected(root, path, before, after)
                .into_diagnostic()?;
        } else if let Some(before) = before {
            plan.remove(path, before);
        }
        Ok(plan)
    }
}
