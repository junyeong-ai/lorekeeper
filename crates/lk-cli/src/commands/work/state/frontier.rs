use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrontierNode {
    node_id: String,
    output_revision: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FrontierProof {
    schema: String,
    work_id: String,
    root_node: String,
    nodes: Vec<FrontierNode>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FrontierSeal {
    pub schema: String,
    pub proof: FrontierProof,
    pub final_result_revision: String,
}

fn parts(node: &Node, nodes: &[Node], covered: &mut BTreeSet<String>) -> miette::Result<()> {
    match &node.kind {
        NodeKind::Inspect { part_id } => {
            if !covered.insert(part_id.clone()) {
                return Err(fail("frontier observations overlap"));
            }
        }
        NodeKind::Reduce { children } => {
            for child in children {
                let node = nodes
                    .iter()
                    .find(|node| &node.id == child)
                    .ok_or_else(|| fail("frontier has an unknown child"))?;
                parts(node, nodes, covered)?;
            }
        }
    }
    Ok(())
}

impl FrontierProof {
    pub(super) fn work_id(&self) -> &str {
        &self.work_id
    }
    pub(super) fn root_node(&self) -> &str {
        &self.root_node
    }

    fn verify_structure(&self, work: &Work) -> miette::Result<()> {
        let nodes = work.nodes()?;
        if self.schema != "work-frontier-v1"
            || self.work_id != work.plan.id
            || self.root_node != nodes.last().expect("nonempty work tree").id
            || self.nodes.is_empty()
        {
            return Err(fail("invalid frontier proof identity"));
        }
        let mut covered = BTreeSet::new();
        for reference in &self.nodes {
            require_id(&reference.node_id)?;
            require_id(&reference.output_revision)?;
            let node = nodes
                .iter()
                .find(|node| node.id == reference.node_id)
                .ok_or_else(|| fail("frontier names an unknown node"))?;
            parts(node, &nodes, &mut covered)?;
        }
        if covered != work.plan.parts.iter().map(|part| part.id.clone()).collect() {
            return Err(fail(
                "frontier does not cover every planned inspection exactly once",
            ));
        }
        Ok(())
    }

    fn verify<'a>(
        &self,
        work: &Work,
        outputs: &'a BTreeMap<String, StoredOutput>,
    ) -> miette::Result<Vec<&'a WorkOutput>> {
        let nodes = work.nodes()?;
        self.verify_structure(work)?;
        let mut result = Vec::new();
        let mut inherited = BTreeMap::new();
        for reference in &self.nodes {
            let node = nodes
                .iter()
                .find(|node| node.id == reference.node_id)
                .expect("verified frontier node");
            let output = outputs
                .get(&reference.node_id)
                .ok_or_else(|| fail("frontier observation is missing"))?;
            if output.output_revision != reference.output_revision
                || revision(&output.output)? != reference.output_revision
                || output.output.work_id != work.plan.id
                || output.output.node_id != node.id
                || !output.output.unresolved.is_empty()
            {
                return Err(fail(
                    "frontier observation identity, bound or processing concerns differ",
                ));
            }
            output.validate_bounds(work)?;
            for (kind, question) in output.output.questions() {
                if let Some(prior) = inherited.insert(question.id.as_str(), (kind, question))
                    && prior != (kind, question)
                {
                    return Err(fail("frontier question identities collide"));
                }
            }
            result.push(&output.output);
        }
        Ok(result)
    }
}

impl FrontierSeal {
    pub(super) fn decode(raw: &str, work: &Work) -> miette::Result<Self> {
        let seal: Self = serde_json::from_str(raw).map_err(fail)?;
        if seal.schema != "work-frontier-seal-v1"
            || seal.proof.work_id != work.plan.id
            || !work.supports_frontier()?
        {
            return Err(fail("invalid frontier seal identity or schema"));
        }
        seal.proof.verify_structure(work)?;
        require_id(&seal.final_result_revision)?;
        Ok(seal)
    }

    pub(super) fn verify_captured(
        &self,
        work: &Work,
        outputs: &BTreeMap<String, StoredOutput>,
    ) -> miette::Result<()> {
        let required = self
            .proof
            .nodes
            .iter()
            .map(|reference| reference.node_id.clone())
            .collect::<Vec<_>>();
        work.verify_stored_dependencies(outputs, &required)?;
        self.proof.verify(work, outputs)?;
        Ok(())
    }
}

impl Work {
    fn supports_frontier(&self) -> miette::Result<bool> {
        let subject: super::super::resources::Subject =
            serde_json::from_value(self.context["subject"].clone()).map_err(fail)?;
        if !matches!(subject, super::super::resources::Subject::Queue { .. }) {
            return Ok(false);
        }
        let kind: lk_queue::TaskKind =
            serde_json::from_value(self.context["purpose"]["task_kind"].clone()).map_err(fail)?;
        if !matches!(
            kind,
            lk_queue::TaskKind::ExtractConcepts | lk_queue::TaskKind::SynthesizeConcept
        ) || self.context["purpose"]["extraction_scope"]["policy"] == "source-proposals-v1"
        {
            return Ok(false);
        }
        Ok(true)
    }

    pub(in crate::commands::work) fn capture_frontier(
        &self,
        root: &Path,
        resources: &[SemanticResource],
    ) -> miette::Result<Option<CompletedWork>> {
        if !self.supports_frontier()? {
            return Ok(None);
        }
        let evaluated = self.evaluate(root, resources)?;
        if evaluated.progress().complete {
            return Ok(None);
        }
        fn collect(
            node: &Node,
            evaluated: &Evaluation<'_>,
            result: &mut Vec<FrontierNode>,
        ) -> bool {
            if let Some(stored) = evaluated.current.get(&node.id) {
                result.push(FrontierNode {
                    node_id: node.id.clone(),
                    output_revision: stored.output_revision.clone(),
                });
                return true;
            }
            match &node.kind {
                NodeKind::Inspect { .. } => false,
                NodeKind::Reduce { children } => children.iter().all(|id| {
                    collect(
                        evaluated
                            .nodes
                            .iter()
                            .find(|node| &node.id == id)
                            .expect("known tree node"),
                        evaluated,
                        result,
                    )
                }),
            }
        }
        let node = evaluated.nodes.last().expect("nonempty work tree");
        let mut nodes = Vec::new();
        if !collect(node, &evaluated, &mut nodes) {
            return Ok(None);
        }
        if nodes.iter().any(|node| {
            !evaluated.current[&node.node_id]
                .output
                .unresolved
                .is_empty()
        }) {
            return Ok(None);
        }
        let proof = FrontierProof {
            schema: "work-frontier-v1".into(),
            work_id: self.plan.id.clone(),
            root_node: node.id.clone(),
            nodes,
        };
        let outputs = proof.verify(self, &evaluated.current)?;
        let bytes = outputs.iter().try_fold(0usize, |size, output| {
            size.checked_add(encoded(*output)?.len())
                .ok_or_else(|| fail("frontier size overflow"))
        })?;
        if bytes > self.max_input_bytes {
            return Ok(None);
        }
        let observations = proof
            .nodes
            .iter()
            .zip(&outputs)
            .map(|(node, output)| {
                json!({
                    "node_id":node.node_id,"result_revision":node.output_revision,"output":output,
                })
            })
            .collect::<Vec<_>>();
        let mut result = json!({
            "kind":"finalize-frontier", "work_id":self.plan.id, "root_node":node.id,
            "max_result_bytes":self.plan.max_result_bytes, "context":self.context,
            "proof":proof, "result_revision":revision(&proof)?, "observations":observations,
            "evidence":self.evidence_guards(&outputs)?,
        });
        result["input_revision"] = revision(&result)?.into();
        if encoded(&result)?.len() > self.max_input_bytes {
            return Ok(None);
        }
        let absent = evaluated
            .nodes
            .iter()
            .map(|node| output_path(&self.plan.id, &node.id))
            .collect::<miette::Result<Vec<_>>>()?
            .into_iter()
            .filter(|path| !evaluated.raw.contains_key(path))
            .collect();
        Ok(Some(CompletedWork {
            work_id: self.plan.id.clone(),
            root_node: node.id.clone(),
            authority: CompletionAuthority::Frontier(proof),
            result,
            guards: evaluated.raw,
            absent,
        }))
    }

    pub(in crate::commands::work) fn capture_native_completion(
        &self,
        root: &Path,
        resources: &[SemanticResource],
        kind: CompletionKind,
    ) -> miette::Result<CompletedWork> {
        match kind {
            CompletionKind::Root => self.capture_completion(root, resources),
            CompletionKind::Frontier => self
                .capture_frontier(root, resources)?
                .ok_or_else(|| fail("current complete frontier is unavailable")),
        }
    }

    pub(super) fn guard_frontier_completion(
        &self,
        mutation: &mut MutationPlan,
        root: &Path,
        resources: &[SemanticResource],
        final_result: &Value,
        raw: String,
    ) -> miette::Result<()> {
        let seal = FrontierSeal::decode(&raw, self)?;
        if seal.final_result_revision != revision(final_result)? {
            return Err(miette::Report::new(CompletionRefusal::ChangedResult));
        }
        let completed = self
            .capture_frontier(root, resources)?
            .ok_or_else(|| miette::Report::new(CompletionRefusal::ChangedObservations))?;
        let CompletionAuthority::Frontier(proof) = completed.authority else {
            unreachable!("frontier capture")
        };
        if seal.proof != proof {
            return Err(miette::Report::new(CompletionRefusal::ChangedObservations));
        }
        for (path, raw) in completed.guards {
            mutation.require_unchanged(path, raw);
        }
        for path in completed.absent {
            mutation.require_absent(path);
        }
        mutation.require_absent(work_path(&self.plan.id, "seal.json")?);
        mutation.require_unchanged(work_path(&self.plan.id, "frontier-seal.json")?, raw);
        Ok(())
    }
}
