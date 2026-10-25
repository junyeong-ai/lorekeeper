use super::*;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::commands) struct ScreeningRejection {
    pub missing: usize,
    pub duplicate: usize,
    pub unknown: usize,
    pub response_revision: String,
}

impl std::fmt::Display for ScreeningRejection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "screening references do not cover the assigned candidates: {} missing, {} duplicate, {} unknown; no decisions were recorded",
            self.missing, self.duplicate, self.unknown
        )
    }
}

impl std::error::Error for ScreeningRejection {}
impl miette::Diagnostic for ScreeningRejection {}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Rejections {
    schema: String,
    selection_id: String,
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

fn require_revision(revision: &str) -> miette::Result<()> {
    require_directory_identity(std::ffi::OsStr::new(revision))
}

impl Rejections {
    fn decode(raw: &str, selection: &Selection) -> miette::Result<Self> {
        let receipt: Self = serde_json::from_str(raw).into_diagnostic()?;
        if receipt.schema != "screening-rejections-v1"
            || receipt.selection_id != selection.id
            || receipt.batches.is_empty()
        {
            return Err(fail("invalid screening rejection identity or schema"));
        }
        for (batch, rejected) in &receipt.batches {
            let index = (0..selection.manifest.batches.len())
                .find(|index| selection.batch_id(*index).is_ok_and(|id| &id == batch))
                .ok_or_else(|| fail("screening rejection names an unknown batch"))?;
            let request = selection.request_for(&selection.manifest.batches[index], index)?;
            require_revision(&rejected.contract_revision)?;
            require_revision(&rejected.rejection.response_revision)?;
            if rejected.input_revision != request["input_revision"]
                || rejected.request_revision != digest(&(&request, &rejected.contract_revision))?
                || rejected.rejection.missing > selection.manifest.batches[index].len()
                || (rejected.rejection.missing == 0
                    && rejected.rejection.duplicate == 0
                    && rejected.rejection.unknown == 0)
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
        Ok(receipt
            .batches
            .get(batch)
            .filter(|rejected| rejected.request_revision == revision)
            .map(|rejected| rejected.rejection.to_string()))
    }

    pub fn record_rejection(
        &self,
        session: &MutationSession,
        pages: &BTreeMap<PathBuf, String>,
        request: &Value,
        contract: &str,
        rejection: ScreeningRejection,
    ) -> miette::Result<()> {
        let root = session.root();
        let captured = self.capture_ledger(root)?;
        let batch = request["batch_id"]
            .as_str()
            .ok_or_else(|| fail("screening request has no batch"))?;
        if self.packet(root, batch)? != *request {
            return Err(fail("screening rejection request changed before recording"));
        }
        let index = (0..self.manifest.batches.len())
            .find(|index| self.batch_id(*index).is_ok_and(|id| id == batch))
            .expect("validated batch");
        let output_path = self.receipt_path(index)?;
        if captured.raw.contains_key(&output_path) {
            return Err(fail(
                "screening rejection cannot replace recorded decisions",
            ));
        }
        let path = self.rejection_path();
        let before = captured.raw.get(&path).cloned();
        let mut receipt = before
            .as_ref()
            .map(|raw| Rejections::decode(raw, self))
            .transpose()?
            .unwrap_or_else(|| Rejections {
                schema: "screening-rejections-v1".into(),
                selection_id: self.id.clone(),
                batches: BTreeMap::new(),
            });
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
        let after = encoded(&receipt)?;
        Rejections::decode(&after, self)?;
        let mut plan = MutationPlan::default();
        self.guard_pages(root, pages, &mut plan)?;
        plan.require_absent(output_path);
        for (guard, bytes) in captured.raw {
            if guard != path {
                plan.require_unchanged(guard, bytes);
            }
        }
        plan.publish_expected(root, path, before, after)
            .into_diagnostic()?;
        session.apply(plan).into_diagnostic()
    }
}
