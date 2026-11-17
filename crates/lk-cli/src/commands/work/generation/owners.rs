use lk_queue::{ConceptAdmission, ConceptOwner};
use miette::IntoDiagnostic;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Default)]
pub(in crate::commands::work) struct Owners(pub(super) Vec<ConceptOwner>);

#[derive(Deserialize)]
#[serde(tag = "decision", rename_all = "kebab-case", deny_unknown_fields)]
pub(super) enum Admission {
    New,
    Reuse { owner_ref: String },
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(tag = "reason", rename_all = "kebab-case", deny_unknown_fields)]
pub(in crate::commands) enum ProposalRejection {
    UnknownOwnerReference,
}

impl std::fmt::Display for ProposalRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownOwnerReference => {
                f.write_str("native reuse reference is outside the admitted owner roster")
            }
        }
    }
}

impl Owners {
    pub(in crate::commands::work) fn build(
        config: &lk_core::config::Config,
        prepared: &super::Prepared,
    ) -> miette::Result<Self> {
        let kind: lk_queue::TaskKind =
            serde_json::from_value(prepared.context["purpose"]["task_kind"].clone())
                .into_diagnostic()?;
        if kind != lk_queue::TaskKind::ExtractConcepts {
            return Ok(Self::default());
        }
        let directory = lk_core::vault_path::concepts_dir(&config.vault.dirs);
        let mut owners = Vec::new();
        for resource in &prepared.resources {
            let path = std::path::Path::new(&resource.address);
            if !prepared.is_reuse_owner(&resource.address, &resource.revision)
                || !path.starts_with(&directory)
                || path.extension().is_none_or(|extension| extension != "md")
            {
                continue;
            }
            let page = lk_vault::parse_page(&resource.text)
                .map_err(|error| miette::miette!("{}: {error}", resource.address))?;
            if page.frontmatter.is_declared_concept() {
                owners.push(ConceptOwner {
                    path: resource.address.clone(),
                    revision: resource.revision.clone(),
                });
            }
        }
        Ok(Self(owners))
    }

    fn reference(index: usize) -> String {
        format!("o{index}")
    }

    pub(in crate::commands::work) fn presentation(&self) -> Value {
        json!(self.0.iter().enumerate().map(|(index, owner)| json!({
            "owner_ref":Self::reference(index), "path":owner.path, "revision":owner.revision,
        })).collect::<Vec<_>>())
    }

    pub(super) fn resolve(
        &self,
        admission: Admission,
    ) -> Result<ConceptAdmission, ProposalRejection> {
        match admission {
            Admission::New => Ok(ConceptAdmission::New),
            Admission::Reuse { owner_ref } => self
                .0
                .iter()
                .enumerate()
                .find(|(index, _)| Self::reference(*index) == owner_ref)
                .map(|(_, owner)| ConceptAdmission::Reuse {
                    owner: owner.clone(),
                })
                .ok_or(ProposalRejection::UnknownOwnerReference),
        }
    }

    pub(super) fn response_limit(
        &self,
        budget: usize,
        empty_reply_bytes: usize,
    ) -> miette::Result<usize> {
        let mut expansion = 0;
        let mut minimum_entry = usize::MAX;
        for (index, owner) in self.0.iter().enumerate() {
            let native = json!({"decision":"reuse", "owner_ref":Self::reference(index)});
            let canonical = serde_json::to_vec(&ConceptAdmission::Reuse {
                owner: owner.clone(),
            })
            .into_diagnostic()?
            .len();
            let native_bytes = serde_json::to_vec(&native).into_diagnostic()?.len();
            expansion = expansion.max(canonical.saturating_sub(native_bytes));
            let entry = json!({"admission":native,"name":"","aliases":[],"rss_evidence":[]});
            minimum_entry = minimum_entry.min(serde_json::to_vec(&entry).into_diagnostic()?.len());
        }
        if expansion == 0 || budget < empty_reply_bytes {
            return Ok(budget);
        }
        let fits = |bytes: usize| {
            let entries =
                bytes.saturating_sub(empty_reply_bytes).saturating_add(1) / (minimum_entry + 1);
            entries
                .checked_mul(expansion)
                .and_then(|extra| bytes.checked_add(extra))
                .is_some_and(|assembled| assembled <= budget)
        };
        let (mut low, mut high) = (empty_reply_bytes, budget);
        while low < high {
            let middle = low + (high - low).div_ceil(2);
            if fits(middle) {
                low = middle;
            } else {
                high = middle - 1;
            }
        }
        Ok(low)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_are_exact_and_expand_only_through_their_local_roster() {
        let owner = ConceptOwner {
            path: "wiki/concepts/nested/protocol.md".into(),
            revision: "a".repeat(64),
        };
        let owners = Owners(vec![owner.clone()]);
        let admission = |reference: &str| Admission::Reuse {
            owner_ref: reference.into(),
        };
        assert_eq!(
            owners.resolve(admission("o0")).unwrap(),
            ConceptAdmission::Reuse { owner }
        );
        for reference in ["o00", "O0", "o1", "../protocol.md", "o0 "] {
            assert!(owners.resolve(admission(reference)).is_err());
        }
        assert!(Owners::default().resolve(admission("o0")).is_err());
        assert!(serde_json::from_value::<Admission>(json!({"decision":"reuse","owner":{"path":"wiki/concepts/nested/protocol.md","revision":"a".repeat(64)}})).is_err());
    }

    #[test]
    fn capacity_reserves_every_repeated_reference_and_keeps_empty_results() {
        for path in [
            "wiki/concepts/protocol.md".to_owned(),
            format!("wiki/concepts/{}.md", "한글\\\"".repeat(256)),
        ] {
            let owners = Owners(vec![ConceptOwner {
                path,
                revision: "a".repeat(64),
            }]);
            let empty = json!({"status":"complete","value":{"concepts":[]},"reason":null});
            let empty_bytes = serde_json::to_vec(&empty).unwrap().len();
            for budget in [empty_bytes, 1024, 8192] {
                let limit = owners.response_limit(budget, empty_bytes).unwrap();
                assert!(limit >= empty_bytes);
                let mut reply = empty.clone();
                let mut growth = 0;
                loop {
                    let concept = json!({"admission":{"decision":"reuse","owner_ref":"o0"},"name":"","aliases":[],"rss_evidence":[]});
                    reply["value"]["concepts"]
                        .as_array_mut()
                        .unwrap()
                        .push(concept.clone());
                    let bytes = serde_json::to_vec(&reply).unwrap().len();
                    if bytes > limit {
                        break;
                    }
                    let mut canonical = concept;
                    canonical["admission"] = serde_json::to_value(
                        owners
                            .resolve(Admission::Reuse {
                                owner_ref: "o0".into(),
                            })
                            .unwrap(),
                    )
                    .unwrap();
                    growth += serde_json::to_vec(&canonical).unwrap().len()
                        - serde_json::to_vec(
                            reply["value"]["concepts"]
                                .as_array()
                                .unwrap()
                                .last()
                                .unwrap(),
                        )
                        .unwrap()
                        .len();
                    assert!(bytes + growth <= budget, "{bytes}+{growth}>{budget}");
                }
            }
        }
    }
}
