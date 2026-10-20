use std::collections::BTreeMap;
use std::path::Path;

use lk_queue::{SectionResult, SynthesisResult};

use super::{TaskStatus, classify_task, read_pending_tasks, resolve_target_path};

pub(super) use lk_core::concept::file_revision as revision;

/// Apply staged synthesis under the queue's writer lock. Sources and the target are
/// validated again at publication; failed results stay available for diagnosis.
#[derive(Default)]
pub(super) struct ApplyResult {
    pub applied: usize,
    pub dropped: usize,
    pub failed: usize,
    pub unreadable: usize,
    pub quarantined: usize,
}

pub(super) fn apply(
    root: &Path,
    queue: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    dry_run: bool,
    session: Option<&lk_vault::MutationSession>,
) -> miette::Result<ApplyResult> {
    let dir = queue.join(lk_queue::RESULTS_SUBDIR);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ApplyResult::default());
        }
        Err(error) => return Err(miette::miette!("read {}: {error}", dir.display())),
    };
    let mut paths = entries
        .map(|entry| entry.map(|e| e.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| miette::miette!("{e}"))?;
    paths.retain(|p| {
        p.to_string_lossy()
            .ends_with(lk_queue::SYNTHESIS_RESULT_SUFFIX)
            || p.to_string_lossy()
                .ends_with(lk_queue::SECTION_RESULT_SUFFIX)
    });
    if paths.is_empty() {
        return Ok(ApplyResult::default());
    }
    paths.sort();
    let pending = read_pending_tasks(queue)?;

    let mut report = ApplyResult::default();
    for path in paths {
        let mut corrupt = false;
        let section = path
            .to_string_lossy()
            .ends_with(lk_queue::SECTION_RESULT_SUFFIX);
        let result = (|| -> miette::Result<bool> {
            let raw = std::fs::read(&path).map_err(|e| miette::miette!("{e}"))?;
            let decoded = if section {
                serde_json::from_slice::<SectionResult>(&raw).map(|result| SynthesisResult {
                    task_id: result.task_id,
                    cache_hash: result.cache_hash,
                    target_revision: result.target_revision,
                    context_revision: String::new(),
                    synthesis: result.body,
                    related: None,
                })
            } else {
                serde_json::from_slice::<SynthesisResult>(&raw)
            };
            let result = match decoded {
                Ok(result) => result,
                Err(error) => {
                    corrupt = true;
                    report.unreadable += 1;
                    report.quarantined += super::quarantine(
                        &dir.join(lk_queue::CORRUPT_SUBDIR),
                        &[(path.clone(), error.to_string())],
                        dry_run,
                    );
                    return Err(miette::miette!("invalid staged section payload: {error}"));
                }
            };
            let Some(task) = pending
                .tasks
                .get(&(result.task_id.clone(), result.cache_hash.clone()))
            else {
                if pending.unparseable > 0 {
                    return Err(miette::miette!(
                        "no matching readable task; repair unparseable queue lines before deciding this result's fate"
                    ));
                }
                let reason =
                    "no matching pending task; its run was retired or the task identity is invalid";
                if !dry_run {
                    quarantine_orphan(queue, &path, reason)?;
                }
                return Err(miette::miette!(
                    "{reason}; result quarantined (or would be in dry-run)"
                ));
            };
            if section {
                let supported = task.kind.supports_target(task.target.kind)
                    && !matches!(
                        task.kind,
                        lk_queue::TaskKind::ExtractConcepts | lk_queue::TaskKind::SynthesizeConcept
                    );
                if !supported || task.anchors().len() != 1 {
                    return Err(miette::miette!(
                        "section result does not match a supported single-section task"
                    ));
                }
            } else if task.kind != lk_queue::TaskKind::SynthesizeConcept
                || task.target.kind != lk_queue::TargetKind::ConceptSynthesis
            {
                return Err(miette::miette!("result requires a concept synthesis task"));
            }
            let state = classify_task(root, task, &mut BTreeMap::new())?;
            if matches!(
                state,
                TaskStatus::Done | TaskStatus::Stale | TaskStatus::MissingTarget
            ) {
                if !dry_run {
                    std::fs::remove_file(&path).map_err(|e| miette::miette!("{e}"))?;
                }
                return Ok(false);
            }
            if state != TaskStatus::Current {
                return Err(miette::miette!(
                    "unreadable target or inconsistent synthesis input"
                ));
            }
            let target = resolve_target_path(&task.target.vault_path)
                .ok_or_else(|| miette::miette!("invalid target"))?;
            let original = super::read_page(root, &target).map_err(|e| miette::miette!("{e}"))?;
            if revision(&original) != result.target_revision {
                return Err(miette::miette!(
                    "target changed; read the current page and regenerate this result"
                ));
            }
            let mut plan = lk_vault::MutationPlan::default();
            let value = if section {
                serde_json::to_value(SectionResult {
                    task_id: result.task_id.clone(),
                    cache_hash: result.cache_hash.clone(),
                    target_revision: result.target_revision.clone(),
                    body: result.synthesis.clone(),
                })
            } else {
                serde_json::to_value(&result)
            }
            .map_err(|e| miette::miette!("{e}"))?;
            super::super::work::guard_queue(
                root,
                config,
                config_revision,
                &result.task_id,
                value,
                &mut plan,
            )?;
            if !section && result.synthesis.trim().is_empty() {
                return Err(miette::miette!(
                    "synthesis must state the evidence or its limits"
                ));
            }
            if task.kind == lk_queue::TaskKind::RefineEvents {
                let heading = task
                    .target
                    .anchor
                    .strip_prefix("## ")
                    .ok_or_else(|| miette::miette!("invalid section anchor"))?;
                let previous = lk_vault::section_body(&original, heading)
                    .ok_or_else(|| miette::miette!("missing event section"))?;
                if event_headings(previous) != event_headings(&result.synthesis) {
                    return Err(miette::miette!(
                        "refinement must preserve the event heading sequence"
                    ));
                }
            }
            let mut content = original.clone();
            let anchors = task.anchors();
            if (anchors.len() == 2) != result.related.is_some() {
                return Err(miette::miette!(
                    "related section must match the task's anchors"
                ));
            }
            for (anchor, body) in anchors
                .iter()
                .zip(std::iter::once(&result.synthesis).chain(result.related.iter()))
            {
                validate_links(root, &target, body)?;
                let mut fence = lk_core::markdown::FenceState::new();
                if body.lines().any(|line| {
                    !fence.apply(line) && fence.is_closed() && line.trim_end().starts_with("## ")
                }) {
                    return Err(miette::miette!(
                        "section content must not introduce a sibling heading"
                    ));
                }
                if !fence.is_closed() {
                    return Err(miette::miette!(
                        "section content has an unclosed code fence"
                    ));
                }
                let heading = anchor
                    .strip_prefix("## ")
                    .ok_or_else(|| miette::miette!("invalid section anchor"))?;
                content = lk_vault::replace_section(&content, heading, body);
            }
            task.generation_policy()
                .map_err(|e| miette::miette!("{e}"))?;
            let content = lk_vault::set_llm_input(
                &content,
                &task.target.kind.completion_key(),
                &task.cache_hash,
            )
            .ok_or_else(|| miette::miette!("cannot stamp completion"))?;
            let content = super::stamp_policy(&content, Some(task), task.target.kind)?;
            if !dry_run {
                if classify_task(root, task, &mut BTreeMap::new())? != TaskStatus::Current
                    || super::read_page(root, &target).map_err(|e| miette::miette!("{e}"))?
                        != original
                {
                    return Err(miette::miette!(
                        "synthesis inputs changed before publication"
                    ));
                }
                validate_links(root, &target, &result.synthesis)?;
                if let Some(body) = &result.related {
                    validate_links(root, &target, body)?;
                }
                let citations: Vec<lk_core::concept::Citation> = task
                    .input
                    .get("citations")
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(|e| miette::miette!("{e}"))?
                    .unwrap_or_default();
                guard_citations(&mut plan, root, &target, &citations)?;
                plan.edit(&target, original, content);
                session
                    .ok_or_else(|| miette::miette!("publication requires mutation session"))?
                    .apply(plan)
                    .map_err(|e| miette::miette!("{e}"))?;
                std::fs::remove_file(&path).map_err(|e| miette::miette!("{e}"))?;
            }
            Ok(true)
        })();
        match result {
            Ok(true) => report.applied += 1,
            Ok(false) => report.dropped += 1,
            Err(error) => {
                if !corrupt {
                    report.failed += 1;
                }
                eprintln!("  ✗ {}: {error}", path.display());
            }
        }
    }
    Ok(report)
}

fn guard_citations(
    plan: &mut lk_vault::MutationPlan,
    root: &Path,
    target: &Path,
    citations: &[lk_core::concept::Citation],
) -> miette::Result<()> {
    for citation in citations {
        if citation.rss_event.is_some() {
            lk_vault::guard_rss_citation(plan, root, target, citation)
                .map_err(|error| miette::miette!("{error}"))?;
        } else {
            let path = resolve_target_path(&citation.path)
                .ok_or_else(|| miette::miette!("invalid citation path: {}", citation.path))?;
            let content = super::read_page(root, &path)
                .map_err(|error| miette::miette!("read {}: {error}", path.display()))?;
            let page = lk_core::frontmatter::parse_page(&content)
                .map_err(|error| miette::miette!("parse {}: {error}", path.display()))?;
            if lk_core::concept::source_revision(&page) != citation.revision {
                return Err(miette::miette!(
                    "synthesis citation changed before publication: {}",
                    path.display()
                ));
            }
            plan.require_unchanged(path, content);
        }
    }
    Ok(())
}

/// Preserve the rejected answer and a durable reason outside the retry path.
pub(super) fn quarantine_orphan(queue: &Path, path: &Path, reason: &str) -> miette::Result<()> {
    let dir = queue.join(lk_queue::RESULTS_SUBDIR).join("orphaned");
    std::fs::create_dir_all(&dir).map_err(|e| miette::miette!("{e}"))?;
    let destination = super::free_path(&dir, path);
    let reason_path = destination.with_file_name(format!(
        "{}.reason.txt",
        destination
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
    ));
    lk_core::fs::write_atomic(&reason_path, format!("{reason}\n").as_bytes(), None)
        .map_err(|e| miette::miette!("cannot preserve quarantine reason: {e}"))?;
    std::fs::rename(path, &destination)
        .map_err(|e| miette::miette!("cannot quarantine {}: {e}", path.display()))?;
    eprintln!(
        "  quarantined {} → {}: {reason}",
        path.display(),
        destination.display()
    );
    Ok(())
}

/// The shared parser ignores external URLs, images, code and local anchors. Every
/// page reference introduced by a staged section must already have a destination.
fn validate_links(root: &Path, target: &Path, body: &str) -> miette::Result<()> {
    for destination in lk_core::link::extract_dests(body) {
        let path = lk_core::link::resolve_dest(target, &destination)
            .ok_or_else(|| miette::miette!("section link escapes the vault: {destination}"))?;
        if !root.join(&path).is_file() {
            return Err(miette::miette!(
                "section links to a missing page: {}",
                path.display()
            ));
        }
    }
    Ok(())
}

/// Event identity is structural; prose and source attribution remain semantic judgments.
fn event_headings(body: &str) -> Vec<&str> {
    let mut fence = lk_core::markdown::FenceState::new();
    body.lines()
        .filter(|line| !fence.apply(line) && fence.is_closed() && line.starts_with("### "))
        .map(str::trim_end)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_citation_guards_refuse_changes_before_publication_and_recovery() {
        const ORIGINAL: &str = "---\ntype: document\ntitle: Evidence\n---\nOriginal evidence.\n";
        const CHANGED: &str = "---\ntype: document\ntitle: Evidence\n---\nChanged evidence.\n";
        let source = Path::new("source.md");
        let target = Path::new("concept.md");
        let citation = lk_core::concept::Citation::new(
            source,
            &lk_core::frontmatter::parse_page(ORIGINAL).unwrap(),
        );
        for replacement in [Some(ORIGINAL), Some(CHANGED), None] {
            let root = tempfile::tempdir().unwrap();
            std::fs::write(root.path().join(source), ORIGINAL).unwrap();
            std::fs::write(root.path().join(target), "before").unwrap();
            let mut plan = lk_vault::MutationPlan::default();
            guard_citations(
                &mut plan,
                root.path(),
                target,
                std::slice::from_ref(&citation),
            )
            .unwrap();
            plan.edit(target, "before".into(), "synthesis and completion".into());
            let state = root.path().join(".lorekeeper/mutation");
            std::fs::create_dir_all(&state).unwrap();
            let journal = serde_json::json!({"version": 1, "plan": plan}).to_string();
            std::fs::write(state.join("pending.json"), &journal).unwrap();
            match replacement {
                Some(content) => std::fs::write(root.path().join(source), content).unwrap(),
                None => std::fs::remove_file(root.path().join(source)).unwrap(),
            }
            let recovered = lk_vault::MutationSession::acquire(root.path());
            if replacement == Some(ORIGINAL) {
                assert!(recovered.is_ok());
                assert_eq!(
                    std::fs::read_to_string(root.path().join(target)).unwrap(),
                    "synthesis and completion"
                );
                assert!(!state.join("pending.json").exists());
            } else {
                assert!(recovered.is_err());
                assert_eq!(
                    std::fs::read_to_string(root.path().join(target)).unwrap(),
                    "before"
                );
                assert_eq!(
                    std::fs::read_to_string(state.join("pending.json")).unwrap(),
                    journal
                );
                assert!(
                    guard_citations(
                        &mut lk_vault::MutationPlan::default(),
                        root.path(),
                        target,
                        std::slice::from_ref(&citation)
                    )
                    .is_err()
                );
            }
        }
    }
}
