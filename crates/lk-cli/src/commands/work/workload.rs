use std::collections::BTreeMap;

use lk_core::semantic::{SemanticAuthority, SemanticRange, SemanticResourceGuard};
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct InspectionWorkload {
    pub characters: usize,
    pub ranges: usize,
    pub resources: usize,
    pub largest_range_characters: usize,
    pub largest_resource_characters: usize,
    pub authorities: BTreeMap<SemanticAuthority, AuthorityWorkload>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct AuthorityWorkload {
    characters: usize,
    resources: usize,
}

impl InspectionWorkload {
    pub(super) fn build(
        ranges: &[SemanticRange],
        resources: &[SemanticResourceGuard],
    ) -> miette::Result<Self> {
        let assigned = ranges
            .iter()
            .map(|range| {
                resources
                    .get(range.resource)
                    .filter(|guard| range.end <= guard.characters)
                    .map(|guard| (range, guard.authority))
                    .ok_or_else(|| miette::miette!("workload range exceeds its resource"))
            })
            .collect::<miette::Result<Vec<_>>>()?;
        Self::from_assignments(assigned)
    }

    fn from_assignments<'a>(
        assigned: impl IntoIterator<Item = (&'a SemanticRange, SemanticAuthority)>,
    ) -> miette::Result<Self> {
        let mut resources = BTreeMap::<usize, (SemanticAuthority, usize)>::new();
        let mut ranges = 0usize;
        let mut largest_range_characters = 0;
        for (range, authority) in assigned {
            let characters = range
                .end
                .checked_sub(range.start)
                .ok_or_else(|| miette::miette!("invalid workload range"))?;
            let resource = resources.entry(range.resource).or_insert((authority, 0));
            if resource.0 != authority {
                return Err(miette::miette!("workload resource authorities differ"));
            }
            resource.1 = add(resource.1, characters)?;
            ranges = add(ranges, 1)?;
            largest_range_characters = largest_range_characters.max(characters);
        }
        let mut result = Self {
            characters: 0,
            ranges,
            resources: resources.len(),
            largest_range_characters,
            largest_resource_characters: 0,
            authorities: BTreeMap::new(),
        };
        for (authority, characters) in resources.values() {
            result.characters = add(result.characters, *characters)?;
            result.largest_resource_characters =
                result.largest_resource_characters.max(*characters);
            let group = result.authorities.entry(*authority).or_default();
            group.characters = add(group.characters, *characters)?;
            group.resources = add(group.resources, 1)?;
        }
        Ok(result)
    }
}

fn add(left: usize, right: usize) -> miette::Result<usize> {
    left.checked_add(right)
        .ok_or_else(|| miette::miette!("workload count overflow"))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub(crate) enum ObservationWorkload {
    Inspect {
        #[serde(flatten)]
        assigned: InspectionWorkload,
    },
    Reduce {
        child_observations: usize,
    },
}

impl ObservationWorkload {
    #[cfg(any(unix, test))]
    pub(in crate::commands) fn from_request(request: &serde_json::Value) -> miette::Result<Self> {
        use miette::IntoDiagnostic;
        use serde::Deserialize;

        match request["kind"].as_str() {
            Some("inspect") => {
                #[derive(Deserialize)]
                struct Assignment {
                    range: SemanticRange,
                    authority: SemanticAuthority,
                }
                let ranges = Vec::<Assignment>::deserialize(&request["input"]["ranges"])
                    .into_diagnostic()?;
                Ok(Self::Inspect {
                    assigned: InspectionWorkload::from_assignments(
                        ranges.iter().map(|range| (&range.range, range.authority)),
                    )?,
                })
            }
            Some("reduce") => Ok(Self::Reduce {
                child_observations: request["children"]
                    .as_array()
                    .ok_or_else(|| miette::miette!("reduction workload requires children"))?
                    .len(),
            }),
            _ => Err(miette::miette!("unknown observation workload kind")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn inspection_counts_assigned_ranges_and_authorities_without_source_text() {
        let request = json!({"kind":"inspect","input":{"ranges":[
            {"range":{"resource":133,"start":12475,"end":21857},"authority":"context"},
            {"range":{"resource":134,"start":0,"end":9012},"authority":"context"},
            {"range":{"resource":135,"start":0,"end":228103},"authority":"context"}
        ]}});
        let before = request.clone();
        let workload = ObservationWorkload::from_request(&request).unwrap();
        assert_eq!(request, before);
        assert_eq!(
            serde_json::to_value(workload).unwrap(),
            json!({"kind":"inspect","characters":246497,"ranges":3,"resources":3,
                "largest_range_characters":228103,"largest_resource_characters":228103,
                "authorities":{"context":{"characters":246497,"resources":3}}})
        );
    }

    #[test]
    fn split_ranges_share_a_resource_and_empty_documents_remain_visible() {
        let resources = [
            SemanticResourceGuard {
                address: "subject.md".into(),
                revision: "revision".into(),
                authority: SemanticAuthority::Subject,
                text_digest: "digest".into(),
                characters: 3,
                bytes: "한🙂a".len(),
            },
            SemanticResourceGuard {
                address: "empty.md".into(),
                revision: "revision".into(),
                authority: SemanticAuthority::Context,
                text_digest: "digest".into(),
                characters: 0,
                bytes: 0,
            },
        ];
        let ranges = [
            SemanticRange {
                resource: 0,
                start: 0,
                end: 1,
            },
            SemanticRange {
                resource: 0,
                start: 1,
                end: 3,
            },
            SemanticRange {
                resource: 1,
                start: 0,
                end: 0,
            },
        ];
        let workload = InspectionWorkload::build(&ranges, &resources).unwrap();
        assert_eq!(workload.characters, 3);
        assert_eq!(workload.ranges, 3);
        assert_eq!(workload.resources, 2);
        assert_eq!(workload.largest_range_characters, 2);
        assert_eq!(workload.largest_resource_characters, 3);
        assert_eq!(
            workload.authorities[&SemanticAuthority::Subject].resources,
            1
        );
        assert_eq!(
            workload.authorities[&SemanticAuthority::Context].resources,
            1
        );
        assert!(
            InspectionWorkload::build(
                &[SemanticRange {
                    resource: 0,
                    start: 0,
                    end: 4
                }],
                &resources
            )
            .is_err()
        );
    }

    #[test]
    fn reduction_counts_observations_without_claiming_source_reading_or_fit() {
        let workload = ObservationWorkload::from_request(&json!({
            "kind":"reduce","children":[{"output":{"summary":"A"}},{"output":{"summary":"B"}}]
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_value(workload).unwrap(),
            json!({"kind":"reduce","child_observations":2})
        );
        for request in [
            json!({"kind":"reduce"}),
            json!({"kind":"inspect"}),
            json!({"kind":"unknown"}),
        ] {
            assert!(ObservationWorkload::from_request(&request).is_err());
        }
    }

    #[test]
    fn invalid_or_overflowing_metadata_is_not_reported_as_an_empty_workload() {
        let range = |resource, start, end| SemanticRange {
            resource,
            start,
            end,
        };
        let inverted = range(0, 2, 1);
        assert!(
            InspectionWorkload::from_assignments([(&inverted, SemanticAuthority::Context)])
                .is_err()
        );
        let first = range(0, 0, usize::MAX);
        let second = range(1, 0, 1);
        assert!(
            InspectionWorkload::from_assignments([
                (&first, SemanticAuthority::Context),
                (&second, SemanticAuthority::Context)
            ])
            .is_err()
        );
        assert!(
            InspectionWorkload::from_assignments([
                (&second, SemanticAuthority::Context),
                (&second, SemanticAuthority::Subject)
            ])
            .is_err()
        );
    }
}
