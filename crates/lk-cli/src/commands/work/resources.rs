use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use lk_core::concept::{Citation, file_revision, source_revision};
use lk_core::config::Config;
use lk_core::semantic::{SemanticAuthority, SemanticResource};
use miette::IntoDiagnostic;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub(super) enum Subject {
    Queue {
        task_id: String,
        cache_hash: String,
        target: String,
        target_revision: String,
    },
    Audit {
        path: String,
        file_revision: String,
        input_revision: String,
    },
}

pub(super) struct Prepared {
    pub subject: Subject,
    pub context: Value,
    pub resources: Vec<SemanticResource>,
    pub pages: BTreeMap<PathBuf, String>,
}

impl Prepared {
    pub fn revision(&self, config_revision: &str) -> miette::Result<String> {
        let bytes = serde_json::to_vec(&(config_revision, &self.subject)).into_diagnostic()?;
        Ok(lk_core::rss_original::digest(&bytes))
    }

    pub fn guard_pages(&self, plan: &mut lk_vault::MutationPlan) {
        for (path, content) in &self.pages {
            plan.require_unchanged(path, content.clone());
        }
    }
}

struct Capture<'a> {
    root: &'a Path,
    pages: BTreeMap<PathBuf, String>,
    resources: BTreeMap<String, SemanticResource>,
}

impl<'a> Capture<'a> {
    fn new(root: &'a Path) -> Self {
        Self {
            root,
            pages: BTreeMap::new(),
            resources: BTreeMap::new(),
        }
    }

    fn page(
        &mut self,
        path: &str,
        expected_file: Option<&str>,
        expected_authored: Option<&str>,
        authority: SemanticAuthority,
    ) -> miette::Result<String> {
        let absolute = lk_vault::page_path(self.root, Path::new(path)).into_diagnostic()?;
        let canonical = absolute.canonicalize().into_diagnostic()?;
        let relative = canonical
            .strip_prefix(self.root)
            .into_diagnostic()?
            .to_path_buf();
        let raw = std::fs::read_to_string(&absolute).into_diagnostic()?;
        let exact = file_revision(&raw);
        if expected_file.is_some_and(|expected| expected != exact) {
            return Err(miette::miette!("semantic source changed: {path}"));
        }
        if let Some(expected) = expected_authored {
            let parsed = lk_vault::parse_page(&raw).map_err(|e| miette::miette!("{path}: {e}"))?;
            if source_revision(&parsed) != expected {
                return Err(miette::miette!("semantic authored source changed: {path}"));
            }
        }
        if self.pages.get(&relative).is_some_and(|prior| prior != &raw) {
            return Err(miette::miette!(
                "semantic source changed during capture: {path}"
            ));
        }
        let address = relative.to_string_lossy().into_owned();
        self.pages.insert(relative, raw.clone());
        self.resources
            .entry(address.clone())
            .or_insert_with(|| SemanticResource {
                address,
                revision: exact,
                authority,
                text: raw.clone(),
            });
        Ok(raw)
    }

    fn citation(&mut self, subject: &str, citation: &Citation) -> miette::Result<()> {
        if citation.rss_event.is_some() {
            lk_vault::validate_rss_citation(self.root, Path::new(subject), citation)
                .into_diagnostic()?;
            self.page(&citation.path, None, None, SemanticAuthority::Page)?;
            let path = format!(
                "{}/events/{}.json",
                lk_core::rss_original::RSS_ORIGINAL_STORE,
                citation.revision
            );
            let raw = self.page(
                &path,
                Some(&citation.revision),
                None,
                SemanticAuthority::RssEvent,
            )?;
            let payload: lk_core::rss_original::RssEventPayload =
                serde_json::from_str(&raw).into_diagnostic()?;
            if citation
                .rss_event
                .as_ref()
                .is_none_or(|event| event.event_id != payload.event_id)
            {
                return Err(miette::miette!("semantic RSS event identity changed"));
            }
        } else {
            self.page(
                &citation.path,
                None,
                Some(&citation.revision),
                SemanticAuthority::Page,
            )?;
        }
        Ok(())
    }

    fn finish(self, subject: Subject, context: Value) -> Prepared {
        Prepared {
            subject,
            context,
            resources: self.resources.into_values().collect(),
            pages: self.pages,
        }
    }
}

pub(super) fn task(root: &Path, config: &Config, task_id: &str) -> miette::Result<Prepared> {
    let task = super::super::queue::current_task(root, task_id)?;
    capture_task(root, config, task)
}

pub(super) fn task_for_publication(
    root: &Path,
    config: &Config,
    task_id: &str,
) -> miette::Result<Prepared> {
    let task = super::super::queue::select_task(
        root,
        task_id,
        super::super::queue::TaskSelection::CurrentOrDoneExtraction,
    )?;
    capture_task(root, config, task)
}

fn capture_task(
    root: &Path,
    config: &Config,
    task: lk_queue::QueueTask,
) -> miette::Result<Prepared> {
    let mut captured = Capture::new(root);
    let target = captured.page(
        &task.target.vault_path,
        None,
        None,
        SemanticAuthority::Subject,
    )?;
    if task.kind != lk_queue::TaskKind::SynthesizeConcept {
        captured.resources.clear();
    }
    let subject = Subject::Queue {
        task_id: task.task_id.clone(),
        cache_hash: task.cache_hash.clone(),
        target: task.target.vault_path.clone(),
        target_revision: file_revision(&target),
    };
    let mut purpose = task.input.clone();
    if let Some(text) = purpose.remove("text") {
        let text = text
            .as_str()
            .ok_or_else(|| miette::miette!("retained task text must be a string"))?;
        let address = format!(
            "retained-input://{}/{}",
            task.target.vault_path,
            task.target.kind.llm_inputs_key()
        );
        captured.resources.insert(
            address.clone(),
            SemanticResource {
                address,
                revision: file_revision(text),
                authority: SemanticAuthority::RetainedInput,
                text: text.to_owned(),
            },
        );
    }
    if let Some(citations) = purpose.remove("citations") {
        for citation in serde_json::from_value::<Vec<Citation>>(citations).into_diagnostic()? {
            captured.citation(&task.target.vault_path, &citation)?;
        }
    }
    if let Some(manifest) = task.input.get("rss_original") {
        let manifest: lk_core::rss_original::RssOriginalManifest =
            serde_json::from_value(manifest.clone()).into_diagnostic()?;
        let mut normalized = BTreeMap::new();
        for event in &manifest.events {
            let path = format!(
                "{}/events/{}.json",
                lk_core::rss_original::RSS_ORIGINAL_STORE,
                event.revision
            );
            let raw = captured.page(
                &path,
                Some(&event.revision),
                None,
                SemanticAuthority::RssEvent,
            )?;
            lk_core::rss_original::verify_payload(&manifest, event, raw.as_bytes())
                .map_err(|e| miette::miette!("{e}"))?;
            normalized.insert(event.event_id.clone(), event.revision.clone());
        }
        purpose.remove("rss_original");
        purpose.insert("rss_evidence".into(), json!({"manifest_revision": manifest.reference().map_err(|e| miette::miette!("{e}"))?, "events": normalized.len()}));
    }
    if task.kind == lk_queue::TaskKind::SynthesizeConcept {
        let scoped = audit(root, config, &task.target.vault_path)?;
        for resource in scoped.resources {
            captured
                .resources
                .entry(resource.address.clone())
                .or_insert(resource);
        }
        for (path, raw) in scoped.pages {
            if captured.pages.get(&path).is_some_and(|prior| prior != &raw) {
                return Err(miette::miette!(
                    "synthesis dependency changed during capture: {}",
                    path.display()
                ));
            }
            captured.pages.insert(path, raw);
        }
        purpose.insert("knowledge_scope".into(), scoped.context["purpose"].clone());
    }
    let context = json!({
        "subject": subject,
        "purpose": {"operation":"queue", "task_kind":task.kind, "target":task.target, "parameters":purpose},
        "authority": "Retained input is the task source; the current target is publication context. Source/page/RSS evidence retains its declared authority. A read or part receipt is not factual verification.",
    });
    Ok(captured.finish(subject, context))
}

pub(super) fn audit(root: &Path, config: &Config, path: &str) -> miette::Result<Prepared> {
    let pages =
        lk_graph::scan::scan_vault(root, config.graph.scope.follow_links).into_diagnostic()?;
    let plan = lk_graph::audit::plan_selected(
        root,
        &config.vault.dirs,
        &pages,
        1,
        &[path.to_owned()],
        u64::MAX,
    )
    .into_diagnostic()?;
    let candidate = plan
        .candidates
        .into_iter()
        .next()
        .ok_or_else(|| miette::miette!("audit subject not found: {path}"))?;
    audit_candidate(root, config, &candidate)
}

/// Preliminary capture from the controller's selected plan; publication rederives scope.
pub(super) fn audit_candidate(
    root: &Path,
    config: &Config,
    candidate: &lk_graph::audit::AuditCandidate,
) -> miette::Result<Prepared> {
    let path = candidate.path.as_str();
    let mut captured = Capture::new(root);
    captured.page(
        path,
        Some(&candidate.file_revision),
        None,
        SemanticAuthority::Subject,
    )?;
    for citation in &candidate.evidence {
        captured.citation(path, citation)?;
    }
    for related in &candidate.related_pages {
        captured.page(
            &related.path,
            None,
            Some(&related.revision),
            SemanticAuthority::Context,
        )?;
    }
    for review in &candidate.reviews {
        let expected = review
            .file_revision
            .as_deref()
            .ok_or_else(|| miette::miette!("unreadable scoped review: {}", review.path))?;
        captured.page(
            &review.path,
            Some(expected),
            review.source_revision.as_deref(),
            SemanticAuthority::Review,
        )?;
        for evidence in &review.evidence {
            for current in evidence.current.iter().chain(&evidence.archived) {
                let authority = if evidence
                    .archived
                    .as_ref()
                    .is_some_and(|archive| archive.path == current.path)
                {
                    SemanticAuthority::Archive
                } else {
                    SemanticAuthority::Page
                };
                captured.page(
                    &current.path,
                    Some(&current.file_revision),
                    current.source_revision.as_deref(),
                    authority,
                )?;
            }
        }
    }
    let subject = Subject::Audit {
        path: path.to_owned(),
        file_revision: candidate.file_revision.clone(),
        input_revision: candidate.input_revision.clone(),
    };
    let context = json!({
        "subject": subject,
        "purpose": {"operation":"audit", "subject_kind":candidate.kind, "path":path, "title":candidate.title, "locale":config.vault.locale,
            "discovery_queries":candidate.discovery.as_ref().map(|discovery| &discovery.queries),
            "review_dependencies":candidate.reviews.iter().map(|review| &review.path).collect::<Vec<_>>()},
        "authority": "The selected page is the audit subject. Concept citations are evidence; a document is its own subject without a self-citation. Related pages are context, not automatically support. Reviews and historical archives are assessments/history, not independent corroboration. Join source-declared scope, time and attribution before judging contradictions; unknown context stays unknown.",
    });
    Ok(captured.finish(subject, context))
}

pub(super) fn refresh(root: &Path, config: &Config, context: &Value) -> miette::Result<Prepared> {
    let subject: Subject = serde_json::from_value(
        context
            .get("subject")
            .cloned()
            .ok_or_else(|| miette::miette!("semantic work has no subject"))?,
    )
    .into_diagnostic()?;
    let prepared = match &subject {
        Subject::Queue { task_id, .. } => task(root, config, task_id)?,
        Subject::Audit { path, .. } => audit(root, config, path)?,
    };
    if prepared.subject != subject || prepared.context != *context {
        return Err(miette::miette!(
            "semantic work inputs changed; replan and reread changed evidence"
        ));
    }
    Ok(prepared)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_audit_capture_matches_live_input_and_refuses_dependency_drift() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let documents = root.join("wiki/documents");
        std::fs::create_dir_all(&documents).unwrap();
        std::fs::write(
            documents.join("a.md"),
            "---\ntype: document\ntitle: A\n---\nSubject. [Context](b.md)\n",
        )
        .unwrap();
        std::fs::write(
            documents.join("b.md"),
            "---\ntype: document\ntitle: B\n---\nContext.\n",
        )
        .unwrap();
        let config_path = root.join("config.yaml");
        std::fs::write(
            &config_path,
            "vault:\n  root: .\nidentity:\n  name: Test\n  email: test@example.com\nsources:\n  notes:\n    type: manual\n    enabled: false\n    params: {inbox_dir: inbox}\n",
        )
        .unwrap();
        let config = Config::load(&config_path).unwrap();
        let pages = lk_graph::scan::scan_vault(&root, false).unwrap();
        let plan = lk_graph::audit::plan_selected(
            &root,
            &config.vault.dirs,
            &pages,
            1,
            &["wiki/documents/a.md".into()],
            u64::MAX,
        )
        .unwrap();
        let candidate = &plan.candidates[0];
        let live = audit(&root, &config, &candidate.path).unwrap();
        let selected = audit_candidate(&root, &config, candidate).unwrap();
        assert_eq!(live.subject, selected.subject);
        assert_eq!(live.context, selected.context);
        assert_eq!(live.resources, selected.resources);
        assert_eq!(live.pages, selected.pages);
        std::fs::write(documents.join("unrelated.md"), "---\ninvalid: [\n---\n").unwrap();
        assert!(audit_candidate(&root, &config, candidate).is_ok());
        std::fs::write(
            documents.join("b.md"),
            "---\ntype: document\ntitle: B\n---\nChanged context.\n",
        )
        .unwrap();
        assert!(audit_candidate(&root, &config, candidate).is_err());
    }
}
