use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ObservationEvidenceField {
    #[serde(rename = "finding-evidence")]
    Finding,
    #[serde(rename = "source-question-evidence")]
    SourceQuestion,
    #[serde(rename = "unresolved-evidence")]
    Unresolved,
    #[serde(rename = "resolution-evidence")]
    Resolution,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ObservationEvidenceKind {
    Empty,
    Reversed,
    OutsideAssignment,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ObservationEvidenceError {
    field: ObservationEvidenceField,
    index: usize,
    reference_index: Option<usize>,
    reference: Option<SemanticRange>,
    kind: ObservationEvidenceKind,
}

impl ObservationEvidenceError {
    pub(super) fn validate(&self, node: &Node, bound: usize) -> miette::Result<()> {
        if self.index >= bound || self.reference_index.is_some_and(|index| index >= bound) {
            return Err(fail("invalid observation evidence rejection index"));
        }
        match (&self.kind, &self.reference, self.reference_index) {
            (ObservationEvidenceKind::Empty, None, None) => Ok(()),
            (_, Some(reference), Some(_)) => {
                let error = validate_references(
                    std::slice::from_ref(reference),
                    &node.ranges,
                    self.field,
                    self.index,
                )
                .err();
                if error.as_ref().is_some_and(|error| error.kind == self.kind) {
                    Ok(())
                } else {
                    Err(fail(
                        "observation evidence rejection does not describe an invalid range",
                    ))
                }
            }
            _ => Err(fail("invalid observation evidence rejection shape")),
        }
    }
}

impl std::fmt::Display for ObservationEvidenceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let collection = match self.field {
            ObservationEvidenceField::Finding => "findings",
            ObservationEvidenceField::SourceQuestion => "source_questions",
            ObservationEvidenceField::Unresolved => "unresolved",
            ObservationEvidenceField::Resolution => "resolutions",
        };
        write!(formatter, "{collection}[{}].evidence", self.index)?;
        if let Some(index) = self.reference_index {
            write!(formatter, "[{index}]")?;
        }
        match self.kind {
            ObservationEvidenceKind::Empty => {
                formatter.write_str(" requires exact evidence references")
            }
            ObservationEvidenceKind::Reversed => {
                formatter.write_str(" has reversed evidence bounds")
            }
            ObservationEvidenceKind::OutsideAssignment => {
                formatter.write_str(" is outside the node's assigned ranges")
            }
        }
    }
}

impl std::error::Error for ObservationEvidenceError {}
impl miette::Diagnostic for ObservationEvidenceError {}

fn validate_references(
    references: &[SemanticRange],
    coverage: &[SemanticRange],
    field: ObservationEvidenceField,
    index: usize,
) -> Result<(), ObservationEvidenceError> {
    if references.is_empty() {
        return Err(ObservationEvidenceError {
            field,
            index,
            reference_index: None,
            reference: None,
            kind: ObservationEvidenceKind::Empty,
        });
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
    for (reference_index, reference) in references.iter().enumerate() {
        let kind = if reference.start > reference.end {
            Some(ObservationEvidenceKind::Reversed)
        } else if !covered.iter().any(|range| {
            range.resource == reference.resource
                && range.start <= reference.start
                && range.end >= reference.end
                && (reference.start < reference.end || range.start == range.end)
        }) {
            Some(ObservationEvidenceKind::OutsideAssignment)
        } else {
            None
        };
        if let Some(kind) = kind {
            return Err(ObservationEvidenceError {
                field,
                index,
                reference_index: Some(reference_index),
                reference: Some(reference.clone()),
                kind,
            });
        }
    }
    Ok(())
}

pub(super) fn validate_output_evidence(
    output: &WorkOutput,
    node: &Node,
) -> Result<(), ObservationEvidenceError> {
    for (index, finding) in output.findings.iter().enumerate() {
        validate_references(
            &finding.evidence,
            &node.ranges,
            ObservationEvidenceField::Finding,
            index,
        )?;
    }
    for (questions, field) in [
        (
            &output.source_questions,
            ObservationEvidenceField::SourceQuestion,
        ),
        (&output.unresolved, ObservationEvidenceField::Unresolved),
    ] {
        for (index, question) in questions.iter().enumerate() {
            validate_references(&question.evidence, &node.ranges, field, index)?;
        }
    }
    for (index, resolution) in output.resolutions.iter().enumerate() {
        validate_references(
            &resolution.evidence,
            &node.ranges,
            ObservationEvidenceField::Resolution,
            index,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(resource: usize, start: usize, end: usize) -> SemanticRange {
        SemanticRange {
            resource,
            start,
            end,
        }
    }

    #[test]
    fn assigned_intervals_keep_gaps_resources_and_empty_range_semantics() {
        let coverage = [
            range(7, 0, 5),
            range(7, 5, 10),
            range(7, 12, 20),
            range(8, 0, 0),
        ];
        for valid in [range(7, 2, 9), range(7, 12, 20), range(8, 0, 0)] {
            assert!(
                validate_references(&[valid], &coverage, ObservationEvidenceField::Finding, 0)
                    .is_ok()
            );
        }
        for invalid in [
            range(7, 8, 13),
            range(0, 0, 1),
            range(7, 5, 5),
            range(7, 4, 3),
            range(7, 19, 21),
        ] {
            let error = validate_references(
                std::slice::from_ref(&invalid),
                &coverage,
                ObservationEvidenceField::Finding,
                2,
            )
            .unwrap_err();
            assert_eq!(error.index, 2);
            assert_eq!(error.reference_index, Some(0));
            assert_eq!(error.reference, Some(invalid));
        }
        let empty = validate_references(&[], &coverage, ObservationEvidenceField::Resolution, 3)
            .unwrap_err();
        assert_eq!(empty.kind, ObservationEvidenceKind::Empty);
        assert_eq!(empty.reference, None);
        assert_eq!(empty.reference_index, None);
    }

    #[test]
    fn typed_locations_cover_all_authored_evidence_collections() {
        let node = Node {
            id: "node".into(),
            kind: NodeKind::Inspect {
                part_id: "part".into(),
            },
            ranges: vec![range(7, 0, 10)],
            refinement: None,
        };
        let finding = WorkFinding {
            statement: "Finding.".into(),
            scope: None,
            effective_time: None,
            attribution: None,
            evidence: vec![range(0, 0, 1)],
        };
        let question = WorkQuestion {
            id: "question".into(),
            question: "Question?".into(),
            evidence: finding.evidence.clone(),
        };
        for field in [
            ObservationEvidenceField::Finding,
            ObservationEvidenceField::SourceQuestion,
            ObservationEvidenceField::Unresolved,
            ObservationEvidenceField::Resolution,
        ] {
            let mut output = WorkOutput {
                work_id: "work".into(),
                node_id: "node".into(),
                input_revision: "input".into(),
                summary: "Read all assigned evidence.".into(),
                findings: Vec::new(),
                source_questions: Vec::new(),
                unresolved: Vec::new(),
                resolutions: Vec::new(),
            };
            match field {
                ObservationEvidenceField::Finding => output.findings.push(finding.clone()),
                ObservationEvidenceField::SourceQuestion => {
                    output.source_questions.push(question.clone())
                }
                ObservationEvidenceField::Unresolved => output.unresolved.push(question.clone()),
                ObservationEvidenceField::Resolution => output.resolutions.push(WorkResolution {
                    id: "question".into(),
                    reason: "A reason.".into(),
                    evidence: finding.evidence.clone(),
                }),
            }
            let error = validate_output_evidence(&output, &node).unwrap_err();
            assert_eq!(error.field, field);
            assert_eq!(error.index, 0);
            error.validate(&node, 1024).unwrap();
            let mut forged = error.clone();
            forged.reference = Some(range(7, 0, 1));
            assert!(forged.validate(&node, 1024).is_err());
        }
    }
}
