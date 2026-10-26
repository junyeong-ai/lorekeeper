use std::collections::{BTreeMap, BTreeSet};

use lk_core::semantic::SemanticRange;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::commands::work::state::{
    QuestionKind, WorkOutput, WorkQuestion, WorkResolution, inherited_questions,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Question {
    question: String,
    evidence: Vec<SemanticRange>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Resolution {
    question_key: usize,
    reason: String,
    evidence: Vec<SemanticRange>,
}

pub(super) struct ActiveQuestions {
    inherited: Vec<(QuestionKind, WorkQuestion)>,
    used_ids: BTreeSet<String>,
}

impl ActiveQuestions {
    pub fn children(request: &Value) -> miette::Result<Vec<WorkOutput>> {
        Ok(match request["kind"].as_str() {
            Some("inspect") => Vec::new(),
            Some("reduce") => request["children"]
                .as_array()
                .filter(|children| !children.is_empty())
                .ok_or_else(|| miette::miette!("reduction requires child observations"))?
                .iter()
                .map(|child| {
                    serde_json::from_value::<WorkOutput>(child["output"].clone())
                        .map_err(|_| miette::miette!("invalid child observation"))
                })
                .collect::<miette::Result<Vec<_>>>()?,
            _ => return Err(miette::miette!("unknown evidence request kind")),
        })
    }

    pub fn from_request(request: &Value) -> miette::Result<Self> {
        let children = Self::children(request)?;
        let inherited = inherited_questions(&children.iter().collect::<Vec<_>>())?
            .into_values()
            .map(|(kind, question)| (kind, question.clone()))
            .collect();
        let used_ids = children
            .iter()
            .flat_map(|child| {
                child
                    .source_questions
                    .iter()
                    .chain(&child.unresolved)
                    .map(|question| question.id.clone())
                    .chain(
                        child
                            .resolutions
                            .iter()
                            .map(|resolution| resolution.id.clone()),
                    )
            })
            .collect();
        Ok(Self {
            inherited,
            used_ids,
        })
    }

    pub fn has_inherited(&self) -> bool {
        !self.inherited.is_empty()
    }

    pub fn retention_overhead(&self) -> miette::Result<usize> {
        if !self.has_inherited() {
            return Ok(0);
        }
        let source = self
            .inherited
            .iter()
            .filter(|(kind, _)| *kind == QuestionKind::Source)
            .count();
        crate::commands::work::state::question_retention_overhead(
            source,
            self.inherited.len() - source,
        )
    }

    pub fn request(&self, request: &Value) -> Value {
        let keys = self
            .inherited
            .iter()
            .enumerate()
            .map(|(key, (_, question))| (question.id.as_str(), key))
            .collect::<BTreeMap<_, _>>();
        let mut request = request.clone();
        if let Some(children) = request.get_mut("children").and_then(Value::as_array_mut) {
            for child in children {
                let child = child.as_object_mut().expect("validated child");
                child.remove("result_revision");
                let output = child["output"].as_object_mut().expect("validated output");
                for field in ["work_id", "node_id", "input_revision"] {
                    output.remove(field);
                }
                if let Some(history) = output.remove("resolutions") {
                    let history = history
                        .as_array()
                        .expect("validated resolutions")
                        .iter()
                        .map(|resolution| {
                            let key = keys
                                .get(resolution["id"].as_str().expect("validated resolution id"));
                            json!([
                                resolution["id"],
                                key,
                                resolution["reason"],
                                resolution["evidence"]
                            ])
                        })
                        .collect::<Vec<_>>();
                    output.insert("resolved_history".into(), history.into());
                }
                for field in ["source_questions", "unresolved"] {
                    if let Some(questions) = output.get_mut(field).and_then(Value::as_array_mut) {
                        for question in questions {
                            let key = keys[question["id"].as_str().expect("validated question id")];
                            *question = json!([key, question["question"], question["evidence"]]);
                        }
                    }
                }
            }
        }
        request
    }

    pub fn apply(
        self,
        output: &mut WorkOutput,
        source_questions: Vec<Question>,
        unresolved: Vec<Question>,
        resolutions: Vec<Resolution>,
    ) -> miette::Result<()> {
        let mut resolved = BTreeSet::new();
        for resolution in resolutions {
            let (_, question) = self.inherited.get(resolution.question_key).ok_or_else(|| {
                miette::miette!("resolution refers to an unknown active question")
            })?;
            if !resolved.insert(resolution.question_key) {
                return Err(miette::miette!("duplicate active question resolution"));
            }
            output.resolutions.push(WorkResolution {
                id: question.id.clone(),
                reason: resolution.reason,
                evidence: resolution.evidence,
            });
        }
        let mut used = self.used_ids;
        for (key, (kind, question)) in self.inherited.into_iter().enumerate() {
            if !resolved.contains(&key) {
                match kind {
                    QuestionKind::Source => output.source_questions.push(question),
                    QuestionKind::Processing => output.unresolved.push(question),
                }
            }
        }
        for (kind, questions, target) in [
            ("source", source_questions, &mut output.source_questions),
            ("processing", unresolved, &mut output.unresolved),
        ] {
            let mut ordinal = 0;
            for question in questions {
                let id = loop {
                    let id = format!("{}:{kind}:{ordinal}", output.node_id);
                    ordinal += 1;
                    if used.insert(id.clone()) {
                        break id;
                    }
                };
                target.push(WorkQuestion {
                    id,
                    question: question.question,
                    evidence: question.evidence,
                });
            }
        }
        Ok(())
    }
}
