use super::*;

const SCHEMA: &str = "work-output-retained-questions-v1";

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RetainedQuestions {
    source_questions: Vec<WorkQuestion>,
    unresolved: Vec<WorkQuestion>,
    positions: Positions,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Positions {
    source_questions: Vec<Span>,
    unresolved: Vec<Span>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Span {
    start: usize,
    end: usize,
}

fn push_position(spans: &mut Vec<Span>, position: usize) {
    if let Some(last) = spans.last_mut()
        && last.end == position
    {
        last.end += 1;
    } else {
        spans.push(Span {
            start: position,
            end: position + 1,
        });
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    output: WorkOutput,
    output_revision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reused_from: Option<Reuse>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    schema: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    retained_questions: Option<RetainedQuestions>,
}

fn present<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}

impl StoredOutput {
    pub(super) fn build(
        output: WorkOutput,
        children: &[&WorkOutput],
        max_result_bytes: usize,
        reused_from: Option<Reuse>,
    ) -> miette::Result<Self> {
        let mut stored = Self {
            output_revision: revision(&output)?,
            output,
            reused_from,
            retained: None,
        };
        let full_bytes = encoded(&stored.output)?.len();
        if full_bytes <= max_result_bytes {
            return Ok(stored);
        }
        let inherited = inherited_questions(children)?;
        let mut retained = RetainedQuestions {
            source_questions: Vec::new(),
            unresolved: Vec::new(),
            positions: Positions::default(),
        };
        for (kind, questions, captured, positions) in [
            (
                QuestionKind::Source,
                &stored.output.source_questions,
                &mut retained.source_questions,
                &mut retained.positions.source_questions,
            ),
            (
                QuestionKind::Processing,
                &stored.output.unresolved,
                &mut retained.unresolved,
                &mut retained.positions.unresolved,
            ),
        ] {
            for (position, question) in questions.iter().enumerate() {
                if inherited.get(question.id.as_str()) == Some(&(kind, question)) {
                    captured.push(question.clone());
                    push_position(positions, position);
                }
            }
        }
        if !retained.source_questions.is_empty() || !retained.unresolved.is_empty() {
            stored.retained = Some(retained);
            if stored.result_bytes()? >= full_bytes {
                stored.retained = None;
            }
        }
        Ok(stored)
    }

    fn build_record(&self) -> miette::Result<Record> {
        let mut output = self.output.clone();
        if let Some(retained) = &self.retained {
            for (questions, captured, positions) in [
                (
                    &mut output.source_questions,
                    &retained.source_questions,
                    &retained.positions.source_questions,
                ),
                (
                    &mut output.unresolved,
                    &retained.unresolved,
                    &retained.positions.unresolved,
                ),
            ] {
                for (position, expected) in positions
                    .iter()
                    .flat_map(|span| span.start..span.end)
                    .rev()
                    .zip(captured.iter().rev())
                {
                    if questions.get(position) != Some(expected) {
                        return Err(fail("retained question differs from its logical output"));
                    }
                    questions.remove(position);
                }
            }
        }
        Ok(Record {
            output,
            output_revision: self.output_revision.clone(),
            reused_from: self.reused_from.clone(),
            schema: self.retained.as_ref().map(|_| SCHEMA.into()),
            retained_questions: self.retained.clone(),
        })
    }

    fn result_bytes(&self) -> miette::Result<usize> {
        let record = self.build_record()?;
        match &record.retained_questions {
            Some(retained) => encoded(&json!({
                "output": record.output,
                "retained_positions": retained.positions,
            }))
            .map(|raw| raw.len()),
            None => encoded(&record.output).map(|raw| raw.len()),
        }
    }

    pub(super) fn validate_bounds(&self, work: &Work) -> miette::Result<()> {
        let actual_bytes = self.result_bytes()?;
        if actual_bytes > work.plan.max_result_bytes {
            return Err(OutputCapacity {
                actual_bytes,
                max_bytes: work.plan.max_result_bytes,
            }
            .into());
        }
        if let Some(retained) = &self.retained {
            let context = json!({
                "source_questions": retained.source_questions,
                "unresolved": retained.unresolved,
            });
            if encoded(&context)?.len() > work.max_input_bytes {
                return Err(fail("retained questions exceed their input bound"));
            }
        }
        Ok(())
    }

    pub(super) fn validate_inheritance(&self, children: &[&WorkOutput]) -> miette::Result<()> {
        let Some(retained) = &self.retained else {
            return Ok(());
        };
        let inherited = inherited_questions(children)?;
        for (kind, questions) in [
            (QuestionKind::Source, &retained.source_questions),
            (QuestionKind::Processing, &retained.unresolved),
        ] {
            for question in questions {
                if inherited.get(question.id.as_str()) != Some(&(kind, question)) {
                    return Err(fail("retained question is not an exact inherited question"));
                }
            }
        }
        Ok(())
    }
}

impl Serialize for StoredOutput {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.build_record()
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for StoredOutput {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let mut record = Record::deserialize(deserializer)?;
        match (&record.schema, &record.retained_questions) {
            (None, None) => {}
            (Some(schema), Some(retained))
                if schema == SCHEMA
                    && (!retained.source_questions.is_empty()
                        || !retained.unresolved.is_empty()) =>
            {
                for (questions, captured, positions) in [
                    (
                        &mut record.output.source_questions,
                        &retained.source_questions,
                        &retained.positions.source_questions,
                    ),
                    (
                        &mut record.output.unresolved,
                        &retained.unresolved,
                        &retained.positions.unresolved,
                    ),
                ] {
                    let count = positions.iter().try_fold(0usize, |count, span| {
                        span.end
                            .checked_sub(span.start)
                            .filter(|length| *length > 0)
                            .and_then(|length| count.checked_add(length))
                    });
                    if count != Some(captured.len())
                        || positions
                            .windows(2)
                            .any(|pair| pair[0].end >= pair[1].start)
                    {
                        return Err(D::Error::custom("invalid retained question positions"));
                    }
                    for (position, question) in positions
                        .iter()
                        .flat_map(|span| span.start..span.end)
                        .zip(captured)
                    {
                        if position > questions.len() {
                            return Err(D::Error::custom(
                                "retained question position is out of range",
                            ));
                        }
                        questions.insert(position, question.clone());
                    }
                }
            }
            _ => return Err(D::Error::custom("invalid retained question record schema")),
        }
        Ok(Self {
            output: record.output,
            output_revision: record.output_revision,
            reused_from: record.reused_from,
            retained: record.retained_questions,
        })
    }
}

#[cfg(unix)]
pub(crate) fn result_bytes(
    output: &WorkOutput,
    children: &[&WorkOutput],
    limit: usize,
) -> miette::Result<usize> {
    StoredOutput::build(output.clone(), children, limit, None)?.result_bytes()
}

#[cfg(unix)]
pub(crate) fn retention_overhead(
    source_questions: usize,
    unresolved: usize,
) -> miette::Result<usize> {
    let positions = Positions {
        source_questions: if source_questions == 0 {
            vec![]
        } else {
            vec![Span {
                start: 0,
                end: source_questions,
            }]
        },
        unresolved: if unresolved == 0 {
            vec![]
        } else {
            vec![Span {
                start: 0,
                end: unresolved,
            }]
        },
    };
    encoded(&json!({"output":{},"retained_positions":positions})).map(|raw| raw.len() - 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question(id: &str) -> WorkQuestion {
        WorkQuestion {
            id: id.into(),
            question: "What remains unverified? ".repeat(20),
            evidence: vec![SemanticRange {
                resource: 0,
                start: 0,
                end: 1,
            }],
        }
    }

    fn output() -> WorkOutput {
        WorkOutput {
            work_id: "w".into(),
            node_id: "n".into(),
            input_revision: "r".into(),
            summary: "Read every assigned child.".into(),
            findings: vec![],
            source_questions: vec![question("a"), question("b")],
            unresolved: vec![question("c")],
            resolutions: vec![],
        }
    }

    #[test]
    fn retained_records_preserve_order_and_refuse_ambiguous_or_noninherited_snapshots() {
        let child = output();
        let mut parent = child.clone();
        parent.node_id = "parent".into();
        parent.source_questions.insert(
            1,
            WorkQuestion {
                question: "A newly observed source uncertainty.".into(),
                ..question("new")
            },
        );
        let stored = StoredOutput::build(parent.clone(), &[&child], 1024, None).unwrap();
        assert!(stored.retained.is_some());
        assert!(stored.result_bytes().unwrap() <= 1024);
        stored.validate_inheritance(&[&child]).unwrap();
        assert!(stored.validate_inheritance(&[]).is_err());
        let raw = encoded(&stored).unwrap();
        let restored: StoredOutput = serde_json::from_str(&raw).unwrap();
        assert_eq!(restored.output, parent);
        assert_eq!(revision(&restored.output).unwrap(), stored.output_revision);
        let wire: Value = serde_json::from_str(&raw).unwrap();
        let mut changed = child.clone();
        changed.source_questions[0].question.push('!');
        assert!(stored.validate_inheritance(&[&changed]).is_err());
        for (field, value) in [
            ("schema", json!("unknown")),
            ("schema", Value::Null),
            ("retained_questions", Value::Null),
        ] {
            let mut corrupt = wire.clone();
            corrupt[field] = value;
            assert!(serde_json::from_value::<StoredOutput>(corrupt).is_err());
        }
        for positions in [
            json!([{"start":0,"end":0}]),
            json!([{"start":0,"end":1}]),
            json!([{"start":0,"end":999}]),
            json!([{"start":2,"end":3},{"start":0,"end":1}]),
            json!([{"start":0,"end":1},{"start":1,"end":2}]),
        ] {
            let mut corrupt = wire.clone();
            corrupt["retained_questions"]["positions"]["source_questions"] = positions;
            assert!(serde_json::from_value::<StoredOutput>(corrupt).is_err());
        }
        let duplicate = raw.replacen(
            "\"schema\":",
            "\"schema\":\"work-output-retained-questions-v1\",\"schema\":",
            1,
        );
        assert!(serde_json::from_str::<StoredOutput>(&duplicate).is_err());
    }

    #[test]
    fn exact_limit_legacy_output_is_not_expanded_by_retention_metadata() {
        for (limit, text) in [
            (1024, "?".into()),
            (2048, "What remains unverified? ".repeat(20)),
        ] {
            let mut child = output();
            child.work_id = "a".repeat(64);
            child.node_id = "b".repeat(64);
            child.input_revision = "c".repeat(64);
            child.source_questions.clear();
            child.unresolved = vec![WorkQuestion {
                id: format!("{}:source-question-0", "d".repeat(64)),
                question: text,
                evidence: vec![SemanticRange {
                    resource: 0,
                    start: 0,
                    end: 1,
                }],
            }];
            let mut parent = child.clone();
            parent.node_id = "e".repeat(64);
            parent.summary.clear();
            parent.summary = "s".repeat(limit - encoded(&parent).unwrap().len());
            assert_eq!(encoded(&parent).unwrap().len(), limit);
            let stored = StoredOutput::build(parent.clone(), &[&child], limit, None).unwrap();
            assert!(stored.result_bytes().unwrap() <= limit);
            assert_eq!(stored.output, parent);
            assert!(stored.retained.is_none());
            assert_eq!(
                serde_json::to_value(&stored).unwrap(),
                json!({"output":parent,"output_revision":revision(&parent).unwrap()})
            );
        }
    }

    #[test]
    fn authored_values_keep_the_result_bound_and_legacy_records_keep_their_wire() {
        let child = output();
        let stored = StoredOutput::build(child.clone(), &[], 1024, None).unwrap();
        assert!(stored.retained.is_none());
        assert!(stored.result_bytes().unwrap() > 1024);
        let legacy = StoredOutput::build(child.clone(), &[], 8192, None).unwrap();
        let expected = json!({"output":child,"output_revision":revision(&child).unwrap()});
        assert_eq!(serde_json::to_value(&legacy).unwrap(), expected);
        let mut parent = child.clone();
        parent.summary = "New judgment ".repeat(200);
        assert!(
            StoredOutput::build(parent, &[&child], 1024, None)
                .unwrap()
                .result_bytes()
                .unwrap()
                > 1024
        );
    }
}
