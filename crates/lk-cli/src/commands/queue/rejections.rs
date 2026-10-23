use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use miette::IntoDiagnostic;
use serde::{Deserialize, Serialize};

use super::application::{
    Proposal, ProposalSnapshot, PublicationRejection, RejectionKind, RejectionScope, TaskIdentity,
};

const DIRECTORY: &str = ".lorekeeper/queue/rejections";
const SCHEMA: &str = "queue-application-rejection-v1";

fn validation_policy() -> String {
    let mut hasher = blake3::Hasher::new();
    for source in [
        include_str!("../queue.rs"),
        include_str!("synthesis.rs"),
        include_str!("application.rs"),
        include_str!("rejections.rs"),
        include_str!("../work.rs"),
        include_str!("../work/selection.rs"),
        include_str!("../work/publication.rs"),
        include_str!("../work/resources.rs"),
        include_str!("../work/section.rs"),
        include_str!("../../../../lk-pipeline/src/lib.rs"),
        include_str!("../../../../lk-pipeline/src/concept_draft.rs"),
    ] {
        hasher.update(source.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    schema: String,
    validation_policy: String,
    config_revision: String,
    task_revision: String,
    identity: TaskIdentity,
    proposals: Vec<Proposal>,
    scope: RejectionScope,
    kind: RejectionKind,
    reason: String,
}

pub(in crate::commands) struct BlockedTask {
    pub owner: String,
    pub reason: String,
    pub needs_revalidation: bool,
}

fn digest(value: &impl Serialize) -> miette::Result<String> {
    Ok(blake3::hash(&serde_json::to_vec(value).into_diagnostic()?)
        .to_hex()
        .to_string())
}

fn path(task_id: &str, cache_hash: &str) -> miette::Result<PathBuf> {
    Ok(Path::new(DIRECTORY).join(format!("{}.json", digest(&(task_id, cache_hash))?)))
}

fn is_revision(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn relative_address(value: &str) -> bool {
    let path = Path::new(value);
    !value.is_empty()
        && path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

fn decode(path: &Path, raw: &str) -> miette::Result<Receipt> {
    let value: serde_json::Value = serde_json::from_str(raw).into_diagnostic()?;
    if !value.is_object() {
        return Err(miette::miette!(
            "queue rejection receipt must be a JSON object"
        ));
    }
    let receipt: Receipt = serde_json::from_value(value).into_diagnostic()?;
    if receipt.schema != SCHEMA
        || !is_revision(&receipt.validation_policy)
        || path != self::path(&receipt.identity.task_id, &receipt.identity.cache_hash)?
        || receipt.reason.trim().is_empty()
        || !receipt.identity.is_valid()
        || !is_revision(&receipt.config_revision)
        || !is_revision(&receipt.task_revision)
        || !receipt.scope.is_valid()
        || receipt.proposals.is_empty()
        || receipt.proposals.windows(2).any(|pair| pair[0] >= pair[1])
        || receipt.proposals.iter().any(|proposal| {
            !relative_address(&proposal.path)
                || !Path::new(&proposal.path).starts_with(".lorekeeper/queue/results")
                || Path::new(&proposal.path).parent()
                    != Some(Path::new(".lorekeeper/queue/results"))
                || !is_revision(&proposal.revision)
        })
    {
        return Err(miette::miette!(
            "invalid queue rejection receipt identity, scope or schema"
        ));
    }
    Ok(receipt)
}

fn read_optional(root: &Path, path: &Path) -> miette::Result<Option<String>> {
    let full = lk_vault::page_path(root, path).into_diagnostic()?;
    match std::fs::symlink_metadata(&full) {
        Ok(metadata) if !metadata.is_file() || metadata.file_type().is_symlink() => {
            return Err(miette::miette!(
                "rejection receipts must be direct regular files"
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).into_diagnostic(),
    }
    std::fs::read_to_string(full).map(Some).into_diagnostic()
}

fn inventory(root: &Path) -> miette::Result<Vec<(PathBuf, String, Receipt)>> {
    let directory = lk_vault::page_path(root, Path::new(DIRECTORY)).into_diagnostic()?;
    let metadata = match std::fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).into_diagnostic(),
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(miette::miette!(
            "rejection store must be a direct directory"
        ));
    }
    let mut entries = std::fs::read_dir(directory)
        .into_diagnostic()?
        .collect::<Result<Vec<_>, _>>()
        .into_diagnostic()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    entries
        .into_iter()
        .map(|entry| {
            let path = Path::new(DIRECTORY).join(entry.file_name());
            if path.extension().is_none_or(|extension| extension != "json") {
                return Err(miette::miette!("unknown rejection receipt artifact"));
            }
            let raw = read_optional(root, &path)?
                .ok_or_else(|| miette::miette!("rejection receipt disappeared"))?;
            let receipt = decode(&path, &raw)?;
            Ok((path, raw, receipt))
        })
        .collect()
}

pub(super) fn validate_store(root: &Path) -> miette::Result<()> {
    inventory(root).map(|_| ())
}

pub(super) fn observe_scope(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    task: &lk_queue::QueueTask,
    snapshots: &[ProposalSnapshot],
) -> miette::Result<(RejectionScope, BTreeMap<PathBuf, String>)> {
    let canonical_root = root.canonicalize().into_diagnostic()?;
    let root = canonical_root.as_path();
    let mut recipes = BTreeMap::new();
    let mut destinations = BTreeSet::new();
    for snapshot in snapshots {
        let (queries, bodies) = if snapshot
            .proposal
            .path
            .ends_with(lk_queue::SECTION_RESULT_SUFFIX)
        {
            let result: lk_queue::SectionResult =
                serde_json::from_str(&snapshot.raw).into_diagnostic()?;
            (Vec::new(), vec![result.body])
        } else if snapshot
            .proposal
            .path
            .ends_with(lk_queue::SYNTHESIS_RESULT_SUFFIX)
        {
            let result: lk_queue::SynthesisResult =
                serde_json::from_str(&snapshot.raw).into_diagnostic()?;
            (
                Vec::new(),
                std::iter::once(result.synthesis)
                    .chain(result.related)
                    .collect(),
            )
        } else {
            let result: lk_queue::TaskResult =
                serde_json::from_str(&snapshot.raw).into_diagnostic()?;
            (result.discovery, Vec::new())
        };
        for body in bodies {
            destinations.extend(lk_core::link::extract_dests(&body));
        }
        recipes.insert(serde_json::to_vec(&queries).into_diagnostic()?, queries);
    }
    let mut revisions = Vec::new();
    let mut owners = BTreeSet::new();
    let mut pages = BTreeMap::new();
    for (recipe, queries) in recipes {
        let scope = crate::commands::work::publication_scope(
            root,
            config,
            config_revision,
            &task.task_id,
            &queries,
        )?;
        revisions.push((recipe, scope.revision));
        for owner in scope.owners {
            owners.insert(
                owner
                    .to_str()
                    .ok_or_else(|| miette::miette!("rejection scope owner is not UTF-8"))?
                    .to_owned(),
            );
        }
        for (path, raw) in scope.pages {
            if pages
                .insert(path, raw.clone())
                .is_some_and(|prior| prior != raw)
            {
                return Err(miette::miette!(
                    "rejection scopes captured conflicting page bytes"
                ));
            }
        }
    }
    let mut links = Vec::new();
    for destination in destinations {
        let Some(path) =
            lk_core::link::resolve_dest(Path::new(&task.target.vault_path), &destination)
                .filter(|path| !path.as_os_str().is_empty())
        else {
            links.push((destination, None, None));
            continue;
        };
        let full = lk_vault::page_path(root, &path).into_diagnostic()?;
        let state = match std::fs::metadata(&full) {
            Ok(metadata) => {
                let owner = full.canonicalize().into_diagnostic()?;
                let owner = owner.strip_prefix(root).into_diagnostic()?.to_path_buf();
                if metadata.is_file() {
                    owners.insert(
                        owner
                            .to_str()
                            .ok_or_else(|| miette::miette!("link owner is not UTF-8"))?
                            .to_owned(),
                    );
                }
                Some((metadata.is_file(), owner))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).into_diagnostic(),
        };
        links.push((destination, Some(path), state));
    }
    Ok((
        RejectionScope {
            revision: digest(&(revisions, links))?,
            owners: owners.into_iter().collect(),
        },
        pages,
    ))
}

pub(super) fn record(
    session: &lk_vault::MutationSession,
    config: &lk_core::config::Config,
    config_revision: &str,
    task: &lk_queue::QueueTask,
    snapshots: &[ProposalSnapshot],
    rejection: &PublicationRejection,
) -> miette::Result<(String, RejectionScope)> {
    let root = session.root();
    let (scope, pages) = observe_scope(root, config, config_revision, task, snapshots)?;
    let identity = TaskIdentity::capture(root, task)?;
    let mut proposals = snapshots
        .iter()
        .map(|snapshot| snapshot.proposal.clone())
        .collect::<Vec<_>>();
    proposals.sort();
    let receipt = Receipt {
        schema: SCHEMA.into(),
        validation_policy: validation_policy(),
        config_revision: config_revision.into(),
        task_revision: digest(task)?,
        identity,
        proposals,
        scope: scope.clone(),
        kind: rejection.kind,
        reason: rejection.detail.clone(),
    };
    let path = self::path(&task.task_id, &task.cache_hash)?;
    let before = read_optional(root, &path)?;
    if let Some(raw) = &before {
        decode(&path, raw)?;
    }
    let after = serde_json::to_string(&receipt).into_diagnostic()?;
    decode(&path, &after)?;
    let mut plan = lk_vault::MutationPlan::default();
    for (path, raw) in pages {
        plan.require_unchanged(path, raw);
    }
    for snapshot in snapshots {
        plan.require_unchanged(&snapshot.proposal.path, snapshot.raw.clone());
    }
    plan.publish_expected(root, &path, before, after)
        .into_diagnostic()?;
    session.apply(plan).into_diagnostic()?;
    Ok((path.to_string_lossy().into_owned(), scope))
}

pub(super) fn remove_matching(
    root: &Path,
    task: &lk_queue::QueueTask,
    plan: &mut lk_vault::MutationPlan,
) -> miette::Result<()> {
    let path = self::path(&task.task_id, &task.cache_hash)?;
    if let Some(raw) = read_optional(root, &path)? {
        decode(&path, &raw)?;
        plan.remove(path, raw);
    }
    Ok(())
}

pub(super) fn prune(
    root: &Path,
    session: Option<&lk_vault::MutationSession>,
) -> miette::Result<usize> {
    let root = root.canonicalize().into_diagnostic()?;
    let receipts = inventory(&root)?;
    if receipts.is_empty() {
        return Ok(0);
    }
    let active = active_proposals(&root)?;
    let queue = root.join(".lorekeeper/queue");
    let mut identities = BTreeSet::new();
    let mut guards = BTreeMap::new();
    for directory in [&queue, &queue.join(lk_queue::PROCESSED_SUBDIR)] {
        for path in super::pending_queue_files(directory)? {
            let metadata = std::fs::symlink_metadata(&path).into_diagnostic()?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(miette::miette!(
                    "retained queue identity requires direct regular files"
                ));
            }
            let (raw, read) = lk_queue::read_tasks_snapshot(&path).into_diagnostic()?;
            if !read.is_rewritable() {
                return Err(miette::miette!(
                    "unparseable retained queue lines prevent rejection cleanup"
                ));
            }
            for task in read.tasks {
                task.validate_retained_input()
                    .map_err(|error| miette::miette!("{error}"))?;
                task.generation_policy()
                    .map_err(|error| miette::miette!("{error}"))?;
                identities.insert((task.task_id, task.cache_hash));
            }
            let relative = path.strip_prefix(&root).into_diagnostic()?.to_path_buf();
            guards.insert(relative, String::from_utf8(raw).into_diagnostic()?);
        }
    }
    let mut plan = lk_vault::MutationPlan::default();
    for (path, raw) in guards {
        plan.require_unchanged(path, raw);
    }
    for snapshots in active.values() {
        for snapshot in snapshots {
            plan.require_unchanged(&snapshot.proposal.path, snapshot.raw.clone());
        }
    }
    let mut removed = 0;
    for (path, raw, receipt) in receipts {
        let key = (receipt.identity.task_id, receipt.identity.cache_hash);
        if !identities.contains(&key) && !active.contains_key(&key) {
            plan.remove(path, raw);
            removed += 1;
        }
    }
    if removed != 0
        && let Some(session) = session
    {
        session.apply(plan).into_diagnostic()?;
    }
    Ok(removed)
}

pub(in crate::commands) fn blocked_tasks(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
) -> miette::Result<BTreeMap<(String, String), BlockedTask>> {
    let root = root.canonicalize().into_diagnostic()?;
    let pending = super::read_pending_tasks(&root.join(".lorekeeper/queue"))?;
    if pending.unparseable != 0 {
        return Err(miette::miette!(
            "unknown pending task targets prevent rejection inspection"
        ));
    }
    let active = active_proposals(&root)?;
    let validation_policy = validation_policy();
    let mut blocked = BTreeMap::new();
    for (_, _, receipt) in inventory(&root)? {
        let key = (
            receipt.identity.task_id.clone(),
            receipt.identity.cache_hash.clone(),
        );
        let Some(snapshots) = active.get(&key) else {
            continue;
        };
        let task = pending.tasks.get(&key).ok_or_else(|| {
            miette::miette!("active rejected proposal has no trusted pending task")
        })?;
        let current = TaskIdentity::capture(&root, task)?;
        let (scope, _) = observe_scope(&root, config, config_revision, task, snapshots)?;
        let proposals = snapshots
            .iter()
            .map(|snapshot| snapshot.proposal.clone())
            .collect::<Vec<_>>();
        let needs_revalidation = receipt.validation_policy != validation_policy
            || receipt.task_revision != digest(task)?
            || receipt.config_revision != config_revision
            || current != receipt.identity
            || proposals != receipt.proposals
            || scope != receipt.scope;
        blocked.insert(
            key,
            BlockedTask {
                owner: current.owner,
                reason: receipt.reason,
                needs_revalidation,
            },
        );
    }
    Ok(blocked)
}

pub(super) struct ProposalInventory {
    pub groups: BTreeMap<(String, String), Vec<ProposalSnapshot>>,
    pub unreadable: Vec<(PathBuf, String)>,
}

fn active_proposals(
    root: &Path,
) -> miette::Result<BTreeMap<(String, String), Vec<ProposalSnapshot>>> {
    let inventory = load_proposals(root)?;
    if !inventory.unreadable.is_empty() {
        return Err(miette::miette!(
            "unreadable staged results have unknown task identities"
        ));
    }
    Ok(inventory.groups)
}

pub(super) fn load_proposals(root: &Path) -> miette::Result<ProposalInventory> {
    let directory = root.join(".lorekeeper/queue/results");
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ProposalInventory {
                groups: BTreeMap::new(),
                unreadable: Vec::new(),
            });
        }
        Err(error) => return Err(error).into_diagnostic(),
    };
    let mut groups: BTreeMap<_, Vec<ProposalSnapshot>> = BTreeMap::new();
    let mut unreadable = Vec::new();
    for entry in entries {
        let entry = entry.into_diagnostic()?;
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let metadata = std::fs::symlink_metadata(&path).into_diagnostic()?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(miette::miette!(
                "staged proposals must be direct regular files"
            ));
        }
        let raw = std::fs::read(&path).into_diagnostic()?;
        let key = if path
            .to_string_lossy()
            .ends_with(lk_queue::SECTION_RESULT_SUFFIX)
        {
            serde_json::from_slice::<lk_queue::SectionResult>(&raw)
                .map(|result| (result.task_id, result.cache_hash))
        } else if path
            .to_string_lossy()
            .ends_with(lk_queue::SYNTHESIS_RESULT_SUFFIX)
        {
            serde_json::from_slice::<lk_queue::SynthesisResult>(&raw)
                .map(|result| (result.task_id, result.cache_hash))
        } else {
            serde_json::from_slice::<lk_queue::TaskResult>(&raw)
                .map(|result| (result.task_id, result.cache_hash))
        };
        let key = match key {
            Ok(key) => key,
            Err(error) => {
                unreadable.push((path, error.to_string()));
                continue;
            }
        };
        let snapshot = super::application::proposal_snapshot(root, &path)?;
        if snapshot.raw.as_bytes() != raw {
            return Err(miette::miette!(
                "staged result changed during identity inspection"
            ));
        }
        groups.entry(key).or_default().push(snapshot);
    }
    for snapshots in groups.values_mut() {
        snapshots.sort_by(|left, right| left.proposal.cmp(&right.proposal));
    }
    Ok(ProposalInventory { groups, unreadable })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receipt() -> Receipt {
        Receipt {
            schema: SCHEMA.into(),
            validation_policy: validation_policy(),
            config_revision: "a".repeat(64),
            task_revision: "b".repeat(64),
            identity: TaskIdentity {
                task_id: "task".into(),
                cache_hash: "c".repeat(32),
                target: lk_queue::TaskTarget {
                    vault_path: "wiki/documents/source.md".into(),
                    kind: lk_queue::TargetKind::DocumentSummary,
                    anchor: "## Summary".into(),
                },
                owner: "wiki/documents/source.md".into(),
            },
            proposals: vec![Proposal {
                path: ".lorekeeper/queue/results/task.section.json".into(),
                revision: "d".repeat(64),
            }],
            scope: RejectionScope {
                revision: "e".repeat(64),
                owners: vec!["wiki/documents/source.md".into()],
            },
            kind: RejectionKind::TargetChanged,
            reason: "Target changed.".into(),
        }
    }

    #[test]
    fn rejection_receipts_require_named_strict_fields_but_preserve_historical_policies() {
        let receipt = receipt();
        let path = path(&receipt.identity.task_id, &receipt.identity.cache_hash).unwrap();
        let value = serde_json::to_value(receipt).unwrap();
        decode(&path, &value.to_string()).unwrap();
        for policy in ["0".repeat(64), "f".repeat(64)] {
            let mut historical = value.clone();
            historical["validation_policy"] = policy.into();
            decode(&path, &historical.to_string()).unwrap();
        }
        let array =
            serde_json::Value::Array(value.as_object().unwrap().values().cloned().collect());
        assert!(decode(&path, &array.to_string()).is_err());
        for field in ["schema", "unknown"] {
            let mut invalid = value.clone();
            invalid[field] = "unknown".into();
            assert!(decode(&path, &invalid.to_string()).is_err());
        }
        let mut invalid = value;
        invalid["identity"]["owner"] = "../outside.md".into();
        assert!(decode(&path, &invalid.to_string()).is_err());
    }

    #[test]
    fn rejected_section_revalidates_when_only_its_missing_destination_appears() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let config_path = root.join("config.yaml");
        std::fs::write(&config_path, "vault:\n  root: .\n  locale: en\nidentity: {name: Test, email: test@example.com}\nsources:\n  notes:\n    type: manual\n    params: {inbox_dir: inbox}\n").unwrap();
        let (config, config_revision) =
            lk_core::config::Config::load_with_revision(&config_path).unwrap();
        let task = lk_queue::QueueLlmClient::new(root.join(".lorekeeper/queue")).build_task(
            "summary",
            lk_queue::TaskKind::Summarize,
            lk_queue::SummarizeRequest {
                text: "Source observations.".into(),
                max_sentences: 3,
                focus: None,
                locale: lk_core::i18n::Locale::En,
                source_type: Some(lk_core::config::SourceType::Manual),
                target: lk_queue::TaskTarget {
                    vault_path: "wiki/documents/source.md".into(),
                    kind: lk_queue::TargetKind::DocumentSummary,
                    anchor: "## Summary".into(),
                },
            },
        );
        let target = format!(
            "---\ntype: document\nllm_inputs: {{summary: {}}}\n---\n## Summary\n\n## Content\nSource observations.\n",
            task.cache_hash
        );
        std::fs::create_dir_all(root.join("wiki/documents")).unwrap();
        std::fs::write(root.join(&task.target.vault_path), &target).unwrap();
        std::fs::create_dir_all(root.join(".lorekeeper/queue/results")).unwrap();
        std::fs::write(
            root.join(".lorekeeper/queue/pending.jsonl"),
            serde_json::to_string(&task).unwrap(),
        )
        .unwrap();
        let result_path = root.join(".lorekeeper/queue/results/summary.section.json");
        let result = lk_queue::SectionResult {
            context_revision: None,
            task_id: task.task_id.clone(),
            cache_hash: task.cache_hash.clone(),
            target_revision: lk_core::concept::file_revision(&target),
            body: "See [details](missing.md).".into(),
        };
        std::fs::write(&result_path, serde_json::to_string(&result).unwrap()).unwrap();
        let snapshots =
            super::super::application::capture_group(&root, &[(result_path.clone(), result)])
                .unwrap();
        let session = lk_vault::MutationSession::acquire(&root).unwrap();
        let (receipt_path, _) = record(
            &session,
            &config,
            &config_revision,
            &task,
            &snapshots,
            &PublicationRejection {
                kind: RejectionKind::InvalidResultLink,
                detail: "Destination is missing.".into(),
            },
        )
        .unwrap();
        let key = (task.task_id.clone(), task.cache_hash.clone());
        assert!(!blocked_tasks(&root, &config, &config_revision).unwrap()[&key].needs_revalidation);
        std::fs::write(
            root.join("wiki/documents/missing.md"),
            "---\ntype: document\n---\nDetails.\n",
        )
        .unwrap();
        assert!(blocked_tasks(&root, &config, &config_revision).unwrap()[&key].needs_revalidation);
        assert_eq!(
            std::fs::read_to_string(root.join(&task.target.vault_path)).unwrap(),
            target
        );
        assert_eq!(
            std::fs::read_to_string(&result_path).unwrap(),
            snapshots[0].raw
        );
        #[cfg(unix)]
        let snapshots = {
            std::os::unix::fs::symlink(&root, root.join("wiki/documents/root-alias.md")).unwrap();
            let result = lk_queue::SectionResult {
                context_revision: None,
                task_id: task.task_id.clone(),
                cache_hash: task.cache_hash.clone(),
                target_revision: lk_core::concept::file_revision(&target),
                body: "See [alias](root-alias.md) and [root](/).".into(),
            };
            std::fs::write(&result_path, serde_json::to_string(&result).unwrap()).unwrap();
            let snapshots =
                super::super::application::capture_group(&root, &[(result_path.clone(), result)])
                    .unwrap();
            let (_, scope) = record(
                &session,
                &config,
                &config_revision,
                &task,
                &snapshots,
                &PublicationRejection {
                    kind: RejectionKind::InvalidResultLink,
                    detail: "Destination is not a page.".into(),
                },
            )
            .unwrap();
            assert!(scope.is_valid());
            assert!(scope.owners.iter().all(|owner| !owner.is_empty()));
            let raw = std::fs::read_to_string(root.join(&receipt_path)).unwrap();
            decode(Path::new(&receipt_path), &raw).unwrap();
            assert!(
                !blocked_tasks(&root, &config, &config_revision).unwrap()[&key].needs_revalidation
            );
            snapshots
        };
        let mut retirement = lk_vault::MutationPlan::default();
        remove_matching(&root, &task, &mut retirement).unwrap();
        retirement.remove(&snapshots[0].proposal.path, snapshots[0].raw.clone());
        session.apply(retirement).unwrap();
        assert!(!root.join(receipt_path).exists());
        assert!(!result_path.exists());
    }

    fn write(root: &Path, relative: &Path, raw: &str) {
        let full = root.join(relative);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, raw).unwrap();
    }

    fn retained_summary(root: &Path) -> lk_queue::QueueTask {
        lk_queue::QueueLlmClient::new(root.join(".lorekeeper/queue")).build_task(
            "summary",
            lk_queue::TaskKind::Summarize,
            lk_queue::SummarizeRequest {
                text: "Original observations.".into(),
                max_sentences: 3,
                focus: None,
                locale: lk_core::i18n::Locale::En,
                source_type: Some(lk_core::config::SourceType::Manual),
                target: lk_queue::TaskTarget {
                    vault_path: "wiki/documents/source.md".into(),
                    kind: lk_queue::TargetKind::DocumentSummary,
                    anchor: "## Summary".into(),
                },
            },
        )
    }

    #[test]
    fn stale_then_orphaned_rejection_is_prunable_only_after_both_references_are_absent() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("wiki/documents")).unwrap();
        let task = retained_summary(&root);
        let mut receipt = receipt();
        receipt.identity.task_id = task.task_id.clone();
        receipt.identity.cache_hash = task.cache_hash.clone();
        let receipt_path = path(&task.task_id, &task.cache_hash).unwrap();
        let raw = serde_json::to_string(&receipt).unwrap();
        write(&root, &receipt_path, &raw);
        write(
            &root,
            Path::new(".lorekeeper/queue/pending.jsonl"),
            &serde_json::to_string(&task).unwrap(),
        );
        let result_path = Path::new(".lorekeeper/queue/results/proposal.section.json");
        write(
            &root,
            result_path,
            &serde_json::to_string(&lk_queue::SectionResult {
                context_revision: None,
                task_id: task.task_id.clone(),
                cache_hash: task.cache_hash.clone(),
                target_revision: "captured".into(),
                body: "Old interpretation.".into(),
            })
            .unwrap(),
        );
        let summary =
            super::super::prune_queue(&root, &root.join(".lorekeeper/queue"), false).unwrap();
        assert_eq!(summary.pruned_missing_target, 1);
        assert_eq!(
            std::fs::read_to_string(root.join(&receipt_path)).unwrap(),
            raw
        );
        super::super::synthesis::quarantine_orphan(
            &root.join(".lorekeeper/queue"),
            &root.join(result_path),
            "no matching pending task",
        )
        .unwrap();
        assert_eq!(prune(&root, None).unwrap(), 1);
        assert_eq!(
            std::fs::read_to_string(root.join(&receipt_path)).unwrap(),
            raw
        );
        let session = lk_vault::MutationSession::acquire(&root).unwrap();
        assert_eq!(prune(&root, Some(&session)).unwrap(), 1);
        assert!(!root.join(receipt_path).exists());
    }

    #[test]
    fn receipt_pruning_preserves_processed_identity_and_refuses_unknown_inventories() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let task = retained_summary(&root);
        let mut receipt = receipt();
        receipt.identity.task_id = task.task_id.clone();
        receipt.identity.cache_hash = task.cache_hash.clone();
        let receipt_path = path(&task.task_id, &task.cache_hash).unwrap();
        let raw = serde_json::to_string(&receipt).unwrap();
        write(&root, &receipt_path, &raw);
        let retained = Path::new(".lorekeeper/queue/processed/old.jsonl");
        write(&root, retained, &serde_json::to_string(&task).unwrap());
        assert_eq!(prune(&root, None).unwrap(), 0);
        let mut invalid = task.clone();
        invalid.cache_hash = "invalid".into();
        write(&root, retained, &serde_json::to_string(&invalid).unwrap());
        assert!(prune(&root, None).is_err());
        assert_eq!(
            std::fs::read_to_string(root.join(&receipt_path)).unwrap(),
            raw
        );
        write(&root, retained, "{unknown");
        assert!(prune(&root, None).is_err());
        assert_eq!(
            std::fs::read_to_string(root.join(&receipt_path)).unwrap(),
            raw
        );
        std::fs::remove_file(root.join(retained)).unwrap();
        let unknown_result = Path::new(".lorekeeper/queue/results/unknown.json");
        write(&root, unknown_result, "{unknown");
        assert!(prune(&root, None).is_err());
        assert_eq!(
            std::fs::read_to_string(root.join(&receipt_path)).unwrap(),
            raw
        );
        std::fs::remove_file(root.join(unknown_result)).unwrap();
        write(&root, &receipt_path, "{}");
        assert!(prune(&root, None).is_err());
        assert_eq!(
            std::fs::read_to_string(root.join(receipt_path)).unwrap(),
            "{}"
        );
    }

    #[test]
    fn concept_grounding_does_not_invent_links_relative_to_the_origin_page() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let config_path = root.join("config.yaml");
        std::fs::write(&config_path, "vault:\n  root: .\n  locale: en\nidentity: {name: Test, email: test@example.com}\nsources:\n  notes:\n    type: manual\n    params: {inbox_dir: inbox}\nexecution:\n  work: {extraction_context: full-context}\n").unwrap();
        let (config, config_revision) =
            lk_core::config::Config::load_with_revision(&config_path).unwrap();
        let task = lk_queue::QueueLlmClient::new(root.join(".lorekeeper/queue")).build_task(
            "extraction",
            lk_queue::TaskKind::ExtractConcepts,
            lk_queue::ExtractConceptsRequest {
                text: "Original observations.".into(),
                source_id: "notes".into(),
                source_type: lk_core::config::SourceType::Manual,
                date: jiff::civil::date(2026, 10, 22),
                focus: None,
                locale: lk_core::i18n::Locale::En,
                categories: vec![],
                rss_original: None,
                concept_policy: lk_core::config::ConceptPolicy::SupportedNew,
                target: lk_queue::TaskTarget {
                    vault_path: "wiki/documents/source.md".into(),
                    kind: lk_queue::TargetKind::DocumentConcepts,
                    anchor: "## Related Concepts".into(),
                },
            },
        );
        write(
            &root,
            Path::new(&task.target.vault_path),
            &format!(
                "---\ntype: document\nllm_inputs: {{concepts: {}}}\n---\n## Content\nOriginal observations.\n\n## Related Concepts\n",
                task.cache_hash
            ),
        );
        write(
            &root,
            Path::new(".lorekeeper/queue/pending.jsonl"),
            &serde_json::to_string(&task).unwrap(),
        );
        let result = lk_queue::TaskResult {
            selection: None,
            context_revision: "uncaptured".into(),
            discovery: vec![],
            concept_policy: lk_core::config::ConceptPolicy::SupportedNew,
            target_revision: "captured".into(),
            task_id: task.task_id.clone(),
            cache_hash: task.cache_hash.clone(),
            target: task.target.clone(),
            date: jiff::civil::date(2026, 10, 22),
            concepts: vec![lk_queue::ReportedConcept {
                admission: lk_queue::ConceptAdmission::New,
                rss_evidence: vec![],
                concept: lk_core::concept::ExtractedConcept {
                    name: "Proposed concept".into(),
                    category: None,
                    aliases: vec![],
                },
                synthesis: Some("See [peer](peer.md).".into()),
            }],
        };
        let result_path = Path::new(".lorekeeper/queue/results/proposal.json");
        write(&root, result_path, &serde_json::to_string(&result).unwrap());
        let snapshots =
            super::super::application::capture_group(&root, &[(root.join(result_path), result)])
                .unwrap();
        let before = observe_scope(&root, &config, &config_revision, &task, &snapshots)
            .unwrap()
            .0;
        write(
            &root,
            Path::new("wiki/documents/peer.md"),
            "---\ntype: document\ntitle: Unrelated\n---\nUnrelated content.\n",
        );
        let after = observe_scope(&root, &config, &config_revision, &task, &snapshots)
            .unwrap()
            .0;
        assert_eq!(before, after);
        assert!(!after.owners.contains(&"wiki/documents/peer.md".into()));
    }
}
