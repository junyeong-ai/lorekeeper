use super::*;
use crate::commands::work::host::{HoldIdentity, HostAnswers};
#[cfg(unix)]
use crate::commands::work::host::{HoldScope, PendingHold, Standing};

const SCHEMA: &str = "screening-rejections-v1";

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

    fn is_valid(&self) -> bool {
        is_revision(self.response_revision())
            && match self {
                Self::ReferenceCoverage {
                    missing,
                    duplicate,
                    unknown,
                    ..
                } => *missing != 0 || *duplicate != 0 || *unknown != 0,
                Self::InvalidReasons {
                    blank,
                    oversized,
                    max_reason_json_bytes,
                    largest_oversized_json_bytes,
                    ..
                } => {
                    (*blank != 0 || *oversized != 0)
                        && if *oversized == 0 {
                            *largest_oversized_json_bytes == 0
                        } else {
                            largest_oversized_json_bytes > max_reason_json_bytes
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

/// A reply that answered this exact request unusably. The same request spends no call.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SemanticHold {
    request_revision: String,
    rejection: ScreeningRejection,
}

/// A host that failed on this packet input. The hold outlives contract changes; only a
/// repeat of the exact request it records exhausts it.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OperationalHold {
    input_revision: String,
    /// The exact request that failed; holds written before it was kept have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    request_revision: Option<String>,
    failure: super::super::state::InvocationFailure,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    exhausted: bool,
}

/// The latest hold of each kind for one task. A hold names the request it stopped; once
/// the undecided rows change, it no longer matches and is simply superseded.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Rejections {
    schema: String,
    task_id: String,
    cache_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    semantic: Option<SemanticHold>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    operational: Option<OperationalHold>,
}

impl Rejections {
    fn path(directory: &Path) -> PathBuf {
        directory.join("rejections.json")
    }

    pub(super) fn decode(raw: &str) -> miette::Result<Self> {
        let rejections: Self = decode_stored(raw)?;
        if rejections.schema != SCHEMA
            || rejections.semantic.is_none() && rejections.operational.is_none()
            || rejections.semantic.as_ref().is_some_and(|hold| {
                !is_revision(&hold.request_revision) || !hold.rejection.is_valid()
            })
            || rejections.operational.as_ref().is_some_and(|hold| {
                !is_revision(&hold.input_revision)
                    || hold
                        .request_revision
                        .as_deref()
                        .is_some_and(|revision| !is_revision(revision))
            })
        {
            return Err(fail("invalid screening rejection receipt"));
        }
        if let Some(hold) = &rejections.operational {
            hold.failure.validate()?;
        }
        Ok(rejections)
    }

    pub(super) fn identity(&self) -> (&str, &str) {
        (&self.task_id, &self.cache_hash)
    }

    pub(super) fn load(
        root: &Path,
        directory: &Path,
        task_id: &str,
        cache_hash: &str,
    ) -> miette::Result<Option<(PathBuf, String, Self)>> {
        let path = Self::path(directory);
        let Some(raw) = read_optional(root, &path)? else {
            return Ok(None);
        };
        let rejections = Self::decode(&raw)?;
        if rejections.identity() != (task_id, cache_hash) {
            return Err(fail("screening rejection receipt belongs to another task"));
        }
        Ok(Some((path, raw, rejections)))
    }

    fn host_identity(&self) -> HoldIdentity {
        HoldIdentity::Screening {
            task_id: self.task_id.clone(),
            cache_hash: self.cache_hash.clone(),
        }
    }

    fn hold_reason(
        &self,
        request: &Request,
        answers: &HostAnswers,
    ) -> miette::Result<Option<String>> {
        if let Some(hold) = &self.semantic
            && hold.request_revision == request.request_revision
        {
            return Ok(Some(hold.rejection.to_string()));
        }
        if let Some(hold) = &self.operational
            && hold.input_revision == request.input_revision()?
        {
            return Ok(answers
                .standing(&self.host_identity(), &hold.failure, hold.exhausted)
                .reason(
                    &hold.failure,
                    &format!("lore work select-retry --task-id {}", self.task_id),
                ));
        }
        Ok(None)
    }
}

/// Every pending unanswered screening hold, for choosing a probe.
#[cfg(unix)]
pub(in crate::commands::work) fn pending_holds(root: &Path) -> miette::Result<Vec<PendingHold>> {
    let mut holds = Vec::new();
    for task_directory in crate::commands::work::host::store_entries(root, Path::new(STORE))? {
        let path = Rejections::path(&task_directory);
        let Some(raw) = read_optional(root, &path)? else {
            continue;
        };
        let rejections = Rejections::decode(&raw)?;
        if Some(path.as_path())
            != directory(&rejections.task_id, &rejections.cache_hash)
                .map(|directory| Rejections::path(&directory))
                .ok()
                .as_deref()
        {
            return Err(fail(
                "screening rejection receipt is outside its task directory",
            ));
        }
        if let Some(hold) = &rejections.operational
            && hold.failure.is_unanswered()
            && !hold.exhausted
        {
            holds.push(PendingHold {
                identity: rejections.host_identity(),
                engine: Some(hold.failure.engine()),
                scope: HoldScope::Queue {
                    task_id: rejections.task_id.clone(),
                    cache_hash: rejections.cache_hash.clone(),
                },
                host: hold.failure.host().cloned(),
            });
        }
    }
    Ok(holds)
}

impl Selection {
    /// Why this exact request must not be sent again, if a retained hold names it.
    pub fn hold_reason(&self, request: &Request) -> miette::Result<Option<String>> {
        match &self.rejections {
            Some(rejections) => rejections.hold_reason(request, &self.answers),
            None => Ok(None),
        }
    }

    /// Clear the operational hold on the current next packet, and nothing else.
    #[cfg(unix)]
    pub fn plan_retry(&self, root: &Path) -> miette::Result<MutationPlan> {
        let request = self
            .next_request(false)?
            .ok_or_else(|| fail("candidate screening has no undecided packet"))?;
        let input = request.input_revision()?;
        plan_update(root, &request, |rejections| {
            if rejections
                .operational
                .take()
                .is_none_or(|hold| hold.input_revision != input)
            {
                return Err(fail("the current packet has no operational hold"));
            }
            Ok(())
        })
    }
}

#[cfg(unix)]
pub(in crate::commands::work) fn plan_rejection(
    root: &Path,
    request: &Request,
    rejection: ScreeningRejection,
) -> miette::Result<MutationPlan> {
    plan_update(root, request, |rejections| {
        rejections.semantic = Some(SemanticHold {
            request_revision: request.request_revision.clone(),
            rejection,
        });
        Ok(())
    })
}

#[cfg(unix)]
pub(in crate::commands::work) fn plan_failed_attempt(
    root: &Path,
    request: &Request,
    failure: super::super::state::InvocationFailure,
) -> miette::Result<MutationPlan> {
    let input_revision = request.input_revision()?;
    let answers = HostAnswers::load(root)?;
    plan_update(root, request, |rejections| {
        let prior = rejections
            .operational
            .as_ref()
            .filter(|hold| hold.input_revision == input_revision);
        if let Some(hold) = prior
            && answers.standing(&rejections.host_identity(), &hold.failure, hold.exhausted)
                != Standing::Released
        {
            return Err(fail("screening packet already has an operational hold"));
        }
        let exhausted = answers.exhausts(
            &failure,
            prior
                .filter(|hold| {
                    hold.request_revision.as_deref() == Some(request.request_revision.as_str())
                })
                .map(|hold| (&hold.failure, hold.exhausted)),
        );
        rejections.operational = Some(OperationalHold {
            input_revision,
            request_revision: Some(request.request_revision.clone()),
            failure,
            exhausted,
        });
        Ok(())
    })
}

#[cfg(unix)]
fn plan_update(
    root: &Path,
    request: &Request,
    update: impl FnOnce(&mut Rejections) -> miette::Result<()>,
) -> miette::Result<MutationPlan> {
    request.validate()?;
    let directory = directory(&request.task_id, &request.cache_hash)?;
    let ledger = CapturedLedger::load(root, &directory, &request.task_id, &request.cache_hash)?;
    if !request.reassess
        && request
            .candidates
            .iter()
            .all(|candidate| ledger.references.contains_key(&candidate.candidate_key))
    {
        return Err(fail("screening attempt cannot replace recorded decisions"));
    }
    let current = Rejections::load(root, &directory, &request.task_id, &request.cache_hash)?;
    let (before, mut rejections) = match current {
        Some((_, raw, rejections)) => (Some(raw), rejections),
        None => (
            None,
            Rejections {
                schema: SCHEMA.into(),
                task_id: request.task_id.clone(),
                cache_hash: request.cache_hash.clone(),
                semantic: None,
                operational: None,
            },
        ),
    };
    update(&mut rejections)?;
    let path = Rejections::path(&directory);
    let mut plan = MutationPlan::default();
    ledger.guard(&mut plan);
    if rejections.semantic.is_none() && rejections.operational.is_none() {
        if let Some(before) = before {
            plan.remove(path, before);
        }
    } else {
        let after = encoded(&rejections)?;
        Rejections::decode(&after)?;
        plan.publish_expected(root, path, before, after)
            .into_diagnostic()?;
    }
    Ok(plan)
}
