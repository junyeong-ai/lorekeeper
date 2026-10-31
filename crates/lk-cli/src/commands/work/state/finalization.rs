use super::*;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FinalizationActionHistory {
    schema: String,
    work_id: String,
    #[serde(deserialize_with = "crate::commands::work::receipt::unique_entries")]
    actions: BTreeMap<String, String>,
}

impl FinalizationActionHistory {
    pub(super) fn decode(raw: &str, work: &Work) -> miette::Result<Self> {
        let history: Self = crate::commands::work::receipt::decode(raw).map_err(fail)?;
        if history.schema != "work-finalization-action-history-v1"
            || history.work_id != work.plan.id
            || history.actions.is_empty()
        {
            return Err(fail(
                "invalid finalization action history identity or schema",
            ));
        }
        for (digest, raw) in &history.actions {
            if *digest != lk_core::rss_original::digest(raw.as_bytes()) {
                return Err(fail(
                    "finalization action history bytes differ from their digest",
                ));
            }
            FinalizationAction::decode(raw, work)?;
        }
        Ok(history)
    }
}

pub(super) struct CapturedActions {
    active: Option<String>,
    history: Option<String>,
    actions: Vec<FinalizationAction>,
}

impl CapturedActions {
    fn load(root: &Path, work: &Work) -> miette::Result<Self> {
        require_settled(root)?;
        let active = read_optional(root, &work_path(&work.plan.id, "finalization-action.json")?)?;
        let history = read_optional(
            root,
            &work_path(&work.plan.id, "finalization-action-history.json")?,
        )?;
        Self::decode(active, history, work)
    }

    pub(super) fn decode(
        active: Option<String>,
        history: Option<String>,
        work: &Work,
    ) -> miette::Result<Self> {
        if history.is_some() && active.is_none() {
            return Err(fail(
                "finalization action history requires its current receipt",
            ));
        }
        let mut actions = Vec::new();
        if let Some(raw) = &active {
            actions.push(FinalizationAction::decode(raw, work)?);
        }
        if let Some(raw) = &history {
            for raw in FinalizationActionHistory::decode(raw, work)?
                .actions
                .values()
            {
                actions.push(FinalizationAction::decode(raw, work)?);
            }
        }
        let mut requests = BTreeMap::new();
        for action in &actions {
            if let Some(prior) = requests.insert(&action.request_revision, action)
                && prior != action
            {
                return Err(fail(
                    "conflicting finalization actions for the same request",
                ));
            }
        }
        Ok(Self {
            active,
            history,
            actions,
        })
    }
}

impl Work {
    pub fn load_finalization_action(
        &self,
        root: &Path,
        request: &Value,
    ) -> miette::Result<Option<String>> {
        self.load_finalization_action_for(root, request, &request["work"]["result_revision"])
    }

    pub fn load_finalization_action_for(
        &self,
        root: &Path,
        request: &Value,
        completion_revision: &Value,
    ) -> miette::Result<Option<String>> {
        let captured = CapturedActions::load(root, self)?;
        let request_revision =
            lk_core::rss_original::digest(&serde_json::to_vec(request).map_err(fail)?);
        let Some(action) = captured
            .actions
            .iter()
            .find(|action| action.request_revision == request_revision)
        else {
            return Ok(None);
        };
        if completion_revision.as_str() != Some(action.completion_revision.as_str()) {
            return Err(fail(
                "finalization action completion differs from its request",
            ));
        }
        Ok(Some(action.reason.clone()))
    }

    pub fn plan_finalization_action(
        &self,
        root: &Path,
        resources: &[SemanticResource],
        expected: &Value,
        request: &Value,
        reason: String,
    ) -> miette::Result<MutationPlan> {
        self.plan_finalization_action_for(
            root,
            resources,
            expected,
            request,
            reason,
            CompletionKind::Root,
        )
    }

    pub(in crate::commands::work) fn plan_finalization_action_for(
        &self,
        root: &Path,
        resources: &[SemanticResource],
        expected: &Value,
        request: &Value,
        reason: String,
        kind: CompletionKind,
    ) -> miette::Result<MutationPlan> {
        let completed = self.capture_native_completion(root, resources, kind)?;
        if completed.result() != expected {
            return Err(fail(
                "completed observations changed before recording required finalization action",
            ));
        }
        let action = FinalizationAction {
            schema: match completed.kind() {
                CompletionKind::Root => "bounded-finalization-action-v1",
                CompletionKind::Frontier => "frontier-finalization-action-v1",
            }
            .into(),
            work_id: self.plan.id.clone(),
            root_node: completed.root_node.clone(),
            completion_revision: completed.completion_revision()?,
            request_revision: lk_core::rss_original::digest(
                &serde_json::to_vec(request).map_err(fail)?,
            ),
            reason,
        };
        let raw = encoded(&action)?;
        FinalizationAction::decode(&raw, self)?;
        let path = work_path(&self.plan.id, "finalization-action.json")?;
        let captured = CapturedActions::load(root, self)?;
        if captured
            .actions
            .iter()
            .any(|prior| prior.request_revision == action.request_revision && prior != &action)
        {
            return Err(fail(
                "conflicting finalization actions for the same request",
            ));
        }
        let history_path = work_path(&self.plan.id, "finalization-action-history.json")?;
        let mut plan = MutationPlan::default();
        completed.plan_read_guards(&mut plan);
        if let Some(before) = &captured.active
            && before != &raw
        {
            let mut history = match &captured.history {
                Some(raw) => FinalizationActionHistory::decode(raw, self)?,
                None => FinalizationActionHistory {
                    schema: "work-finalization-action-history-v1".into(),
                    work_id: self.plan.id.clone(),
                    actions: BTreeMap::new(),
                },
            };
            history.actions.insert(
                lk_core::rss_original::digest(before.as_bytes()),
                before.clone(),
            );
            let after = encoded(&history)?;
            if captured.history.as_deref() == Some(after.as_str()) {
                plan.require_unchanged(history_path, after);
            } else {
                plan.publish_expected(root, history_path, captured.history, after)
                    .map_err(fail)?;
            }
        } else {
            match captured.history {
                Some(raw) => plan.require_unchanged(history_path, raw),
                None => plan.require_absent(history_path),
            }
        }
        if captured.active.as_deref() == Some(raw.as_str()) {
            plan.require_unchanged(path, raw);
        } else {
            plan.publish_expected(root, path, captured.active, raw)
                .map_err(fail)?;
        }
        Ok(plan)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{context, finish, limits, resources};
    use super::*;

    #[test]
    fn replacement_retains_exact_actions_and_guards_both_files() {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let source = resources();
        let work =
            Work::create(&session, &source, "subject", context("subject"), limits()).unwrap();
        finish(&work, &session, &source);
        let completed = work.result(root.path(), &source).unwrap();
        let first = json!({"work":completed,"rules":"first"});
        let second = json!({"work":completed,"rules":"second"});
        session
            .apply(
                work.plan_finalization_action(
                    root.path(),
                    &source,
                    &completed,
                    &first,
                    "First exact hold.".into(),
                )
                .unwrap(),
            )
            .unwrap();
        let active_path = work_path(&work.plan.id, "finalization-action.json").unwrap();
        let history_path = work_path(&work.plan.id, "finalization-action-history.json").unwrap();
        let old = std::fs::read_to_string(root.path().join(&active_path)).unwrap();
        let mutation = work
            .plan_finalization_action(
                root.path(),
                &source,
                &completed,
                &second,
                "Second exact hold.".into(),
            )
            .unwrap();
        std::fs::write(root.path().join(&history_path), "{}").unwrap();
        assert!(session.apply(mutation).is_err());
        assert_eq!(
            std::fs::read_to_string(root.path().join(&active_path)).unwrap(),
            old
        );
        std::fs::remove_file(root.path().join(&history_path)).unwrap();
        session
            .apply(
                work.plan_finalization_action(
                    root.path(),
                    &source,
                    &completed,
                    &second,
                    "Second exact hold.".into(),
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(
            work.load_finalization_action(root.path(), &first)
                .unwrap()
                .as_deref(),
            Some("First exact hold.")
        );
        assert_eq!(
            work.load_finalization_action(root.path(), &second)
                .unwrap()
                .as_deref(),
            Some("Second exact hold.")
        );
        let history_raw = std::fs::read_to_string(root.path().join(&history_path)).unwrap();
        let history = FinalizationActionHistory::decode(&history_raw, &work).unwrap();
        assert_eq!(history.actions.values().collect::<Vec<_>>(), vec![&old]);
        let active = std::fs::read_to_string(root.path().join(&active_path)).unwrap();
        FinalizationAction::decode(&active, &work).unwrap();
        let unchanged = work
            .plan_finalization_action(
                root.path(),
                &source,
                &completed,
                &second,
                "Second exact hold.".into(),
            )
            .unwrap();
        std::fs::write(root.path().join(&active_path), format!("{active}\n")).unwrap();
        assert!(session.apply(unchanged).is_err());
        assert_eq!(
            std::fs::read_to_string(root.path().join(&history_path)).unwrap(),
            history_raw
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join(&active_path)).unwrap(),
            format!("{active}\n")
        );
        std::fs::write(root.path().join(&active_path), &active).unwrap();
        let mut corrupt: Value = serde_json::from_str(&history_raw).unwrap();
        let digest = lk_core::rss_original::digest(old.as_bytes());
        corrupt["actions"][&digest] = json!(format!("{old}\n"));
        let duplicate = history_raw.replacen(
            &format!("\"{digest}\":"),
            &format!("\"{digest}\":null,\"{digest}\":"),
            1,
        );
        for raw in ["{}".into(), encoded(&corrupt).unwrap(), duplicate] {
            std::fs::write(root.path().join(&history_path), &raw).unwrap();
            assert!(work.load_finalization_action(root.path(), &second).is_err());
            assert!(
                work.plan_finalization_action(
                    root.path(),
                    &source,
                    &completed,
                    &second,
                    "Second exact hold.".into()
                )
                .is_err()
            );
            assert_eq!(
                std::fs::read_to_string(root.path().join(&active_path)).unwrap(),
                active
            );
            assert_eq!(
                std::fs::read_to_string(root.path().join(&history_path)).unwrap(),
                raw
            );
        }
    }

    #[test]
    fn interrupted_replacement_recovers_history_and_current_receipt_together() {
        for completed_edits in [0, 1, 2] {
            let root = tempfile::tempdir().unwrap();
            let session = MutationSession::acquire(root.path()).unwrap();
            let source = resources();
            let work =
                Work::create(&session, &source, "subject", context("subject"), limits()).unwrap();
            finish(&work, &session, &source);
            let completed = work.result(root.path(), &source).unwrap();
            let first = json!({"work":completed,"rules":"first"});
            let second = json!({"work":completed,"rules":"second"});
            session
                .apply(
                    work.plan_finalization_action(
                        root.path(),
                        &source,
                        &completed,
                        &first,
                        "First hold.".into(),
                    )
                    .unwrap(),
                )
                .unwrap();
            let mutation = work
                .plan_finalization_action(
                    root.path(),
                    &source,
                    &completed,
                    &second,
                    "Second hold.".into(),
                )
                .unwrap();
            let plan_revision = lk_core::concept::file_revision(&encoded(&mutation).unwrap());
            let mutation = serde_json::to_value(mutation).unwrap();
            let steps = mutation["steps"].as_array().unwrap();
            let first_edit = steps
                .iter()
                .position(|step| {
                    step["kind"] == "create"
                        || (step["kind"] == "edit" && step["before"] != step["after"])
                })
                .unwrap();
            for step in steps.iter().skip(first_edit).take(completed_edits) {
                std::fs::write(
                    root.path().join(step["path"].as_str().unwrap()),
                    step["after"].as_str().unwrap(),
                )
                .unwrap();
            }
            let count = first_edit + completed_edits;
            let journal = root.path().join(".lorekeeper/mutation");
            std::fs::write(
                journal.join("pending.json"),
                encoded(&json!({"version":1,"plan":mutation})).unwrap(),
            )
            .unwrap();
            std::fs::write(journal.join("progress.json"), encoded(&json!({"plan_revision":plan_revision,"completed":count,"checksum":lk_core::concept::file_revision(&format!("{plan_revision}:{count}"))})).unwrap()).unwrap();
            drop(session);
            assert!(work.load_finalization_action(root.path(), &first).is_err());
            let _session = MutationSession::acquire(root.path()).unwrap();
            assert_eq!(
                work.load_finalization_action(root.path(), &first)
                    .unwrap()
                    .as_deref(),
                Some("First hold.")
            );
            assert_eq!(
                work.load_finalization_action(root.path(), &second)
                    .unwrap()
                    .as_deref(),
                Some("Second hold.")
            );
            assert!(!journal.join("pending.json").exists());
        }
    }
}
