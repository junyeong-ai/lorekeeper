use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use miette::IntoDiagnostic;

/// Acquire without waiting. `pin` is the configuration the caller derived its writes from,
/// or `Absent` when it loaded none.
pub(super) fn acquire_mutation_now(
    root: &Path,
    pin: &super::ConfigurationPin,
) -> miette::Result<lk_vault::MutationSession> {
    pin.validate()?;
    super::workflow::validate_root(root)?;
    let claim = lk_vault::MutationClaim::acquire(root).into_diagnostic()?;
    let pin = pin.clone();
    finish_claim(claim, move || pin.validate())
}

/// The selection checked around acquisition and recovery also guards every publication
/// through the session, so a plan derived after the selection changed never publishes.
fn finish_claim(
    claim: lk_vault::MutationClaim,
    validate: impl Fn() -> miette::Result<()> + Send + Sync + 'static,
) -> miette::Result<lk_vault::MutationSession> {
    validate()?;
    super::workflow::validate_root(claim.root())?;
    let session = claim.into_session().into_diagnostic()?;
    validate()?;
    super::workflow::validate_root(session.root())?;
    let root = session.root().to_path_buf();
    Ok(session.with_precondition(move || {
        validate()
            .and_then(|()| super::workflow::validate_root(&root))
            .map_err(|error| error.to_string())
    }))
}

pub(super) async fn acquire_mutation(
    root: &Path,
    wait: bool,
    pin: &super::ConfigurationPin,
) -> miette::Result<lk_vault::MutationSession> {
    let pin = pin.clone();
    acquire_mutation_checked(root, wait, move || pin.validate()).await
}

pub(super) async fn acquire_mutation_checked(
    root: &Path,
    wait: bool,
    validate: impl Fn() -> miette::Result<()> + Send + Sync + 'static,
) -> miette::Result<lk_vault::MutationSession> {
    let mut waiting = false;
    loop {
        validate()?;
        super::workflow::validate_root(root)?;
        match lk_vault::MutationClaim::acquire(root) {
            Ok(claim) => return finish_claim(claim, validate),
            Err(lk_vault::VaultError::MutationBusy) if wait => {
                if !waiting {
                    eprintln!("Waiting for exclusive vault access...");
                    waiting = true;
                }
            }
            Err(error) => return Err(error).into_diagnostic(),
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

pub(super) struct Publication<'a> {
    session: &'a lk_vault::MutationSession,
    client: Arc<dyn lk_queue::LlmClient>,
    requests: BTreeMap<String, Vec<lk_queue::QueueTask>>,
}

impl<'a> Publication<'a> {
    pub fn new(
        session: &'a lk_vault::MutationSession,
        client: Arc<dyn lk_queue::LlmClient>,
    ) -> Self {
        Self {
            session,
            client,
            requests: BTreeMap::new(),
        }
    }

    pub async fn publish<'b>(
        &mut self,
        outputs: impl IntoIterator<Item = &'b lk_pipeline::RenderResult>,
    ) -> miette::Result<()> {
        self.publish_with_plan(outputs, lk_vault::MutationPlan::default())
            .await
    }

    pub async fn publish_with_plan<'b>(
        &mut self,
        outputs: impl IntoIterator<Item = &'b lk_pipeline::RenderResult>,
        mut plan: lk_vault::MutationPlan,
    ) -> miette::Result<()> {
        self.collect_requests().await?;
        let session = self.session;
        let outputs = outputs.into_iter().collect::<Vec<_>>();
        let targets = outputs
            .iter()
            .map(|output| output.path.to_string())
            .collect::<BTreeSet<_>>();
        let tasks = targets
            .iter()
            .filter_map(|path| self.requests.get(path))
            .flatten()
            .cloned()
            .collect::<Vec<_>>();
        let queue = session.root().join(".lorekeeper/queue");
        let _claim = if tasks.is_empty() {
            None
        } else {
            super::queue::claim_writer(&queue, false)?
        };
        let tasks = if tasks.is_empty() {
            tasks
        } else {
            lk_queue::new_requests(&queue, &tasks).into_diagnostic()?
        };
        for output in outputs {
            if let Some(capture) = &output.rss_original {
                lk_vault::publish_rss_original(session, capture).into_diagnostic()?;
            }
            let page = lk_vault::parse_page(&output.content).map_err(|e| miette::miette!("{e}"))?;
            if page
                .frontmatter
                .get("type")
                .and_then(serde_json::Value::as_str)
                == Some("document")
            {
                lk_vault::guard_rss_evidence(
                    &mut plan,
                    session.root(),
                    output.path.as_ref(),
                    &output.content,
                )
                .into_diagnostic()?;
                lk_vault::plan_document_publication(
                    session,
                    output.path.as_ref(),
                    &output.content,
                    output.expected_original.as_ref(),
                    &mut plan,
                )
                .into_diagnostic()?;
            } else {
                output
                    .plan_publication(&mut plan, session.root())
                    .into_diagnostic()?;
                if output.expected_original.as_ref().and_then(Option::as_ref)
                    == Some(&output.content)
                {
                    plan.require_unchanged(output.path.as_ref(), output.content.clone());
                }
            }
        }
        if let Some(first) = tasks.first() {
            plan.create(
                Path::new(".lorekeeper/queue").join(format!("{}.jsonl", first.task_id)),
                lk_queue::serialize_tasks(&tasks).into_diagnostic()?,
            );
        }
        session.apply(plan).map_err(crate::commands::typed_error)?;
        for path in targets {
            self.requests.remove(&path);
        }
        Ok(())
    }

    async fn collect_requests(&mut self) -> miette::Result<()> {
        for task in self.client.take_requests().await {
            task.validate_retained_input()
                .map_err(|error| miette::miette!("{error}"))?;
            self.requests
                .entry(task.target.vault_path.clone())
                .or_default()
                .push(task);
        }
        Ok(())
    }

    pub async fn finish(mut self) -> miette::Result<()> {
        self.collect_requests().await?;
        if self.requests.is_empty() {
            Ok(())
        } else {
            Err(miette::miette!(
                "semantic requests remain for unpublished targets: {}",
                self.requests.keys().cloned().collect::<Vec<_>>().join(", ")
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lk_queue::LlmClient;

    #[tokio::test]
    async fn finishing_refuses_requests_buffered_after_the_last_publication() {
        let directory = tempfile::tempdir().unwrap();
        let session = lk_vault::MutationSession::acquire(directory.path()).unwrap();
        let client = Arc::new(lk_queue::QueueLlmClient::new(
            directory.path().join(".lorekeeper/queue"),
        ));
        let mut publication = Publication::new(&session, client.clone());
        publication.publish(&[]).await.unwrap();
        client
            .summarize(lk_queue::SummarizeRequest {
                text: "Retained source observation".into(),
                max_sentences: 3,
                locale: lk_core::i18n::Locale::En,
                source_type: None,
                focus: None,
                target: lk_queue::TaskTarget {
                    vault_path: "daily/source/2026-10-20.md".into(),
                    kind: lk_queue::TargetKind::DailySummary,
                    anchor: "## Summary".into(),
                },
            })
            .await
            .unwrap();
        let error = publication.finish().await.unwrap_err();
        assert!(error.to_string().contains("unpublished targets"));
        assert!(!directory.path().join("daily").exists());
        assert!(!directory.path().join(".lorekeeper/queue").exists());
    }
    #[tokio::test]
    async fn a_session_publishes_only_under_the_configuration_it_was_acquired_with() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config.yaml");
        let original = "llm:\n  provider: queue\n";
        std::fs::write(&config, original).unwrap();
        let config = config.canonicalize().unwrap();
        let vault = directory.path().join("vault");
        std::fs::create_dir_all(&vault).unwrap();
        let pin = super::super::ConfigurationPin::from_loaded(
            config.clone(),
            lk_core::concept::file_revision(original),
        );
        let session = acquire_mutation(&vault, false, &pin).await.unwrap();
        let plan = || {
            let mut plan = lk_vault::MutationPlan::default();
            plan.create("page.md", "body".into());
            plan
        };

        std::fs::write(&config, "llm:\n  provider: noop\n").unwrap();
        let error = session.apply(plan()).unwrap_err();
        assert!(
            error.to_string().contains("selected configuration changed"),
            "{error}"
        );
        assert!(!vault.join("page.md").exists());

        std::fs::write(&config, original).unwrap();
        session.apply(plan()).unwrap();
        assert!(vault.join("page.md").exists());
    }
}
