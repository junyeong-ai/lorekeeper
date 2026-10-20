use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use miette::IntoDiagnostic;

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
        session.apply(plan).into_diagnostic()?;
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
}
